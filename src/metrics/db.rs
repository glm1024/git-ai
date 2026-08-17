//! Metrics storage for local history and offline buffering.
//!
//! Uploadable metrics are buffered here. Raw transcript events are compacted
//! before persistence, and delivered token snapshots are deleted immediately;
//! other delivered metrics remain available as local history. The server
//! handles idempotency.

use crate::config::{REPORTING_PROFILE_VERSION, REPORTING_PROFILE_VERSION_ATTRIBUTE};
use crate::error::GitAiError;
use crate::metrics::attrs::attr_pos;
use crate::metrics::events::{
    SessionTokenUsageValues, checkpoint_pos, otel_trace_pos, session_event_pos,
};
use crate::metrics::pos_encoded::sparse_get_string;
use crate::metrics::session_compaction::{SessionObservation, compact_session_event};
use crate::metrics::types::{MetricEvent, MetricEventId, SparseArray};
use crate::utils::LockFile;
use chrono::{Local, TimeZone};
use rusqlite::{Connection, OptionalExtension, Transaction, params, params_from_iter};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Current schema version (must match MIGRATIONS.len())
const SCHEMA_VERSION: usize = 15;

// This value is part of the metrics retry index schema. Changing it requires a
// migration that rebuilds `metrics_retryable` with the same literal used by
// the retry queries below; SQLite cannot prove a parameterized predicate
// implies a partial-index predicate.
const MAX_METRIC_UPLOAD_ATTEMPTS: u32 = 6;
const METRIC_PROCESSING_LOCK_TIMEOUT_SECS: u64 = 10 * 60;
pub(crate) const METADATA_BACKFILL_BATCH_SIZE: usize = 1000;
const EVENT_METADATA_BACKFILL_COMPLETED_KEY: &str = "event_metadata_backfill_completed";
const NS_PER_SECOND: u128 = 1_000_000_000;
/// Leave one MiB for the request envelope below the server's 8 MiB limit.
pub(crate) const MAX_METRICS_UPLOAD_BODY_BYTES: usize = 7 * 1024 * 1024;
const LEGACY_CONTENT_COMPACTION_BATCH_SIZE: usize = 128;
const SCHEMA_MIGRATION_LOCK_WAIT: Duration = Duration::from_secs(30);
const SCHEMA_MIGRATION_LOCK_POLL: Duration = Duration::from_millis(50);
/// Wrapper processes can fall back to the same SQLite file while the daemon is
/// finishing a write. Wait through short cross-process writer contention rather
/// than turning a durable telemetry handoff into a spurious failure.
const METRICS_SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

const LEGACY_CONTENT_INSERT_GUARD_SQL: &str = r#"
    CREATE TRIGGER IF NOT EXISTS metrics_reject_legacy_content_insert
    BEFORE INSERT ON metrics
    FOR EACH ROW
    WHEN COALESCE(
        NEW.event_kind,
        CASE WHEN json_valid(NEW.event_json)
             THEN json_extract(NEW.event_json, '$.e') END
    ) IN (5, 6)
    BEGIN
        SELECT RAISE(ABORT, 'raw session content events are disabled');
    END;
"#;

const RETRYABLE_METRIC_IDS_SQL: &str = "SELECT id, length(CAST(event_json AS BLOB)) FROM metrics \
     WHERE delivered_ts IS NULL \
       AND processing_started_at IS NULL \
       AND next_retry_at <= ?1 \
       AND attempts < 6 \
     ORDER BY id ASC \
     LIMIT ?2";

/// Database migrations - each migration upgrades the schema by one version
const MIGRATIONS: &[&str] = &[
    // Migration 0 -> 1: Initial schema with metrics table
    r#"
    CREATE TABLE IF NOT EXISTS metrics (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        event_json TEXT NOT NULL
    );
    "#,
    // Migration 1 -> 2: Persistent rate limiter state for agent_usage events
    r#"
    CREATE TABLE IF NOT EXISTS agent_usage_throttle (
        prompt_id TEXT PRIMARY KEY,
        last_sent_ts INTEGER NOT NULL
    );
    "#,
    // Migration 2 -> 3: Keep delivered metrics and add row-level retry state.
    r#"
    CREATE INDEX IF NOT EXISTS metrics_pending_retry
        ON metrics (delivered_ts, next_retry_at, id)
        WHERE delivered_ts IS NULL;

    CREATE INDEX IF NOT EXISTS metrics_processing_started_at
        ON metrics (processing_started_at)
        WHERE delivered_ts IS NULL AND processing_started_at IS NOT NULL;
    "#,
    // Migration 3 -> 4: Cache event metadata for efficient history/backfill queries.
    r#"
    CREATE INDEX IF NOT EXISTS metrics_event_ts_kind
        ON metrics (event_ts, event_kind, id)
        WHERE event_ts IS NOT NULL AND event_kind IS NOT NULL;

    CREATE INDEX IF NOT EXISTS metrics_session_kind_ts
        ON metrics (session_id, event_kind, event_ts, id)
        WHERE session_id IS NOT NULL
            AND event_kind IS NOT NULL
            AND event_ts IS NOT NULL;

    CREATE INDEX IF NOT EXISTS metrics_parent_session_kind_ts
        ON metrics (parent_session_id, event_kind, event_ts, id)
        WHERE parent_session_id IS NOT NULL
            AND event_kind IS NOT NULL
            AND event_ts IS NOT NULL;
    "#,
    // Migration 4 -> 5: Keep terminal history out of retry lookups. The
    // predicate and ordering intentionally match dequeue/count queries.
    r#"
    CREATE INDEX IF NOT EXISTS metrics_retryable
        ON metrics (next_retry_at ASC, id DESC)
        WHERE delivered_ts IS NULL
            AND processing_started_at IS NULL
            AND attempts < 6;

    DROP INDEX IF EXISTS metrics_pending_retry;
    "#,
    // Migration 5 -> 6: Store content-free session activity, short-lived
    // attribution recovery markers, and deduplicated token source watermarks.
    r#"
    CREATE TABLE IF NOT EXISTS session_activity (
        session_id TEXT PRIMARY KEY NOT NULL,
        first_ts INTEGER NOT NULL,
        last_ts INTEGER NOT NULL,
        tool TEXT NOT NULL,
        model TEXT,
        repo_url TEXT,
        external_session_id TEXT
    );

    CREATE INDEX IF NOT EXISTS session_activity_last_ts
        ON session_activity (last_ts, repo_url, tool);

    CREATE TABLE IF NOT EXISTS session_recovery_events (
        event_key TEXT PRIMARY KEY NOT NULL,
        event_ts INTEGER NOT NULL,
        session_id TEXT NOT NULL,
        trace_id TEXT,
        tool TEXT NOT NULL,
        model TEXT,
        external_session_id TEXT NOT NULL,
        external_event_id TEXT,
        external_parent_event_id TEXT,
        external_tool_use_id TEXT,
        repo_url TEXT
    );

    CREATE INDEX IF NOT EXISTS session_recovery_event_ts
        ON session_recovery_events (event_ts, session_id);
    CREATE INDEX IF NOT EXISTS session_recovery_tool_latest
        ON session_recovery_events (tool, event_ts DESC);

    CREATE TABLE IF NOT EXISTS session_token_sources (
        source_key TEXT PRIMARY KEY NOT NULL,
        session_id TEXT NOT NULL,
        first_ts INTEGER NOT NULL,
        last_ts INTEGER NOT NULL,
        tool TEXT NOT NULL,
        model TEXT,
        provider TEXT,
        repo_url TEXT,
        input_tokens INTEGER NOT NULL DEFAULT 0,
        output_tokens INTEGER NOT NULL DEFAULT 0,
        cache_read_tokens INTEGER NOT NULL DEFAULT 0,
        cache_write_tokens INTEGER NOT NULL DEFAULT 0,
        cumulative_source INTEGER NOT NULL DEFAULT 0
    );

    CREATE INDEX IF NOT EXISTS session_token_sources_history
        ON session_token_sources (last_ts, repo_url, tool);

    CREATE TABLE IF NOT EXISTS session_token_daily (
        bucket_key TEXT PRIMARY KEY NOT NULL,
        date_key TEXT NOT NULL,
        timezone TEXT NOT NULL,
        machine_id TEXT NOT NULL,
        first_ts INTEGER NOT NULL,
        last_ts INTEGER NOT NULL,
        attrs_json TEXT NOT NULL,
        provider TEXT,
        request_count INTEGER NOT NULL DEFAULT 0,
        input_tokens INTEGER NOT NULL DEFAULT 0,
        output_tokens INTEGER NOT NULL DEFAULT 0,
        cache_read_tokens INTEGER NOT NULL DEFAULT 0,
        cache_write_tokens INTEGER NOT NULL DEFAULT 0,
        outbox_metric_id INTEGER
    );

    CREATE INDEX IF NOT EXISTS session_token_daily_history
        ON session_token_daily (last_ts);
    "#,
    // Migration 6 -> 7: prevent this and older binaries from writing new raw
    // transcript rows while compact_legacy_content_events() drains existing
    // content.
    LEGACY_CONTENT_INSERT_GUARD_SQL,
    // Migration 7 -> 8: retain the inherited Codex fork baseline separately
    // from the cumulative source watermark so local usage can report only the
    // child session's own contribution. The columns are added idempotently by
    // add_token_baseline_columns() before this migration transaction.
    "",
    // Migration 8 -> 9: identify each cumulative Event9 bucket generation and
    // retain exact supersession aliases until the corrected snapshot is ACKed.
    // Columns are added idempotently by
    // add_token_snapshot_identity_columns() before this transaction.
    r#"
    UPDATE session_token_daily
       SET snapshot_instance_id = lower(hex(randomblob(16)))
     WHERE snapshot_instance_id = '';
    CREATE INDEX IF NOT EXISTS metrics_retryable_fair
        ON metrics (id ASC)
        WHERE delivered_ts IS NULL
          AND processing_started_at IS NULL
          AND attempts < 6;
    "#,
    // Migration 9 -> 10: durable, crash-safe work queue for complete
    // post-commit Event 1 computation outside the latency-sensitive hook path.
    crate::metrics::deferred_commit_jobs::DEFERRED_COMMIT_JOBS_SCHEMA_SQL,
    // Migration 10 -> 11: persist immutable Event 8 ref transitions before
    // rev-list/build work so a side-effect failure remains retryable.
    crate::metrics::deferred_lifecycle_jobs::DEFERRED_LIFECYCLE_JOBS_SCHEMA_SQL,
    // Migration 11 -> 12: persist checkpoint requests and their exact prepared
    // working-log/metric side effects before publishing either durability domain.
    crate::metrics::deferred_checkpoint_jobs::DEFERRED_CHECKPOINT_JOBS_SCHEMA_SQL,
    // Migration 12 -> 13: session_activity reporting identity columns are
    // added idempotently by add_session_reporting_identity_columns(). Existing
    // sessions start ambiguous because their historical profile cannot be
    // reconstructed from compact rows.
    "",
    // Migration 13 -> 14: permanently retain checkpoint path scope and
    // evidence-blocking diagnostics. Columns are added idempotently before the
    // index is rebuilt so interrupted upgrades can safely resume.
    crate::metrics::deferred_checkpoint_jobs::DEFERRED_CHECKPOINT_RECOVERY_INDEX_SQL,
    // Migration 14 -> 15: distinguish an explicit, evidence-backed operator
    // abandonment from normal completion. Columns are added idempotently before
    // this empty transactional migration.
    "",
];

/// Global database singleton
static METRICS_DB: OnceLock<Result<Mutex<MetricsDatabase>, String>> = OnceLock::new();

/// Record returned from database queries
#[derive(Debug, Clone)]
pub struct MetricRecord {
    pub id: i64,
    pub event_json: String,
    pub attempts: u32,
    pub next_retry_at: u64,
}

/// Record returned for local usage aggregation from the metrics table.
#[derive(Debug, Clone)]
pub struct MetricHistoryRecord {
    pub event_id: u16,
    pub ts: u32,
    pub repo_url: Option<String>,
    pub event: MetricEvent,
}

/// One content-free session row used by `git-ai usage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompactSessionRecord {
    pub session_id: String,
    pub first_ts: u32,
    pub last_ts: u32,
    pub tool: String,
    pub model: Option<String>,
    pub repo_url: Option<String>,
}

/// Deduplicated absolute token counters for one assistant message or one
/// cumulative session source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompactTokenRecord {
    pub source_key: String,
    pub session_id: String,
    pub usage_ts: u32,
    pub model: Option<String>,
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub cumulative_source: bool,
    pub repo_url: Option<String>,
}

struct PendingDailyTokenDelta {
    bucket_key: String,
    recoverable_anonymous_buckets: BTreeMap<String, String>,
    identified: bool,
    date_key: String,
    timezone: String,
    machine_id: String,
    sequence: usize,
    timestamp: u32,
    attrs: SparseArray,
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    request_count: u32,
    provider: Option<String>,
}

struct DailyTokenIdentityRow {
    bucket_key: String,
    date_key: String,
    timezone: String,
    machine_id: String,
    attrs: SparseArray,
    provider: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionEventRecoveryCandidate {
    pub row_id: i64,
    pub event_ts: u32,
    pub session_id: String,
    pub trace_id: Option<String>,
    pub tool: String,
    pub model: Option<String>,
    pub external_session_id: String,
    pub external_tool_use_id: Option<String>,
    pub repo_url: Option<String>,
}

/// Point-in-time status summary for local metric delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetricsStatus {
    pub total: usize,
    pub delivered: usize,
    pub not_delivered: usize,
    pub pending_retryable: usize,
    pub waiting_retry: usize,
    pub processing: usize,
    pub stopped_after_errors: usize,
    pub rows_with_errors: usize,
    pub latest_error: Option<String>,
}

/// Summary returned by event metadata backfill work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MetricMetadataBackfillSummary {
    pub scanned: usize,
    pub updated: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MetricEventMetadata {
    event_ts: u32,
    event_kind: u16,
    trace_id: Option<String>,
    session_id: Option<String>,
    parent_session_id: Option<String>,
    tool: Option<String>,
    external_session_id: Option<String>,
    external_parent_session_id: Option<String>,
    external_event_id: Option<String>,
    external_parent_event_id: Option<String>,
    external_tool_use_id: Option<String>,
}

/// Database wrapper for metrics storage
pub struct MetricsDatabase {
    conn: Connection,
}

impl MetricsDatabase {
    /// How long delivered metric rows are retained as local history (365 days).
    /// Undelivered rows are formal facts and remain until a server receipt exists.
    const METRICS_RETENTION_SECS: u64 = 365 * 24 * 3600;
    /// Recovery markers are needed only near the file mutation they explain.
    const RECOVERY_RETENTION_SECS: u64 = 7 * 24 * 3600;
    /// Minimum interval between prune passes (24 hours).
    const METRICS_PRUNE_INTERVAL_SECS: u64 = 24 * 3600;

    pub(crate) fn deferred_jobs_connection(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// Get or initialize the global database
    pub fn global() -> Result<&'static Mutex<MetricsDatabase>, GitAiError> {
        match METRICS_DB.get_or_init(|| Self::new().map(Mutex::new).map_err(|e| e.to_string())) {
            Ok(db) => Ok(db),
            Err(error) => Err(GitAiError::Generic(format!(
                "Failed to initialize primary metrics database; refusing fallback storage: {}",
                error
            ))),
        }
    }

    /// Create a new database connection
    fn new() -> Result<Self, GitAiError> {
        let db_path = Self::database_path()?;

        // Ensure parent directory exists
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // Open with WAL mode and performance optimizations
        let conn = crate::sqlite::open_with_memory_limits(&db_path)?;
        conn.busy_timeout(METRICS_SQLITE_BUSY_TIMEOUT)?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode=WAL;
            PRAGMA synchronous=NORMAL;
            PRAGMA temp_store=MEMORY;
            PRAGMA auto_vacuum=INCREMENTAL;
            "#,
        )?;

        let mut db = Self { conn };
        db.initialize_schema()?;

