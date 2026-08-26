//! Crash-safe Event 8 ref-transition intents.
//!
//! A git command has already moved the ref before lifecycle enumeration runs.
//! The immutable old/new tips therefore enter SQLite first. Expensive `rev-list`
//! work can fail or the daemon can crash afterwards without losing the
//! transition: periodic processing rebuilds the complete chunk bundle and
//! atomically moves it into the normal metrics outbox.

use crate::error::GitAiError;
use crate::git::repository::{
    Repository, discover_repository_in_path_no_git_exec, from_bare_repository,
};
use crate::metrics::db::MetricsDatabase;
use crate::metrics::types::{MetricEvent, MetricEventId, SparseArray};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const DEFERRED_LIFECYCLE_JOBS_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS deferred_lifecycle_metric_jobs (
    job_key TEXT PRIMARY KEY NOT NULL,
    repo_identity TEXT NOT NULL,
    repository_workdir TEXT NOT NULL,
    git_dir TEXT NOT NULL,
    git_common_dir TEXT NOT NULL,
    operation_kind TEXT NOT NULL,
    old_tip TEXT NOT NULL,
    new_tip TEXT NOT NULL,
    branch TEXT,
    semantics TEXT NOT NULL,
    attrs_json TEXT NOT NULL,
    event_ts INTEGER NOT NULL,
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

CREATE INDEX IF NOT EXISTS deferred_lifecycle_metric_jobs_due
    ON deferred_lifecycle_metric_jobs (state, next_retry_at, created_at)
    WHERE state != 'done';
"#;

const PROCESSING_LEASE_SECS: u64 = 10 * 60;
const INITIAL_RETRY_BACKOFF_SECS: u64 = 5;
const MAX_RETRY_BACKOFF_SECS: u64 = 60 * 60;
const MAX_PERIODIC_JOBS_PER_PASS: usize = 1;
const DONE_JOB_COMPACTION_BATCH_SIZE: usize = 100;

#[derive(Debug, Clone)]
pub(crate) struct DeferredLifecycleMetricJobSpec {
    pub repo_identity: String,
    pub repository_workdir: String,
    pub git_dir: String,
    pub git_common_dir: String,
    pub operation_kind: String,
    pub old_tip: String,
    pub new_tip: String,
    pub branch: Option<String>,
    pub semantics: String,
    pub attrs_json: String,
    pub event_ts: u32,
}

impl DeferredLifecycleMetricJobSpec {
    pub(crate) fn from_transition(
        repo: &Repository,
        operation_kind: &str,
        old_tip: &str,
        new_tip: &str,
        branch: Option<String>,
        semantics: &str,
        attrs: &SparseArray,
    ) -> Result<Self, GitAiError> {
        let repository_workdir = repo.workdir()?;
        let git_dir = repo.path().to_path_buf();
        let git_common_dir = repo.common_dir().to_path_buf();
        Ok(Self {
            repo_identity: repository_identity(&git_common_dir),
            repository_workdir: repository_workdir.to_string_lossy().to_string(),
            git_dir: git_dir.to_string_lossy().to_string(),
            git_common_dir: git_common_dir.to_string_lossy().to_string(),
            operation_kind: operation_kind.to_string(),
            old_tip: old_tip.to_string(),
            new_tip: new_tip.to_string(),
            branch,
            semantics: semantics.to_string(),
            attrs_json: serde_json::to_string(attrs)?,
            event_ts: unix_now().min(u64::from(u32::MAX)) as u32,
        })
    }

    fn job_key(&self) -> String {
        let mut hasher = Sha256::new();
        for value in [
            self.repo_identity.as_str(),
            self.operation_kind.as_str(),
            self.old_tip.as_str(),
            self.new_tip.as_str(),
            self.branch.as_deref().unwrap_or(""),
            self.semantics.as_str(),
        ] {
            hasher.update((value.len() as u64).to_be_bytes());
            hasher.update(value.as_bytes());
        }
        format!("{:x}", hasher.finalize())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ClaimedDeferredLifecycleMetricJob {
    pub job_key: String,
    pub lease_token: String,
    pub repo_identity: String,
    pub repository_workdir: String,
    pub git_dir: String,
    pub git_common_dir: String,
    pub operation_kind: String,
    pub old_tip: String,
    pub new_tip: String,
    pub branch: Option<String>,
    pub semantics: String,
    pub attrs_json: String,
    pub event_ts: u32,
    pub attempts: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeferredLifecycleProcessSummary {
    pub completed: usize,
    pub failed: usize,
}

pub(crate) fn enqueue(spec: &DeferredLifecycleMetricJobSpec) -> Result<bool, GitAiError> {
    let db = MetricsDatabase::global()?;
    let mut db = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    enqueue_on_connection(db.deferred_jobs_connection(), spec, unix_now())
}

/// The durable insert is the acknowledgement boundary. Best-effort immediate
/// processing reduces latency, but any build/persist failure remains queued.
pub(crate) fn enqueue_and_try_process(
    spec: &DeferredLifecycleMetricJobSpec,
) -> Result<(), GitAiError> {
    enqueue(spec)?;
    let summary = process_due_jobs(1);
    if summary.failed > 0 {
        tracing::warn!(
            job_key = %spec.job_key(),
            "deferred lifecycle metric build failed; durable retry remains pending"
        );
    }
    Ok(())
}

pub(crate) fn count_outstanding() -> Result<usize, GitAiError> {
    let db = MetricsDatabase::global()?;
    let mut db = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    count_outstanding_on_connection(db.deferred_jobs_connection())
}

pub(crate) fn process_periodic_jobs() -> DeferredLifecycleProcessSummary {
    compact_done_jobs_global();
    process_due_jobs(MAX_PERIODIC_JOBS_PER_PASS)
}

pub(crate) fn process_jobs_for_await() -> DeferredLifecycleProcessSummary {
    compact_done_jobs_global();
    process_due_jobs(usize::MAX)
}

fn compact_done_jobs_global() {
    let result = MetricsDatabase::global().and_then(|db| {
        let mut db = db
            .lock()
            .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
        compact_done_payloads_on_connection(
            db.deferred_jobs_connection(),
            DONE_JOB_COMPACTION_BATCH_SIZE,
        )
    });
    if let Err(error) = result {
        tracing::warn!(%error, "deferred lifecycle metrics: failed to compact completed jobs");
    }
}

fn process_due_jobs(limit: usize) -> DeferredLifecycleProcessSummary {
    let mut summary = DeferredLifecycleProcessSummary::default();
    for _ in 0..limit {
        let claimed = match claim_due_global(unix_now()) {
            Ok(Some(job)) => job,
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(%error, "deferred lifecycle metrics: failed to claim job");
                summary.failed += 1;
                break;
            }
        };
        match compute_complete_events(&claimed) {
            Ok(events) => match complete_global(&claimed, &events, unix_now()) {
                Ok(true) => summary.completed += 1,
                Ok(false) => {}
                Err(error) => {
                    mark_failed_and_log(&claimed, &error);
                    summary.failed += 1;
                }
            },
            Err(error) => {
                mark_failed_and_log(&claimed, &error);
                summary.failed += 1;
            }
        }
    }
    summary
}

fn mark_failed_and_log(job: &ClaimedDeferredLifecycleMetricJob, error: &GitAiError) {
    if let Err(mark_error) = mark_failed_global(job, &error.to_string(), unix_now()) {
        tracing::warn!(
            job_key = %job.job_key,
            %mark_error,
            "deferred lifecycle metrics: failed to persist retry state"
        );
    }
    tracing::warn!(
        job_key = %job.job_key,
        attempt = job.attempts,
        %error,
        "deferred lifecycle metrics: computation failed and was deferred"
    );
}

fn claim_due_global(now: u64) -> Result<Option<ClaimedDeferredLifecycleMetricJob>, GitAiError> {
    let db = MetricsDatabase::global()?;
    let mut db = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    claim_due_on_connection(db.deferred_jobs_connection(), now, PROCESSING_LEASE_SECS)
}

fn mark_failed_global(
    job: &ClaimedDeferredLifecycleMetricJob,
    error: &str,
    now: u64,
) -> Result<bool, GitAiError> {
    let db = MetricsDatabase::global()?;
    let mut db = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    mark_failed_on_connection(db.deferred_jobs_connection(), job, error, now)
}

fn complete_global(
    job: &ClaimedDeferredLifecycleMetricJob,
    events: &[MetricEvent],
    now: u64,
) -> Result<bool, GitAiError> {
    let event_jsons = events
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()?;
    let db = MetricsDatabase::global()?;
    let mut db = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    complete_on_connection(db.deferred_jobs_connection(), job, &event_jsons, now)
}

fn compute_complete_events(
    job: &ClaimedDeferredLifecycleMetricJob,
) -> Result<Vec<MetricEvent>, GitAiError> {
    let repo = reopen_repository(job)?;
    let attrs: SparseArray = serde_json::from_str(&job.attrs_json)?;
    crate::daemon::rewrite_metrics::build_ref_lifecycle_transition_events_from_snapshot(
        &repo,
        &job.operation_kind,
        &job.old_tip,
        &job.new_tip,
        job.branch.as_deref(),
        &job.semantics,
        attrs,
        job.event_ts,
    )
}

fn reopen_repository(job: &ClaimedDeferredLifecycleMetricJob) -> Result<Repository, GitAiError> {
    let workdir = PathBuf::from(&job.repository_workdir);
    if workdir.exists()
        && let Ok(repo) = discover_repository_in_path_no_git_exec(&workdir)
        && repository_identity(repo.common_dir()) == job.repo_identity
        && transition_objects_are_available(&repo, &job.old_tip, &job.new_tip)
    {
        return Ok(repo);
    }
    for path in [&job.git_dir, &job.git_common_dir] {
        let path = PathBuf::from(path);
        if path.exists()
            && let Ok(repo) = from_bare_repository(&path)
            && repository_identity(repo.common_dir()) == job.repo_identity
            && transition_objects_are_available(&repo, &job.old_tip, &job.new_tip)
        {
            return Ok(repo);
        }
    }
    Err(GitAiError::Generic(format!(
        "repository objects for deferred lifecycle {} -> {} are unavailable",
        job.old_tip, job.new_tip
    )))
}

fn transition_objects_are_available(repo: &Repository, old_tip: &str, new_tip: &str) -> bool {
    [old_tip, new_tip].iter().all(|sha| {
        repo.revparse_single(sha)
            .and_then(|object| object.peel_to_commit())
            .is_ok()
    })
}

fn repository_identity(git_common_dir: &Path) -> String {
    let stable = git_common_dir
        .canonicalize()
        .unwrap_or_else(|_| git_common_dir.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(stable.to_string_lossy().as_bytes());
    format!("{:x}", hasher.finalize())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn retry_backoff_seconds(attempts: u32) -> u64 {
    let shift = attempts.saturating_sub(1).min(20);
    INITIAL_RETRY_BACKOFF_SECS
        .saturating_mul(1u64 << shift)
        .min(MAX_RETRY_BACKOFF_SECS)
}

fn u64_to_sqlite(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

pub(crate) fn enqueue_on_connection(
    conn: &mut Connection,
    spec: &DeferredLifecycleMetricJobSpec,
    now: u64,
) -> Result<bool, GitAiError> {
    let inserted = conn.execute(
        r#"
        INSERT INTO deferred_lifecycle_metric_jobs (
            job_key, repo_identity, repository_workdir, git_dir, git_common_dir,
            operation_kind, old_tip, new_tip, branch, semantics, attrs_json,
            event_ts, state, attempts, next_retry_at, created_at, updated_at
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
            'pending', 0, 0, ?13, ?13
        )
        ON CONFLICT(job_key) DO NOTHING
        "#,
        params![
            spec.job_key(),
            spec.repo_identity,
            spec.repository_workdir,
            spec.git_dir,
            spec.git_common_dir,
            spec.operation_kind,
            spec.old_tip,
            spec.new_tip,
            spec.branch,
            spec.semantics,
            spec.attrs_json,
            i64::from(spec.event_ts),
            u64_to_sqlite(now),
        ],
    )?;
    Ok(inserted == 1)
}

pub(crate) fn claim_due_on_connection(
    conn: &mut Connection,
    now: u64,
    lease_secs: u64,
) -> Result<Option<ClaimedDeferredLifecycleMetricJob>, GitAiError> {
    let tx = conn.transaction()?;
    let expired_before = now.saturating_sub(lease_secs);
    let job_key: Option<String> = tx
        .query_row(
            r#"
            SELECT job_key
            FROM deferred_lifecycle_metric_jobs
            WHERE (state = 'pending' AND next_retry_at <= ?1)
               OR (state = 'processing' AND processing_started_at <= ?2)
            ORDER BY created_at ASC, job_key ASC
            LIMIT 1
            "#,
            params![u64_to_sqlite(now), u64_to_sqlite(expired_before)],
            |row| row.get(0),
        )
        .optional()?;
    let Some(job_key) = job_key else {
        tx.commit()?;
        return Ok(None);
    };
    let lease_token = crate::uuid::generate_v4();
    let updated = tx.execute(
        r#"
        UPDATE deferred_lifecycle_metric_jobs
        SET state = 'processing',
            attempts = attempts + 1,
            processing_started_at = ?2,
            lease_token = ?3,
            updated_at = ?2
        WHERE job_key = ?1
          AND (
              (state = 'pending' AND next_retry_at <= ?2)
              OR (state = 'processing' AND processing_started_at <= ?4)
          )
        "#,
        params![
            job_key,
            u64_to_sqlite(now),
            lease_token,
            u64_to_sqlite(expired_before)
        ],
    )?;
    if updated != 1 {
        tx.commit()?;
        return Ok(None);
    }
    let claimed = load_claimed_job(&tx, &job_key, &lease_token)?;
    tx.commit()?;
    Ok(Some(claimed))
}

fn load_claimed_job(
    tx: &Transaction<'_>,
    job_key: &str,
    lease_token: &str,
) -> Result<ClaimedDeferredLifecycleMetricJob, GitAiError> {
    tx.query_row(
        r#"
        SELECT repo_identity, repository_workdir, git_dir, git_common_dir,
               operation_kind, old_tip, new_tip, branch, semantics, attrs_json,
               event_ts, attempts
        FROM deferred_lifecycle_metric_jobs
        WHERE job_key = ?1 AND state = 'processing' AND lease_token = ?2
        "#,
        params![job_key, lease_token],
        |row| {
            let event_ts: i64 = row.get(10)?;
            let attempts: i64 = row.get(11)?;
            Ok(ClaimedDeferredLifecycleMetricJob {
                job_key: job_key.to_string(),
                lease_token: lease_token.to_string(),
                repo_identity: row.get(0)?,
                repository_workdir: row.get(1)?,
                git_dir: row.get(2)?,
                git_common_dir: row.get(3)?,
                operation_kind: row.get(4)?,
                old_tip: row.get(5)?,
                new_tip: row.get(6)?,
                branch: row.get(7)?,
                semantics: row.get(8)?,
                attrs_json: row.get(9)?,
                event_ts: event_ts.max(0).min(i64::from(u32::MAX)) as u32,
                attempts: attempts.max(0).min(i64::from(u32::MAX)) as u32,
            })
        },
    )
    .map_err(GitAiError::from)
}

pub(crate) fn mark_failed_on_connection(
    conn: &mut Connection,
    job: &ClaimedDeferredLifecycleMetricJob,
    error: &str,
    now: u64,
) -> Result<bool, GitAiError> {
    let next_retry_at = now.saturating_add(retry_backoff_seconds(job.attempts));
    let updated = conn.execute(
        r#"
        UPDATE deferred_lifecycle_metric_jobs
        SET state = 'pending',
            next_retry_at = ?3,
            processing_started_at = NULL,
            lease_token = NULL,
            last_error = ?4,
            updated_at = ?5
        WHERE job_key = ?1 AND state = 'processing' AND lease_token = ?2
        "#,
        params![
            job.job_key,
            job.lease_token,
            u64_to_sqlite(next_retry_at),
            error,
            u64_to_sqlite(now),
        ],
    )?;
    Ok(updated == 1)
}

pub(crate) fn complete_on_connection(
    conn: &mut Connection,
    job: &ClaimedDeferredLifecycleMetricJob,
    event_jsons: &[String],
    now: u64,
) -> Result<bool, GitAiError> {
    validate_event_bundle(job, event_jsons)?;
    let tx = conn.transaction()?;
    let owns_lease: bool = tx.query_row(
        r#"
        SELECT EXISTS(
            SELECT 1
            FROM deferred_lifecycle_metric_jobs
            WHERE job_key = ?1 AND state = 'processing' AND lease_token = ?2
        )
        "#,
        params![job.job_key, job.lease_token],
        |row| row.get(0),
    )?;
    if !owns_lease {
        tx.rollback()?;
        return Ok(false);
    }
    let mut metric_ids = Vec::with_capacity(event_jsons.len());
    for event_json in event_jsons {
        let event: MetricEvent = serde_json::from_str(event_json)?;
        tx.execute(
            "INSERT INTO metrics (event_json, event_ts, event_kind) VALUES (?1, ?2, ?3)",
            params![
                event_json,
                i64::from(event.timestamp),
                i64::from(event.event_id)
            ],
        )?;
        metric_ids.push(tx.last_insert_rowid());
    }
    let metric_ids_json = serde_json::to_string(&metric_ids)?;
    let updated = tx.execute(
        r#"
        UPDATE deferred_lifecycle_metric_jobs
        SET state = 'done',
            processing_started_at = NULL,
            lease_token = NULL,
            last_error = NULL,
            metric_ids_json = ?3,
            repository_workdir = '',
            git_dir = '',
            git_common_dir = '',
            operation_kind = '',
            old_tip = '',
            new_tip = '',
            branch = NULL,
            semantics = '',
            attrs_json = '',
            event_ts = 0,
            updated_at = ?4,
            completed_at = ?4
        WHERE job_key = ?1 AND state = 'processing' AND lease_token = ?2
        "#,
        params![
            job.job_key,
            job.lease_token,
            metric_ids_json,
            u64_to_sqlite(now)
        ],
    )?;
    if updated != 1 {
        tx.rollback()?;
        return Ok(false);
    }
    tx.commit()?;
    Ok(true)
}

fn validate_event_bundle(
    job: &ClaimedDeferredLifecycleMetricJob,
    event_jsons: &[String],
) -> Result<(), GitAiError> {
    if event_jsons.len() > crate::daemon::rewrite_metrics::MAX_LIFECYCLE_CHUNKS {
        return Err(GitAiError::Generic(
            "deferred lifecycle bundle exceeds chunk limit".to_string(),
        ));
    }
    let mut total_bytes = 0usize;
    let mut expected_operation_id: Option<Value> = None;
    for (index, event_json) in event_jsons.iter().enumerate() {
        if event_json.len() > crate::daemon::rewrite_metrics::MAX_LIFECYCLE_CHUNK_EVENT_BYTES {
            return Err(GitAiError::Generic(
                "deferred lifecycle event exceeds per-chunk byte limit".to_string(),
            ));
        }
        total_bytes = total_bytes.checked_add(event_json.len()).ok_or_else(|| {
            GitAiError::Generic("deferred lifecycle bundle byte size overflowed".to_string())
        })?;
        if total_bytes > crate::daemon::rewrite_metrics::MAX_LIFECYCLE_BUNDLE_EVENT_BYTES {
            return Err(GitAiError::Generic(
                "deferred lifecycle bundle exceeds byte limit".to_string(),
            ));
        }
        let event: MetricEvent = serde_json::from_str(event_json)?;
        if event.event_id != MetricEventId::LifecycleTransition as u16
            || event.timestamp != job.event_ts
            || event.values.get("1").and_then(Value::as_str) != Some(job.operation_kind.as_str())
            || event.values.get("2").and_then(Value::as_str) != Some(job.old_tip.as_str())
            || event.values.get("3").and_then(Value::as_str) != Some(job.new_tip.as_str())
            || event.values.get("6").and_then(Value::as_u64) != Some(index as u64)
            || event.values.get("7").and_then(Value::as_u64) != Some(event_jsons.len() as u64)
            || event.values.get("8").and_then(Value::as_str) != Some(job.semantics.as_str())
        {
            return Err(GitAiError::Generic(
                "deferred lifecycle bundle metadata is inconsistent".to_string(),
            ));
        }
        let operation_id = event.values.get("0").cloned();
        if let Some(expected) = &expected_operation_id {
            if operation_id.as_ref() != Some(expected) {
                return Err(GitAiError::Generic(
                    "deferred lifecycle operation id is inconsistent".to_string(),
                ));
            }
        } else {
            expected_operation_id = operation_id;
        }
    }
    Ok(())
}

pub(crate) fn compact_done_payloads_on_connection(
    conn: &mut Connection,
    limit: usize,
) -> Result<usize, GitAiError> {
    if limit == 0 {
        return Ok(0);
    }
    let updated = conn.execute(
        r#"
        UPDATE deferred_lifecycle_metric_jobs
        SET repository_workdir = '',
            git_dir = '',
            git_common_dir = '',
            operation_kind = '',
            old_tip = '',
            new_tip = '',
            branch = NULL,
            semantics = '',
            attrs_json = '',
            event_ts = 0
        WHERE job_key IN (
            SELECT job_key
            FROM deferred_lifecycle_metric_jobs
            WHERE state = 'done'
              AND (
                  repository_workdir != ''
                  OR git_dir != ''
                  OR git_common_dir != ''
                  OR operation_kind != ''
                  OR old_tip != ''
                  OR new_tip != ''
                  OR branch IS NOT NULL
                  OR semantics != ''
                  OR attrs_json != ''
                  OR event_ts != 0
              )
            ORDER BY completed_at ASC, job_key ASC
            LIMIT ?1
        )
        "#,
        params![i64::try_from(limit).unwrap_or(i64::MAX)],
    )?;
    Ok(updated)
}

pub(crate) fn count_outstanding_on_connection(conn: &mut Connection) -> Result<usize, GitAiError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM deferred_lifecycle_metric_jobs WHERE state != 'done'",
        [],
        |row| row.get(0),
    )?;
    Ok(count.max(0) as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        let conn = crate::sqlite::open_in_memory_with_memory_limits().expect("sqlite");
        conn.execute_batch(
            r#"
            CREATE TABLE metrics (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_json TEXT NOT NULL,
                event_ts INTEGER,
                event_kind INTEGER
            );
            "#,
        )
        .expect("metrics schema");
        conn.execute_batch(DEFERRED_LIFECYCLE_JOBS_SCHEMA_SQL)
            .expect("job schema");
        conn
    }

    fn spec() -> DeferredLifecycleMetricJobSpec {
        DeferredLifecycleMetricJobSpec {
            repo_identity: "repo".to_string(),
            repository_workdir: "/work".to_string(),
            git_dir: "/work/.git".to_string(),
            git_common_dir: "/work/.git".to_string(),
            operation_kind: "reset".to_string(),
            old_tip: "a".repeat(40),
            new_tip: "b".repeat(40),
            branch: Some("main".to_string()),
            semantics: "ref_transition".to_string(),
            attrs_json: "{}".to_string(),
            event_ts: 123,
        }
    }

    fn lifecycle_event(job: &ClaimedDeferredLifecycleMetricJob) -> String {
        serde_json::json!({
            "t": job.event_ts,
            "e": MetricEventId::LifecycleTransition as u16,
            "v": {
                "0": "sha256:operation",
                "1": job.operation_kind,
                "2": job.old_tip,
                "3": job.new_tip,
                "4": [job.old_tip],
                "5": [],
                "6": 0,
                "7": 1,
                "8": job.semantics
            },
            "a": {}
        })
        .to_string()
    }

    #[test]
    fn build_failure_keeps_old_and_new_tips_for_durable_retry() {
        let mut conn = setup();
        let spec = spec();
        assert!(enqueue_on_connection(&mut conn, &spec, 10).unwrap());
        let first = claim_due_on_connection(&mut conn, 10, 600)
            .unwrap()
            .expect("claimed");
        assert_eq!(first.old_tip, spec.old_tip);
        assert_eq!(first.new_tip, spec.new_tip);
        assert!(mark_failed_on_connection(&mut conn, &first, "rev-list failed", 10).unwrap());
        assert_eq!(count_outstanding_on_connection(&mut conn).unwrap(), 1);

        let retry = claim_due_on_connection(&mut conn, 15, 600)
            .unwrap()
            .expect("retried");
        assert_eq!(retry.old_tip, spec.old_tip);
        assert_eq!(retry.new_tip, spec.new_tip);
        assert!(
            complete_on_connection(&mut conn, &retry, &[lifecycle_event(&retry)], 16,).unwrap()
        );
        assert_eq!(count_outstanding_on_connection(&mut conn).unwrap(), 0);
        let metric_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(metric_count, 1);
    }

    #[test]
    fn outbox_insert_failure_rolls_back_job_completion_and_can_be_retried() {
        let mut conn = setup();
        let spec = spec();
        enqueue_on_connection(&mut conn, &spec, 10).unwrap();
        let claimed = claim_due_on_connection(&mut conn, 10, 600)
            .unwrap()
            .expect("claimed");
        conn.execute_batch(
            r#"
            CREATE TRIGGER reject_lifecycle_metric
            BEFORE INSERT ON metrics
            BEGIN
                SELECT RAISE(ABORT, 'injected outbox failure');
            END;
            "#,
        )
        .unwrap();
        assert!(
            complete_on_connection(&mut conn, &claimed, &[lifecycle_event(&claimed)], 11,).is_err()
        );
        let state: String = conn
            .query_row(
                "SELECT state FROM deferred_lifecycle_metric_jobs WHERE job_key = ?1",
                params![claimed.job_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "processing");
        let metric_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(metric_count, 0);
    }
}