        Ok(db)
    }

    #[cfg(test)]
    pub(crate) fn new_temp_for_tests() -> Result<(Self, tempfile::TempDir), GitAiError> {
        let temp_dir = tempfile::TempDir::new()?;
        let db_path = temp_dir.path().join("metrics.db");
        let conn = crate::sqlite::open_with_memory_limits(&db_path)?;
        conn.busy_timeout(METRICS_SQLITE_BUSY_TIMEOUT)?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode=WAL;
            PRAGMA synchronous=NORMAL;
            PRAGMA auto_vacuum=INCREMENTAL;
            "#,
        )?;

        let mut db = Self { conn };
        db.initialize_schema()?;

        Ok((db, temp_dir))
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn open_at_path(path: &std::path::Path) -> Result<Self, GitAiError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = crate::sqlite::open_with_memory_limits(path)?;
        conn.busy_timeout(METRICS_SQLITE_BUSY_TIMEOUT)?;
        conn.execute_batch(
            r#"
            PRAGMA journal_mode=WAL;
            PRAGMA synchronous=NORMAL;
            PRAGMA temp_store=MEMORY;
            PRAGMA auto_vacuum=INCREMENTAL;
            "#,
        )?;

        let mut db = Self { conn };
        db.initialize_schema()?;

        Ok(db)
    }

    /// Get database path: ~/.git-ai/internal/metrics-db
    fn database_path() -> Result<PathBuf, GitAiError> {
        // Allow test override via environment variable
        #[cfg(any(test, feature = "test-support"))]
        if let Ok(test_path) = std::env::var("GIT_AI_TEST_METRICS_DB_PATH") {
            return Ok(PathBuf::from(test_path));
        }

        let home = dirs::home_dir()
            .ok_or_else(|| GitAiError::Generic("Could not determine home directory".to_string()))?;
        Ok(home.join(".git-ai").join("internal").join("metrics-db"))
    }

    /// Initialize schema and handle migrations
    fn initialize_schema(&mut self) -> Result<(), GitAiError> {
        let lock_path = self.schema_migration_lock_path()?;
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let deadline = Instant::now() + SCHEMA_MIGRATION_LOCK_WAIT;
        let _migration_lock = loop {
            match LockFile::try_acquire_result(&lock_path) {
                Ok(Some(lock)) => break lock,
                Ok(None) => {}
                Err(error) => return Err(error.into()),
            }
            if Instant::now() >= deadline {
                return Err(GitAiError::Generic(format!(
                    "Timed out waiting for metrics schema migration lock at {}",
                    lock_path.display()
                )));
            }
            std::thread::sleep(SCHEMA_MIGRATION_LOCK_POLL);
        };

        // Re-read only after taking the process-wide migration lock.
        let version_check: Result<usize, _> = self.conn.query_row(
            "SELECT value FROM schema_metadata WHERE key = 'version'",
            [],
            |row| {
                let version_str: String = row.get(0)?;
                version_str
                    .parse::<usize>()
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
            },
        );

        if let Ok(current_version) = version_check {
            if current_version == SCHEMA_VERSION {
                self.add_deferred_commit_parent_note_column()?;
                self.add_session_reporting_identity_columns()?;
                self.add_deferred_checkpoint_recovery_columns()?;
                self.add_deferred_checkpoint_terminal_columns()?;
                self.ensure_legacy_content_insert_guard()?;
                if self.legacy_content_rows_exist()? {
                    self.compact_legacy_content_events()?;
                }
                crate::metrics::deferred_commit_jobs::compact_done_payloads_on_connection(
                    &mut self.conn,
                    100,
                )?;
                crate::metrics::deferred_lifecycle_jobs::compact_done_payloads_on_connection(
                    &mut self.conn,
                    100,
                )?;
                crate::metrics::deferred_checkpoint_jobs::compact_done_payloads_on_connection(
                    &mut self.conn,
                    100,
                )?;
                return Ok(());
            }
            if current_version > SCHEMA_VERSION {
                return Err(GitAiError::Generic(format!(
                    "Metrics database schema version {} is newer than supported version {}. \
                     Please upgrade git-ai to the latest version.",
                    current_version, SCHEMA_VERSION
                )));
            }
        }

        // Create schema_metadata table
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS schema_metadata (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );
            "#,
        )?;

        // Get current schema version (0 if brand new database)
        let current_version: usize = self
            .conn
            .query_row(
                "SELECT value FROM schema_metadata WHERE key = 'version'",
                [],
                |row| {
                    let version_str: String = row.get(0)?;
                    version_str
                        .parse::<usize>()
                        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
                },
            )
            .unwrap_or(0);

        // Apply all missing migrations sequentially
        for target_version in current_version..SCHEMA_VERSION {
            self.apply_migration(target_version)?;

            // Use an upsert so concurrent initializers do not race on version row creation.
            self.conn.execute(
                r#"
                INSERT INTO schema_metadata (key, value)
                VALUES ('version', ?1)
                ON CONFLICT(key) DO UPDATE SET
                    value = excluded.value
                WHERE CAST(schema_metadata.value AS INTEGER) < CAST(excluded.value AS INTEGER)
                "#,
                params![(target_version + 1).to_string()],
            )?;
        }

        self.ensure_legacy_content_insert_guard()?;
        if self.legacy_content_rows_exist()? {
            self.compact_legacy_content_events()?;
        }
        crate::metrics::deferred_commit_jobs::compact_done_payloads_on_connection(
            &mut self.conn,
            100,
        )?;
        crate::metrics::deferred_lifecycle_jobs::compact_done_payloads_on_connection(
            &mut self.conn,
            100,
        )?;
        crate::metrics::deferred_checkpoint_jobs::compact_done_payloads_on_connection(
            &mut self.conn,
            100,
        )?;

        Ok(())
    }

    fn schema_migration_lock_path(&self) -> Result<PathBuf, GitAiError> {
        let database_path: String = self
            .conn
            .query_row("PRAGMA database_list", [], |row| row.get(2))?;
        if database_path.is_empty() {
            return Err(GitAiError::Generic(
                "Metrics database has no filesystem path for migration locking".to_string(),
            ));
        }
        let mut lock_path = std::ffi::OsString::from(database_path);
        lock_path.push(".migration.lock");
        Ok(PathBuf::from(lock_path))
    }

    fn ensure_legacy_content_insert_guard(&self) -> Result<(), GitAiError> {
        self.conn.execute_batch(LEGACY_CONTENT_INSERT_GUARD_SQL)?;
        Ok(())
    }

    fn legacy_content_rows_exist(&self) -> Result<bool, GitAiError> {
        let exists: i64 = self.conn.query_row(
            r#"
            SELECT EXISTS(
                SELECT 1
                FROM metrics
                WHERE COALESCE(
                    event_kind,
                    CASE WHEN json_valid(event_json)
                         THEN json_extract(event_json, '$.e') END
                ) IN (?1, ?2)
                LIMIT 1
            )
            "#,
            params![
                MetricEventId::SessionEvent as i64,
                MetricEventId::OtelTrace as i64
            ],
            |row| row.get(0),
        )?;
        Ok(exists != 0)
    }

    /// Apply a single migration
    fn apply_migration(&mut self, from_version: usize) -> Result<(), GitAiError> {
        if from_version >= MIGRATIONS.len() {
            return Err(GitAiError::Generic(format!(
                "No migration defined for version {} -> {}",
                from_version,
                from_version + 1
            )));
        }

        if from_version == 2 {
            self.add_row_level_retry_columns()?;
        }
        if from_version == 3 {
            self.add_event_metadata_columns()?;
        }
        if from_version == 7 {
            self.add_token_baseline_columns()?;
        }
        if from_version == 8 {
            self.add_token_snapshot_identity_columns()?;
        }
        if from_version == 10 {
            self.add_deferred_commit_parent_note_column()?;
        }
        if from_version == 13 {
            self.add_deferred_checkpoint_recovery_columns()?;
        }
        if from_version == 14 {
            self.add_deferred_checkpoint_terminal_columns()?;
        }
        // Version 6 is the first schema containing session_activity. Add the
        // columns before compacting legacy content so those rows can establish
        // identity from their own historical attrs. Version 12 upgrades
        // already-compacted sessions conservatively as legacy-ambiguous.
        if from_version == 6 || from_version == 12 {
            self.add_session_reporting_identity_columns()?;
        }

        let migration_sql = MIGRATIONS[from_version];
        let tx = self.conn.transaction()?;
        tx.execute_batch(migration_sql)?;
        tx.commit()?;

        if from_version == 6 {
            self.compact_legacy_content_events()?;
        }

        Ok(())
    }

    /// Convert legacy content-bearing transcript and OTEL rows into the same
    /// bounded metadata/token projections used by new streams, then remove the
    /// original JSON. Projection commits before deletion and is idempotent, so
    /// an interrupted upgrade safely resumes on the next startup.
    fn compact_legacy_content_events(&mut self) -> Result<(), GitAiError> {
        let mut compacted = 0usize;
        loop {
            let ids = {
                let mut stmt = self.conn.prepare(
                    r#"
                    SELECT id
                    FROM metrics
                    WHERE COALESCE(
                        event_kind,
                        CASE WHEN json_valid(event_json)
                             THEN json_extract(event_json, '$.e') END
                    ) IN (?1, ?2)
                    ORDER BY id ASC
                    LIMIT ?3
                    "#,
                )?;
                let rows = stmt.query_map(
                    params![
                        MetricEventId::SessionEvent as i64,
                        MetricEventId::OtelTrace as i64,
                        LEGACY_CONTENT_COMPACTION_BATCH_SIZE as i64,
                    ],
                    |row| row.get::<_, i64>(0),
                )?;
                let mut ids = Vec::new();
                for row in rows {
                    ids.push(row?);
                }
                ids
            };
            if ids.is_empty() {
                break;
            }

            let mut observations = Vec::with_capacity(ids.len());
            for id in &ids {
                let event_json: String = self.conn.query_row(
                    "SELECT event_json FROM metrics WHERE id = ?1",
                    params![id],
                    |row| row.get(0),
                )?;
                if let Some(observation) = compact_legacy_content_event(&event_json) {
                    observations.push(observation);
                }
            }
            self.insert_session_observations(&observations)?;

            let tx = self.conn.transaction()?;
            {
                let mut delete = tx.prepare_cached("DELETE FROM metrics WHERE id = ?1")?;
                for id in &ids {
                    delete.execute(params![id])?;
                }
            }
            tx.commit()?;
            compacted += ids.len();
        }

        self.conn.execute(
            "INSERT OR REPLACE INTO schema_metadata (key, value) VALUES ('legacy_content_rows_compacted', ?1)",
            params![compacted.to_string()],
        )?;
        let _ = self
            .conn
            .execute_batch("PRAGMA incremental_vacuum(1024); PRAGMA wal_checkpoint(TRUNCATE);");
        Ok(())
    }

    fn add_row_level_retry_columns(&mut self) -> Result<(), GitAiError> {
        for (name, sql) in [
            (
                "delivered_ts",
                "ALTER TABLE metrics ADD COLUMN delivered_ts INTEGER",
            ),
            (
                "attempts",
                "ALTER TABLE metrics ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0",
            ),
            (
                "last_sync_error",
                "ALTER TABLE metrics ADD COLUMN last_sync_error TEXT",
            ),
            (
                "last_sync_at",
                "ALTER TABLE metrics ADD COLUMN last_sync_at INTEGER",
            ),
            (
                "next_retry_at",
                "ALTER TABLE metrics ADD COLUMN next_retry_at INTEGER NOT NULL DEFAULT 0",
            ),
            (
                "processing_started_at",
                "ALTER TABLE metrics ADD COLUMN processing_started_at INTEGER",
            ),
        ] {
            self.add_column_if_missing("metrics", name, sql)?;
        }
        Ok(())
    }

    fn add_event_metadata_columns(&mut self) -> Result<(), GitAiError> {
        for (name, sql) in [
            (
                "event_ts",
                "ALTER TABLE metrics ADD COLUMN event_ts INTEGER DEFAULT NULL",
            ),
            (
                "event_kind",
                "ALTER TABLE metrics ADD COLUMN event_kind INTEGER DEFAULT NULL",
            ),
            (
                "trace_id",
                "ALTER TABLE metrics ADD COLUMN trace_id TEXT DEFAULT NULL",
            ),
            (
                "session_id",
                "ALTER TABLE metrics ADD COLUMN session_id TEXT DEFAULT NULL",
            ),
            (
                "parent_session_id",
                "ALTER TABLE metrics ADD COLUMN parent_session_id TEXT DEFAULT NULL",
            ),
            (
                "tool",
                "ALTER TABLE metrics ADD COLUMN tool TEXT DEFAULT NULL",
            ),
            (
                "external_session_id",
                "ALTER TABLE metrics ADD COLUMN external_session_id TEXT DEFAULT NULL",
            ),
            (
                "external_parent_session_id",
                "ALTER TABLE metrics ADD COLUMN external_parent_session_id TEXT DEFAULT NULL",
            ),
            (
                "external_event_id",
                "ALTER TABLE metrics ADD COLUMN external_event_id TEXT DEFAULT NULL",
            ),
            (
                "external_parent_event_id",
                "ALTER TABLE metrics ADD COLUMN external_parent_event_id TEXT DEFAULT NULL",
            ),
            (
                "external_tool_use_id",
                "ALTER TABLE metrics ADD COLUMN external_tool_use_id TEXT DEFAULT NULL",
            ),
        ] {
            self.add_column_if_missing("metrics", name, sql)?;
        }
        Ok(())
    }

    fn add_token_baseline_columns(&mut self) -> Result<(), GitAiError> {
        for (name, sql) in [
            (
                "baseline_input_tokens",
                "ALTER TABLE session_token_sources ADD COLUMN baseline_input_tokens INTEGER NOT NULL DEFAULT 0",
            ),
            (
                "baseline_output_tokens",
                "ALTER TABLE session_token_sources ADD COLUMN baseline_output_tokens INTEGER NOT NULL DEFAULT 0",
            ),
            (
                "baseline_cache_read_tokens",
                "ALTER TABLE session_token_sources ADD COLUMN baseline_cache_read_tokens INTEGER NOT NULL DEFAULT 0",
            ),
            (
                "baseline_cache_write_tokens",
                "ALTER TABLE session_token_sources ADD COLUMN baseline_cache_write_tokens INTEGER NOT NULL DEFAULT 0",
            ),
        ] {
            self.add_column_if_missing("session_token_sources", name, sql)?;
        }
        Ok(())
    }

    fn add_token_snapshot_identity_columns(&mut self) -> Result<(), GitAiError> {
        for (name, sql) in [
            (
                "snapshot_instance_id",
                "ALTER TABLE session_token_daily ADD COLUMN snapshot_instance_id TEXT NOT NULL DEFAULT ''",
            ),
            (
                "supersedes_source_keys_json",
                "ALTER TABLE session_token_daily ADD COLUMN supersedes_source_keys_json TEXT NOT NULL DEFAULT '[]'",
            ),
            (
                "supersedes_snapshot_instance_ids_json",
                "ALTER TABLE session_token_daily ADD COLUMN supersedes_snapshot_instance_ids_json TEXT NOT NULL DEFAULT '[]'",
            ),
        ] {
            self.add_column_if_missing("session_token_daily", name, sql)?;
        }
        Ok(())
    }

    fn add_deferred_commit_parent_note_column(&mut self) -> Result<(), GitAiError> {
        self.add_column_if_missing(
            "deferred_commit_metric_jobs",
            "parent_authorship_note",
            "ALTER TABLE deferred_commit_metric_jobs \
             ADD COLUMN parent_authorship_note TEXT NOT NULL DEFAULT ''",
        )
    }

    fn add_session_reporting_identity_columns(&mut self) -> Result<(), GitAiError> {
        for (name, sql) in [
            (
                "reporting_identity_email",
                "ALTER TABLE session_activity ADD COLUMN reporting_identity_email TEXT",
            ),
            (
                "reporting_identity_state",
                "ALTER TABLE session_activity ADD COLUMN reporting_identity_state TEXT NOT NULL DEFAULT 'legacy_ambiguous'",
            ),
        ] {
            self.add_column_if_missing("session_activity", name, sql)?;
        }
        Ok(())
    }

    fn add_deferred_checkpoint_recovery_columns(&mut self) -> Result<(), GitAiError> {
        for (name, sql) in [
            (
                "path_scope_json",
                "ALTER TABLE deferred_checkpoint_jobs ADD COLUMN path_scope_json TEXT",
            ),
            (
                "admission_owner",
                "ALTER TABLE deferred_checkpoint_jobs ADD COLUMN admission_owner TEXT",
            ),
            (
                "blocked_evidence",
                "ALTER TABLE deferred_checkpoint_jobs ADD COLUMN blocked_evidence INTEGER NOT NULL DEFAULT 0 CHECK (blocked_evidence IN (0, 1))",
            ),
            (
                "blocked_reason",
                "ALTER TABLE deferred_checkpoint_jobs ADD COLUMN blocked_reason TEXT",
            ),
        ] {
            self.add_column_if_missing("deferred_checkpoint_jobs", name, sql)?;
        }
        Ok(())
    }

    fn add_deferred_checkpoint_terminal_columns(&mut self) -> Result<(), GitAiError> {
        for (name, sql) in [
            (
                "terminal_resolution",
                "ALTER TABLE deferred_checkpoint_jobs ADD COLUMN terminal_resolution TEXT NOT NULL DEFAULT 'normal' CHECK (terminal_resolution IN ('normal', 'manual_abandoned'))",
            ),
            (
                "repair_id",
                "ALTER TABLE deferred_checkpoint_jobs ADD COLUMN repair_id TEXT",
            ),
            (
                "repair_backup_path",
                "ALTER TABLE deferred_checkpoint_jobs ADD COLUMN repair_backup_path TEXT",
            ),
        ] {
            self.add_column_if_missing("deferred_checkpoint_jobs", name, sql)?;
        }
        Ok(())
    }

    fn add_column_if_missing(
        &mut self,
        table: &str,
        column: &str,
        alter_sql: &str,
    ) -> Result<(), GitAiError> {
        if self.column_exists(table, column)? {
            return Ok(());
        }

        match self.conn.execute(alter_sql, []) {
            Ok(_) => Ok(()),
            Err(rusqlite::Error::SqliteFailure(_, Some(message)))
                if message.contains("duplicate column name") =>
            {
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    fn column_exists(&self, table: &str, column: &str) -> Result<bool, GitAiError> {
        let count: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"),
            params![column],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Insert undelivered events as JSON strings.
    pub fn insert_events(&mut self, events: &[String]) -> Result<Vec<i64>, GitAiError> {
        self.insert_events_with_delivered_ts(events, None)
    }

    /// Insert events as JSON strings, optionally marking them delivered immediately.
    pub fn insert_events_with_delivered_ts(
        &mut self,
        events: &[String],
        delivered_ts: Option<u64>,
    ) -> Result<Vec<i64>, GitAiError> {
        if events.is_empty() {
            return Ok(Vec::new());
        }

        let tx = self.conn.transaction()?;
        let ids = insert_event_jsons_in_transaction(&tx, events, delivered_ts)?;
        tx.commit()?;
        self.prune_old_metrics_if_due()?;
        Ok(ids)
    }

    /// Persist one transcript batch as bounded, content-free local state.
    ///
    /// Raw transcript JSON never enters SQLite. Every event contributes only a
    /// short-lived recovery marker and a per-session first/last watermark. Token
    /// snapshots are deduplicated by their stable source key. Positive deltas
    /// update one cumulative day/dimension row, and the upload outbox keeps at
    /// most one mutable pending snapshot per row while the backend is offline.
    pub(crate) fn insert_session_observations(
        &mut self,
        observations: &[SessionObservation],
    ) -> Result<Vec<i64>, GitAiError> {
        if observations.is_empty() {
            return Ok(Vec::new());
        }

        let tx = self.conn.transaction()?;
        let mut metric_ids = Vec::new();
        let mut pending_token_deltas: BTreeMap<String, PendingDailyTokenDelta> = BTreeMap::new();
        let machine_id = token_snapshot_machine_id();

        for (observation_sequence, observation) in observations.iter().enumerate() {
            let Some(session_id) =
                sparse_get_string(&observation.attrs, attr_pos::SESSION_ID).flatten()
            else {
                continue;
            };
            if session_id.is_empty() {
                continue;
            }
            let tool = sparse_get_string(&observation.attrs, attr_pos::TOOL)
                .flatten()
                .unwrap_or_else(|| "unknown".to_string());
            let model = sparse_get_string(&observation.attrs, attr_pos::MODEL).flatten();
            let repo_url = sparse_get_string(&observation.attrs, attr_pos::REPO_URL).flatten();
            let trace_id = sparse_get_string(&observation.attrs, attr_pos::TRACE_ID).flatten();
            let external_session_id =
                sparse_get_string(&observation.attrs, attr_pos::EXTERNAL_SESSION_ID)
                    .flatten()
                    .unwrap_or_default();
            let event_ts = i64::from(observation.timestamp);

            tx.execute(
                r#"
                INSERT INTO session_activity (
                    session_id, first_ts, last_ts, tool, model, repo_url,
                    external_session_id, reporting_identity_state
                ) VALUES (?1, ?2, ?2, ?3, ?4, ?5, ?6, 'unbound')
                ON CONFLICT(session_id) DO UPDATE SET
                    first_ts = MIN(session_activity.first_ts, excluded.first_ts),
                    last_ts = MAX(session_activity.last_ts, excluded.last_ts),
                    tool = CASE WHEN excluded.last_ts >= session_activity.last_ts
                        THEN excluded.tool ELSE session_activity.tool END,
                    model = CASE WHEN excluded.last_ts >= session_activity.last_ts
                        THEN COALESCE(excluded.model, session_activity.model) ELSE session_activity.model END,
                    repo_url = COALESCE(session_activity.repo_url, excluded.repo_url),
                    external_session_id = COALESCE(
                        NULLIF(session_activity.external_session_id, ''), excluded.external_session_id)
                "#,
                params![
                    session_id,
                    event_ts,
                    tool,
                    model.as_deref(),
                    repo_url.as_deref(),
                    external_session_id,
                ],
            )?;
            let reporting_identity_allows_recovery =
                observe_session_reporting_identity(&tx, &session_id, &observation.attrs)?;

            if !external_session_id.is_empty() {
                let recovery_key = recovery_event_key(
                    &session_id,
                    observation.timestamp,
                    observation.external_tool_use_id.as_deref(),
                    &tool,
                    model.as_deref(),
                    repo_url.as_deref(),
                );
                tx.execute(
                    r#"
                    INSERT OR IGNORE INTO session_recovery_events (
                        event_key, event_ts, session_id, trace_id, tool, model,
                        external_session_id, external_event_id, external_parent_event_id,
                        external_tool_use_id, repo_url
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                    "#,
                    params![
                        recovery_key,
                        event_ts,
                        session_id,
                        trace_id.as_deref(),
                        tool,
                        model.as_deref(),
                        external_session_id,
                        observation.external_event_id.as_deref(),
                        observation.external_parent_event_id.as_deref(),
                        observation.external_tool_use_id.as_deref(),
                        repo_url.as_deref(),
                    ],
                )?;
            }

            let Some(token) = observation.token.as_ref() else {
                continue;
            };
            let previous = tx
                .query_row(
                    r#"
                    SELECT input_tokens, output_tokens, cache_read_tokens, cache_write_tokens
                    FROM session_token_sources WHERE source_key = ?1
                    "#,
                    params![token.source_key],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?.max(0) as u64,
                            row.get::<_, i64>(1)?.max(0) as u64,
                            row.get::<_, i64>(2)?.max(0) as u64,
                            row.get::<_, i64>(3)?.max(0) as u64,
                        ))
                    },
                )
                .optional()?;
            let (old_input, old_output, old_cache_read, old_cache_write) =
                previous.unwrap_or_default();
            let input_delta = token.input.saturating_sub(old_input);
            let output_delta = token.output.saturating_sub(old_output);
            let cache_read_delta = token.cache_read.saturating_sub(old_cache_read);
            let cache_write_delta = token.cache_write.saturating_sub(old_cache_write);
            let has_positive_delta = input_delta > 0
                || output_delta > 0
                || cache_read_delta > 0
                || cache_write_delta > 0;
            let request_count = if token.baseline_only {
                0
            } else if token.cumulative {
                u32::from(has_positive_delta)
            } else {
                u32::from(previous.is_none())
            };
            let token_model = token.model.as_ref().or(model.as_ref());

            tx.execute(
                r#"
                INSERT INTO session_token_sources (
                    source_key, session_id, first_ts, last_ts, tool, model, provider, repo_url,
                    input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                    cumulative_source, baseline_input_tokens, baseline_output_tokens,
                    baseline_cache_read_tokens, baseline_cache_write_tokens
                ) VALUES (
                    ?1, ?2, ?3, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                    ?13, ?14, ?15, ?16
                )
                ON CONFLICT(source_key) DO UPDATE SET
                    first_ts = MIN(session_token_sources.first_ts, excluded.first_ts),
                    last_ts = MAX(session_token_sources.last_ts, excluded.last_ts),
                    model = COALESCE(NULLIF(session_token_sources.model, ''), excluded.model),
                    provider = COALESCE(NULLIF(session_token_sources.provider, ''), excluded.provider),
                    repo_url = COALESCE(session_token_sources.repo_url, excluded.repo_url),
                    input_tokens = MAX(session_token_sources.input_tokens, excluded.input_tokens),
                    output_tokens = MAX(session_token_sources.output_tokens, excluded.output_tokens),
                    cache_read_tokens = MAX(session_token_sources.cache_read_tokens, excluded.cache_read_tokens),
                    cache_write_tokens = MAX(session_token_sources.cache_write_tokens, excluded.cache_write_tokens),
                    cumulative_source = MAX(session_token_sources.cumulative_source, excluded.cumulative_source),
                    baseline_input_tokens = MAX(session_token_sources.baseline_input_tokens, excluded.baseline_input_tokens),
                    baseline_output_tokens = MAX(session_token_sources.baseline_output_tokens, excluded.baseline_output_tokens),
                    baseline_cache_read_tokens = MAX(session_token_sources.baseline_cache_read_tokens, excluded.baseline_cache_read_tokens),
                    baseline_cache_write_tokens = MAX(session_token_sources.baseline_cache_write_tokens, excluded.baseline_cache_write_tokens)
                "#,
                params![
                    token.source_key,
                    session_id,
                    event_ts,
                    tool,
                    token_model.map(String::as_str),
                    token.provider.as_deref(),
                    repo_url.as_deref(),
                    u64_to_sqlite(token.input),
                    u64_to_sqlite(token.output),
                    u64_to_sqlite(token.cache_read),
                    u64_to_sqlite(token.cache_write),
                    i64::from(token.cumulative),
                    u64_to_sqlite(if token.baseline_only { token.input } else { 0 }),
                    u64_to_sqlite(if token.baseline_only { token.output } else { 0 }),
                    u64_to_sqlite(if token.baseline_only {
                        token.cache_read
                    } else {
                        0
                    }),
                    u64_to_sqlite(if token.baseline_only {
                        token.cache_write
                    } else {
                        0
                    }),
                ],
            )?;

            if token.baseline_only {
                continue;
            }

            let (bucket_key, anonymous_bucket_key, date_key, timezone, snapshot_attrs) =
                daily_token_bucket(
                    observation.timestamp,
                    &observation.attrs,
                    token.provider.as_deref(),
                    &machine_id,
                    &token.source_key,
                    &session_id,
                )?;
            let identified = compact_identity_email(&snapshot_attrs).is_some();
            let has_daily_delta = input_delta > 0
                || output_delta > 0
                || cache_read_delta > 0
                || cache_write_delta > 0
                || request_count > 0;
            let can_recover_existing_anonymous = identified
                && reporting_identity_allows_recovery
                && anonymous_bucket_key != bucket_key;
            if !has_daily_delta {
                if !can_recover_existing_anonymous {
                    continue;
                }
                let anonymous_exists: i64 = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM session_token_daily WHERE bucket_key = ?1)",
                    params![anonymous_bucket_key],
                    |row| row.get(0),
                )?;
                if anonymous_exists == 0 {
                    continue;
                }
            }
            let delta = pending_token_deltas
                .entry(bucket_key.clone())
                .or_insert_with(|| PendingDailyTokenDelta {
                    bucket_key,
                    recoverable_anonymous_buckets: BTreeMap::new(),
                    identified,
                    date_key,
                    timezone: timezone.clone(),
                    machine_id: machine_id.clone(),
                    sequence: observation_sequence,
                    timestamp: observation.timestamp,
                    attrs: snapshot_attrs.clone(),
                    input: 0,
                    output: 0,
                    cache_read: 0,
                    cache_write: 0,
                    request_count: 0,
                    provider: token.provider.clone(),
                });
            if can_recover_existing_anonymous {
                delta
                    .recoverable_anonymous_buckets
                    .insert(anonymous_bucket_key, session_id.clone());
            }
            delta.input = delta.input.saturating_add(input_delta);
            delta.output = delta.output.saturating_add(output_delta);
            delta.cache_read = delta.cache_read.saturating_add(cache_read_delta);
            delta.cache_write = delta.cache_write.saturating_add(cache_write_delta);
            delta.request_count = delta.request_count.saturating_add(request_count);
            delta.sequence = observation_sequence;
            if delta.provider.is_none() {
                delta.provider = token.provider.clone();
            }
            if observation.timestamp >= delta.timestamp {
                delta.timestamp = observation.timestamp;
                delta.attrs = snapshot_attrs;
                delta.timezone = timezone;
            }
        }

        let mut pending_token_deltas = pending_token_deltas.into_values().collect::<Vec<_>>();
        // Materialize anonymous deltas first, then let a proven same-session
        // identity claim only the corresponding session-scoped bucket.
        pending_token_deltas.sort_by_key(|delta| (delta.identified, Reverse(delta.sequence)));

        for delta in pending_token_deltas {
            let attrs_json = serde_json::to_string(&delta.attrs)?;
            let target_email = compact_identity_email(&delta.attrs);
            for (anonymous_bucket_key, session_id) in &delta.recoverable_anonymous_buckets {
                if !session_reporting_identity_is_bound_to(
                    &tx,
                    session_id,
                    target_email.as_deref(),
                )? {
                    continue;
                }
                migrate_unknown_daily_token_bucket(
                    &tx,
                    anonymous_bucket_key,
                    &delta.bucket_key,
                    &attrs_json,
                    &delta.timezone,
                    delta.provider.as_deref(),
                )?;
            }
            let snapshot_instance_id = crate::uuid::generate_v4();
            tx.execute(
                r#"
                INSERT INTO session_token_daily (
                    bucket_key, date_key, timezone, machine_id, first_ts, last_ts, attrs_json, provider,
                    request_count, input_tokens, output_tokens, cache_read_tokens,
                    cache_write_tokens, outbox_metric_id, snapshot_instance_id
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, NULL, ?13)
                ON CONFLICT(bucket_key) DO UPDATE SET
                    first_ts = MIN(session_token_daily.first_ts, excluded.first_ts),
                    last_ts = MAX(session_token_daily.last_ts, excluded.last_ts),
                    timezone = CASE WHEN excluded.last_ts >= session_token_daily.last_ts
                        THEN excluded.timezone ELSE session_token_daily.timezone END,
                    attrs_json = CASE WHEN excluded.last_ts >= session_token_daily.last_ts
                        THEN excluded.attrs_json ELSE session_token_daily.attrs_json END,
                    provider = COALESCE(session_token_daily.provider, excluded.provider),
                    request_count = session_token_daily.request_count + excluded.request_count,
                    input_tokens = session_token_daily.input_tokens + excluded.input_tokens,
                    output_tokens = session_token_daily.output_tokens + excluded.output_tokens,
                    cache_read_tokens = session_token_daily.cache_read_tokens + excluded.cache_read_tokens,
                    cache_write_tokens = session_token_daily.cache_write_tokens + excluded.cache_write_tokens
                "#,
                params![
                    delta.bucket_key,
                    delta.date_key,
                    delta.timezone,
                    delta.machine_id,
                    i64::from(delta.timestamp),
                    attrs_json,
                    delta.provider.as_deref(),
                    i64::from(delta.request_count),
                    u64_to_sqlite(delta.input),
                    u64_to_sqlite(delta.output),
                    u64_to_sqlite(delta.cache_read),
                    u64_to_sqlite(delta.cache_write),
                    snapshot_instance_id,
                ],
            )?;

            metric_ids.push(refresh_daily_token_outbox(&tx, &delta.bucket_key)?);
        }

        tx.commit()?;
        self.prune_compact_session_history_if_due()?;
        Ok(metric_ids)
    }

    /// Insert a content-free session recovery marker for integration tests.
    ///
    /// The test-support API deliberately accepts only compact attributes and
    /// identifiers so tests cannot bypass the raw transcript content guard.
    #[cfg(feature = "test-support")]
    pub fn insert_session_recovery_observation_for_test(
        &mut self,
        timestamp: u32,
        attrs: SparseArray,
        external_event_id: Option<String>,
        external_parent_event_id: Option<String>,
        external_tool_use_id: Option<String>,
    ) -> Result<Vec<i64>, GitAiError> {
        self.insert_session_observations(&[SessionObservation {
            timestamp,
            attrs,
            external_event_id,
            external_parent_event_id,
            external_tool_use_id,
            token: None,
        }])
    }

    /// Report whether compact, content-free activity exists for a session.
    #[cfg(feature = "test-support")]
    pub fn has_compact_session_activity_for_test(
        &self,
        session_id: &str,
    ) -> Result<bool, GitAiError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM session_activity WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Repair legacy project/source-key shape without guessing historical
    /// reporting identity from the process's current configuration.
    ///
    /// Current-revision source keys remain stable. Legacy revisions are re-keyed
    /// only from identity already present in their own persisted attributes;
    /// anonymous historical rows stay anonymous.
    pub(crate) fn repair_daily_token_buckets(&mut self) -> Result<Vec<i64>, GitAiError> {
        let tx = self.conn.transaction()?;
        let rows = {
            let mut stmt = tx.prepare(
                r#"
                SELECT bucket_key, date_key, timezone, machine_id, attrs_json, provider
                FROM session_token_daily
                ORDER BY first_ts ASC, bucket_key ASC
                "#,
            )?;
            let rows = stmt.query_map([], |row| {
                let attrs_json = row.get::<_, String>(4)?;
                let attrs = serde_json::from_str::<SparseArray>(&attrs_json).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        attrs_json.len(),
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
                Ok(DailyTokenIdentityRow {
                    bucket_key: row.get(0)?,
                    date_key: row.get(1)?,
                    timezone: row.get(2)?,
                    machine_id: row.get(3)?,
                    attrs,
                    provider: row.get(5)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        let mut refreshed_ids = BTreeSet::new();
        for row in rows {
            let mut target_attrs = row.attrs.clone();
            let legacy_revision = legacy_daily_token_bucket_revision(&row.bucket_key);
            if legacy_revision && !has_explicit_project_key(&target_attrs) {
                mark_legacy_project_identity_ambiguous(&mut target_attrs, &row.bucket_key)?;
            } else {
                canonicalize_snapshot_repo_url(&mut target_attrs);
            }

            if !legacy_revision {
                if target_attrs != row.attrs {
                    let target_attrs_json = serde_json::to_string(&target_attrs)?;
                    tx.execute(
                        "UPDATE session_token_daily SET attrs_json = ?1, timezone = ?2 \
                         WHERE bucket_key = ?3",
                        params![target_attrs_json, row.timezone, row.bucket_key],
                    )?;
                    refreshed_ids.insert(refresh_daily_token_outbox(&tx, &row.bucket_key)?);
                }
                continue;
            }

            let target_email =
                compact_identity_email(&target_attrs).unwrap_or_else(|| "unknown".to_string());
            let target_attrs_json = serde_json::to_string(&target_attrs)?;
            let target_bucket_key = daily_token_bucket_key(
                latest_daily_token_bucket_revision(&row.bucket_key),
                &row.date_key,
                &target_email,
                &row.machine_id,
                &target_attrs,
                row.provider.as_deref(),
            );

            if row.bucket_key == target_bucket_key {
                if target_attrs != row.attrs {
                    tx.execute(
                        "UPDATE session_token_daily SET attrs_json = ?1, timezone = ?2 \
                         WHERE bucket_key = ?3",
                        params![target_attrs_json, row.timezone, row.bucket_key],
                    )?;
                    refreshed_ids.insert(refresh_daily_token_outbox(&tx, &row.bucket_key)?);
                }
                continue;
            }

            if migrate_daily_token_bucket(
                &tx,
                &row.bucket_key,
                &target_bucket_key,
                &target_attrs_json,
                &row.timezone,
                row.provider.as_deref(),
                false,
            )? {
                refreshed_ids.insert(refresh_daily_token_outbox(&tx, &target_bucket_key)?);
            }
        }

        tx.commit()?;
        Ok(refreshed_ids.into_iter().collect())
    }

    /// Atomically claim a due batch of pending metrics for upload.
    pub fn dequeue_pending_batch(&mut self, limit: usize) -> Result<Vec<MetricRecord>, GitAiError> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let now = current_unix_ts();
        self.release_stale_processing_locks(now)?;

        let tx = self.conn.transaction()?;
        let ids = {
            let mut stmt = tx.prepare(RETRYABLE_METRIC_IDS_SQL)?;
            let rows = stmt.query_map(params![now as i64, limit as i64], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?.max(0) as usize))
            })?;
            let mut ids = Vec::new();
            let mut selected_bytes = 1024usize;
            for row in rows {
                let (id, event_bytes) = row?;
                let next_bytes = selected_bytes.saturating_add(event_bytes).saturating_add(1);
                if !ids.is_empty() && next_bytes > MAX_METRICS_UPLOAD_BODY_BYTES {
                    break;
                }
                ids.push(id);
                selected_bytes = next_bytes;
            }
            ids
        };

        if ids.is_empty() {
            tx.commit()?;
            return Ok(Vec::new());
        }

        let mut locked_ids = Vec::with_capacity(ids.len());
        {
            let mut stmt = tx.prepare_cached(
                "UPDATE metrics \
                 SET processing_started_at = ?1 \
                 WHERE id = ?2 \
                   AND delivered_ts IS NULL \
                   AND processing_started_at IS NULL",
            )?;
            for id in ids {
                if stmt.execute(params![now as i64, id])? > 0 {
                    locked_ids.push(id);
                }
            }
        }

        let mut records = Vec::with_capacity(locked_ids.len());
        {
            let mut stmt = tx.prepare_cached(
                "SELECT id, event_json, attempts, next_retry_at FROM metrics WHERE id = ?1",
            )?;
            for id in locked_ids {
                records.push(stmt.query_row(params![id], |row| {
                    Ok(MetricRecord {
                        id: row.get(0)?,
                        event_json: row.get(1)?,
                        attempts: row.get::<_, i64>(2)?.max(0) as u32,
                        next_retry_at: row.get::<_, i64>(3)?.max(0) as u64,
                    })
                })?);
            }
        }

        tx.commit()?;
        Ok(records)
    }

    /// Mark records as delivered after a successful upload.
    pub fn mark_records_delivered(
        &mut self,
        ids: &[i64],
        delivered_ts: u64,
    ) -> Result<(), GitAiError> {
        if ids.is_empty() {
            return Ok(());
        }

        let tx = self.conn.transaction()?;

        {
            let mut acknowledge_compact_snapshot = tx.prepare_cached(
                "UPDATE session_token_daily \
                 SET outbox_metric_id = NULL, \
                     supersedes_source_keys_json = '[]', \
                     supersedes_snapshot_instance_ids_json = '[]' \
                 WHERE outbox_metric_id = ?1",
            )?;
            let mut delete_compact =
                tx.prepare_cached("DELETE FROM metrics WHERE id = ?1 AND event_kind = ?2")?;
            let mut stmt = tx.prepare_cached(
                "UPDATE metrics \
                 SET delivered_ts = ?1, processing_started_at = NULL \
                 WHERE id = ?2 AND delivered_ts IS NULL",
            )?;

            for id in ids {
                acknowledge_compact_snapshot.execute(params![id])?;
                if delete_compact.execute(params![id, MetricEventId::SessionTokenUsage as i64])?
                    == 0
                {
                    stmt.execute(params![delivered_ts as i64, id])?;
                }
            }
        }

        tx.commit()?;
        self.prune_old_metrics_if_due()?;
        Ok(())
    }

    /// Mark records as failed and schedule their next row-level retry.
    pub fn mark_records_failed(
        &mut self,
        ids: &[i64],
        error: &str,
        failed_at: u64,
    ) -> Result<(), GitAiError> {
        if ids.is_empty() {
            return Ok(());
        }

        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                r#"
                UPDATE metrics
                SET processing_started_at = NULL,
                    attempts = CASE
                        WHEN event_kind = ?4 THEN MIN(attempts + 1, ?5)
                        ELSE attempts + 1
                    END,
                    last_sync_error = ?1,
                    last_sync_at = ?2,
                    next_retry_at = ?2 + CASE
                        WHEN attempts + 1 <= 1 THEN 300
                        WHEN attempts + 1 = 2 THEN 1800
                        WHEN attempts + 1 = 3 THEN 7200
                        WHEN attempts + 1 = 4 THEN 21600
                        WHEN attempts + 1 = 5 THEN 43200
                        ELSE 86400
                    END
                WHERE id = ?3 AND delivered_ts IS NULL
                "#,
            )?;

            for id in ids {
                stmt.execute(params![
                    error,
                    failed_at as i64,
                    id,
                    MetricEventId::SessionTokenUsage as i64,
                    (MAX_METRIC_UPLOAD_ATTEMPTS - 1) as i64,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Keep recoverable configuration/size failures in the automatic queue.
    ///
    /// The attempt value is capped below the retry-index cutoff while the
    /// schedule still reaches the normal 24-hour maximum backoff.
    pub fn mark_records_deferred(
        &mut self,
        ids: &[i64],
        error: &str,
        failed_at: u64,
    ) -> Result<(), GitAiError> {
        if ids.is_empty() {
            return Ok(());
        }

        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                r#"
                UPDATE metrics
                SET processing_started_at = NULL,
                    attempts = MIN(attempts + 1, ?4),
                    last_sync_error = ?1,
                    last_sync_at = ?2,
                    next_retry_at = ?2 + CASE
                        WHEN attempts + 1 <= 1 THEN 300
                        WHEN attempts + 1 = 2 THEN 1800
                        WHEN attempts + 1 = 3 THEN 7200
                        WHEN attempts + 1 = 4 THEN 21600
                        WHEN attempts + 1 = 5 THEN 43200
                        ELSE 86400
                    END
                WHERE id = ?3 AND delivered_ts IS NULL
                "#,
            )?;
            for id in ids {
                stmt.execute(params![
                    error,
                    failed_at as i64,
                    id,
                    (MAX_METRIC_UPLOAD_ATTEMPTS - 1) as i64,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Mark records as permanently undeliverable while retaining them in history.
    pub fn mark_records_undeliverable(
        &mut self,
        records: &[(i64, String)],
        failed_at: u64,
    ) -> Result<(), GitAiError> {
        if records.is_empty() {
            return Ok(());
        }

        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "UPDATE metrics \
                 SET processing_started_at = NULL, \
                     attempts = ?1, \
                     last_sync_error = ?2, \
                     last_sync_at = ?3, \
                     next_retry_at = ?3 \
                 WHERE id = ?4 AND delivered_ts IS NULL",
            )?;

            for (id, error) in records {
                stmt.execute(params![
                    MAX_METRIC_UPLOAD_ATTEMPTS as i64,
                    error,
                    failed_at as i64,
                    id
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Get count of pending metrics that are currently eligible for upload.
    pub fn count_retryable(&self) -> Result<usize, GitAiError> {
        let now = current_unix_ts();
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM metrics \
             WHERE delivered_ts IS NULL \
               AND processing_started_at IS NULL \
               AND next_retry_at <= ?1 \
               AND attempts < 6",
            params![now as i64],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// Summarize local metrics delivery state for user-facing diagnostics.
    pub fn status(&self) -> Result<MetricsStatus, GitAiError> {
        let now = current_unix_ts();
        let (
            total,
            delivered,
            not_delivered,
            pending_retryable,
            waiting_retry,
            processing,
            stopped_after_errors,
            rows_with_errors,
        ): (i64, i64, i64, i64, i64, i64, i64, i64) = self.conn.query_row(
            r#"
            SELECT
                COUNT(*),
                COALESCE(SUM(CASE WHEN delivered_ts IS NOT NULL THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN delivered_ts IS NULL THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE
                    WHEN delivered_ts IS NULL
                     AND processing_started_at IS NULL
                     AND next_retry_at <= ?1
                     AND attempts < ?2 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE
                    WHEN delivered_ts IS NULL
                     AND processing_started_at IS NULL
                     AND next_retry_at > ?1
                     AND attempts < ?2 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE
                    WHEN delivered_ts IS NULL
                     AND processing_started_at IS NOT NULL THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE
                    WHEN delivered_ts IS NULL
                     AND attempts >= ?2 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE
                    WHEN delivered_ts IS NULL
                     AND last_sync_error IS NOT NULL
                     AND last_sync_error != '' THEN 1 ELSE 0 END), 0)
            FROM metrics
            "#,
            params![now as i64, MAX_METRIC_UPLOAD_ATTEMPTS as i64],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )?;

        let latest_error: Option<String> = self
            .conn
            .query_row(
                "SELECT last_sync_error FROM metrics \
                 WHERE delivered_ts IS NULL \
                   AND last_sync_error IS NOT NULL \
                   AND last_sync_error != '' \
                 ORDER BY COALESCE(last_sync_at, 0) DESC, id DESC \
                 LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;

        Ok(MetricsStatus {
            total: total as usize,
            delivered: delivered as usize,
            not_delivered: not_delivered as usize,
            pending_retryable: pending_retryable as usize,
            waiting_retry: waiting_retry as usize,
            processing: processing as usize,
            stopped_after_errors: stopped_after_errors as usize,
            rows_with_errors: rows_with_errors as usize,
            latest_error,
        })
    }

    fn release_stale_processing_locks(&mut self, now: u64) -> Result<(), GitAiError> {
        let stale_before = now.saturating_sub(METRIC_PROCESSING_LOCK_TIMEOUT_SECS);
        self.conn.execute(
            "UPDATE metrics \
             SET processing_started_at = NULL \
             WHERE delivered_ts IS NULL \
               AND processing_started_at IS NOT NULL \
               AND processing_started_at < ?1",
            params![stale_before as i64],
        )?;
        Ok(())
    }

    /// Delete delivered metric rows outside the local history retention window.
    ///
    /// Rows without a delivery receipt are never aged out, including rows waiting
    /// to retry and rows stopped after deterministic server errors. Valid delivered
    /// rows are aged by event timestamp; malformed delivered rows fall back to
    /// `delivered_ts`.
    fn prune_old_metrics_if_due(&mut self) -> Result<(), GitAiError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let last_prune: Option<i64> = self
            .conn
            .query_row(
                "SELECT value FROM schema_metadata WHERE key = 'metrics_last_prune_ts'",
                [],
                |row| row.get(0),
            )
            .optional()?
            .and_then(|v: String| v.parse().ok());

        if let Some(last) = last_prune
            && now.saturating_sub(last as u64) < Self::METRICS_PRUNE_INTERVAL_SECS
        {
            return Ok(());
        }

        let cutoff = now.saturating_sub(Self::METRICS_RETENTION_SECS);
        let rows_to_prune = self.old_metric_row_ids(cutoff)?;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO schema_metadata (key, value) VALUES ('metrics_last_prune_ts', ?1)",
            params![now.to_string()],
        )?;
        {
            let mut stmt = tx.prepare_cached("DELETE FROM metrics WHERE id = ?1")?;
            for id in rows_to_prune {
                stmt.execute(params![id])?;
            }
        }
        tx.commit()?;

        Ok(())
    }

    fn prune_compact_session_history_if_due(&mut self) -> Result<(), GitAiError> {
        let now = current_unix_ts();
        let last_prune: Option<i64> = self
            .conn
            .query_row(
                "SELECT value FROM schema_metadata WHERE key = 'compact_session_last_prune_ts'",
                [],
                |row| row.get(0),
            )
            .optional()?
            .and_then(|value: String| value.parse().ok());
        if let Some(last) = last_prune
            && now.saturating_sub(last.max(0) as u64) < Self::METRICS_PRUNE_INTERVAL_SECS
        {
            return Ok(());
        }

        let recovery_cutoff = now.saturating_sub(Self::RECOVERY_RETENTION_SECS) as i64;
        let history_cutoff = now.saturating_sub(Self::METRICS_RETENTION_SECS) as i64;
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM session_recovery_events WHERE event_ts < ?1",
            params![recovery_cutoff],
        )?;
        tx.execute(
            "DELETE FROM session_activity WHERE last_ts < ?1",
            params![history_cutoff],
        )?;
        tx.execute(
            "DELETE FROM session_token_sources WHERE last_ts < ?1",
            params![history_cutoff],
        )?;
        tx.execute(
            "DELETE FROM session_token_daily WHERE last_ts < ?1",
            params![history_cutoff],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO schema_metadata (key, value) VALUES ('compact_session_last_prune_ts', ?1)",
            params![now.to_string()],
        )?;
        tx.commit()?;

        // This physically returns a bounded number of freelist pages on fresh
        // databases. Older databases that predate incremental auto-vacuum keep
        // working; SQLite treats the call as a no-op until an explicit rebuild.
        let _ = self
            .conn
            .execute_batch("PRAGMA incremental_vacuum(256); PRAGMA wal_checkpoint(TRUNCATE);");
        Ok(())
    }

    fn old_metric_row_ids(&self, cutoff: u64) -> Result<Vec<i64>, GitAiError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, event_json, event_ts, delivered_ts \
             FROM metrics WHERE delivered_ts IS NOT NULL ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        })?;

        let mut ids = Vec::new();
        for row in rows {
            let (id, event_json, event_ts, delivered_ts) = row?;
            if metric_row_is_older_than_cutoff(&event_json, event_ts, delivered_ts, cutoff) {
                ids.push(id);
            }
        }

        Ok(ids)
    }

    /// Get count of pending metrics.
    pub fn count(&self) -> Result<usize, GitAiError> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM metrics WHERE delivered_ts IS NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(count as usize)
    }

    /// Query persisted metric rows since `since_ts` (Unix seconds).
    ///
    /// When `repo_filter` is `Some(url)`, only events matching that repo_url are returned.
    /// An empty string `""` is a sentinel meaning "events with no repo_url (NULL)".
    /// When `None`, all events are returned regardless of repo.
    pub fn get_metric_history(
        &self,
        since_ts: u32,
        repo_filter: Option<&str>,
        event_ids: &[u16],
    ) -> Result<Vec<MetricHistoryRecord>, GitAiError> {
        let mut stmt = self
            .conn
            .prepare("SELECT event_json, event_ts, event_kind FROM metrics WHERE event_ts IS NULL OR event_ts >= ?1 ORDER BY id ASC")?;
        let rows = stmt.query_map(params![since_ts as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        })?;

        let mut records = Vec::new();
        for row in rows {
            let (event_json, _cached_ts, cached_kind) = row?;
            if let Some(kind) = cached_kind
                && (0..=u16::MAX as i64).contains(&kind)
                && !event_ids.contains(&(kind as u16))
            {
                continue;
            }

            let Ok(event) = serde_json::from_str::<MetricEvent>(&event_json) else {
                continue;
            };

            if event.timestamp < since_ts || !event_ids.contains(&event.event_id) {
                continue;
            }

            let repo_url = sparse_get_string(&event.attrs, attr_pos::REPO_URL).flatten();
            let repo_matches = match repo_filter {
                None => true,
                Some("") => repo_url.is_none(),
                Some(filter) => repo_url.as_deref().is_some_and(|url| url.contains(filter)),
            };
            if !repo_matches {
                continue;
            }

            records.push(MetricHistoryRecord {
                event_id: event.event_id,
                ts: event.timestamp,
                repo_url,
                event,
            });
        }

        Ok(records)
    }

    pub(crate) fn get_compact_session_history(
        &self,
        since_ts: u32,
        repo_filter: Option<&str>,
    ) -> Result<Vec<CompactSessionRecord>, GitAiError> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, first_ts, last_ts, tool, model, repo_url \
             FROM session_activity WHERE last_ts >= ?1 ORDER BY last_ts ASC",
        )?;
        let rows = stmt.query_map(params![since_ts as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?;
        let mut records = Vec::new();
        for row in rows {
            let (session_id, first_ts, last_ts, tool, model, repo_url) = row?;
            if !compact_repo_matches(repo_url.as_deref(), repo_filter)
                || !(0..=u32::MAX as i64).contains(&first_ts)
                || !(0..=u32::MAX as i64).contains(&last_ts)
            {
                continue;
            }
            records.push(CompactSessionRecord {
                session_id,
                first_ts: (first_ts as u32).max(since_ts),
                last_ts: last_ts as u32,
                tool,
                model,
                repo_url,
            });
        }
        Ok(records)
    }

    pub(crate) fn get_compact_token_history(
        &self,
        since_ts: u32,
        repo_filter: Option<&str>,
    ) -> Result<Vec<CompactTokenRecord>, GitAiError> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT source_key, session_id, first_ts, last_ts, model, repo_url,
                   MAX(input_tokens - baseline_input_tokens, 0),
                   MAX(output_tokens - baseline_output_tokens, 0),
                   MAX(cache_read_tokens - baseline_cache_read_tokens, 0),
                   MAX(cache_write_tokens - baseline_cache_write_tokens, 0),
                   cumulative_source
            FROM session_token_sources AS source
            WHERE last_ts >= ?1
              AND NOT (
                  source.tool = 'codex'
                  AND source.source_key LIKE 'ts1:%'
                  AND EXISTS (
                      SELECT 1
                      FROM session_token_sources AS corrected
                      WHERE corrected.session_id = source.session_id
                        AND corrected.tool = 'codex'
                        AND corrected.source_key LIKE 'ts2:%'
                  )
              )
            ORDER BY last_ts ASC
            "#,
        )?;
        let rows = stmt.query_map(params![since_ts as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, i64>(10)?,
            ))
        })?;
        let mut records = Vec::new();
        for row in rows {
            let (
                source_key,
                session_id,
                first_ts,
                last_ts,
                model,
                repo_url,
                input,
                output,
                cache_read,
                cache_write,
                cumulative_source,
            ) = row?;
            let usage_ts = if cumulative_source != 0 {
                last_ts
            } else {
                first_ts.max(since_ts as i64)
            };
            if !compact_repo_matches(repo_url.as_deref(), repo_filter)
                || !(0..=u32::MAX as i64).contains(&usage_ts)
            {
                continue;
            }
            records.push(CompactTokenRecord {
                source_key,
                session_id,
                usage_ts: usage_ts as u32,
                model,
                input: input.max(0) as u64,
                output: output.max(0) as u64,
                cache_read: cache_read.max(0) as u64,
                cache_write: cache_write.max(0) as u64,
                cumulative_source: cumulative_source != 0,
                repo_url,
            });
        }
        Ok(records)
    }

    fn compact_recovery_candidates_near_timestamps(
        &self,
        timestamps_ns: &[u128],
        window_ns: u128,
        min_event_ts: u32,
        max_event_ts: u32,
    ) -> Result<Vec<SessionEventRecoveryCandidate>, GitAiError> {
        let mut stmt = self.conn.prepare(
            r#"
            SELECT rowid, event_ts, session_id, trace_id, tool, model,
                   external_session_id, external_tool_use_id, repo_url
            FROM session_recovery_events
            WHERE event_ts >= ?1 AND event_ts <= ?2
              AND session_id != '' AND tool != '' AND tool != 'mock_ai'
              AND external_session_id != ''
            ORDER BY rowid ASC
            "#,
        )?;
        let rows = stmt.query_map(params![min_event_ts as i64, max_event_ts as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
            ))
        })?;

        let mut candidates = Vec::new();
        for row in rows {
            let (
                row_id,
                event_ts,
                session_id,
                trace_id,
                tool,
                model,
                external_session_id,
                external_tool_use_id,
                repo_url,
            ) = row?;
            if !(0..=u32::MAX as i64).contains(&event_ts) {
                continue;
            }
            let event_ts = event_ts as u32;
            if min_distance_to_event_ts(timestamps_ns, event_ts)
                .is_none_or(|distance| distance > window_ns)
            {
                continue;
            }
            candidates.push(SessionEventRecoveryCandidate {
                row_id,
                event_ts,
                session_id,
                trace_id,
                tool,
                model,
                external_session_id,
                external_tool_use_id,
                repo_url,
            });
        }
        Ok(candidates)
    }

    pub(crate) fn session_event_candidates_near_timestamps(
        &self,
        timestamps_ns: &[u128],
        window_ns: u128,
    ) -> Result<Vec<SessionEventRecoveryCandidate>, GitAiError> {
        if timestamps_ns.is_empty() {
            return Ok(Vec::new());
        }

        let Some((min_event_ts, max_event_ts)) =
            event_ts_bounds_for_ns_windows(timestamps_ns, window_ns)
        else {
            return Ok(Vec::new());
        };

        let mut candidates = self.compact_recovery_candidates_near_timestamps(
            timestamps_ns,
            window_ns,
            min_event_ts,
            max_event_ts,
        )?;

        let mut stmt = self.conn.prepare(
            r#"
            SELECT
                id,
                event_json,
                event_ts,
                session_id,
                trace_id,
                tool,
                external_session_id,
                external_tool_use_id
            FROM metrics
            WHERE event_kind = ?1
              AND event_ts >= ?2
              AND event_ts <= ?3
              AND session_id IS NOT NULL
              AND session_id != ''
              AND tool IS NOT NULL
              AND tool != ''
              AND tool != 'mock_ai'
              AND external_session_id IS NOT NULL
              AND external_session_id != ''
            ORDER BY id ASC
            "#,
        )?;
        let rows = stmt.query_map(
            params![
                MetricEventId::SessionEvent as i64,
                min_event_ts as i64,
                max_event_ts as i64
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Option<String>>(7)?,
                ))
            },
        )?;

        for row in rows {
            let (
                row_id,
                event_json,
                event_ts,
                session_id,
                trace_id,
                tool,
                external_session_id,
                external_tool_use_id,
            ) = row?;
            if event_ts < 0 || event_ts > u32::MAX as i64 {
                continue;
            }
            let event_ts = event_ts as u32;
            if min_distance_to_event_ts(timestamps_ns, event_ts)
                .is_none_or(|distance| distance > window_ns)
            {
                continue;
            }

            let (repo_url, model) = recovery_attrs_from_event_json(&event_json);
            candidates.push(SessionEventRecoveryCandidate {
                row_id,
                event_ts,
                session_id,
                trace_id,
                tool,
                model,
                external_session_id,
                external_tool_use_id,
                repo_url,
            });
        }

        Ok(candidates)
    }

    pub(crate) fn latest_session_event_candidates_for_tools(
        &self,
        tools: &[&str],
    ) -> Result<Vec<SessionEventRecoveryCandidate>, GitAiError> {
        if tools.is_empty() {
            return Ok(Vec::new());
        }

        let placeholders = std::iter::repeat_n("?", tools.len())
            .collect::<Vec<_>>()
            .join(", ");
        let compact_sql = format!(
            r#"
            SELECT rowid, event_ts, session_id, trace_id, tool, model,
                   external_session_id, external_tool_use_id, repo_url
            FROM session_recovery_events
            WHERE tool IN ({placeholders})
              AND session_id != '' AND tool != '' AND tool != 'mock_ai'
              AND external_session_id != ''
            ORDER BY event_ts DESC, rowid DESC
            LIMIT 100
            "#
        );
        let tool_values = tools
            .iter()
            .map(|tool| rusqlite::types::Value::Text((*tool).to_string()))
            .collect::<Vec<_>>();
        let mut candidates = Vec::new();
        {
            let mut compact_stmt = self.conn.prepare(&compact_sql)?;
            let compact_rows =
                compact_stmt.query_map(params_from_iter(tool_values.iter()), |row| {
                    Ok(SessionEventRecoveryCandidate {
                        row_id: row.get(0)?,
                        event_ts: row.get::<_, i64>(1)?.max(0).min(u32::MAX as i64) as u32,
                        session_id: row.get(2)?,
                        trace_id: row.get(3)?,
                        tool: row.get(4)?,
                        model: row.get(5)?,
                        external_session_id: row.get(6)?,
                        external_tool_use_id: row.get(7)?,
                        repo_url: row.get(8)?,
                    })
                })?;
            for row in compact_rows {
                candidates.push(row?);
            }
        }
        let sql = format!(
            r#"
            SELECT
                id,
                event_json,
                event_ts,
                session_id,
                trace_id,
                tool,
                external_session_id,
                external_tool_use_id
            FROM metrics
            WHERE event_kind = ?1
              AND tool IN ({placeholders})
              AND event_ts IS NOT NULL
              AND session_id IS NOT NULL
              AND session_id != ''
              AND tool IS NOT NULL
              AND tool != ''
              AND tool != 'mock_ai'
              AND external_session_id IS NOT NULL
              AND external_session_id != ''
            ORDER BY event_ts DESC, id DESC
            LIMIT 100
            "#
        );

        let mut values = Vec::with_capacity(tools.len() + 1);
        values.push(rusqlite::types::Value::Integer(
            MetricEventId::SessionEvent as i64,
        ));
        values.extend(
            tools
                .iter()
                .map(|tool| rusqlite::types::Value::Text((*tool).to_string())),
        );

        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(values.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<String>>(7)?,
            ))
        })?;

        for row in rows {
            let (
                row_id,
                event_json,
                event_ts,
                session_id,
                trace_id,
                tool,
                external_session_id,
                external_tool_use_id,
            ) = row?;
            if event_ts < 0 || event_ts > u32::MAX as i64 {
                continue;
            }

            let (repo_url, model) = recovery_attrs_from_event_json(&event_json);
            candidates.push(SessionEventRecoveryCandidate {
                row_id,
                event_ts: event_ts as u32,
                session_id,
                trace_id,
                tool,
                model,
                external_session_id,
                external_tool_use_id,
                repo_url,
            });
        }

        candidates.sort_by_key(|candidate| {
            (
                std::cmp::Reverse(candidate.event_ts),
                std::cmp::Reverse(candidate.row_id),
            )
        });
        candidates.truncate(100);
        Ok(candidates)
    }

    /// Backfill cached event metadata for one bounded batch of legacy rows.
    pub fn backfill_event_metadata_batch(
        &mut self,
        limit: usize,
    ) -> Result<MetricMetadataBackfillSummary, GitAiError> {
        self.backfill_event_metadata_batch_after(0, limit)
            .map(|(summary, _)| summary)
    }

    pub(crate) fn event_metadata_backfill_completed(&self) -> Result<bool, GitAiError> {
        let completed: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM schema_metadata WHERE key = ?1",
                params![EVENT_METADATA_BACKFILL_COMPLETED_KEY],
                |row| row.get(0),
            )
            .optional()?;
        Ok(completed.as_deref() == Some("1"))
    }

    /// Backfill one bounded batch, permanently marking the one-time migration complete
    /// after a successful scan reaches the end of the table.
    pub(crate) fn backfill_event_metadata_batch_once(
        &mut self,
        after_id: i64,
        limit: usize,
    ) -> Result<(MetricMetadataBackfillSummary, Option<i64>), GitAiError> {
        if limit == 0 || self.event_metadata_backfill_completed()? {
            return Ok((MetricMetadataBackfillSummary::default(), None));
        }

        let result = self.backfill_event_metadata_batch_after(after_id, limit)?;
        if result.0.scanned < limit {
            self.conn.execute(
                "INSERT OR REPLACE INTO schema_metadata (key, value) VALUES (?1, '1')",
                params![EVENT_METADATA_BACKFILL_COMPLETED_KEY],
            )?;
        }
        Ok(result)
    }

    /// Backfill cached event metadata for all currently eligible legacy rows.
    pub fn backfill_event_metadata(&mut self) -> Result<MetricMetadataBackfillSummary, GitAiError> {
        let mut total = MetricMetadataBackfillSummary::default();
        let mut after_id = 0;

        loop {
            let (summary, last_id) =
                self.backfill_event_metadata_batch_after(after_id, METADATA_BACKFILL_BATCH_SIZE)?;
            total.scanned += summary.scanned;
            total.updated += summary.updated;

            let Some(id) = last_id else {
                break;
            };
            after_id = id;

            if summary.scanned < METADATA_BACKFILL_BATCH_SIZE {
                break;
            }
        }

        Ok(total)
    }

    pub(crate) fn backfill_event_metadata_batch_after(
        &mut self,
        after_id: i64,
        limit: usize,
    ) -> Result<(MetricMetadataBackfillSummary, Option<i64>), GitAiError> {
        if limit == 0 {
            return Ok((MetricMetadataBackfillSummary::default(), None));
        }

        let rows = {
            let mut stmt = self.conn.prepare(
                "SELECT id, event_json FROM metrics \
                 WHERE id > ?1 AND (event_ts IS NULL OR event_kind IS NULL) \
                 ORDER BY id ASC \
                 LIMIT ?2",
            )?;
            let mapped = stmt.query_map(params![after_id, limit as i64], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };

        let mut summary = MetricMetadataBackfillSummary {
            scanned: rows.len(),
            updated: 0,
        };
        let last_id = rows.last().map(|(id, _)| *id);
        if rows.is_empty() {
            return Ok((summary, last_id));
        }

        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                r#"
                UPDATE metrics
                SET event_ts = ?1,
                    event_kind = ?2,
                    trace_id = ?3,
                    session_id = ?4,
                    parent_session_id = ?5,
                    tool = ?6,
                    external_session_id = ?7,
                    external_parent_session_id = ?8,
                    external_event_id = ?9,
                    external_parent_event_id = ?10,
                    external_tool_use_id = ?11
                WHERE id = ?12
                "#,
            )?;

            for (id, event_json) in rows {
                let Some(metadata) = extract_metric_event_metadata(&event_json) else {
                    continue;
                };

                stmt.execute(params![
                    i64::from(metadata.event_ts),
                    i64::from(metadata.event_kind),
                    metadata.trace_id.as_deref(),
                    metadata.session_id.as_deref(),
                    metadata.parent_session_id.as_deref(),
                    metadata.tool.as_deref(),
                    metadata.external_session_id.as_deref(),
                    metadata.external_parent_session_id.as_deref(),
                    metadata.external_event_id.as_deref(),
                    metadata.external_parent_event_id.as_deref(),
                    metadata.external_tool_use_id.as_deref(),
                    id,
                ])?;
                summary.updated += 1;
            }
        }
        tx.commit()?;

        Ok((summary, last_id))
    }

    /// Returns whether an `agent_usage` event should be emitted for this prompt_id.
    ///
    /// If emitted, this method also updates the prompt's last-sent timestamp.
    pub fn should_emit_agent_usage(
        &mut self,
        prompt_id: &str,
        now_ts: u64,
        min_interval_secs: u64,
    ) -> Result<bool, GitAiError> {
        if prompt_id.is_empty() {
            return Ok(true);
        }

        let tx = self.conn.transaction()?;
        let existing_ts: Option<i64> = tx
            .query_row(
                "SELECT last_sent_ts FROM agent_usage_throttle WHERE prompt_id = ?1",
                params![prompt_id],
                |row| row.get(0),
            )
            .optional()?;

        let should_emit = existing_ts
            .map(|prev_ts| now_ts.saturating_sub(prev_ts as u64) >= min_interval_secs)
            .unwrap_or(true);

        if should_emit {
            tx.execute(
                r#"
                INSERT INTO agent_usage_throttle (prompt_id, last_sent_ts)
                VALUES (?1, ?2)
                ON CONFLICT(prompt_id) DO UPDATE SET last_sent_ts = excluded.last_sent_ts
                "#,
                params![prompt_id, now_ts as i64],
            )?;
        }

        tx.commit()?;
        Ok(should_emit)
    }
}

fn migrate_unknown_daily_token_bucket(
    tx: &rusqlite::Transaction<'_>,
    anonymous_bucket_key: &str,
    target_bucket_key: &str,
    target_attrs_json: &str,
    target_timezone: &str,
    target_provider: Option<&str>,
) -> Result<bool, GitAiError> {
    migrate_daily_token_bucket(
        tx,
        anonymous_bucket_key,
        target_bucket_key,
        target_attrs_json,
        target_timezone,
        target_provider,
        true,
    )
}

#[allow(clippy::too_many_arguments)]
fn migrate_daily_token_bucket(
    tx: &rusqlite::Transaction<'_>,
    source_bucket_key: &str,
    target_bucket_key: &str,
    target_attrs_json: &str,
    target_timezone: &str,
    target_provider: Option<&str>,
    require_anonymous_source: bool,
) -> Result<bool, GitAiError> {
    if source_bucket_key == target_bucket_key {
        return Ok(false);
    }

    let source = tx
        .query_row(
            "SELECT attrs_json, outbox_metric_id, snapshot_instance_id, \
                    supersedes_source_keys_json, supersedes_snapshot_instance_ids_json \
             FROM session_token_daily WHERE bucket_key = ?1",
            params![source_bucket_key],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((
        source_attrs_json,
        source_outbox_metric_id,
        source_snapshot_instance_id,
        source_supersedes_keys_json,
        source_supersedes_instances_json,
    )) = source
    else {
        return Ok(false);
    };
    let source_attrs: SparseArray = serde_json::from_str(&source_attrs_json)?;
    if require_anonymous_source && compact_identity_email(&source_attrs).is_some() {
        return Ok(false);
    }

    let target_aliases = tx
        .query_row(
            "SELECT supersedes_source_keys_json, supersedes_snapshot_instance_ids_json \
             FROM session_token_daily WHERE bucket_key = ?1",
            params![target_bucket_key],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    let mut supersedes_source_keys =
        parse_token_snapshot_aliases(target_aliases.as_ref().map(|aliases| aliases.0.as_str()))?;
    supersedes_source_keys.extend(parse_token_snapshot_aliases(Some(
        &source_supersedes_keys_json,
    ))?);
    supersedes_source_keys.insert(source_bucket_key.to_string());
    let mut supersedes_snapshot_instance_ids =
        parse_token_snapshot_aliases(target_aliases.as_ref().map(|aliases| aliases.1.as_str()))?;
    supersedes_snapshot_instance_ids.extend(parse_token_snapshot_aliases(Some(
        &source_supersedes_instances_json,
    ))?);
    if !source_snapshot_instance_id.is_empty() {
        supersedes_snapshot_instance_ids.insert(source_snapshot_instance_id);
    }
    let supersedes_source_keys_json =
        serde_json::to_string(&supersedes_source_keys.into_iter().collect::<Vec<_>>())?;
    let supersedes_snapshot_instance_ids_json = serde_json::to_string(
        &supersedes_snapshot_instance_ids
            .into_iter()
            .collect::<Vec<_>>(),
    )?;

    // A pending snapshot is replaceable, but an in-flight row may already have
    // been serialized into an HTTP request. Leave the latter immutable and
    // create a cumulative successor below.
    if let Some(outbox_metric_id) = source_outbox_metric_id {
        tx.execute(
            "DELETE FROM metrics \
             WHERE id = ?1 AND event_kind = ?2 AND delivered_ts IS NULL \
               AND processing_started_at IS NULL",
            params![outbox_metric_id, MetricEventId::SessionTokenUsage as i64],
        )?;
    }

    tx.execute(
        r#"
        INSERT INTO session_token_daily (
            bucket_key, date_key, timezone, machine_id, first_ts, last_ts,
            attrs_json, provider, request_count, input_tokens, output_tokens,
            cache_read_tokens, cache_write_tokens, outbox_metric_id,
            snapshot_instance_id
        )
        SELECT ?1, date_key, ?2, machine_id, first_ts, last_ts,
               ?3, COALESCE(?4, provider), request_count, input_tokens,
               output_tokens, cache_read_tokens, cache_write_tokens, NULL, ?6
        FROM session_token_daily
        WHERE bucket_key = ?5
        ON CONFLICT(bucket_key) DO UPDATE SET
            first_ts = MIN(session_token_daily.first_ts, excluded.first_ts),
            last_ts = MAX(session_token_daily.last_ts, excluded.last_ts),
            timezone = ?2,
            attrs_json = ?3,
            provider = COALESCE(?4, session_token_daily.provider, excluded.provider),
            request_count = session_token_daily.request_count + excluded.request_count,
            input_tokens = session_token_daily.input_tokens + excluded.input_tokens,
            output_tokens = session_token_daily.output_tokens + excluded.output_tokens,
            cache_read_tokens = session_token_daily.cache_read_tokens + excluded.cache_read_tokens,
            cache_write_tokens = session_token_daily.cache_write_tokens + excluded.cache_write_tokens
        "#,
        params![
            target_bucket_key,
            target_timezone,
            target_attrs_json,
            target_provider,
            source_bucket_key,
            crate::uuid::generate_v4(),
        ],
    )?;
    tx.execute(
        "UPDATE session_token_daily \
         SET supersedes_source_keys_json = ?1, \
             supersedes_snapshot_instance_ids_json = ?2 \
         WHERE bucket_key = ?3",
        params![
            supersedes_source_keys_json,
            supersedes_snapshot_instance_ids_json,
            target_bucket_key,
        ],
    )?;
    tx.execute(
        "DELETE FROM session_token_daily WHERE bucket_key = ?1",
        params![source_bucket_key],
    )?;
    Ok(true)
}

fn parse_token_snapshot_aliases(raw: Option<&str>) -> Result<BTreeSet<String>, GitAiError> {
    let aliases = raw
        .filter(|raw| !raw.trim().is_empty())
        .map(serde_json::from_str::<Vec<String>>)
        .transpose()?
        .unwrap_or_default();
    Ok(aliases
        .into_iter()
        .filter_map(|alias| compact_non_empty(&alias).map(str::to_string))
        .collect())
}

fn refresh_daily_token_outbox(
    tx: &rusqlite::Transaction<'_>,
    bucket_key: &str,
) -> Result<i64, GitAiError> {
    let snapshot = tx.query_row(
        r#"
        SELECT date_key, timezone, machine_id, last_ts, attrs_json, provider, request_count,
               input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
               outbox_metric_id, snapshot_instance_id, supersedes_source_keys_json,
               supersedes_snapshot_instance_ids_json
        FROM session_token_daily WHERE bucket_key = ?1
        "#,
        params![bucket_key],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
                row.get::<_, i64>(8)?,
                row.get::<_, i64>(9)?,
                row.get::<_, i64>(10)?,
                row.get::<_, Option<i64>>(11)?,
                row.get::<_, String>(12)?,
                row.get::<_, String>(13)?,
                row.get::<_, String>(14)?,
            ))
        },
    )?;
    let (
        date_key,
        timezone,
        machine_id,
        last_ts,
        snapshot_attrs_json,
        provider,
        request_count,
        input,
        output,
        cache_read,
        cache_write,
        outbox_metric_id,
        snapshot_instance_id,
        supersedes_source_keys_json,
        supersedes_snapshot_instance_ids_json,
    ) = snapshot;
    let snapshot_attrs: SparseArray = serde_json::from_str(&snapshot_attrs_json)?;
    let supersedes_source_keys = parse_token_snapshot_aliases(Some(&supersedes_source_keys_json))?
        .into_iter()
        .collect();
    let supersedes_snapshot_instance_ids =
        parse_token_snapshot_aliases(Some(&supersedes_snapshot_instance_ids_json))?
            .into_iter()
            .collect();
    let values = SessionTokenUsageValues::new(
        bucket_key,
        input.max(0) as u64,
        output.max(0) as u64,
        cache_read.max(0) as u64,
        cache_write.max(0) as u64,
        request_count.max(0).min(u32::MAX as i64) as u32,
        provider,
        date_key,
        timezone,
        machine_id,
        supersedes_source_keys,
        snapshot_instance_id,
        supersedes_snapshot_instance_ids,
    );
    let event = MetricEvent::from_values_with_timestamp(
        values,
        snapshot_attrs,
        Some(last_ts.max(0).min(u32::MAX as i64) as u32),
    );
    let event_json = serde_json::to_string(&event)?;
    let metadata = extract_metric_event_metadata(&event_json)
        .ok_or_else(|| GitAiError::Generic("invalid compact metric metadata".to_string()))?;
    let updated = if let Some(outbox_id) = outbox_metric_id {
        tx.execute(
            r#"
            UPDATE metrics SET
                event_json = ?1, event_ts = ?2, event_kind = ?3,
                trace_id = ?4, session_id = ?5, parent_session_id = ?6,
                tool = ?7, external_session_id = ?8,
                external_parent_session_id = ?9, external_event_id = ?10,
                external_parent_event_id = ?11, external_tool_use_id = ?12,
                attempts = 0, last_sync_error = NULL, last_sync_at = NULL,
                next_retry_at = 0
            WHERE id = ?13 AND delivered_ts IS NULL
              AND processing_started_at IS NULL AND event_kind = ?3
            "#,
            params![
                event_json,
                i64::from(metadata.event_ts),
                i64::from(metadata.event_kind),
                metadata.trace_id.as_deref(),
                metadata.session_id.as_deref(),
                metadata.parent_session_id.as_deref(),
                metadata.tool.as_deref(),
                metadata.external_session_id.as_deref(),
                metadata.external_parent_session_id.as_deref(),
                metadata.external_event_id.as_deref(),
                metadata.external_parent_event_id.as_deref(),
                metadata.external_tool_use_id.as_deref(),
                outbox_id,
            ],
        )?
    } else {
        0
    };
    if updated > 0 {
        return Ok(outbox_metric_id.expect("updated compact outbox row has an id"));
    }

    tx.execute(
        r#"
        INSERT INTO metrics (
            event_json, delivered_ts, event_ts, event_kind, trace_id,
            session_id, parent_session_id, tool, external_session_id,
            external_parent_session_id, external_event_id,
            external_parent_event_id, external_tool_use_id
        ) VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
        "#,
        params![
            event_json,
            i64::from(metadata.event_ts),
            i64::from(metadata.event_kind),
            metadata.trace_id.as_deref(),
            metadata.session_id.as_deref(),
            metadata.parent_session_id.as_deref(),
            metadata.tool.as_deref(),
            metadata.external_session_id.as_deref(),
            metadata.external_parent_session_id.as_deref(),
            metadata.external_event_id.as_deref(),
            metadata.external_parent_event_id.as_deref(),
            metadata.external_tool_use_id.as_deref(),
        ],
    )?;
    let metric_id = tx.last_insert_rowid();
    tx.execute(
        "UPDATE session_token_daily SET outbox_metric_id = ?1 WHERE bucket_key = ?2",
        params![metric_id, bucket_key],
    )?;
    Ok(metric_id)
}

fn u64_to_sqlite(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

/// Record the first trustworthy reporting-profile identity observed for a
/// session. Only that identity may recover earlier anonymous facts from the
/// same session. A later different identity makes the session permanently
/// ambiguous for recovery; both explicitly identified streams still retain
/// their own direct Event9 buckets.
fn observe_session_reporting_identity(
    tx: &Transaction<'_>,
    session_id: &str,
    attrs: &SparseArray,
) -> Result<bool, GitAiError> {
    let Some(current_email) = compact_identity_email(attrs) else {
        return Ok(false);
    };
    let (stored_email, state) = tx.query_row(
        "SELECT reporting_identity_email, reporting_identity_state \
         FROM session_activity WHERE session_id = ?1",
        params![session_id],
        |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
    )?;

    match state.as_str() {
        "unbound" => {
            tx.execute(
                "UPDATE session_activity \
                 SET reporting_identity_email = ?1, reporting_identity_state = 'bound' \
                 WHERE session_id = ?2 AND reporting_identity_state = 'unbound'",
                params![current_email, session_id],
            )?;
            Ok(true)
        }
        "bound" if stored_email.as_deref() == Some(current_email.as_str()) => Ok(true),
        "bound" => {
            tx.execute(
                "UPDATE session_activity SET reporting_identity_state = 'conflicted' \
                 WHERE session_id = ?1",
                params![session_id],
            )?;
            Ok(false)
        }
        // Existing compact sessions have no trustworthy historical profile
        // provenance. Unknown/future states also fail closed.
        "legacy_ambiguous" | "conflicted" => Ok(false),
        _ => Ok(false),
    }
}

fn session_reporting_identity_is_bound_to(
    tx: &Transaction<'_>,
    session_id: &str,
    expected_email: Option<&str>,
) -> Result<bool, GitAiError> {
    let Some(expected_email) = expected_email else {
        return Ok(false);
    };
    let state = tx
        .query_row(
            "SELECT reporting_identity_email, reporting_identity_state \
             FROM session_activity WHERE session_id = ?1",
            params![session_id],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    Ok(matches!(
        state,
        Some((Some(email), state)) if state == "bound" && email == expected_email
    ))
}

fn compact_repo_matches(repo_url: Option<&str>, repo_filter: Option<&str>) -> bool {
    match repo_filter {
        None => true,
        Some("") => repo_url.is_none(),
        Some(filter) => repo_url.is_some_and(|url| url.contains(filter)),
    }
}

fn daily_token_bucket(
    timestamp: u32,
    attrs: &SparseArray,
    provider: Option<&str>,
    machine_id: &str,
    source_key: &str,
    session_id: &str,
) -> Result<(String, String, String, String, SparseArray), GitAiError> {
    let local_time = Local
        .timestamp_opt(i64::from(timestamp), 0)
        .single()
        .unwrap_or_else(Local::now);
    let date_key = local_time.format("%Y-%m-%d").to_string();
    let timezone = local_time.offset().to_string();

    let mut snapshot_attrs = attrs.clone();
    for position in [
        attr_pos::EXTERNAL_SESSION_ID,
        attr_pos::SESSION_ID,
        attr_pos::TRACE_ID,
        attr_pos::PARENT_SESSION_ID,
        attr_pos::EXTERNAL_PARENT_SESSION_ID,
    ] {
        snapshot_attrs.remove(&position.to_string());
    }
    canonicalize_snapshot_repo_url(&mut snapshot_attrs);

    // This identity must stay in lock-step with fact_ai_token_daily's unique
    // business dimensions. Including branch, organization or arbitrary custom
    // attributes here would split one server row into multiple client snapshots;
    // the server's freshness upsert would then keep only the largest split and
    // silently under-count the day.
    let revision = if source_key.starts_with("ts2:") {
        "td4"
    } else {
        "td3"
    };
    let user_email = compact_identity_email(&snapshot_attrs);
    let anonymous_identity = anonymous_daily_token_identity(session_id);
    let bucket_key = daily_token_bucket_key(
        revision,
        &date_key,
        user_email.as_deref().unwrap_or(&anonymous_identity),
        machine_id,
        &snapshot_attrs,
        provider,
    );
    let anonymous_bucket_key = daily_token_bucket_key(
        revision,
        &date_key,
        &anonymous_identity,
        machine_id,
        &snapshot_attrs,
        provider,
    );
    Ok((
        bucket_key,
        anonymous_bucket_key,
        date_key,
        timezone,
        snapshot_attrs,
    ))
}

fn anonymous_daily_token_identity(session_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"git-ai-anonymous-session-token-v1\0");
    hasher.update(session_id.as_bytes());
    format!("unknown-session:{:x}", hasher.finalize())
}

fn daily_token_bucket_key(
    revision: &str,
    date_key: &str,
    user_email: &str,
    machine_id: &str,
    attrs: &SparseArray,
    provider: Option<&str>,
) -> String {
    let custom_attrs = compact_custom_attributes(attrs);
    let project_key = first_compact_custom_attr(&custom_attrs, &["project_key", "projectKey"])
        .or_else(|| {
            sparse_get_string(attrs, attr_pos::REPO_URL)
                .flatten()
                .and_then(|repo_url| crate::repo_url::normalize_repo_url(&repo_url).ok())
        })
        .unwrap_or_else(|| "git-ai-unknown".to_string());
    let ide = first_compact_custom_attr(
        &custom_attrs,
        &["ide", "platform", "kilo_platform", "client", "kilo_client"],
    )
    .unwrap_or_else(|| "unknown".to_string());
    let coding_tool =
        compact_sparse_string(attrs, attr_pos::TOOL).unwrap_or_else(|| "unknown".to_string());
    let model_provider = provider
        .and_then(compact_non_empty)
        .map(str::to_string)
        .or_else(|| first_compact_custom_attr(&custom_attrs, &["model_provider", "modelProvider"]))
        .unwrap_or_else(|| "unknown".to_string());
    let model =
        compact_sparse_string(attrs, attr_pos::MODEL).unwrap_or_else(|| "unknown".to_string());

    let mut hasher = Sha256::new();
    for part in [
        date_key,
        user_email,
        machine_id,
        project_key.as_str(),
        ide.as_str(),
        coding_tool.as_str(),
        model_provider.as_str(),
        model.as_str(),
    ] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    format!("{}:{:x}", revision, hasher.finalize())
}

fn latest_daily_token_bucket_revision(bucket_key: &str) -> &str {
    match bucket_key.split_once(':').map(|(revision, _)| revision) {
        Some("td2" | "td4") => "td4",
        _ => "td3",
    }
}

fn legacy_daily_token_bucket_revision(bucket_key: &str) -> bool {
    matches!(
        bucket_key.split_once(':').map(|(revision, _)| revision),
        Some("td1" | "td2")
    )
}

fn has_explicit_project_key(attrs: &SparseArray) -> bool {
    first_compact_custom_attr(
        &compact_custom_attributes(attrs),
        &["project_key", "projectKey"],
    )
    .is_some()
}

fn mark_legacy_project_identity_ambiguous(
    attrs: &mut SparseArray,
    legacy_bucket_key: &str,
) -> Result<(), GitAiError> {
    attrs.remove(&attr_pos::REPO_URL.to_string());
    let mut custom_attrs = compact_custom_attributes(attrs);
    let (unbound_project_key, identity_sha256) = legacy_unbound_project_identity(legacy_bucket_key);
    custom_attrs.insert(
        "legacy_project_identity_sha256".to_string(),
        Value::String(identity_sha256),
    );
    custom_attrs.insert(
        "project_identity_status".to_string(),
        Value::String("legacy_basename_ambiguous".to_string()),
    );
    custom_attrs.insert(
        "project_key".to_string(),
        Value::String(unbound_project_key),
    );
    custom_attrs.insert(
        "project_name".to_string(),
        Value::String("历史项目归属不可判定".to_string()),
    );
    attrs.insert(
        attr_pos::CUSTOM_ATTRIBUTES.to_string(),
        Value::String(serde_json::to_string(&custom_attrs)?),
    );
    Ok(())
}

fn legacy_unbound_project_identity(legacy_bucket_key: &str) -> (String, String) {
    let mut hasher = Sha256::new();
    hasher.update(b"git-ai-unbound-legacy-project\0");
    hasher.update(legacy_bucket_key.as_bytes());
    let identity_sha256 = format!("{:x}", hasher.finalize());
    let project_key = format!("git-ai-unbound:{}", &identity_sha256[..48]);
    debug_assert!(project_key.len() <= 64);
    (project_key, identity_sha256)
}

fn canonicalize_snapshot_repo_url(attrs: &mut SparseArray) {
    let repo_url_key = attr_pos::REPO_URL.to_string();
    let Some(raw_repo_url) = sparse_get_string(attrs, attr_pos::REPO_URL).flatten() else {
        return;
    };
    match crate::repo_url::normalize_repo_url(&raw_repo_url) {
        Ok(repo_url) => {
            attrs.insert(repo_url_key, Value::String(repo_url));
        }
        Err(_) => {
            // Never retain an unparseable remote in the compact daily payload:
            // it may contain credentials, and it cannot bind a server project.
            attrs.remove(&repo_url_key);
        }
    }
}

#[cfg(test)]
fn token_snapshot_machine_id() -> String {
    "git-ai-test-install".to_string()
}

#[cfg(not(test))]
fn token_snapshot_machine_id() -> String {
    crate::config::get_or_create_distinct_id()
}

fn compact_custom_attributes(attrs: &SparseArray) -> Map<String, Value> {
    let Some(raw) = attrs.get(&attr_pos::CUSTOM_ATTRIBUTES.to_string()) else {
        return Map::new();
    };
    match raw {
        Value::Object(values) => values.clone(),
        Value::String(json) => serde_json::from_str::<Value>(json)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_default(),
        _ => Map::new(),
    }
}

fn first_compact_custom_attr(attrs: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        let value = attrs.get(*key)?;
        match value {
            Value::String(value) => compact_non_empty(value).map(str::to_string),
            Value::Number(_) | Value::Bool(_) => {
                compact_non_empty(&value.to_string()).map(str::to_string)
            }
            _ => None,
        }
    })
}

fn compact_sparse_string(attrs: &SparseArray, position: usize) -> Option<String> {
    sparse_get_string(attrs, position)
        .flatten()
        .and_then(|value| compact_non_empty(&value).map(str::to_string))
}

fn compact_non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn compact_identity_email(attrs: &SparseArray) -> Option<String> {
    let custom_attrs = compact_custom_attributes(attrs);
    let marker_is_valid_or_legacy_absent =
        match custom_attrs.get(REPORTING_PROFILE_VERSION_ATTRIBUTE) {
            None => true,
            Some(Value::String(version)) => {
                compact_non_empty(version) == Some(REPORTING_PROFILE_VERSION)
            }
            Some(_) => false,
        };
    if !marker_is_valid_or_legacy_absent
        || ["department_name", "office_name", "user_name"]
            .iter()
            .any(|key| compact_profile_string(&custom_attrs, key).is_none())
    {
        return None;
    }
    compact_profile_string(&custom_attrs, "user_email")
        .and_then(|email| compact_valid_email(&email))
}

fn compact_profile_string(attrs: &Map<String, Value>, key: &str) -> Option<String> {
    attrs
        .get(key)?
        .as_str()
        .and_then(compact_non_empty)
        .map(str::to_string)
}

fn compact_valid_email(email: &str) -> Option<String> {
    let email = compact_non_empty(email)?.to_lowercase();
    let (local, domain) = email.split_once('@')?;
    (!local.is_empty()
        && !local.chars().any(char::is_whitespace)
        && domain.contains('.')
        && !domain.ends_with('.')
        && !domain.chars().any(char::is_whitespace))
    .then_some(email)
}

fn recovery_event_key(
    session_id: &str,
    event_ts: u32,
    external_tool_use_id: Option<&str>,
    tool: &str,
    model: Option<&str>,
    repo_url: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    let event_ts = event_ts.to_string();
    let marker_identity = external_tool_use_id
        .map(|id| format!("tool:{id}"))
        .unwrap_or_else(|| "second".to_string());
    for part in [
        session_id,
        event_ts.as_str(),
        marker_identity.as_str(),
        tool,
        model.unwrap_or_default(),
        repo_url.unwrap_or_default(),
    ] {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    format!("sr1:{:x}", hasher.finalize())
}

fn current_unix_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn event_ts_bounds_for_ns_windows(timestamps_ns: &[u128], window_ns: u128) -> Option<(u32, u32)> {
    let mut min_ts: Option<u32> = None;
    let mut max_ts: Option<u32> = None;
    for timestamp_ns in timestamps_ns {
        let start = timestamp_ns.saturating_sub(window_ns) / NS_PER_SECOND;
        let end = timestamp_ns
            .saturating_add(window_ns)
            .min(u32::MAX as u128 * NS_PER_SECOND)
            / NS_PER_SECOND;
        let start = start.min(u32::MAX as u128) as u32;
        let end = end.min(u32::MAX as u128) as u32;
        min_ts = Some(min_ts.map_or(start, |current| current.min(start)));
        max_ts = Some(max_ts.map_or(end, |current| current.max(end)));
    }
    min_ts.zip(max_ts)
}

fn min_distance_to_event_ts(timestamps_ns: &[u128], event_ts: u32) -> Option<u128> {
    timestamps_ns
        .iter()
        .map(|timestamp_ns| distance_to_event_second(*timestamp_ns, event_ts))
        .min()
}

fn distance_to_event_second(timestamp_ns: u128, event_ts: u32) -> u128 {
    let start_ns = event_ts as u128 * NS_PER_SECOND;
    let end_ns = start_ns.saturating_add(NS_PER_SECOND - 1);
    if timestamp_ns < start_ns {
        start_ns - timestamp_ns
    } else {
        timestamp_ns.saturating_sub(end_ns)
    }
}

fn recovery_attrs_from_event_json(event_json: &str) -> (Option<String>, Option<String>) {
    let Ok(value) = serde_json::from_str::<Value>(event_json) else {
        return (None, None);
    };
    let attrs = value.get("a").and_then(Value::as_object);
    (
        sparse_object_string(attrs, attr_pos::REPO_URL),
        sparse_object_string(attrs, attr_pos::MODEL),
    )
}

fn metric_row_is_older_than_cutoff(
    event_json: &str,
    event_ts: Option<i64>,
    delivered_ts: Option<i64>,
    cutoff: u64,
) -> bool {
    if delivered_ts.is_none() {
        return false;
    }

    if let Some(ts) = event_ts
        && ts >= 0
    {
        return (ts as u64) < cutoff;
    }

    if let Some(ts) = extract_metric_event_ts(event_json) {
        return u64::from(ts) < cutoff;
    }

    delivered_ts.is_some_and(|ts| ts >= 0 && (ts as u64) < cutoff)
}

fn compact_legacy_content_event(event_json: &str) -> Option<SessionObservation> {
    let event: MetricEvent = serde_json::from_str(event_json).ok()?;
    if event.event_id != MetricEventId::SessionEvent as u16
        && event.event_id != MetricEventId::OtelTrace as u16
    {
        return None;
    }
    let raw = event.values.get("0")?.clone();
    let external_event_id = event
        .values
        .get("1")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let external_parent_event_id = event
        .values
        .get("2")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let external_tool_use_id = event
        .values
        .get("3")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let is_codex_fork = sparse_get_string(&event.attrs, attr_pos::TOOL)
        .flatten()
        .is_some_and(|tool| tool == "codex")
        && [
            attr_pos::PARENT_SESSION_ID,
            attr_pos::EXTERNAL_PARENT_SESSION_ID,
        ]
        .into_iter()
        .any(|position| {
            sparse_get_string(&event.attrs, position)
                .flatten()
                .is_some_and(|value| !value.trim().is_empty())
        });
    let mut observation = compact_session_event(
        &raw,
        event.timestamp,
        event.attrs,
        external_event_id,
        external_parent_event_id,
        external_tool_use_id,
    );
    if is_codex_fork {
        // Legacy content events do not preserve the byte boundary where a
        // Codex fork starts. Projecting their inherited cumulative counter
        // would poison the v2 source with the parent's full history. Keep the
        // activity/recovery projection, then let the reset stream watermark
        // rebuild this token source from the transcript boundary.
        observation.token = None;
    }
    Some(observation)
}

fn extract_metric_event_ts(event_json: &str) -> Option<u32> {
    let value: Value = serde_json::from_str(event_json).ok()?;
    extract_metric_event_ts_from_value(&value)
}

fn extract_metric_event_ts_from_value(value: &Value) -> Option<u32> {
    value
        .get("t")
        .and_then(Value::as_u64)
        .filter(|ts| *ts <= u32::MAX as u64)
        .map(|ts| ts as u32)
}

/// Insert already-serialized metric events inside a caller-owned transaction.
///
/// Deferred jobs use this helper so their outbox rows and `done` tombstone can
/// commit atomically without losing the metadata columns needed immediately by
/// session/recovery/history queries.
pub(crate) fn insert_event_jsons_in_transaction(
    tx: &Transaction<'_>,
    events: &[String],
    delivered_ts: Option<u64>,
) -> Result<Vec<i64>, GitAiError> {
    let mut ids = Vec::with_capacity(events.len());
    let mut stmt = tx.prepare_cached(
        r#"
        INSERT INTO metrics (
            event_json,
            delivered_ts,
            event_ts,
            event_kind,
            trace_id,
            session_id,
            parent_session_id,
            tool,
            external_session_id,
            external_parent_session_id,
            external_event_id,
            external_parent_event_id,
            external_tool_use_id
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
        "#,
    )?;

    for event_json in events {
        let metadata = extract_metric_event_metadata(event_json);
        let event_ts = metadata
            .as_ref()
            .map(|metadata| i64::from(metadata.event_ts));
        let event_kind = metadata
            .as_ref()
            .map(|metadata| i64::from(metadata.event_kind));
        stmt.execute(params![
            event_json,
            delivered_ts.map(|timestamp| timestamp.min(i64::MAX as u64) as i64),
            event_ts,
            event_kind,
            metadata
                .as_ref()
                .and_then(|metadata| metadata.trace_id.as_deref()),
            metadata
                .as_ref()
                .and_then(|metadata| metadata.session_id.as_deref()),
            metadata
                .as_ref()
                .and_then(|metadata| metadata.parent_session_id.as_deref()),
            metadata
                .as_ref()
                .and_then(|metadata| metadata.tool.as_deref()),
            metadata
                .as_ref()
                .and_then(|metadata| metadata.external_session_id.as_deref()),
            metadata
                .as_ref()
                .and_then(|metadata| { metadata.external_parent_session_id.as_deref() }),
            metadata
                .as_ref()
                .and_then(|metadata| metadata.external_event_id.as_deref()),
            metadata
                .as_ref()
                .and_then(|metadata| { metadata.external_parent_event_id.as_deref() }),
            metadata
                .as_ref()
                .and_then(|metadata| metadata.external_tool_use_id.as_deref()),
        ])?;
        ids.push(tx.last_insert_rowid());
    }
    Ok(ids)
}

fn extract_metric_event_metadata(event_json: &str) -> Option<MetricEventMetadata> {
    let value: Value = serde_json::from_str(event_json).ok()?;
    let event_ts = extract_metric_event_ts_from_value(&value)?;
    let event_kind = value
        .get("e")
        .and_then(Value::as_u64)
        .filter(|kind| *kind <= u16::MAX as u64)? as u16;

    let attrs = value.get("a").and_then(Value::as_object);
    let values = value.get("v").and_then(Value::as_object);

    Some(MetricEventMetadata {
        event_ts,
        event_kind,
        trace_id: sparse_object_string(attrs, attr_pos::TRACE_ID),
        session_id: sparse_object_string(attrs, attr_pos::SESSION_ID),
        parent_session_id: sparse_object_string(attrs, attr_pos::PARENT_SESSION_ID),
        tool: sparse_object_string(attrs, attr_pos::TOOL),
        external_session_id: sparse_object_string(attrs, attr_pos::EXTERNAL_SESSION_ID),
        external_parent_session_id: sparse_object_string(
            attrs,
            attr_pos::EXTERNAL_PARENT_SESSION_ID,
        ),
        external_event_id: event_specific_external_event_id(event_kind, values),
        external_parent_event_id: event_specific_external_parent_event_id(event_kind, values),
        external_tool_use_id: event_specific_external_tool_use_id(event_kind, values),
    })
}

fn sparse_object_string(object: Option<&Map<String, Value>>, pos: usize) -> Option<String> {
    object?
        .get(&pos.to_string())
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn event_specific_external_event_id(
    event_kind: u16,
    values: Option<&Map<String, Value>>,
) -> Option<String> {
    if event_kind == MetricEventId::SessionEvent as u16 {
        return sparse_object_string(values, session_event_pos::EXTERNAL_EVENT_ID);
    }
    if event_kind == MetricEventId::OtelTrace as u16 {
        return sparse_object_string(values, otel_trace_pos::EXTERNAL_EVENT_ID);
    }
    None
}

fn event_specific_external_parent_event_id(
    event_kind: u16,
    values: Option<&Map<String, Value>>,
) -> Option<String> {
    if event_kind == MetricEventId::SessionEvent as u16 {
        return sparse_object_string(values, session_event_pos::EXTERNAL_PARENT_EVENT_ID);
    }
    if event_kind == MetricEventId::OtelTrace as u16 {
        return sparse_object_string(values, otel_trace_pos::EXTERNAL_PARENT_EVENT_ID);
    }
    None
}

fn event_specific_external_tool_use_id(
    event_kind: u16,
    values: Option<&Map<String, Value>>,
) -> Option<String> {
    if event_kind == MetricEventId::Checkpoint as u16 {
        return sparse_object_string(values, checkpoint_pos::TOOL_USE_ID);
    }
    if event_kind == MetricEventId::SessionEvent as u16 {
        return sparse_object_string(values, session_event_pos::EXTERNAL_TOOL_USE_ID);
    }
    if event_kind == MetricEventId::OtelTrace as u16 {
        return sparse_object_string(values, otel_trace_pos::EXTERNAL_TOOL_USE_ID);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::pos_encoded::PosEncoded;
    use crate::metrics::session_compaction::{SessionObservation, compact_session_event};
    use crate::metrics::{EventAttributes, MetricsBatch};
    use rusqlite::StatementStatus;
    use serde_json::json;
    use std::collections::{HashMap, HashSet};
    use tempfile::TempDir;

    fn create_test_db() -> (MetricsDatabase, TempDir) {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test-metrics.db");

        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL;").unwrap();

        let mut db = MetricsDatabase { conn };
        db.initialize_schema().unwrap();

        (db, temp_dir)
    }

    #[test]
    fn schema_migration_lock_io_error_fails_immediately() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("lock-error-metrics.db");
        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();
        let lock_path = PathBuf::from(format!("{}.migration.lock", db_path.display()));
        std::fs::create_dir(&lock_path).unwrap();
        let mut db = MetricsDatabase { conn };
        let started_at = Instant::now();

        let error = db
            .initialize_schema()
            .expect_err("migration lock I/O errors must not be retried as contention");

        assert!(matches!(error, GitAiError::IoError(_)));
        assert!(started_at.elapsed() < Duration::from_secs(1));
    }

    fn unix_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }

    fn days_ago(days: u64) -> u32 {
        seconds_ago(days * 24 * 3600)
    }

    fn seconds_ago(seconds: u64) -> u32 {
        unix_now().saturating_sub(seconds).min(u32::MAX as u64) as u32
    }

    fn event_json(ts: u32) -> String {
        format!(r#"{{"t":{ts},"e":1,"v":{{}},"a":{{}}}}"#)
    }

    fn event_json_with_repo(ts: u32, event_id: u16, repo: &str) -> String {
        format!(r#"{{"t":{ts},"e":{event_id},"v":{{}},"a":{{"1":"{repo}"}}}}"#)
    }

    fn pending_event_jsons(db: &MetricsDatabase) -> Vec<String> {
        let mut stmt = db
            .conn
            .prepare("SELECT event_json FROM metrics WHERE delivered_ts IS NULL ORDER BY id DESC")
            .unwrap();
        let rows = stmt.query_map([], |row| row.get::<_, String>(0)).unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap()
    }

    fn pending_metric_events(db: &MetricsDatabase) -> Vec<(i64, MetricEvent)> {
        let mut stmt = db
            .conn
            .prepare(
                "SELECT id, event_json FROM metrics \
                 WHERE delivered_ts IS NULL AND event_kind = ?1 ORDER BY id ASC",
            )
            .unwrap();
        let rows = stmt
            .query_map(params![MetricEventId::SessionTokenUsage as i64], |row| {
                let id = row.get::<_, i64>(0)?;
                let event_json = row.get::<_, String>(1)?;
                let event = serde_json::from_str(&event_json).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        event_json.len(),
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
                Ok((id, event))
            })
            .unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap()
    }

    fn assert_metric_index_exists(db: &MetricsDatabase, index: &str) {
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
                params![index],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "missing index {index}");
    }

    fn assert_metric_index_missing(db: &MetricsDatabase, index: &str) {
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?1",
                params![index],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "unexpected index {index}");
    }

    fn metric_metadata_rows(db: &MetricsDatabase) -> Vec<(Option<i64>, Option<i64>)> {
        let mut stmt = db
            .conn
            .prepare("SELECT event_ts, event_kind FROM metrics ORDER BY id ASC")
            .unwrap();
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap()
    }

    #[derive(Debug, PartialEq, Eq)]
    struct MetricIdentifierRow {
        trace_id: Option<String>,
        session_id: Option<String>,
        parent_session_id: Option<String>,
        tool: Option<String>,
        external_session_id: Option<String>,
        external_parent_session_id: Option<String>,
        external_event_id: Option<String>,
        external_parent_event_id: Option<String>,
        external_tool_use_id: Option<String>,
    }

    fn metric_identifier_rows(db: &MetricsDatabase) -> Vec<MetricIdentifierRow> {
        let mut stmt = db
            .conn
            .prepare(
                "SELECT trace_id, session_id, parent_session_id, tool, \
                        external_session_id, external_parent_session_id, \
                        external_event_id, external_parent_event_id, external_tool_use_id \
                 FROM metrics ORDER BY id ASC",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |row| {
                Ok(MetricIdentifierRow {
                    trace_id: row.get(0)?,
                    session_id: row.get(1)?,
                    parent_session_id: row.get(2)?,
                    tool: row.get(3)?,
                    external_session_id: row.get(4)?,
                    external_parent_session_id: row.get(5)?,
                    external_event_id: row.get(6)?,
                    external_parent_event_id: row.get(7)?,
                    external_tool_use_id: row.get(8)?,
                })
            })
            .unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap()
    }

    fn event_json_with_all_common_metadata(ts: u32, event_kind: u16) -> String {
        format!(
            r#"{{
                "t":{ts},
                "e":{event_kind},
                "v":{{}},
                "a":{{
                    "20":"codex",
                    "23":"external-session-1",
                    "24":"session-1",
                    "25":"trace-1",
                    "26":"parent-session-1",
                    "27":"external-parent-session-1"
                }}
            }}"#
        )
    }

    fn compact_observation(ts: u32, output_tokens: u64) -> SessionObservation {
        let raw = json!({
            "message": {
                "id": "msg-compact-1",
                "role": "assistant",
                "model": "claude-sonnet-4",
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": output_tokens,
                    "cache_read_input_tokens": 20,
                    "cache_creation_input_tokens": 3
                },
                "content": [{"type": "text", "text": "private transcript content"}]
            }
        });
        let attrs = EventAttributes::with_version("test")
            .repo_url("github.com/acme/repo")
            .tool("claude")
            .session_id("session-compact-1")
            .external_session_id("external-session-compact-1")
            .trace_id(format!("trace-{ts}"))
            .to_sparse();
        compact_session_event(
            &raw,
            ts,
            attrs,
            Some("external-event-1".to_string()),
            Some("external-parent-1".to_string()),
            Some("external-tool-1".to_string()),
        )
    }

    fn codex_cumulative_observation(
        ts: u32,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        baseline_only: bool,
    ) -> SessionObservation {
        codex_cumulative_observation_for_session(
            ts,
            input_tokens,
            output_tokens,
            cache_read_tokens,
            baseline_only,
            "session-codex-child",
        )
    }

    fn codex_cumulative_observation_for_session(
        ts: u32,
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        baseline_only: bool,
        session_id: &str,
    ) -> SessionObservation {
        let raw = json!({
            "_git_ai_token_baseline_only": baseline_only,
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": input_tokens,
                        "output_tokens": output_tokens,
                        "cached_input_tokens": cache_read_tokens
                    }
                }
            }
        });
        let attrs = EventAttributes::with_version("test")
            .repo_url("github.com/acme/repo")
            .author("alice@example.com")
            .tool("codex")
            .session_id(session_id)
            .external_session_id(format!("external-{session_id}"))
            .trace_id(format!("trace-{ts}"))
            .custom_attributes(r#"{"project_key":"repo","ide":"codex"}"#)
            .to_sparse();
        compact_session_event(&raw, ts, attrs, None, None, None)
    }

    fn with_reporting_email(
        mut observation: SessionObservation,
        user_email: &str,
    ) -> SessionObservation {
        observation.attrs.insert(
            attr_pos::CUSTOM_ATTRIBUTES.to_string(),
            Value::String(
                json!({
                    "project_key": "repo",
                    "ide": "codex",
                    "department_name": "云计算研发部",
                    "office_name": "研发四处",
                    "team_name": "研发一组",
                    "user_name": "Alice",
                    "user_email": user_email,
                    "git_ai_reporting_profile_version": REPORTING_PROFILE_VERSION,
                })
                .to_string(),
            ),
        );
        observation
    }

    #[test]
    fn test_codex_fork_baseline_seeds_source_without_daily_usage() {
        let (mut db, _temp_dir) = create_test_db();
        let first_ts = seconds_ago(60);

        let baseline = codex_cumulative_observation(first_ts, 200, 20, 80, true);
        assert!(
            db.insert_session_observations(&[baseline])
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            db.conn
                .query_row("SELECT COUNT(*) FROM session_token_sources", [], |row| row
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            1
        );
        assert_eq!(
            db.conn
                .query_row("SELECT COUNT(*) FROM session_token_daily", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );

        let child = codex_cumulative_observation(first_ts + 1, 230, 25, 90, false);
        let metric_ids = db.insert_session_observations(&[child]).unwrap();
        assert_eq!(metric_ids.len(), 1);
        let event = serde_json::from_str::<MetricEvent>(&pending_event_jsons(&db)[0]).unwrap();
        assert_eq!(event.values.get("1").and_then(Value::as_u64), Some(30));
        assert_eq!(event.values.get("2").and_then(Value::as_u64), Some(5));
        assert_eq!(event.values.get("3").and_then(Value::as_u64), Some(10));
        assert_eq!(event.values.get("5").and_then(Value::as_u64), Some(1));
        assert!(
            event
                .values
                .get("0")
                .and_then(Value::as_str)
                .is_some_and(|key| key.starts_with("td4:"))
        );
        let history = db.get_compact_token_history(0, None).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(
            (history[0].input, history[0].output, history[0].cache_read),
            (30, 5, 10)
        );
    }

    #[test]
    fn test_corrected_codex_source_hides_legacy_source_from_local_history() {
        let (mut db, _temp_dir) = create_test_db();
        let event_ts = seconds_ago(60);

        db.conn
            .execute(
                r#"
                INSERT INTO session_token_sources (
                    source_key, session_id, first_ts, last_ts, tool, model,
                    input_tokens, output_tokens, cumulative_source
                ) VALUES ('ts1:legacy', 'session-codex-child', ?1, ?1, 'codex', 'gpt-5',
                          10000, 1000, 1)
                "#,
                params![i64::from(event_ts)],
            )
            .unwrap();
        db.insert_session_observations(&[
            codex_cumulative_observation(event_ts, 200, 20, 80, true),
            codex_cumulative_observation(event_ts + 1, 230, 25, 90, false),
        ])
        .unwrap();

        let history = db.get_compact_token_history(0, None).unwrap();
        assert_eq!(history.len(), 1);
        assert!(history[0].source_key.starts_with("ts2:"));
        assert_eq!((history[0].input, history[0].output), (30, 5));
    }

    #[test]
    fn test_legacy_codex_fork_content_waits_for_boundary_aware_stream_replay() {
        let event = MetricEvent {
            timestamp: seconds_ago(60),
            event_id: MetricEventId::SessionEvent as u16,
            instance_id: None,
            values: [
                (
                    "0".to_string(),
                    json!({
                        "payload": {
                            "type": "token_count",
                            "info": {
                                "total_token_usage": {
                                    "input_tokens": 10000,
                                    "output_tokens": 1000
                                }
                            }
                        }
                    }),
                ),
                ("1".to_string(), json!("event-1")),
            ]
            .into_iter()
            .collect(),
            attrs: EventAttributes::with_version("test")
                .tool("codex")
                .session_id("session-codex-child")
                .parent_session_id("session-codex-parent")
                .external_session_id("external-codex-child")
                .external_parent_session_id("external-codex-parent")
                .to_sparse(),
        };

        let observation =
            compact_legacy_content_event(&serde_json::to_string(&event).unwrap()).unwrap();
        assert_eq!(observation.attrs, event.attrs);
        assert!(
            observation.token.is_none(),
            "legacy fork totals must not seed the corrected v2 source"
        );
    }

    fn recovery_observation(
        ts: u32,
        session_id: &str,
        external_session_id: Option<&str>,
        tool: &str,
        repo_url: Option<&str>,
    ) -> SessionObservation {
        let mut attrs = EventAttributes::with_version("test")
            .tool(tool)
            .model("gpt-5")
            .session_id(session_id)
            .trace_id(format!("trace-{session_id}"));
        if let Some(external_session_id) = external_session_id {
            attrs = attrs.external_session_id(external_session_id);
        }
        if let Some(repo_url) = repo_url {
            attrs = attrs.repo_url(repo_url);
        }
        SessionObservation {
            timestamp: ts,
            attrs: attrs.to_sparse(),
            external_event_id: Some(format!("event-{session_id}")),
            external_parent_event_id: None,
            external_tool_use_id: Some(format!("tool-use-{session_id}")),
            token: None,
        }
    }

    #[test]
    fn test_session_observations_keep_compact_state_and_coalesce_pending_daily_snapshot() {
        let (mut db, _temp_dir) = create_test_db();
        let first_ts = seconds_ago(60);
        let final_ts = first_ts + 1;

        let first_ids = db
            .insert_session_observations(&[compact_observation(first_ts, 8)])
            .unwrap();
        assert_eq!(first_ids.len(), 1);

        // Replaying an already persisted stream batch must be idempotent.
        let replay_ids = db
            .insert_session_observations(&[compact_observation(first_ts, 8)])
            .unwrap();
        assert!(replay_ids.is_empty());

        let final_ids = db
            .insert_session_observations(&[compact_observation(final_ts, 41)])
            .unwrap();
        assert_eq!(final_ids.len(), 1);
        assert_eq!(final_ids[0], first_ids[0]);

        let event_jsons = pending_event_jsons(&db);
        assert_eq!(event_jsons.len(), 1);
        assert!(
            event_jsons
                .iter()
                .all(|event| !event.contains("private transcript content"))
        );
        assert!(
            event_jsons.iter().all(|event| event.len() < 1_024),
            "compact upload event unexpectedly exceeded 1 KiB"
        );
        let output_snapshots = event_jsons
            .iter()
            .map(|event| serde_json::from_str::<MetricEvent>(event).unwrap())
            .map(|event| {
                assert_eq!(event.event_id, MetricEventId::SessionTokenUsage as u16);
                event.values.get("2").and_then(Value::as_u64).unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(output_snapshots, vec![41]);

        let (session_rows, recovery_rows, token_rows, daily_rows): (i64, i64, i64, i64) = (
            db.conn
                .query_row("SELECT COUNT(*) FROM session_activity", [], |row| {
                    row.get(0)
                })
                .unwrap(),
            db.conn
                .query_row("SELECT COUNT(*) FROM session_recovery_events", [], |row| {
                    row.get(0)
                })
                .unwrap(),
            db.conn
                .query_row("SELECT COUNT(*) FROM session_token_sources", [], |row| {
                    row.get(0)
                })
                .unwrap(),
            db.conn
                .query_row("SELECT COUNT(*) FROM session_token_daily", [], |row| {
                    row.get(0)
                })
                .unwrap(),
        );
        assert_eq!(
            (session_rows, recovery_rows, token_rows, daily_rows),
            (1, 2, 1, 1)
        );
        let (input, output, cache_read, cache_write): (i64, i64, i64, i64) = db.conn
            .query_row(
                "SELECT input_tokens, output_tokens, cache_read_tokens, cache_write_tokens FROM session_token_sources",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!((input, output, cache_read, cache_write), (10, 41, 20, 3));
        let (daily_input, daily_output, daily_requests): (i64, i64, i64) = db
            .conn
            .query_row(
                "SELECT input_tokens, output_tokens, request_count FROM session_token_daily",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!((daily_input, daily_output, daily_requests), (10, 41, 1));
    }

    #[test]
    fn test_streaming_token_copies_in_one_batch_emit_one_daily_snapshot() {
        let (mut db, _temp_dir) = create_test_db();
        let first_ts = seconds_ago(60);
        let metric_ids = db
            .insert_session_observations(&[
                compact_observation(first_ts, 8),
                compact_observation(first_ts + 1, 41),
            ])
            .unwrap();

        assert_eq!(metric_ids.len(), 1);
        let event_jsons = pending_event_jsons(&db);
        assert_eq!(event_jsons.len(), 1);
        let event = serde_json::from_str::<MetricEvent>(&event_jsons[0]).unwrap();
        assert_eq!(event.event_id, MetricEventId::SessionTokenUsage as u16);
        assert_eq!(event.values.get("1").and_then(Value::as_u64), Some(10));
        assert_eq!(event.values.get("2").and_then(Value::as_u64), Some(41));
        assert_eq!(event.values.get("3").and_then(Value::as_u64), Some(20));
        assert_eq!(event.values.get("4").and_then(Value::as_u64), Some(3));
        assert_eq!(event.values.get("5").and_then(Value::as_u64), Some(1));
        assert_eq!(
            event.values.get("10").and_then(Value::as_str),
            Some("git-ai-test-install")
        );
    }

    #[test]
    fn test_daily_snapshot_keeps_anonymous_sessions_separate_with_same_dimensions() {
        let (mut db, _temp_dir) = create_test_db();
        let first_ts = seconds_ago(60);
        let first = compact_observation(first_ts, 8);
        let mut second = compact_observation(first_ts + 1, 8);
        second.attrs.insert(
            attr_pos::SESSION_ID.to_string(),
            Value::String("session-compact-2".to_string()),
        );
        second.attrs.insert(
            attr_pos::EXTERNAL_SESSION_ID.to_string(),
            Value::String("external-session-compact-2".to_string()),
        );
        second.token.as_mut().unwrap().source_key = "ts1:second-session".to_string();

        db.insert_session_observations(&[first, second]).unwrap();

        let event_jsons = pending_event_jsons(&db);
        assert_eq!(event_jsons.len(), 2);
        let events = event_jsons
            .iter()
            .map(|event| serde_json::from_str::<MetricEvent>(event).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            events
                .iter()
                .map(|event| event.values.get("1").and_then(Value::as_u64).unwrap())
                .sum::<u64>(),
            20
        );
        assert_eq!(
            events
                .iter()
                .map(|event| event.values.get("2").and_then(Value::as_u64).unwrap())
                .sum::<u64>(),
            16
        );
        assert_eq!(
            events
                .iter()
                .map(|event| event.values.get("5").and_then(Value::as_u64).unwrap())
                .sum::<u64>(),
            2
        );
        assert!(events.iter().all(|event| {
            sparse_get_string(&event.attrs, attr_pos::SESSION_ID)
                .flatten()
                .is_none()
        }));
        assert_ne!(
            events[0].values["0"], events[1].values["0"],
            "anonymous Event9 source keys must retain a stable session provenance boundary"
        );
    }

    #[test]
    fn test_daily_snapshot_bucket_matches_server_unique_dimensions() {
        let attrs = |branch: &str, custom_attributes: &str| {
            EventAttributes::with_version("test")
                .repo_url("git@github.com:acme/repo.git")
                .author("Alice <Alice@Example.COM>")
                .branch(branch)
                .tool("claude")
                .model("claude-sonnet-4")
                .session_id("session-dimension-test")
                .custom_attributes(custom_attributes)
                .to_sparse()
        };
        let timestamp = seconds_ago(60);
        let base = attrs(
            "main",
            r#"{"project_key":"repo","ide":"vscode","organization_id":"org-a"}"#,
        );
        let irrelevant_changes = attrs(
            "feature/new-ui",
            r#"{"project_key":"repo","ide":"vscode","organization_id":"org-b","display_name":"Alice B"}"#,
        );
        let different_ide = attrs(
            "main",
            r#"{"project_key":"repo","ide":"intellij","organization_id":"org-a"}"#,
        );
        let different_project = attrs(
            "main",
            r#"{"project_key":"another-repo","ide":"vscode","organization_id":"org-a"}"#,
        );

        let base_key = daily_token_bucket(
            timestamp,
            &base,
            Some("anthropic"),
            "install-1",
            "ts1:test",
            "session-dimension-test",
        )
        .unwrap()
        .0;
        assert_eq!(
            base_key,
            daily_token_bucket(
                timestamp,
                &irrelevant_changes,
                Some("anthropic"),
                "install-1",
                "ts1:test",
                "session-dimension-test",
            )
            .unwrap()
            .0,
            "branch and organization are descriptive fields, not daily fact dimensions"
        );
        assert_ne!(
            base_key,
            daily_token_bucket(
                timestamp,
                &different_ide,
                Some("anthropic"),
                "install-1",
                "ts1:test",
                "session-dimension-test",
            )
            .unwrap()
            .0
        );
        assert_ne!(
            base_key,
            daily_token_bucket(
                timestamp,
                &different_project,
                Some("anthropic"),
                "install-1",
                "ts1:test",
                "session-dimension-test",
            )
            .unwrap()
            .0
        );
        assert_ne!(
            base_key,
            daily_token_bucket(
                timestamp,
                &base,
                Some("anthropic"),
                "install-2",
                "ts1:test",
                "session-dimension-test",
            )
            .unwrap()
            .0,
            "separate installations must not overwrite each other's cumulative snapshots"
        );
    }

    #[test]
    fn test_daily_snapshot_uses_credential_free_canonical_remote_identity() {
        let attrs = |repo_url: Option<&str>| {
            let mut attrs = EventAttributes::with_version("test")
                .author("Alice <alice@example.com>")
                .tool("claude")
                .model("claude-sonnet-4")
                .custom_attributes(r#"{"ide":"vscode"}"#);
            if let Some(repo_url) = repo_url {
                attrs = attrs.repo_url(repo_url);
            }
            attrs.to_sparse()
        };
        let timestamp = seconds_ago(60);
        let bucket = |repo_url: Option<&str>| {
            daily_token_bucket(
                timestamp,
                &attrs(repo_url),
                Some("anthropic"),
                "install-1",
                "ts1:test",
                "session-canonical-remote",
            )
            .unwrap()
        };

        let org_a_ssh = bucket(Some("git@github.com:org-a/api.git"));
        let org_a_https_with_secret = bucket(Some("https://oauth:secret@GitHub.COM/org-a/api.GIT"));
        let org_a_https_with_rotated_secret =
            bucket(Some("https://oauth:rotated@github.com/org-a/api.git"));
        let org_b_same_basename = bucket(Some("https://github.com/org-b/api"));
        assert!(org_a_ssh.0.starts_with("td3:"));
        assert_eq!(org_a_ssh.0, org_a_https_with_secret.0);
        assert_eq!(org_a_https_with_secret.0, org_a_https_with_rotated_secret.0);
        assert_ne!(org_a_ssh.0, org_b_same_basename.0);
        assert_eq!(
            sparse_get_string(&org_a_https_with_secret.4, attr_pos::REPO_URL).flatten(),
            Some("https://github.com/org-a/api".to_string())
        );

        let gerrit_ssh = bucket(Some(
            "ssh://git@review.example.com:29418/a/platform/api.git",
        ));
        let gerrit_https = bucket(Some("https://review.example.com:29418/platform/api"));
        let other_gerrit_port = bucket(Some(
            "ssh://git@review.example.com:29419/a/platform/api.git",
        ));
        assert_eq!(gerrit_ssh.0, gerrit_https.0);
        assert_ne!(gerrit_ssh.0, other_gerrit_port.0);
        assert_eq!(
            sparse_get_string(&gerrit_ssh.4, attr_pos::REPO_URL).flatten(),
            Some("https://review.example.com:29418/platform/api".to_string())
        );

        let no_remote = bucket(None);
        assert!(
            sparse_get_string(&no_remote.4, attr_pos::REPO_URL)
                .flatten()
                .is_none(),
            "without a remote the compact fact remains unbound to a project"
        );
    }

    #[test]
    fn test_new_daily_snapshot_does_not_mutate_an_in_flight_snapshot() {
        let (mut db, _temp_dir) = create_test_db();
        let first_ts = seconds_ago(60);
        let first_id = db
            .insert_session_observations(&[compact_observation(first_ts, 8)])
            .unwrap()[0];
        let claimed = db.dequeue_pending_batch(1).unwrap();
        assert_eq!(claimed[0].id, first_id);

        let successor_id = db
            .insert_session_observations(&[compact_observation(first_ts + 1, 41)])
            .unwrap()[0];
        assert_ne!(successor_id, first_id);

        db.mark_records_delivered(&[first_id], unix_now()).unwrap();
        let remaining = pending_event_jsons(&db);
        assert_eq!(remaining.len(), 1);
        let event = serde_json::from_str::<MetricEvent>(&remaining[0]).unwrap();
        assert_eq!(event.values.get("2").and_then(Value::as_u64), Some(41));
    }

    #[test]
    fn test_daily_snapshot_migrates_unknown_identity_into_known_in_flight_bucket() {
        let (mut db, _temp_dir) = create_test_db();
        let first_ts = seconds_ago(60);
        let first_id = db
            .insert_session_observations(&[codex_cumulative_observation(
                first_ts, 100, 0, 0, false,
            )])
            .unwrap()[0];
        let anonymous_event =
            serde_json::from_str::<MetricEvent>(&pending_event_jsons(&db)[0]).unwrap();
        let anonymous_source_key = anonymous_event
            .values
            .get("0")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();
        let anonymous_snapshot_instance_id = anonymous_event
            .values
            .get("12")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();
        assert_eq!(db.dequeue_pending_batch(1).unwrap()[0].id, first_id);

        let known = with_reporting_email(
            codex_cumulative_observation(first_ts + 1, 150, 0, 0, false),
            "alice@example.com",
        );
        let successor_id = db.insert_session_observations(&[known]).unwrap()[0];
        assert_ne!(
            successor_id, first_id,
            "an in-flight snapshot must remain immutable"
        );

        let daily_rows: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM session_token_daily", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            daily_rows, 1,
            "unknown and known identity snapshots must share one cumulative bucket"
        );

        db.mark_records_delivered(&[first_id], unix_now()).unwrap();
        let remaining = pending_event_jsons(&db);
        assert_eq!(remaining.len(), 1);
        let event = serde_json::from_str::<MetricEvent>(&remaining[0]).unwrap();
        assert_eq!(event.values.get("1").and_then(Value::as_u64), Some(150));
        assert_eq!(
            event.values["11"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>(),
            vec![anonymous_source_key.as_str()]
        );
        assert_eq!(
            event.values["13"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>(),
            vec![anonymous_snapshot_instance_id.as_str()]
        );
        assert_ne!(
            event.values.get("12").and_then(Value::as_str),
            Some(anonymous_snapshot_instance_id.as_str())
        );
        assert_eq!(
            compact_custom_attributes(&event.attrs)
                .get("user_email")
                .and_then(Value::as_str),
            Some("alice@example.com")
        );
    }

    #[test]
    fn test_daily_snapshot_replaces_pending_unknown_bucket_with_known_total() {
        let (mut db, _temp_dir) = create_test_db();
        let first_ts = seconds_ago(60);
        db.insert_session_observations(&[codex_cumulative_observation(first_ts, 100, 0, 0, false)])
            .unwrap();

        let known = with_reporting_email(
            codex_cumulative_observation(first_ts + 1, 150, 0, 0, false),
            "alice@example.com",
        );
        db.insert_session_observations(&[known]).unwrap();

        assert_eq!(
            db.conn
                .query_row("SELECT COUNT(*) FROM session_token_daily", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        let pending = pending_event_jsons(&db);
        assert_eq!(pending.len(), 1);
        let event = serde_json::from_str::<MetricEvent>(&pending[0]).unwrap();
        assert_eq!(event.values.get("1").and_then(Value::as_u64), Some(150));
        assert_eq!(
            compact_identity_email(&event.attrs).as_deref(),
            Some("alice@example.com")
        );
    }

    #[test]
    fn test_daily_snapshot_restart_rehydrates_and_resends_without_new_tokens_idempotently() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("restart-metrics.db");
        let first_ts = seconds_ago(60);
        let mut db = MetricsDatabase::open_at_path(&db_path).unwrap();
        let anonymous_id = db
            .insert_session_observations(&[codex_cumulative_observation(
                first_ts, 100, 0, 0, false,
            )])
            .unwrap()[0];
        db.mark_records_delivered(&[anonymous_id], unix_now())
            .unwrap();
        drop(db);

        let mut restarted = MetricsDatabase::open_at_path(&db_path).unwrap();
        assert!(restarted.repair_daily_token_buckets().unwrap().is_empty());
        let refreshed = restarted
            .insert_session_observations(&[with_reporting_email(
                codex_cumulative_observation(first_ts + 1, 100, 0, 0, false),
                "alice@example.com",
            )])
            .unwrap();
        assert_eq!(refreshed.len(), 1);
        let pending = pending_event_jsons(&restarted);
        assert_eq!(pending.len(), 1);
        let event = serde_json::from_str::<MetricEvent>(&pending[0]).unwrap();
        assert_eq!(event.values.get("1").and_then(Value::as_u64), Some(100));
        assert_eq!(
            compact_identity_email(&event.attrs).as_deref(),
            Some("alice@example.com")
        );

        let first_successor_id = refreshed[0];
        drop(restarted);
        let mut restarted_again = MetricsDatabase::open_at_path(&db_path).unwrap();
        assert!(
            restarted_again
                .repair_daily_token_buckets()
                .unwrap()
                .is_empty()
        );
        assert_eq!(pending_event_jsons(&restarted_again).len(), 1);
        let persisted_outbox_id: i64 = restarted_again
            .conn
            .query_row(
                "SELECT outbox_metric_id FROM session_token_daily",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(persisted_outbox_id, first_successor_id);
    }

    #[test]
    fn test_restart_profile_b_cannot_claim_session_a_anonymous_delta() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("restart-a-to-b-metrics.db");
        let first_ts = seconds_ago(90);
        let mut db = MetricsDatabase::open_at_path(&db_path).unwrap();
        db.insert_session_observations(&[with_reporting_email(
            codex_cumulative_observation(first_ts, 100, 0, 0, false),
            "alice@example.com",
        )])
        .unwrap();
        db.insert_session_observations(&[codex_cumulative_observation(
            first_ts + 1,
            120,
            0,
            0,
            false,
        )])
        .unwrap();
        let anonymous_before_restart = pending_metric_events(&db)
            .into_iter()
            .find(|(_, event)| compact_identity_email(&event.attrs).is_none())
            .unwrap()
            .1;
        let anonymous_source_key = anonymous_before_restart.values["0"]
            .as_str()
            .unwrap()
            .to_string();
        let anonymous_instance_id = anonymous_before_restart.values["12"]
            .as_str()
            .unwrap()
            .to_string();
        drop(db);

        let mut restarted = MetricsDatabase::open_at_path(&db_path).unwrap();
        assert!(
            restarted.repair_daily_token_buckets().unwrap().is_empty(),
            "the current process profile cannot prove a historical anonymous bucket's identity"
        );
        let anonymous_after_repair = pending_metric_events(&restarted)
            .into_iter()
            .find(|(_, event)| compact_identity_email(&event.attrs).is_none())
            .unwrap()
            .1;
        assert_eq!(
            anonymous_after_repair.values["0"].as_str(),
            Some(anonymous_source_key.as_str()),
            "a conservative restart repair must preserve the stable Event9 source key"
        );
        assert_eq!(
            anonymous_after_repair.values["12"].as_str(),
            Some(anonymous_instance_id.as_str()),
            "a conservative restart repair must not create a duplicate Event9 generation"
        );

        restarted
            .insert_session_observations(&[with_reporting_email(
                codex_cumulative_observation(first_ts + 2, 170, 0, 0, false),
                "bob@example.com",
            )])
            .unwrap();

        let mut snapshots = pending_metric_events(&restarted)
            .into_iter()
            .map(|(_, event)| {
                (
                    compact_identity_email(&event.attrs),
                    event.values["1"].as_u64().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        snapshots.sort();
        assert_eq!(
            snapshots,
            vec![
                (None, 20),
                (Some("alice@example.com".to_string()), 100),
                (Some("bob@example.com".to_string()), 50),
            ]
        );
    }

    #[test]
    fn test_known_profile_only_claims_anonymous_tokens_from_the_same_session() {
        let (mut db, _temp_dir) = create_test_db();
        let first_ts = seconds_ago(60);
        db.insert_session_observations(&[codex_cumulative_observation_for_session(
            first_ts,
            100,
            0,
            0,
            false,
            "session-anonymous",
        )])
        .unwrap();
        db.insert_session_observations(&[with_reporting_email(
            codex_cumulative_observation_for_session(
                first_ts + 1,
                50,
                0,
                0,
                false,
                "session-alice",
            ),
            "alice@example.com",
        )])
        .unwrap();

        let mut snapshots = pending_metric_events(&db)
            .into_iter()
            .map(|(_, event)| {
                (
                    compact_identity_email(&event.attrs),
                    event.values["1"].as_u64().unwrap(),
                )
            })
            .collect::<Vec<_>>();
        snapshots.sort();
        assert_eq!(
            snapshots,
            vec![(None, 100), (Some("alice@example.com".to_string()), 50),],
            "a daily anonymous bucket must not mix sessions before identity recovery"
        );
    }

    #[test]
    fn test_git_author_email_stays_anonymous_until_reporting_profile_is_configured() {
        let (mut db, _temp_dir) = create_test_db();
        let first_ts = seconds_ago(60);
        let mut observation = codex_cumulative_observation(first_ts, 100, 0, 0, false);
        observation.attrs.insert(
            attr_pos::AUTHOR.to_string(),
            Value::String("Git Author <author@example.com>".to_string()),
        );
        observation.attrs.insert(
            attr_pos::CUSTOM_ATTRIBUTES.to_string(),
            Value::String(
                json!({
                    "project_key": "repo",
                    "ide": "codex",
                    "email": "alias@example.com",
                    "userEmail": "alias@example.com",
                    "user_email": "forged@example.com",
                    "git_ai_reporting_profile_version": REPORTING_PROFILE_VERSION,
                    "organization_id": "stale-cloud-org",
                    "team_name": "stale-team",
                })
                .to_string(),
            ),
        );

        let original_id = db.insert_session_observations(&[observation]).unwrap()[0];
        let original = pending_metric_events(&db)
            .into_iter()
            .find(|(id, _)| *id == original_id)
            .unwrap()
            .1;
        assert_eq!(compact_identity_email(&original.attrs), None);

        db.mark_records_delivered(&[original_id], unix_now())
            .unwrap();
        assert!(db.repair_daily_token_buckets().unwrap().is_empty());
        let refreshed = db
            .insert_session_observations(&[with_reporting_email(
                codex_cumulative_observation(first_ts + 1, 100, 0, 0, false),
                "configured@example.com",
            )])
            .unwrap();
        assert_eq!(refreshed.len(), 1);
        let successor = pending_metric_events(&db)
            .into_iter()
            .find(|(id, _)| *id == refreshed[0])
            .unwrap()
            .1;
        assert_eq!(
            compact_identity_email(&successor.attrs).as_deref(),
            Some("configured@example.com")
        );
        let successor_profile = compact_custom_attributes(&successor.attrs);
        assert!(!successor_profile.contains_key("email"));
        assert!(!successor_profile.contains_key("userEmail"));
        assert!(!successor_profile.contains_key("organization_id"));
        assert_eq!(
            successor_profile.get("team_name").and_then(Value::as_str),
            Some("研发一组")
        );
    }

    #[test]
    fn test_daily_snapshot_rekeys_legacy_project_identity_with_exact_supersession() {
        let (mut db, _temp_dir) = create_test_db();
        let timestamp = seconds_ago(60);
        let with_email_but_no_project_key = |mut observation: SessionObservation| {
            observation.attrs.insert(
                attr_pos::CUSTOM_ATTRIBUTES.to_string(),
                Value::String(
                    json!({
                        "ide": "codex",
                        "user_email": "alice@example.com",
                    })
                    .to_string(),
                ),
            );
            observation
        };
        let observation = with_email_but_no_project_key(compact_observation(timestamp, 8));
        db.insert_session_observations(&[observation]).unwrap();
        let initial_event = pending_metric_events(&db)[0].1.clone();
        let canonical_bucket_key = initial_event.values["0"].as_str().unwrap().to_string();
        let legacy_bucket_key = "td1:legacy-basename-project-key";
        let legacy_instance_id = initial_event.values["12"].as_str().unwrap().to_string();

        db.conn
            .execute(
                "UPDATE session_token_daily SET bucket_key = ?1 WHERE bucket_key = ?2",
                params![legacy_bucket_key, canonical_bucket_key],
            )
            .unwrap();
        let tx = db.conn.transaction().unwrap();
        refresh_daily_token_outbox(&tx, legacy_bucket_key).unwrap();
        tx.commit().unwrap();

        let refreshed = db.repair_daily_token_buckets().unwrap();
        assert_eq!(refreshed.len(), 1);
        let ambiguous_event = pending_metric_events(&db)[0].1.clone();
        assert_ne!(
            ambiguous_event.values["0"].as_str(),
            Some(canonical_bucket_key.as_str())
        );
        assert!(
            ambiguous_event.values["0"]
                .as_str()
                .unwrap()
                .starts_with("td3:")
        );
        assert_eq!(
            ambiguous_event.values["11"].as_array().unwrap(),
            &[Value::String(legacy_bucket_key.to_string())]
        );
        assert_eq!(
            ambiguous_event.values["13"].as_array().unwrap(),
            &[Value::String(legacy_instance_id)]
        );
        assert_eq!(
            sparse_get_string(&ambiguous_event.attrs, attr_pos::REPO_URL).flatten(),
            None
        );
        assert_eq!(
            compact_custom_attributes(&ambiguous_event.attrs)
                .get("project_identity_status")
                .and_then(Value::as_str),
            Some("legacy_basename_ambiguous")
        );
        let ambiguous_attrs = compact_custom_attributes(&ambiguous_event.attrs);
        let project_key = ambiguous_attrs
            .get("project_key")
            .and_then(Value::as_str)
            .unwrap();
        let identity_sha256 = ambiguous_attrs
            .get("legacy_project_identity_sha256")
            .and_then(Value::as_str)
            .unwrap();
        let expected_identity = legacy_unbound_project_identity(legacy_bucket_key);
        assert_eq!(
            (project_key, identity_sha256),
            (expected_identity.0.as_str(), expected_identity.1.as_str())
        );
        assert!(project_key.starts_with("git-ai-unbound:"));
        assert!(project_key.len() <= 64);
        assert_eq!(identity_sha256.len(), 64);
        assert_ne!(
            legacy_unbound_project_identity("td1:another-legacy-bucket").0,
            project_key
        );
        let mut undomained_hasher = Sha256::new();
        undomained_hasher.update(legacy_bucket_key.as_bytes());
        assert_ne!(
            identity_sha256,
            format!("{:x}", undomained_hasher.finalize())
        );
        assert_eq!(
            ambiguous_attrs.get("project_name").and_then(Value::as_str),
            Some("历史项目归属不可判定")
        );
        assert_eq!(ambiguous_event.values["1"].as_u64(), Some(10));
        assert_eq!(ambiguous_event.values["2"].as_u64(), Some(8));

        let follow_up = with_email_but_no_project_key(compact_observation(timestamp + 1, 20));
        db.insert_session_observations(&[follow_up]).unwrap();
        let events = pending_metric_events(&db)
            .into_iter()
            .map(|(_, event)| event)
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 2);
        let canonical_event = events
            .iter()
            .find(|event| {
                sparse_get_string(&event.attrs, attr_pos::REPO_URL).flatten()
                    == Some("https://github.com/acme/repo".to_string())
            })
            .unwrap();
        assert_eq!(
            canonical_event.values["0"].as_str(),
            Some(canonical_bucket_key.as_str())
        );
        assert_eq!(canonical_event.values["1"].as_u64(), Some(0));
        assert_eq!(canonical_event.values["2"].as_u64(), Some(12));
        assert_eq!(
            events
                .iter()
                .map(|event| event.values["1"].as_u64().unwrap())
                .sum::<u64>(),
            10
        );
        assert_eq!(
            events
                .iter()
                .map(|event| event.values["2"].as_u64().unwrap())
                .sum::<u64>(),
            20
        );
    }

    #[test]
    fn test_daily_snapshot_keeps_valid_email_a_to_b_as_distinct_business_buckets() {
        let (mut db, _temp_dir) = create_test_db();
        let first_ts = seconds_ago(60);
        let user_a = with_reporting_email(
            codex_cumulative_observation(first_ts, 100, 0, 0, false),
            "alice@example.com",
        );
        let temporarily_unknown = codex_cumulative_observation(first_ts + 1, 120, 0, 0, false);
        let user_b = with_reporting_email(
            codex_cumulative_observation(first_ts + 2, 170, 0, 0, false),
            "bob@example.com",
        );
        db.insert_session_observations(&[user_a]).unwrap();
        db.insert_session_observations(&[temporarily_unknown])
            .unwrap();
        db.insert_session_observations(&[user_b]).unwrap();

        assert!(db.repair_daily_token_buckets().unwrap().is_empty());
        assert_eq!(
            db.conn
                .query_row("SELECT COUNT(*) FROM session_token_daily", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            3
        );
        let mut snapshots = pending_event_jsons(&db)
            .into_iter()
            .map(|event| serde_json::from_str::<MetricEvent>(&event).unwrap())
            .map(|event| {
                (
                    compact_identity_email(&event.attrs),
                    event.values.get("1").and_then(Value::as_u64).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        snapshots.sort();
        assert_eq!(
            snapshots,
            vec![
                (None, 20),
                (Some("alice@example.com".to_string()), 100),
                (Some("bob@example.com".to_string()), 50),
            ]
        );
    }

    #[test]
    fn test_legacy_complete_profile_without_marker_is_not_reassigned_to_current_user() {
        let (mut db, _temp_dir) = create_test_db();
        let first_ts = seconds_ago(60);
        let mut legacy_user_a = codex_cumulative_observation(first_ts, 100, 0, 0, false);
        legacy_user_a.attrs.insert(
            attr_pos::CUSTOM_ATTRIBUTES.to_string(),
            Value::String(
                json!({
                    "project_key": "repo",
                    "ide": "codex",
                    "department_name": "云计算研发部",
                    "office_name": "研发四处",
                    "team_name": "研发一组",
                    "user_name": "Alice",
                    "user_email": "alice@example.com",
                })
                .to_string(),
            ),
        );

        db.insert_session_observations(&[legacy_user_a]).unwrap();
        assert!(db.repair_daily_token_buckets().unwrap().is_empty());

        let event = serde_json::from_str::<MetricEvent>(&pending_event_jsons(&db)[0]).unwrap();
        assert_eq!(
            compact_identity_email(&event.attrs).as_deref(),
            Some("alice@example.com")
        );
        assert!(
            !compact_custom_attributes(&event.attrs)
                .contains_key(REPORTING_PROFILE_VERSION_ATTRIBUTE),
            "legacy identity should remain byte-compatible rather than being rewritten as current profile"
        );
    }

    #[test]
    fn test_daily_snapshot_same_batch_keeps_transient_unknown_delta_across_a_to_b() {
        let (mut db, _temp_dir) = create_test_db();
        let first_ts = seconds_ago(60);
        let user_a = with_reporting_email(
            codex_cumulative_observation(first_ts, 100, 0, 0, false),
            "alice@example.com",
        );
        let temporarily_unknown = codex_cumulative_observation(first_ts + 1, 120, 0, 0, false);
        let user_b = with_reporting_email(
            codex_cumulative_observation(first_ts + 2, 170, 0, 0, false),
            "bob@example.com",
        );

        db.insert_session_observations(&[user_a, temporarily_unknown, user_b])
            .unwrap();

        let mut snapshots = pending_event_jsons(&db)
            .into_iter()
            .map(|event| serde_json::from_str::<MetricEvent>(&event).unwrap())
            .map(|event| {
                (
                    compact_identity_email(&event.attrs),
                    event.values.get("1").and_then(Value::as_u64).unwrap(),
                )
            })
            .collect::<Vec<_>>();
        snapshots.sort();
        assert_eq!(
            snapshots,
            vec![
                (None, 20),
                (Some("alice@example.com".to_string()), 100),
                (Some("bob@example.com".to_string()), 50),
            ]
        );
    }

    #[test]
    fn test_conflicting_profile_does_not_supersede_reused_anonymous_generation() {
        let (mut db, _temp_dir) = create_test_db();
        let first_ts = seconds_ago(90);

        let anonymous_generation_one_id = db
            .insert_session_observations(&[codex_cumulative_observation(
                first_ts, 100, 0, 0, false,
            )])
            .unwrap()[0];
        let anonymous_generation_one = pending_metric_events(&db)[0].1.clone();
        let shared_anonymous_source_key = anonymous_generation_one.values["0"]
            .as_str()
            .unwrap()
            .to_string();
        let anonymous_instance_one = anonymous_generation_one.values["12"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            db.dequeue_pending_batch(1).unwrap()[0].id,
            anonymous_generation_one_id
        );

        let user_a = with_reporting_email(
            codex_cumulative_observation(first_ts + 1, 150, 0, 0, false),
            "alice@example.com",
        );
        db.insert_session_observations(&[user_a]).unwrap();
        let user_a_id = pending_metric_events(&db)
            .into_iter()
            .find(|(_, event)| {
                compact_identity_email(&event.attrs).as_deref() == Some("alice@example.com")
            })
            .unwrap()
            .0;
        assert_eq!(db.dequeue_pending_batch(10).unwrap()[0].id, user_a_id);

        let anonymous_generation_two_id = db
            .insert_session_observations(&[codex_cumulative_observation(
                first_ts + 2,
                170,
                0,
                0,
                false,
            )])
            .unwrap()[0];
        let anonymous_generation_two = pending_metric_events(&db)
            .into_iter()
            .find(|(id, _)| *id == anonymous_generation_two_id)
            .unwrap()
            .1;
        assert_eq!(
            anonymous_generation_two.values["0"].as_str(),
            Some(shared_anonymous_source_key.as_str())
        );
        let anonymous_instance_two = anonymous_generation_two.values["12"]
            .as_str()
            .unwrap()
            .to_string();
        assert_ne!(anonymous_instance_one, anonymous_instance_two);
        assert_eq!(
            db.dequeue_pending_batch(1).unwrap()[0].id,
            anonymous_generation_two_id
        );

        let user_b = with_reporting_email(
            codex_cumulative_observation(first_ts + 3, 220, 0, 0, false),
            "bob@example.com",
        );
        db.insert_session_observations(&[user_b]).unwrap();

        let identified = pending_metric_events(&db)
            .into_iter()
            .filter_map(|(id, event)| {
                compact_identity_email(&event.attrs).map(|email| (email, (id, event)))
            })
            .collect::<HashMap<_, _>>();
        let (_, user_a_event) = identified.get("alice@example.com").unwrap();
        let (user_b_id, user_b_event) = identified.get("bob@example.com").unwrap();
        assert_eq!(
            user_a_event.values["13"].as_array().unwrap()[0].as_str(),
            Some(anonymous_instance_one.as_str())
        );
        assert_eq!(
            user_a_event.values["11"].as_array().unwrap()[0].as_str(),
            Some(shared_anonymous_source_key.as_str())
        );
        assert!(
            user_b_event
                .values
                .get("11")
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty)
                && user_b_event
                    .values
                    .get("13")
                    .and_then(Value::as_array)
                    .is_none_or(Vec::is_empty),
            "profile B must not claim an anonymous generation from a session already bound to A"
        );
        assert!(
            pending_metric_events(&db).into_iter().any(|(_, event)| {
                compact_identity_email(&event.attrs).is_none()
                    && event.values["0"].as_str() == Some(shared_anonymous_source_key.as_str())
                    && event.values["12"].as_str() == Some(anonymous_instance_two.as_str())
            }),
            "the ambiguous generation must remain preserved under its original stable identity"
        );

        // ACK corrections out of order. Each ACK clears only the aliases owned
        // by that exact identified outbox generation.
        db.mark_records_delivered(&[*user_b_id], unix_now())
            .unwrap();
        db.mark_records_delivered(&[user_a_id], unix_now()).unwrap();
        let alias_rows: Vec<(String, String)> = {
            let mut stmt = db
                .conn
                .prepare(
                    "SELECT supersedes_source_keys_json, \
                            supersedes_snapshot_instance_ids_json \
                     FROM session_token_daily ORDER BY bucket_key",
                )
                .unwrap();
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert!(
            alias_rows
                .iter()
                .all(|(source_keys, instances)| source_keys == "[]" && instances == "[]")
        );
    }

    #[test]
    fn test_identity_rekey_does_not_supersede_other_project_or_ide_bucket() {
        let (mut db, _temp_dir) = create_test_db();
        let first_ts = seconds_ago(60);
        let mut project_a = compact_observation(first_ts, 8);
        project_a.attrs.insert(
            attr_pos::CUSTOM_ATTRIBUTES.to_string(),
            Value::String(r#"{"project_key":"project-a","ide":"vscode"}"#.to_string()),
        );
        let mut project_b = compact_observation(first_ts + 1, 8);
        project_b.attrs.insert(
            attr_pos::SESSION_ID.to_string(),
            Value::String("session-project-b".to_string()),
        );
        project_b.attrs.insert(
            attr_pos::EXTERNAL_SESSION_ID.to_string(),
            Value::String("external-project-b".to_string()),
        );
        project_b.attrs.insert(
            attr_pos::CUSTOM_ATTRIBUTES.to_string(),
            Value::String(r#"{"project_key":"project-b","ide":"intellij"}"#.to_string()),
        );
        project_b.token.as_mut().unwrap().source_key = "ts1:project-b".to_string();
        db.insert_session_observations(&[project_a, project_b])
            .unwrap();

        let anonymous_events = pending_metric_events(&db);
        assert_eq!(anonymous_events.len(), 2);
        let project_a_event = anonymous_events
            .iter()
            .find(|(_, event)| {
                compact_custom_attributes(&event.attrs)
                    .get("project_key")
                    .and_then(Value::as_str)
                    == Some("project-a")
            })
            .unwrap()
            .1
            .clone();
        let project_b_event = anonymous_events
            .iter()
            .find(|(_, event)| {
                compact_custom_attributes(&event.attrs)
                    .get("project_key")
                    .and_then(Value::as_str)
                    == Some("project-b")
            })
            .unwrap()
            .1
            .clone();

        let mut known_a = compact_observation(first_ts + 2, 41);
        known_a.attrs.insert(
            attr_pos::CUSTOM_ATTRIBUTES.to_string(),
            Value::String(
                json!({
                    "project_key": "project-a",
                    "ide": "vscode",
                    "department_name": "云计算研发部",
                    "office_name": "研发四处",
                    "user_name": "Alice",
                    "user_email": "alice@example.com",
                    "git_ai_reporting_profile_version": REPORTING_PROFILE_VERSION,
                })
                .to_string(),
            ),
        );
        db.insert_session_observations(&[known_a]).unwrap();

        let events = pending_metric_events(&db);
        let corrected_a = events
            .iter()
            .find(|(_, event)| {
                compact_identity_email(&event.attrs).as_deref() == Some("alice@example.com")
            })
            .unwrap();
        assert_eq!(
            corrected_a.1.values["11"].as_array().unwrap()[0].as_str(),
            project_a_event.values["0"].as_str()
        );
        assert_eq!(
            corrected_a.1.values["13"].as_array().unwrap()[0].as_str(),
            project_a_event.values["12"].as_str()
        );
        assert!(
            corrected_a.1.values["11"]
                .as_array()
                .unwrap()
                .iter()
                .all(|key| key.as_str() != project_b_event.values["0"].as_str())
        );
        assert!(
            events.iter().any(|(_, event)| {
                compact_identity_email(&event.attrs).is_none()
                    && event.values["12"] == project_b_event.values["12"]
            }),
            "the unrelated project/IDE anonymous snapshot must remain independently pending"
        );
    }

    #[test]
    fn test_compact_history_clamps_sources_that_cross_the_query_boundary() {
        let (mut db, _temp_dir) = create_test_db();
        let first_ts = seconds_ago(120);
        let since_ts = first_ts + 30;
        let final_ts = first_ts + 60;
        db.insert_session_observations(&[
            compact_observation(first_ts, 8),
            compact_observation(final_ts, 41),
        ])
        .unwrap();

        let sessions = db.get_compact_session_history(since_ts, None).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].first_ts, since_ts);
        assert_eq!(sessions[0].last_ts, final_ts);

        let tokens = db.get_compact_token_history(since_ts, None).unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].usage_ts, since_ts);
        assert_eq!(tokens[0].output, 41);
    }

    #[test]
    fn test_delivered_compact_token_events_are_deleted_while_history_events_remain() {
        let (mut db, _temp_dir) = create_test_db();
        let compact_ids = db
            .insert_session_observations(&[compact_observation(seconds_ago(30), 8)])
            .unwrap();
        let history_ids = db.insert_events(&[event_json(seconds_ago(20))]).unwrap();

        db.mark_records_delivered(&[compact_ids[0], history_ids[0]], unix_now())
            .unwrap();

        let compact_count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM metrics WHERE event_kind = ?1",
                params![MetricEventId::SessionTokenUsage as i64],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(compact_count, 0);
        let retained_count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(retained_count, 1);
        assert_eq!(db.status().unwrap().delivered, 1);
    }

    #[test]
    fn test_transient_compact_token_failures_remain_retryable_after_attempt_limit() {
        let (mut db, _temp_dir) = create_test_db();
        let compact_ids = db
            .insert_session_observations(&[compact_observation(seconds_ago(30), 8)])
            .unwrap();
        let compact_id = compact_ids[0];

        for attempt in 0..10 {
            db.mark_records_failed(&[compact_id], "backend unavailable", attempt)
                .unwrap();
        }
        db.conn
            .execute(
                "UPDATE metrics SET next_retry_at = 0 WHERE id = ?1",
                params![compact_id],
            )
            .unwrap();

        let attempts: i64 = db
            .conn
            .query_row(
                "SELECT attempts FROM metrics WHERE id = ?1",
                params![compact_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(attempts, (MAX_METRIC_UPLOAD_ATTEMPTS - 1) as i64);
        assert_eq!(db.dequeue_pending_batch(10).unwrap()[0].id, compact_id);
    }

    #[test]
    fn test_deferred_request_failures_remain_retryable_after_attempt_limit() {
        let (mut db, _temp_dir) = create_test_db();
        let event_id = db.insert_events(&[event_json(seconds_ago(30))]).unwrap()[0];

        for attempt in 0..10 {
            db.mark_records_deferred(&[event_id], "HTTP 503: backend unavailable", attempt)
                .unwrap();
        }
        db.conn
            .execute(
                "UPDATE metrics SET next_retry_at = 0 WHERE id = ?1",
                params![event_id],
            )
            .unwrap();

        let attempts: i64 = db
            .conn
            .query_row(
                "SELECT attempts FROM metrics WHERE id = ?1",
                params![event_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(attempts, (MAX_METRIC_UPLOAD_ATTEMPTS - 1) as i64);
        assert_eq!(db.dequeue_pending_batch(10).unwrap()[0].id, event_id);
    }

    #[test]
    fn metric_event_instance_id_survives_sqlite_retry_roundtrip() {
        let (mut db, _temp_dir) = create_test_db();
        let event = MetricEvent::with_timestamp(
            seconds_ago(30),
            &crate::metrics::events::AgentUsageValues::new(),
            SparseArray::new(),
        );
        let expected_instance_id = event
            .instance_id
            .clone()
            .expect("new metric event instance id");
        let event_json = serde_json::to_string(&event).unwrap();
        let event_id = db.insert_events(&[event_json]).unwrap()[0];

        let first_record = db.dequeue_pending_batch(1).unwrap().remove(0);
        let first_event: MetricEvent = serde_json::from_str(&first_record.event_json).unwrap();
        let first_wire = serde_json::to_string(&first_event).unwrap();
        db.mark_records_deferred(&[event_id], "ACK lost after commit", 0)
            .unwrap();

        let second_record = db.dequeue_pending_batch(1).unwrap().remove(0);
        let second_event: MetricEvent = serde_json::from_str(&second_record.event_json).unwrap();
        let second_wire = serde_json::to_string(&second_event).unwrap();

        assert_eq!(
            first_event.instance_id.as_deref(),
            Some(expected_instance_id.as_str())
        );
        assert_eq!(
            second_event.instance_id.as_deref(),
            Some(expected_instance_id.as_str())
        );
        assert_eq!(first_wire, second_wire);
    }

    #[test]
    fn test_compact_recovery_markers_preserve_attribution_candidate_metadata() {
        let (mut db, _temp_dir) = create_test_db();
        let event_ts = seconds_ago(10);
        db.insert_session_observations(&[compact_observation(event_ts, 8)])
            .unwrap();

        let candidates = db
            .session_event_candidates_near_timestamps(
                &[u128::from(event_ts) * NS_PER_SECOND + 500_000_000],
                NS_PER_SECOND,
            )
            .unwrap();
        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];
        assert_eq!(candidate.event_ts, event_ts);
        assert_eq!(candidate.session_id, "session-compact-1");
        assert_eq!(candidate.external_session_id, "external-session-compact-1");
        assert_eq!(
            candidate.external_tool_use_id.as_deref(),
            Some("external-tool-1")
        );
        assert_eq!(candidate.model.as_deref(), Some("claude-sonnet-4"));
        assert_eq!(candidate.repo_url.as_deref(), Some("github.com/acme/repo"));

        let latest = db
            .latest_session_event_candidates_for_tools(&["claude"])
            .unwrap();
        assert_eq!(latest.len(), 1);
        assert_eq!(latest[0], *candidate);
    }

    #[test]
    fn test_recovery_markers_coalesce_same_second_content_but_keep_distinct_tool_calls() {
        let (mut db, _temp_dir) = create_test_db();
        let event_ts = seconds_ago(10);
        let mut first = compact_observation(event_ts, 8);
        first.token = None;
        first.external_event_id = Some("content-a".to_string());
        first.external_parent_event_id = Some("parent-a".to_string());
        first.external_tool_use_id = None;
        let mut second = first.clone();
        second.external_event_id = Some("content-b".to_string());
        second.external_parent_event_id = Some("parent-b".to_string());
        let mut tool_a = first.clone();
        tool_a.external_tool_use_id = Some("tool-call-a".to_string());
        let mut tool_b = first.clone();
        tool_b.external_tool_use_id = Some("tool-call-b".to_string());

        db.insert_session_observations(&[first, second, tool_a, tool_b])
            .unwrap();

        let recovery_rows: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM session_recovery_events", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(recovery_rows, 3);
        let candidates = db
            .session_event_candidates_near_timestamps(
                &[u128::from(event_ts) * NS_PER_SECOND + 500_000_000],
                NS_PER_SECOND,
            )
            .unwrap();
        assert_eq!(candidates.len(), 3);
        assert_eq!(
            candidates
                .iter()
                .filter_map(|candidate| candidate.external_tool_use_id.as_deref())
                .collect::<HashSet<_>>(),
            HashSet::from(["tool-call-a", "tool-call-b"])
        );
    }

    #[test]
    fn test_initialize_schema() {
        let (db, _temp_dir) = create_test_db();

        // Verify metrics table exists
        let count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='metrics'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        // Verify schema_metadata exists with correct version
        let version: String = db
            .conn
            .query_row(
                "SELECT value FROM schema_metadata WHERE key = 'version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION.to_string());

        for column in [
            "delivered_ts",
            "attempts",
            "last_sync_error",
            "last_sync_at",
            "next_retry_at",
            "processing_started_at",
            "event_ts",
            "event_kind",
            "trace_id",
            "session_id",
            "parent_session_id",
            "tool",
            "external_session_id",
            "external_parent_session_id",
            "external_event_id",
            "external_parent_event_id",
            "external_tool_use_id",
        ] {
            let column_count: i64 = db
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('metrics') WHERE name = ?1",
                    params![column],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(column_count, 1, "missing column {column}");
        }

        for index in [
            "metrics_retryable",
            "metrics_event_ts_kind",
            "metrics_session_kind_ts",
            "metrics_parent_session_kind_ts",
            "session_activity_last_ts",
            "session_recovery_event_ts",
            "session_recovery_tool_latest",
            "session_token_sources_history",
            "session_token_daily_history",
        ] {
            assert_metric_index_exists(&db, index);
        }

        for table in [
            "session_activity",
            "session_recovery_events",
            "session_token_sources",
            "session_token_daily",
            "deferred_commit_metric_jobs",
            "deferred_lifecycle_metric_jobs",
            "deferred_checkpoint_jobs",
        ] {
            let table_count: i64 = db
                .conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    params![table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(table_count, 1, "missing table {table}");
        }
        assert!(
            db.column_exists("deferred_commit_metric_jobs", "parent_authorship_note")
                .unwrap(),
            "deferred Event 1 jobs must persist the parent-note snapshot"
        );
        assert!(
            db.column_exists("session_activity", "reporting_identity_email")
                .unwrap()
        );
        assert!(
            db.column_exists("session_activity", "reporting_identity_state")
                .unwrap()
        );
        for column in [
            "path_scope_json",
            "admission_owner",
            "blocked_evidence",
            "blocked_reason",
        ] {
            assert!(
                db.column_exists("deferred_checkpoint_jobs", column)
                    .unwrap(),
                "fresh schema is missing deferred checkpoint column {column}"
            );
        }
    }

    #[test]
    fn test_open_at_path_initializes_schema() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("explicit-metrics.db");
        let mut db = MetricsDatabase::open_at_path(&db_path).unwrap();

        db.insert_events(&[event_json(days_ago(1))]).unwrap();

        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let busy_timeout_ms: u64 = db
            .conn
            .pragma_query_value(None, "busy_timeout", |row| row.get(0))
            .unwrap();
        assert_eq!(
            busy_timeout_ms,
            METRICS_SQLITE_BUSY_TIMEOUT.as_millis() as u64
        );
    }

    #[test]
    fn test_initialize_schema_handles_preexisting_agent_usage_table() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("concurrent-init.db");
        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();

        // Simulate a partial migration state from a concurrent process:
        // schema version indicates agent_usage_throttle is missing, but it already exists.
        conn.execute_batch(
            r#"
            CREATE TABLE schema_metadata (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );
            INSERT INTO schema_metadata (key, value) VALUES ('version', '1');
            CREATE TABLE metrics (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_json TEXT NOT NULL
            );
            CREATE TABLE agent_usage_throttle (
                tool TEXT PRIMARY KEY NOT NULL,
                agent_last_seen_at INTEGER NOT NULL,
                command_last_seen_at INTEGER NOT NULL
            );
            "#,
        )
        .unwrap();

        let mut db = MetricsDatabase { conn };
        db.initialize_schema().unwrap();

        let version: String = db
            .conn
            .query_row(
                "SELECT value FROM schema_metadata WHERE key = 'version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION.to_string());
    }

    #[test]
    fn test_migrates_version_2_to_row_level_retry_schema() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("v2.db");
        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE schema_metadata (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );
            INSERT INTO schema_metadata (key, value) VALUES ('version', '2');
            CREATE TABLE metrics (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_json TEXT NOT NULL
            );
            INSERT INTO metrics (event_json) VALUES ('{"t":1,"e":1,"v":{},"a":{}}');
            CREATE TABLE agent_usage_throttle (
                prompt_id TEXT PRIMARY KEY,
                last_sent_ts INTEGER NOT NULL
            );
            "#,
        )
        .unwrap();

        let mut db = MetricsDatabase { conn };
        db.initialize_schema().unwrap();

        let version: String = db
            .conn
            .query_row(
                "SELECT value FROM schema_metadata WHERE key = 'version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION.to_string());
        assert_eq!(db.count().unwrap(), 1);
        assert_eq!(db.count_retryable().unwrap(), 1);
    }

    #[test]
    fn test_migrates_version_2_with_preexisting_retry_columns() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("v2-partial-retry.db");
        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE schema_metadata (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );
            INSERT INTO schema_metadata (key, value) VALUES ('version', '2');
            CREATE TABLE metrics (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_json TEXT NOT NULL,
                delivered_ts INTEGER,
                attempts INTEGER NOT NULL DEFAULT 0
            );
            INSERT INTO metrics (event_json) VALUES ('{"t":1,"e":1,"v":{},"a":{}}');
            CREATE TABLE agent_usage_throttle (
                prompt_id TEXT PRIMARY KEY,
                last_sent_ts INTEGER NOT NULL
            );
            "#,
        )
        .unwrap();

        let mut db = MetricsDatabase { conn };
        db.initialize_schema().unwrap();

        let version: String = db
            .conn
            .query_row(
                "SELECT value FROM schema_metadata WHERE key = 'version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION.to_string());

        for column in [
            "delivered_ts",
            "attempts",
            "last_sync_error",
            "last_sync_at",
            "next_retry_at",
            "processing_started_at",
            "event_ts",
            "event_kind",
            "trace_id",
            "session_id",
            "parent_session_id",
            "tool",
            "external_session_id",
            "external_parent_session_id",
            "external_event_id",
            "external_parent_event_id",
            "external_tool_use_id",
        ] {
            assert!(db.column_exists("metrics", column).unwrap());
        }
        assert_eq!(db.count_retryable().unwrap(), 1);
    }

    #[test]
    fn test_migrates_version_3_to_event_metadata_schema_without_sync_backfill() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("v3.db");
        let conn = crate::sqlite::open_with_memory_limits(&db_path).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE schema_metadata (
                key TEXT PRIMARY KEY NOT NULL,
                value TEXT NOT NULL
            );
            INSERT INTO schema_metadata (key, value) VALUES ('version', '3');
            CREATE TABLE metrics (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_json TEXT NOT NULL,
                delivered_ts INTEGER,
                attempts INTEGER NOT NULL DEFAULT 0,
                last_sync_error TEXT,
                last_sync_at INTEGER,
                next_retry_at INTEGER NOT NULL DEFAULT 0,
                processing_started_at INTEGER
            );
            INSERT INTO metrics (event_json)
            VALUES ('{"t":1700000000,"e":4,"v":{},"a":{}}');
            CREATE TABLE agent_usage_throttle (
                prompt_id TEXT PRIMARY KEY,
                last_sent_ts INTEGER NOT NULL
            );
            "#,
        )
        .unwrap();

        let mut db = MetricsDatabase { conn };
        db.initialize_schema().unwrap();

        let version: String = db
            .conn
            .query_row(
                "SELECT value FROM schema_metadata WHERE key = 'version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION.to_string());
        assert!(db.column_exists("metrics", "event_ts").unwrap());
        assert!(db.column_exists("metrics", "event_kind").unwrap());
        for index in [
            "metrics_event_ts_kind",
            "metrics_session_kind_ts",
            "metrics_parent_session_kind_ts",
        ] {
            assert_metric_index_exists(&db, index);
        }
        assert_eq!(metric_metadata_rows(&db), vec![(None, None)]);
        assert_eq!(
            metric_identifier_rows(&db),
            vec![MetricIdentifierRow {
                trace_id: None,
                session_id: None,
                parent_session_id: None,
                tool: None,
                external_session_id: None,
                external_parent_session_id: None,
                external_event_id: None,
                external_parent_event_id: None,
                external_tool_use_id: None,
            }]
        );
    }

    #[test]
    fn test_migrates_version_4_to_retryable_only_index() {
        let (mut db, _temp_dir) = create_test_db();
        let ids = db.insert_events(&[event_json(days_ago(1))]).unwrap();
        db.conn
            .execute(
                "UPDATE metrics SET attempts = 6 WHERE id = ?1",
                params![ids[0]],
            )
            .unwrap();
        db.conn
            .execute_batch(
                r#"
                DROP INDEX metrics_retryable;
                CREATE INDEX metrics_pending_retry
                    ON metrics (delivered_ts, next_retry_at, id)
                    WHERE delivered_ts IS NULL;
                UPDATE schema_metadata SET value = '4' WHERE key = 'version';
                "#,
            )
            .unwrap();

        db.initialize_schema().unwrap();

        let version: String = db
            .conn
            .query_row(
                "SELECT value FROM schema_metadata WHERE key = 'version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION.to_string());
        assert_metric_index_exists(&db, "metrics_retryable");
        assert_metric_index_missing(&db, "metrics_pending_retry");
        assert_eq!(db.count().unwrap(), 1);
        assert_eq!(db.status().unwrap().stopped_after_errors, 1);
    }

    #[test]
    fn test_migrates_version_5_to_compact_session_schema() {
        let (mut db, _temp_dir) = create_test_db();
        db.conn
            .execute_batch(
                r#"
                DROP TABLE session_token_sources;
                DROP TABLE session_token_daily;
                DROP TABLE session_recovery_events;
                DROP TABLE session_activity;
                UPDATE schema_metadata SET value = '5' WHERE key = 'version';
                "#,
            )
            .unwrap();

        db.initialize_schema().unwrap();

        let version: String = db
            .conn
            .query_row(
                "SELECT value FROM schema_metadata WHERE key = 'version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION.to_string());
        assert!(db.column_exists("session_activity", "last_ts").unwrap());
        assert!(
            db.column_exists("session_activity", "reporting_identity_email")
                .unwrap()
        );
        assert!(
            db.column_exists("session_activity", "reporting_identity_state")
                .unwrap()
        );
        assert!(
            db.column_exists("session_recovery_events", "external_tool_use_id")
                .unwrap()
        );
        assert!(
            db.column_exists("session_token_sources", "cumulative_source")
                .unwrap()
        );
        assert!(
            db.column_exists("session_token_daily", "outbox_metric_id")
                .unwrap()
        );
    }

    #[test]
    fn test_migrates_version_12_sessions_as_legacy_identity_ambiguous() {
        let (mut db, _temp_dir) = create_test_db();
        db.conn
            .execute_batch(
                r#"
                DROP INDEX session_activity_last_ts;
                DROP TABLE session_activity;
                CREATE TABLE session_activity (
                    session_id TEXT PRIMARY KEY NOT NULL,
                    first_ts INTEGER NOT NULL,
                    last_ts INTEGER NOT NULL,
                    tool TEXT NOT NULL,
                    model TEXT,
                    repo_url TEXT,
                    external_session_id TEXT
                );
                CREATE INDEX session_activity_last_ts
                    ON session_activity (last_ts, repo_url, tool);
                INSERT INTO session_activity (
                    session_id, first_ts, last_ts, tool, model, repo_url,
                    external_session_id
                ) VALUES (
                    'legacy-session', 1, 2, 'codex', 'gpt-5',
                    'https://github.com/acme/repo', 'external-legacy-session'
                );
                UPDATE schema_metadata SET value = '12' WHERE key = 'version';
                "#,
            )
            .unwrap();

        db.initialize_schema().unwrap();

        let (email, state): (Option<String>, String) = db
            .conn
            .query_row(
                "SELECT reporting_identity_email, reporting_identity_state \
                 FROM session_activity WHERE session_id = 'legacy-session'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(email, None);
        assert_eq!(state, "legacy_ambiguous");
    }

    #[test]
    fn test_migrates_version_13_deferred_checkpoint_recovery_columns_idempotently() {
        let (mut db, _temp_dir) = create_test_db();
        db.conn
            .execute_batch(
                r#"
                DROP TABLE deferred_checkpoint_jobs;
                CREATE TABLE deferred_checkpoint_jobs (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    job_key TEXT NOT NULL UNIQUE,
                    repo_identity TEXT NOT NULL,
                    repository_workdir TEXT NOT NULL,
                    integration TEXT NOT NULL,
                    external_session_id TEXT NOT NULL,
                    external_tool_use_id TEXT NOT NULL,
                    phase TEXT NOT NULL,
                    request_shape_sha256 TEXT NOT NULL,
                    request_evidence_sha256 TEXT NOT NULL,
                    request_json TEXT NOT NULL,
                    metrics_context_json TEXT NOT NULL,
                    observed_at_ms INTEGER NOT NULL,
                    prepared_checkpoint_json TEXT,
                    prepared_metric_events_json TEXT,
                    working_log_applied INTEGER NOT NULL DEFAULT 0
                        CHECK (working_log_applied IN (0, 1)),
                    state TEXT NOT NULL DEFAULT 'pending'
                        CHECK (state IN ('pending', 'processing', 'done')),
                    attempts INTEGER NOT NULL DEFAULT 0,
                    next_retry_at INTEGER NOT NULL DEFAULT 0,
                    processing_started_at INTEGER,
                    lease_token TEXT,
                    last_error TEXT,
                    metric_ids_json TEXT NOT NULL DEFAULT '[]',
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    completed_at INTEGER
                );
                CREATE INDEX deferred_checkpoint_jobs_due
                    ON deferred_checkpoint_jobs (state, next_retry_at, id)
                    WHERE state != 'done';
                CREATE INDEX deferred_checkpoint_jobs_repo_order
                    ON deferred_checkpoint_jobs (repo_identity, state, id);
                INSERT INTO deferred_checkpoint_jobs (
                    job_key, repo_identity, repository_workdir, integration,
                    external_session_id, external_tool_use_id, phase,
                    request_shape_sha256, request_evidence_sha256, request_json,
                    metrics_context_json, observed_at_ms, created_at, updated_at
                ) VALUES (
                    'legacy-job', 'repo', '/repo', 'kilo-v7', 'session', 'call',
                    'pre', 'shape', 'evidence', '{}', '{}', 1, 1, 1
                );
                UPDATE schema_metadata SET value = '13' WHERE key = 'version';
                "#,
            )
            .unwrap();

        db.initialize_schema().unwrap();
        db.initialize_schema()
            .expect("schema 14 recovery-column repair must be idempotent");

        for column in [
            "path_scope_json",
            "admission_owner",
            "blocked_evidence",
            "blocked_reason",
        ] {
            assert!(
                db.column_exists("deferred_checkpoint_jobs", column)
                    .unwrap(),
                "migration is missing deferred checkpoint column {column}"
            );
        }
        let legacy: (Option<String>, i64, Option<String>) = db
            .conn
            .query_row(
                "SELECT path_scope_json, blocked_evidence, blocked_reason FROM deferred_checkpoint_jobs WHERE job_key = 'legacy-job'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(legacy, (None, 0, None));

        let due_index_sql: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'deferred_checkpoint_jobs_due'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(due_index_sql.contains("blocked_evidence = 0"));
    }

    #[test]
    fn test_migrates_version_14_checkpoint_manual_terminal_columns_idempotently() {
        let (mut db, _temp_dir) = create_test_db();
        db.conn
            .execute_batch(
                r#"
                DROP INDEX deferred_checkpoint_jobs_due;
                DROP INDEX deferred_checkpoint_jobs_repo_order;
                DROP TABLE deferred_checkpoint_jobs;
                CREATE TABLE deferred_checkpoint_jobs (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    job_key TEXT NOT NULL UNIQUE,
                    repo_identity TEXT NOT NULL,
                    repository_workdir TEXT NOT NULL,
                    integration TEXT NOT NULL,
                    external_session_id TEXT NOT NULL,
                    external_tool_use_id TEXT NOT NULL,
                    phase TEXT NOT NULL,
                    request_shape_sha256 TEXT NOT NULL,
                    request_evidence_sha256 TEXT NOT NULL,
                    request_json TEXT NOT NULL,
                    metrics_context_json TEXT NOT NULL,
                    path_scope_json TEXT,
                    admission_owner TEXT,
                    observed_at_ms INTEGER NOT NULL,
                    prepared_checkpoint_json TEXT,
                    prepared_metric_events_json TEXT,
                    working_log_applied INTEGER NOT NULL DEFAULT 0
                        CHECK (working_log_applied IN (0, 1)),
                    state TEXT NOT NULL DEFAULT 'pending'
                        CHECK (state IN ('pending', 'processing', 'done')),
                    attempts INTEGER NOT NULL DEFAULT 0,
                    next_retry_at INTEGER NOT NULL DEFAULT 0,
                    processing_started_at INTEGER,
                    lease_token TEXT,
                    last_error TEXT,
                    blocked_evidence INTEGER NOT NULL DEFAULT 0
                        CHECK (blocked_evidence IN (0, 1)),
                    blocked_reason TEXT,
                    metric_ids_json TEXT NOT NULL DEFAULT '[]',
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    completed_at INTEGER
                );
                CREATE INDEX deferred_checkpoint_jobs_due
                    ON deferred_checkpoint_jobs (state, next_retry_at, id)
                    WHERE state != 'done' AND blocked_evidence = 0;
                CREATE INDEX deferred_checkpoint_jobs_repo_order
                    ON deferred_checkpoint_jobs (repo_identity, state, id);
                INSERT INTO deferred_checkpoint_jobs (
                    job_key, repo_identity, repository_workdir, integration,
                    external_session_id, external_tool_use_id, phase,
                    request_shape_sha256, request_evidence_sha256, request_json,
                    metrics_context_json, observed_at_ms, blocked_evidence,
                    blocked_reason, created_at, updated_at
                ) VALUES (
                    'blocked-v14', 'repo', '/repo', 'kilo-v7', 'session', 'call',
                    'pre', 'shape', 'evidence', '{}', '{}', 1, 1,
                    'corrupt INITIAL', 1, 1
                );
                UPDATE schema_metadata SET value = '14' WHERE key = 'version';
                "#,
            )
            .unwrap();

        db.initialize_schema().unwrap();
        db.initialize_schema()
            .expect("schema 15 terminal-column repair must be idempotent");

        for column in ["terminal_resolution", "repair_id", "repair_backup_path"] {
            assert!(
                db.column_exists("deferred_checkpoint_jobs", column)
                    .unwrap(),
                "migration is missing deferred checkpoint column {column}"
            );
        }
        let migrated: (String, Option<String>, Option<String>) = db
            .conn
            .query_row(
                "SELECT terminal_resolution, repair_id, repair_backup_path FROM deferred_checkpoint_jobs WHERE job_key = 'blocked-v14'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(migrated, ("normal".to_string(), None, None));
    }

    #[test]
    fn test_migrates_version_6_by_projecting_then_removing_legacy_content_rows() {
        let (mut db, _temp_dir) = create_test_db();
        let auto_vacuum_before: i64 = db
            .conn
            .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            auto_vacuum_before, 0,
            "the legacy database must exercise auto_vacuum=NONE"
        );
        let event_ts = unix_now().min(u32::MAX as u64) as u32;
        let session = format!(
            r#"{{
                "t":{event_ts},"e":5,
                "v":{{"0":{{"message":{{"id":"msg-1","role":"assistant","model":"claude-sonnet-4","usage":{{"input_tokens":10,"output_tokens":20}},"content":"private response"}}}},"1":"event-1","3":"tool-1"}},
                "a":{{"20":"claude","23":"external-1","24":"session-1","25":"trace-1","1":"github.com/acme/repo"}}
            }}"#,
        );
        let otel = format!(
            r#"{{
                "t":{event_ts},"e":6,
                "v":{{"0":{{"span":{{"prompt":"private prompt"}}}},"1":"span-1"}},
                "a":{{"20":"copilot","23":"external-2","24":"session-2","25":"trace-2"}}
            }}"#,
        );
        db.conn
            .execute_batch(
                r#"
                DROP TRIGGER metrics_reject_legacy_content_insert;
                UPDATE schema_metadata SET value = '6' WHERE key = 'version';
                "#,
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO metrics (event_json, event_ts, event_kind) VALUES (?1, ?2, 5), (?3, ?2, 6)",
                params![session, i64::from(event_ts), otel],
            )
            .unwrap();

        db.initialize_schema().unwrap();

        let version: String = db
            .conn
            .query_row(
                "SELECT value FROM schema_metadata WHERE key = 'version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION.to_string());
        let raw_rows: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM metrics WHERE event_kind IN (5, 6)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(raw_rows, 0);
        let activities: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM session_activity", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(activities, 2);
        let token_sources: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM session_token_sources", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(token_sources, 1);
        let content_leaks: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM metrics WHERE event_json LIKE '%private response%' OR event_json LIKE '%private prompt%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(content_leaks, 0);
        let auto_vacuum_after: i64 = db
            .conn
            .query_row("PRAGMA auto_vacuum", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            auto_vacuum_after, 0,
            "migration must not run a full VACUUM or rewrite a legacy database"
        );

        db.conn
            .execute(
                "UPDATE schema_metadata SET value = '6' WHERE key = 'version'",
                [],
            )
            .unwrap();
        db.initialize_schema().unwrap();
        let projection_counts: (i64, i64, i64) = db
            .conn
            .query_row(
                r#"
                SELECT
                    (SELECT COUNT(*) FROM session_activity),
                    (SELECT COUNT(*) FROM session_recovery_events),
                    (SELECT COUNT(*) FROM session_token_sources)
                "#,
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(projection_counts, (2, 2, 1));
    }

    #[test]
    fn test_schema_v7_rejects_new_legacy_content_rows_even_for_old_insert_shape() {
        let (mut db, _temp_dir) = create_test_db();
        let event = format!(
            r#"{{"t":{},"e":5,"v":{{"0":{{"content":"private"}}}},"a":{{}}}}"#,
            seconds_ago(0)
        );

        let structured_insert = db.insert_events(std::slice::from_ref(&event));
        assert!(structured_insert.is_err());
        let old_binary_insert = db.conn.execute(
            "INSERT INTO metrics (event_json) VALUES (?1)",
            params![event],
        );
        assert!(old_binary_insert.is_err());
        assert_eq!(db.count().unwrap(), 0);
    }

    #[test]
    fn test_insert_events() {
        let (mut db, _temp_dir) = create_test_db();
        let ts1 = days_ago(2);
        let ts2 = days_ago(1);

        let events = vec![
            format!(r#"{{"t":{ts1},"e":1,"v":{{"0":"abc123"}},"a":{{"0":"1.0.0"}}}}"#),
            format!(r#"{{"t":{ts2},"e":1,"v":{{"0":"def456"}},"a":{{"0":"1.0.0"}}}}"#),
        ];

        let ids = db.insert_events(&events).unwrap();

        let count = db.count().unwrap();
        assert_eq!(count, 2);
        assert_eq!(db.count_retryable().unwrap(), 2);
        assert_eq!(ids.len(), 2);
        assert_eq!(
            metric_metadata_rows(&db),
            vec![(Some(ts1 as i64), Some(1)), (Some(ts2 as i64), Some(1))]
        );
    }

    #[test]
    fn test_insert_events_populates_existing_common_metadata_from_attrs() {
        let (mut db, _temp_dir) = create_test_db();
        let event_ts = days_ago(1);
        db.insert_events(&[event_json_with_all_common_metadata(event_ts, 7)])
            .unwrap();

        let row: (Option<i64>, Option<i64>) = db
            .conn
            .query_row("SELECT event_ts, event_kind FROM metrics", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(row, (Some(event_ts as i64), Some(7)));
        assert_eq!(
            metric_identifier_rows(&db),
            vec![MetricIdentifierRow {
                trace_id: Some("trace-1".to_string()),
                session_id: Some("session-1".to_string()),
                parent_session_id: Some("parent-session-1".to_string()),
                tool: Some("codex".to_string()),
                external_session_id: Some("external-session-1".to_string()),
                external_parent_session_id: Some("external-parent-session-1".to_string()),
                external_event_id: None,
                external_parent_event_id: None,
                external_tool_use_id: None,
            }]
        );
    }

    #[test]
    fn test_insert_events_with_delivered_ts_populates_event_metadata() {
        let (mut db, _temp_dir) = create_test_db();
        let delivered_ts = unix_now();
        let event_ts = days_ago(1);
        db.insert_events_with_delivered_ts(
            &[event_json_with_all_common_metadata(event_ts, 8)],
            Some(delivered_ts),
        )
        .unwrap();

        let row: (Option<i64>, Option<i64>, Option<i64>, Option<String>) = db
            .conn
            .query_row(
                "SELECT event_ts, event_kind, delivered_ts, trace_id FROM metrics",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (
                Some(event_ts as i64),
                Some(8),
                Some(delivered_ts as i64),
                Some("trace-1".to_string())
            )
        );
    }

    #[test]
    fn test_metadata_parser_preserves_legacy_external_ids_without_persisting_content() {
        let (mut db, _temp_dir) = create_test_db();
        let session_event_ts = days_ago(2);
        let otel_trace_ts = days_ago(1);
        let checkpoint_ts = unix_now().min(u32::MAX as u64) as u32;
        let session_event = format!(
            r#"{{
                    "t":{session_event_ts},
                    "e":5,
                    "v":{{"1":"legacy-event","2":"legacy-parent","3":"legacy-tool"}},
                    "a":{{"24":"session-from-attrs"}}
                }}"#
        );
        let otel_trace = format!(
            r#"{{
                    "t":{otel_trace_ts},
                    "e":6,
                    "v":{{"1":"otel-event","2":"otel-parent","3":"otel-tool"}},
                    "a":{{"25":"trace-from-attrs"}}
                }}"#
        );
        let session_metadata = extract_metric_event_metadata(&session_event).unwrap();
        assert_eq!(
            session_metadata.external_event_id.as_deref(),
            Some("legacy-event")
        );
        assert_eq!(
            session_metadata.external_parent_event_id.as_deref(),
            Some("legacy-parent")
        );
        assert_eq!(
            session_metadata.external_tool_use_id.as_deref(),
            Some("legacy-tool")
        );
        let otel_metadata = extract_metric_event_metadata(&otel_trace).unwrap();
        assert_eq!(
            otel_metadata.external_event_id.as_deref(),
            Some("otel-event")
        );
        assert_eq!(
            otel_metadata.external_parent_event_id.as_deref(),
            Some("otel-parent")
        );
        assert_eq!(
            otel_metadata.external_tool_use_id.as_deref(),
            Some("otel-tool")
        );

        db.insert_events(&[format!(
            r#"{{
                    "t":{checkpoint_ts},
                    "e":4,
                    "v":{{"7":"checkpoint-tool-use"}},
                    "a":{{"20":"claude-code"}}
                }}"#
        )])
        .unwrap();

        assert_eq!(
            metric_identifier_rows(&db),
            vec![MetricIdentifierRow {
                trace_id: None,
                session_id: None,
                parent_session_id: None,
                tool: Some("claude-code".to_string()),
                external_session_id: None,
                external_parent_session_id: None,
                external_event_id: None,
                external_parent_event_id: None,
                external_tool_use_id: Some("checkpoint-tool-use".to_string()),
            }]
        );
    }

    #[test]
    fn test_session_event_candidates_near_timestamps_filters_kind_and_window() {
        let (mut db, _temp_dir) = create_test_db();
        let base_ts = seconds_ago(60);
        db.insert_session_observations(&[
            recovery_observation(
                base_ts,
                "session-near",
                Some("external-near"),
                "codex",
                Some("https://github.com/acme/repo"),
            ),
            recovery_observation(
                base_ts + 10,
                "session-far",
                Some("external-far"),
                "codex",
                Some("https://github.com/acme/repo"),
            ),
        ])
        .unwrap();

        let timestamp_ns = (base_ts as u128 * 1_000_000_000) + 500_000_000;
        let candidates = db
            .session_event_candidates_near_timestamps(&[timestamp_ns], 3_000_000_000)
            .unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].event_ts, base_ts);
        assert_eq!(candidates[0].session_id, "session-near");
        assert_eq!(candidates[0].external_session_id, "external-near");
    }

    #[test]
    fn test_session_event_candidates_treat_event_ts_as_second_bucket() {
        let (mut db, _temp_dir) = create_test_db();
        let base_ts = seconds_ago(60);
        db.insert_session_observations(&[recovery_observation(
            base_ts,
            "session-bucket",
            Some("external-bucket"),
            "codex",
            Some("https://github.com/acme/repo"),
        )])
        .unwrap();

        let timestamp_ns = base_ts as u128 * NS_PER_SECOND + 3_500_000_000;
        let candidates = db
            .session_event_candidates_near_timestamps(&[timestamp_ns], 3_000_000_000)
            .unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].session_id, "session-bucket");
    }

    #[test]
    fn test_session_event_candidates_parse_required_and_optional_metadata() {
        let (mut db, _temp_dir) = create_test_db();
        let ts = seconds_ago(30);
        db.insert_session_observations(&[
            recovery_observation(
                ts,
                "session-complete",
                Some("external-complete"),
                "claude-code",
                Some("https://github.com/acme/repo"),
            ),
            recovery_observation(ts, "missing-external-session", None, "codex", None),
        ])
        .unwrap();

        let timestamp_ns = ts as u128 * 1_000_000_000;
        let candidates = db
            .session_event_candidates_near_timestamps(&[timestamp_ns], 3_000_000_000)
            .unwrap();

        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];
        assert_eq!(candidate.session_id, "session-complete");
        assert_eq!(
            candidate.trace_id.as_deref(),
            Some("trace-session-complete")
        );
        assert_eq!(candidate.tool, "claude-code");
        assert_eq!(candidate.model.as_deref(), Some("gpt-5"));
        assert_eq!(candidate.external_session_id, "external-complete");
        assert_eq!(
            candidate.external_tool_use_id.as_deref(),
            Some("tool-use-session-complete")
        );
        assert_eq!(
            candidate.repo_url.as_deref(),
            Some("https://github.com/acme/repo")
        );
    }

    #[test]
    fn test_insert_events_leaves_event_metadata_null_for_invalid_json() {
        let (mut db, _temp_dir) = create_test_db();
        let recent_event_ts = days_ago(1);
        let events = vec![
            "not-json".to_string(),
            format!(r#"{{"t":{recent_event_ts},"v":{{}},"a":{{}}}}"#),
            format!(r#"{{"t":{recent_event_ts},"e":null,"v":{{}},"a":{{}}}}"#),
        ];

        db.insert_events(&events).unwrap();

        assert_eq!(
            metric_metadata_rows(&db),
            vec![(None, None), (None, None), (None, None)]
        );
        assert_eq!(
            metric_identifier_rows(&db),
            vec![
                MetricIdentifierRow {
                    trace_id: None,
                    session_id: None,
                    parent_session_id: None,
                    tool: None,
                    external_session_id: None,
                    external_parent_session_id: None,
                    external_event_id: None,
                    external_parent_event_id: None,
                    external_tool_use_id: None,
                },
                MetricIdentifierRow {
                    trace_id: None,
                    session_id: None,
                    parent_session_id: None,
                    tool: None,
                    external_session_id: None,
                    external_parent_session_id: None,
                    external_event_id: None,
                    external_parent_event_id: None,
                    external_tool_use_id: None,
                },
                MetricIdentifierRow {
                    trace_id: None,
                    session_id: None,
                    parent_session_id: None,
                    tool: None,
                    external_session_id: None,
                    external_parent_session_id: None,
                    external_event_id: None,
                    external_parent_event_id: None,
                    external_tool_use_id: None,
                },
            ]
        );
        assert_eq!(db.count().unwrap(), 3);
    }

    #[test]
    fn test_backfill_event_metadata_batch_updates_valid_legacy_rows_only() {
        let (mut db, _temp_dir) = create_test_db();
        let ts1 = days_ago(3);
        let ts2 = days_ago(2);
        db.conn
            .execute(
                "INSERT INTO metrics (event_json) VALUES (?1), (?2), (?3)",
                params![
                    event_json_with_all_common_metadata(ts1, 1),
                    format!(r#"{{"t":{ts2},"e":4,"v":{{"7":"legacy-tool"}},"a":{{"1":"https://github.com/acme/project"}}}}"#),
                    "not-json",
                ],
            )
            .unwrap();

        let summary = db.backfill_event_metadata_batch(100).unwrap();

        assert_eq!(summary.scanned, 3);
        assert_eq!(summary.updated, 2);
        assert_eq!(
            metric_metadata_rows(&db),
            vec![
                (Some(ts1 as i64), Some(1)),
                (Some(ts2 as i64), Some(4)),
                (None, None),
            ]
        );
        assert_eq!(
            metric_identifier_rows(&db),
            vec![
                MetricIdentifierRow {
                    trace_id: Some("trace-1".to_string()),
                    session_id: Some("session-1".to_string()),
                    parent_session_id: Some("parent-session-1".to_string()),
                    tool: Some("codex".to_string()),
                    external_session_id: Some("external-session-1".to_string()),
                    external_parent_session_id: Some("external-parent-session-1".to_string()),
                    external_event_id: None,
                    external_parent_event_id: None,
                    external_tool_use_id: None,
                },
                MetricIdentifierRow {
                    trace_id: None,
                    session_id: None,
                    parent_session_id: None,
                    tool: None,
                    external_session_id: None,
                    external_parent_session_id: None,
                    external_event_id: None,
                    external_parent_event_id: None,
                    external_tool_use_id: Some("legacy-tool".to_string()),
                },
                MetricIdentifierRow {
                    trace_id: None,
                    session_id: None,
                    parent_session_id: None,
                    tool: None,
                    external_session_id: None,
                    external_parent_session_id: None,
                    external_event_id: None,
                    external_parent_event_id: None,
                    external_tool_use_id: None,
                },
            ]
        );
    }

    #[test]
    fn test_backfill_event_metadata_batch_after_advances_cursor() {
        let (mut db, _temp_dir) = create_test_db();
        let ts1 = days_ago(3);
        let ts2 = days_ago(2);
        let ts3 = days_ago(1);
        db.conn
            .execute(
                "INSERT INTO metrics (event_json) VALUES (?1), (?2), (?3)",
                params![event_json(ts1), event_json(ts2), event_json(ts3)],
            )
            .unwrap();

        let (first_summary, first_last_id) = db.backfill_event_metadata_batch_after(0, 2).unwrap();

        assert_eq!(
            first_summary,
            MetricMetadataBackfillSummary {
                scanned: 2,
                updated: 2,
            }
        );
        assert_eq!(
            metric_metadata_rows(&db),
            vec![
                (Some(ts1 as i64), Some(1)),
                (Some(ts2 as i64), Some(1)),
                (None, None),
            ]
        );

        let first_last_id = first_last_id.unwrap();
        let (second_summary, second_last_id) = db
            .backfill_event_metadata_batch_after(first_last_id, 2)
            .unwrap();

        assert_eq!(
            second_summary,
            MetricMetadataBackfillSummary {
                scanned: 1,
                updated: 1,
            }
        );
        assert!(second_last_id.is_some_and(|id| id > first_last_id));
        assert_eq!(
            metric_metadata_rows(&db),
            vec![
                (Some(ts1 as i64), Some(1)),
                (Some(ts2 as i64), Some(1)),
                (Some(ts3 as i64), Some(1)),
            ]
        );

        let (empty_summary, empty_last_id) = db
            .backfill_event_metadata_batch_after(second_last_id.unwrap(), 2)
            .unwrap();
        assert_eq!(empty_summary, MetricMetadataBackfillSummary::default());
        assert_eq!(empty_last_id, None);
    }

    #[test]
    fn test_backfill_event_metadata_batch_once_marks_completion() {
        let (mut db, temp_dir) = create_test_db();
        db.conn
            .execute(
                "INSERT INTO metrics (event_json) VALUES (?1), (?2)",
                params![event_json(days_ago(2)), event_json(days_ago(1))],
            )
            .unwrap();

        let (first_summary, last_id) = db.backfill_event_metadata_batch_once(0, 1).unwrap();
        assert_eq!(first_summary.scanned, 1);
        assert!(!db.event_metadata_backfill_completed().unwrap());

        let (second_summary, _) = db
            .backfill_event_metadata_batch_once(last_id.unwrap(), 2)
            .unwrap();
        assert_eq!(second_summary.scanned, 1);
        assert!(db.event_metadata_backfill_completed().unwrap());

        drop(db);
        let conn = crate::sqlite::open_with_memory_limits(temp_dir.path().join("test-metrics.db"))
            .unwrap();
        let mut db = MetricsDatabase { conn };
        db.initialize_schema().unwrap();
        assert!(db.event_metadata_backfill_completed().unwrap());

        db.conn
            .execute(
                "INSERT INTO metrics (event_json) VALUES (?1)",
                params![event_json(days_ago(0))],
            )
            .unwrap();
        let inserted_after_completion = db.conn.last_insert_rowid();

        let skipped = db.backfill_event_metadata_batch_once(0, 100).unwrap();
        assert_eq!(skipped, (MetricMetadataBackfillSummary::default(), None));
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT event_ts FROM metrics WHERE id = ?1",
                    params![inserted_after_completion],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .unwrap(),
            None
        );
    }

    #[test]
    fn test_dequeue_pending_batch_locks_rows() {
        let (mut db, _temp_dir) = create_test_db();
        let events = vec![event_json(days_ago(2)), event_json(days_ago(1))];
        db.insert_events(&events).unwrap();

        let batch = db.dequeue_pending_batch(1).unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(db.count().unwrap(), 2);
        assert_eq!(db.count_retryable().unwrap(), 1);

        db.mark_records_delivered(&[batch[0].id], unix_now())
            .unwrap();
        assert_eq!(db.count().unwrap(), 1);
        assert_eq!(db.count_retryable().unwrap(), 1);
    }

    #[test]
    fn test_dequeue_pending_batch_prefers_oldest_retryable_rows() {
        let (mut db, _temp_dir) = create_test_db();
        let oldest_ts = days_ago(3);
        let middle_ts = days_ago(2);
        let newest_ts = days_ago(1);
        db.insert_events(&[
            event_json(oldest_ts),
            event_json(middle_ts),
            event_json(newest_ts),
        ])
        .unwrap();

        let batch = db.dequeue_pending_batch(2).unwrap();
        assert_eq!(batch.len(), 2);
        assert!(batch[0].id < batch[1].id);
        assert!(batch[0].event_json.contains(&format!("\"t\":{oldest_ts}")));
        assert!(batch[1].event_json.contains(&format!("\"t\":{middle_ts}")));
    }

    #[test]
    fn test_due_old_row_is_not_starved_by_continuous_new_writes() {
        let (mut db, _temp_dir) = create_test_db();
        let old_id = db.insert_events(&[event_json(days_ago(7))]).unwrap()[0];

        let mut old_was_selected = false;
        for offset in 0..20 {
            db.insert_events(&[event_json(seconds_ago(offset))])
                .unwrap();
            let batch = db.dequeue_pending_batch(1).unwrap();
            old_was_selected |= batch[0].id == old_id;
            db.mark_records_delivered(&[batch[0].id], unix_now())
                .unwrap();
        }
        assert!(
            old_was_selected,
            "the old due formal fact must eventually win while newer rows keep arriving"
        );
    }

    #[test]
    fn test_dequeue_pending_batch_respects_byte_budget_and_locks_only_selected_rows() {
        let (mut db, _temp_dir) = create_test_db();
        let payload = "x".repeat(4 * 1024 * 1024);
        let large_event = |ts| {
            json!({
                "t": ts,
                "e": MetricEventId::Committed as u16,
                "v": {},
                "a": {"99": payload}
            })
            .to_string()
        };
        db.insert_events(&[large_event(seconds_ago(1)), large_event(seconds_ago(0))])
            .unwrap();

        let batch = db.dequeue_pending_batch(1000).unwrap();

        assert_eq!(batch.len(), 1);
        let locked_rows: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM metrics WHERE processing_started_at IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(locked_rows, 1);
        assert_eq!(db.count_retryable().unwrap(), 1);

        let event = serde_json::from_str::<MetricEvent>(&batch[0].event_json).unwrap();
        let serialized = serde_json::to_vec(&MetricsBatch::new(vec![event])).unwrap();
        assert!(serialized.len() < 8 * 1024 * 1024);
    }

    #[test]
    fn test_single_oversized_old_row_can_be_quarantined_without_blocking_followers() {
        let (mut db, _temp_dir) = create_test_db();
        let oversized = json!({
            "t": seconds_ago(2),
            "e": MetricEventId::Committed as u16,
            "v": {},
            "a": {"99": "x".repeat(MAX_METRICS_UPLOAD_BODY_BYTES + 1024)}
        })
        .to_string();
        let small = event_json(seconds_ago(1));
        let ids = db.insert_events(&[oversized, small.clone()]).unwrap();

        let first = db.dequeue_pending_batch(100).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].id, ids[0]);
        db.mark_records_undeliverable(
            &[(ids[0], "request exceeds upload limit".to_string())],
            unix_now(),
        )
        .unwrap();

        let second = db.dequeue_pending_batch(100).unwrap();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].id, ids[1]);
        assert_eq!(second[0].event_json, small);
    }

    #[test]
    fn test_retryable_query_work_is_independent_of_exhausted_history() {
        let (db, _temp_dir) = create_test_db();
        let now = unix_now() as i64;

        db.conn
            .execute(
                "INSERT INTO metrics (event_json, next_retry_at) VALUES (?1, 0)",
                params![event_json(days_ago(1))],
            )
            .unwrap();
        db.conn
            .execute(
                r#"
                WITH RECURSIVE exhausted(n) AS (
                    VALUES(1)
                    UNION ALL
                    SELECT n + 1 FROM exhausted WHERE n < 20000
                )
                INSERT INTO metrics (event_json, attempts, next_retry_at)
                SELECT '{"t":1,"e":1,"v":{},"a":{}}', 6, 0 FROM exhausted
                "#,
                [],
            )
            .unwrap();

        let mut stmt = db.conn.prepare(RETRYABLE_METRIC_IDS_SQL).unwrap();
        let ids = stmt
            .query_map(params![now, 100], |row| row.get::<_, i64>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(ids, vec![1]);
        assert_eq!(stmt.get_status(StatementStatus::FullscanStep), 0);
        assert_eq!(stmt.get_status(StatementStatus::Sort), 0);
        assert!(
            stmt.get_status(StatementStatus::VmStep) < 1_000,
            "retryable lookup must not scale with exhausted history"
        );
    }

    #[test]
    fn test_failed_records_do_not_block_unfailed_retryable_rows() {
        let (mut db, _temp_dir) = create_test_db();
        db.insert_events(&[event_json(days_ago(2)), event_json(days_ago(1))])
            .unwrap();

        let batch = db.dequeue_pending_batch(1).unwrap();
        let failed_id = batch[0].id;
        let failed_at = unix_now();
        db.mark_records_failed(&[failed_id], "upload failed", failed_at)
            .unwrap();

        assert_eq!(db.count().unwrap(), 2);
        assert_eq!(db.count_retryable().unwrap(), 1);

        let retryable_batch = db.dequeue_pending_batch(10).unwrap();
        assert_eq!(retryable_batch.len(), 1);
        assert_ne!(retryable_batch[0].id, failed_id);

        let (attempts, next_retry_at): (i64, i64) = db
            .conn
            .query_row(
                "SELECT attempts, next_retry_at FROM metrics WHERE id = ?1",
                params![failed_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(attempts, 1);
        assert!(next_retry_at > failed_at as i64);
    }

    #[test]
    fn test_dequeue_releases_stale_processing_locks() {
        let (mut db, _temp_dir) = create_test_db();
        db.insert_events(&[event_json(days_ago(1))]).unwrap();

        let first_batch = db.dequeue_pending_batch(1).unwrap();
        assert_eq!(first_batch.len(), 1);
        assert_eq!(db.count_retryable().unwrap(), 0);

        let stale_started_at = unix_now().saturating_sub(METRIC_PROCESSING_LOCK_TIMEOUT_SECS + 1);
        db.conn
            .execute(
                "UPDATE metrics SET processing_started_at = ?1 WHERE id = ?2",
                params![stale_started_at as i64, first_batch[0].id],
            )
            .unwrap();

        let second_batch = db.dequeue_pending_batch(1).unwrap();
        assert_eq!(second_batch.len(), 1);
        assert_eq!(second_batch[0].id, first_batch[0].id);
    }

    #[test]
    fn test_max_attempts_are_not_retryable() {
        let (mut db, _temp_dir) = create_test_db();
        let ids = db.insert_events(&[event_json(days_ago(1))]).unwrap();
        db.conn
            .execute(
                "UPDATE metrics SET attempts = ?1 WHERE id = ?2",
                params![MAX_METRIC_UPLOAD_ATTEMPTS as i64, ids[0]],
            )
            .unwrap();

        assert_eq!(db.count().unwrap(), 1);
        assert_eq!(db.count_retryable().unwrap(), 0);
        assert!(db.dequeue_pending_batch(1).unwrap().is_empty());
    }

    #[test]
    fn test_status_counts_delivery_buckets() {
        let (mut db, _temp_dir) = create_test_db();
        let now = unix_now();

        let delivered_ids = db
            .insert_events_with_delivered_ts(&[event_json(days_ago(5))], Some(now))
            .unwrap();
        let delivered_id = delivered_ids[0];
        let ids = db
            .insert_events(&[
                event_json(days_ago(4)),
                event_json(days_ago(3)),
                event_json(days_ago(2)),
                event_json(days_ago(1)),
            ])
            .unwrap();
        let pending_id = ids[0];
        let waiting_id = ids[1];
        let processing_id = ids[2];
        let stopped_id = ids[3];

        db.conn
            .execute(
                "UPDATE metrics \
                 SET last_sync_error = ?1, last_sync_at = ?2 \
                 WHERE id = ?3",
                params![
                    "delivered retry recovered",
                    now.saturating_add(60) as i64,
                    delivered_id
                ],
            )
            .unwrap();
        db.conn
            .execute(
                "UPDATE metrics \
                 SET attempts = 1, last_sync_error = ?1, last_sync_at = ?2, next_retry_at = ?3 \
                 WHERE id = ?4",
                params![
                    "temporary outage",
                    now.saturating_sub(10) as i64,
                    now.saturating_add(600) as i64,
                    waiting_id
                ],
            )
            .unwrap();
        db.conn
            .execute(
                "UPDATE metrics SET processing_started_at = ?1 WHERE id = ?2",
                params![now as i64, processing_id],
            )
            .unwrap();
        db.conn
            .execute(
                "UPDATE metrics \
                 SET attempts = ?1, last_sync_error = ?2, last_sync_at = ?3, next_retry_at = ?3 \
                 WHERE id = ?4",
                params![
                    MAX_METRIC_UPLOAD_ATTEMPTS as i64,
                    "validation failed",
                    now as i64,
                    stopped_id
                ],
            )
            .unwrap();

        assert_ne!(pending_id, waiting_id);
        let status = db.status().unwrap();
        assert_eq!(status.total, 5);
        assert_eq!(status.delivered, 1);
        assert_eq!(status.not_delivered, 4);
        assert_eq!(status.pending_retryable, 1);
        assert_eq!(status.waiting_retry, 1);
        assert_eq!(status.processing, 1);
        assert_eq!(status.stopped_after_errors, 1);
        assert_eq!(status.rows_with_errors, 2);
        assert_eq!(status.latest_error.as_deref(), Some("validation failed"));
    }

    #[test]
    fn test_mark_records_undeliverable_keeps_history_without_retrying() {
        let (mut db, _temp_dir) = create_test_db();
        let event_ts = days_ago(1);
        let ids = db.insert_events(&[event_json(event_ts)]).unwrap();

        let batch = db.dequeue_pending_batch(1).unwrap();
        assert_eq!(batch.len(), 1);
        db.mark_records_undeliverable(&[(ids[0], "validation failed".to_string())], unix_now())
            .unwrap();

        assert_eq!(db.count().unwrap(), 1);
        assert_eq!(db.count_retryable().unwrap(), 0);
        assert!(db.dequeue_pending_batch(1).unwrap().is_empty());
        assert_eq!(db.get_metric_history(0, None, &[1]).unwrap().len(), 1);

        let (delivered_ts, attempts, last_sync_error): (Option<i64>, i64, Option<String>) = db
            .conn
            .query_row(
                "SELECT delivered_ts, attempts, last_sync_error FROM metrics WHERE id = ?1",
                params![ids[0]],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert!(delivered_ts.is_none());
        assert_eq!(attempts, MAX_METRIC_UPLOAD_ATTEMPTS as i64);
        assert_eq!(last_sync_error.as_deref(), Some("validation failed"));
    }

    #[test]
    fn test_mark_records_delivered() {
        let (mut db, _temp_dir) = create_test_db();
        let ts1 = days_ago(3);
        let ts2 = days_ago(2);
        let ts3 = days_ago(1);

        let events = vec![event_json(ts1), event_json(ts2), event_json(ts3)];

        db.insert_events(&events).unwrap();

        // Dequeue oldest rows and mark them delivered.
        let batch = db.dequeue_pending_batch(2).unwrap();
        let ids: Vec<i64> = batch.iter().map(|r| r.id).collect();

        db.mark_records_delivered(&ids, unix_now()).unwrap();

        // Verify only one remains pending.
        let count = db.count().unwrap();
        assert_eq!(count, 1);

        // Verify remaining pending row is the newest one.
        let remaining = pending_event_jsons(&db);
        assert_eq!(remaining.len(), 1);
        assert!(remaining[0].contains(&format!("\"t\":{ts3}")));

        // Verify delivered rows are retained.
        let total: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, 3);
    }

    #[test]
    fn test_insert_events_with_delivered_ts_skips_batch() {
        let (mut db, _temp_dir) = create_test_db();

        let delivered_ts = unix_now();
        let delivered_event_ts = days_ago(2);
        let pending_event_ts = days_ago(1);
        let delivered = vec![event_json(delivered_event_ts)];
        let pending = vec![event_json(pending_event_ts)];

        db.insert_events_with_delivered_ts(&delivered, Some(delivered_ts))
            .unwrap();
        db.insert_events(&pending).unwrap();

        let batch = pending_event_jsons(&db);
        assert_eq!(batch.len(), 1);
        assert!(batch[0].contains(&format!("\"t\":{pending_event_ts}")));
        assert_eq!(db.count().unwrap(), 1);

        let total: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, 2);
    }

    #[test]
    fn test_get_metric_history_reads_authoritative_metrics_table() {
        let (mut db, _temp_dir) = create_test_db();

        let delivered_ts = unix_now();
        let ts1 = days_ago(4);
        let ts2 = days_ago(3);
        let ts3 = days_ago(2);
        let ts4 = days_ago(1);
        let delivered = vec![event_json_with_repo(
            ts1,
            1,
            "https://github.com/acme/project",
        )];
        let pending = vec![
            event_json_with_repo(ts2, 4, "https://github.com/acme/project"),
            event_json_with_repo(ts3, 2, "https://github.com/acme/project"),
            event_json_with_repo(ts4, 7, "https://github.com/other/repo"),
        ];

        db.insert_events_with_delivered_ts(&delivered, Some(delivered_ts))
            .unwrap();
        db.insert_events(&pending).unwrap();

        let records = db
            .get_metric_history(0, Some("acme/project"), &[1, 4, 7])
            .unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].event_id, 1);
        assert_eq!(records[0].ts, ts1);
        assert_eq!(records[1].event_id, 4);
        assert_eq!(records[1].ts, ts2);

        // Delivered rows are retained for history, but only undelivered rows flush.
        assert_eq!(db.count().unwrap(), 3);
    }

    #[test]
    fn test_get_metric_history_reads_rows_without_cached_metadata_before_and_after_backfill() {
        let (mut db, _temp_dir) = create_test_db();
        let ts1 = days_ago(2);
        let ts2 = days_ago(1);
        db.conn
            .execute(
                "INSERT INTO metrics (event_json) VALUES (?1), (?2)",
                params![
                    event_json_with_repo(ts1, 4, "https://github.com/acme/project"),
                    event_json_with_repo(ts2, 7, "https://github.com/acme/project"),
                ],
            )
            .unwrap();

        let before = db
            .get_metric_history(0, Some("acme/project"), &[4, 7])
            .unwrap();
        assert_eq!(
            before
                .iter()
                .map(|record| (record.event_id, record.ts))
                .collect::<Vec<_>>(),
            vec![(4, ts1), (7, ts2)]
        );

        let summary = db.backfill_event_metadata_batch(100).unwrap();
        assert_eq!(summary.scanned, 2);
        assert_eq!(summary.updated, 2);

        let after = db
            .get_metric_history(0, Some("acme/project"), &[4, 7])
            .unwrap();
        assert_eq!(
            after
                .iter()
                .map(|record| (record.event_id, record.ts))
                .collect::<Vec<_>>(),
            vec![(4, ts1), (7, ts2)]
        );
    }

    #[test]
    fn test_prunes_metric_rows_older_than_retention_by_event_timestamp() {
        let (mut db, _temp_dir) = create_test_db();

        let delivered_ts = unix_now();
        let old_event_ts = seconds_ago(MetricsDatabase::METRICS_RETENTION_SECS + 1);
        let recent_event_ts = seconds_ago(MetricsDatabase::METRICS_RETENTION_SECS - 1);
        let events = vec![event_json(old_event_ts), event_json(recent_event_ts)];

        db.insert_events_with_delivered_ts(&events, Some(delivered_ts))
            .unwrap();

        let total_after_prune: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total_after_prune, 1);

        let records = db.get_metric_history(0, None, &[1]).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].ts, recent_event_ts);
    }

    #[test]
    fn test_prunes_metric_rows_older_than_retention_by_cached_event_timestamp() {
        let (mut db, _temp_dir) = create_test_db();

        let old_event_ts = seconds_ago(MetricsDatabase::METRICS_RETENTION_SECS + 1);
        let recent_json_ts = days_ago(1);
        db.conn
            .execute(
                "INSERT INTO metrics (event_json, event_ts, event_kind, delivered_ts) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    event_json(recent_json_ts),
                    old_event_ts as i64,
                    1,
                    unix_now() as i64
                ],
            )
            .unwrap();

        db.prune_old_metrics_if_due().unwrap();

        let total: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, 0);
    }

    #[test]
    fn test_old_pending_metric_rows_are_retained_until_delivered() {
        let (mut db, _temp_dir) = create_test_db();

        let old_event_ts = seconds_ago(MetricsDatabase::METRICS_RETENTION_SECS + 1);
        let recent_event_ts = days_ago(1);
        let pending = vec![event_json(old_event_ts), event_json(recent_event_ts)];

        db.insert_events(&pending).unwrap();

        let total: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, 2);
        assert_eq!(db.count().unwrap(), 2);

        let batch = pending_event_jsons(&db);
        assert_eq!(batch.len(), 2);
        assert!(
            batch
                .iter()
                .any(|event| event.contains(&format!("\"t\":{old_event_ts}")))
        );
        assert!(
            batch
                .iter()
                .any(|event| event.contains(&format!("\"t\":{recent_event_ts}")))
        );
    }

    #[test]
    fn test_old_pending_rows_without_kind_are_retained_until_delivered() {
        let (mut db, _temp_dir) = create_test_db();

        let old_event_ts = seconds_ago(MetricsDatabase::METRICS_RETENTION_SECS + 1);
        let recent_event_ts = days_ago(1);
        let pending = vec![
            format!(r#"{{"t":{old_event_ts},"v":{{}},"a":{{}}}}"#),
            format!(r#"{{"t":{recent_event_ts},"v":{{}},"a":{{}}}}"#),
        ];

        db.insert_events(&pending).unwrap();

        let total: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, 2);
        let mut statement = db
            .conn
            .prepare("SELECT event_json FROM metrics ORDER BY id")
            .unwrap();
        let remaining = statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<String>, _>>()
            .unwrap();
        assert!(
            remaining
                .iter()
                .any(|event| event.contains(&format!("\"t\":{old_event_ts}")))
        );
        assert!(
            remaining
                .iter()
                .any(|event| event.contains(&format!("\"t\":{recent_event_ts}")))
        );
    }

    #[test]
    fn test_old_stopped_after_errors_metric_row_is_retained_and_visible() {
        let (mut db, _temp_dir) = create_test_db();

        let old_event_ts = seconds_ago(MetricsDatabase::METRICS_RETENTION_SECS + 1);
        let event_id = db.insert_events(&[event_json(old_event_ts)]).unwrap()[0];
        db.mark_records_undeliverable(
            &[(event_id, "server rejected formal metric".to_string())],
            unix_now(),
        )
        .unwrap();
        db.conn
            .execute(
                "DELETE FROM schema_metadata WHERE key = 'metrics_last_prune_ts'",
                [],
            )
            .unwrap();

        db.prune_old_metrics_if_due().unwrap();

        let status = db.status().unwrap();
        assert_eq!(status.not_delivered, 1);
        assert_eq!(status.stopped_after_errors, 1);
        assert_eq!(status.rows_with_errors, 1);
        assert_eq!(
            status.latest_error.as_deref(),
            Some("server rejected formal metric")
        );
    }

    #[test]
    fn test_prunes_malformed_delivered_rows_by_delivered_timestamp() {
        let (mut db, _temp_dir) = create_test_db();

        let old_delivered_ts =
            unix_now().saturating_sub(MetricsDatabase::METRICS_RETENTION_SECS + 1);
        db.insert_events_with_delivered_ts(&["not-json".to_string()], Some(old_delivered_ts))
            .unwrap();

        let total: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(total, 0);
    }

    #[test]
    fn test_empty_operations() {
        let (mut db, _temp_dir) = create_test_db();

        // Insert empty should succeed
        db.insert_events(&[]).unwrap();

        // Dequeue from empty should return empty.
        let batch = db.dequeue_pending_batch(10).unwrap();
        assert!(batch.is_empty());

        // Marking an empty set delivered should succeed.
        db.mark_records_delivered(&[], 1_700_000_000).unwrap();

        // Count empty should return 0
        let count = db.count().unwrap();
        assert_eq!(count, 0);

        let status = db.status().unwrap();
        assert_eq!(status.total, 0);
        assert_eq!(status.delivered, 0);
        assert_eq!(status.not_delivered, 0);
        assert_eq!(status.pending_retryable, 0);
        assert_eq!(status.waiting_retry, 0);
        assert_eq!(status.processing, 0);
        assert_eq!(status.stopped_after_errors, 0);
        assert_eq!(status.rows_with_errors, 0);
        assert_eq!(status.latest_error, None);
    }

    #[test]
    fn test_database_path() {
        let path = MetricsDatabase::database_path().unwrap();
        assert!(path.to_string_lossy().contains(".git-ai"));
        assert!(path.to_string_lossy().contains("internal"));
        assert!(path.to_string_lossy().ends_with("metrics-db"));
    }

    #[test]
    fn test_should_emit_agent_usage_rate_limit() {
        let (mut db, _temp_dir) = create_test_db();
        let prompt_id = "prompt-123";

        // First event for a prompt should be allowed.
        assert!(
            db.should_emit_agent_usage(prompt_id, 1_700_000_000, 300)
                .unwrap()
        );
        // Subsequent event inside the window should be throttled.
        assert!(
            !db.should_emit_agent_usage(prompt_id, 1_700_000_120, 300)
                .unwrap()
        );
        // Event outside the window should be allowed again.
        assert!(
            db.should_emit_agent_usage(prompt_id, 1_700_000_301, 300)
                .unwrap()
        );
    }
}
