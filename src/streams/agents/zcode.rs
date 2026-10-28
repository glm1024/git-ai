//! ZCode SQLite usage and exact tool-generation metadata. No transcript bodies are read.

use crate::streams::agent::{Agent, PathResolverKind, StreamDescriptor};
use crate::streams::sweep::{DiscoveredSession, StreamFormat, SweepStrategy};
use crate::streams::types::{StreamBatch, StreamError};
use crate::streams::watermark::{WatermarkStrategy, WatermarkType, ZCodeSeenUsage, ZCodeWatermark};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZCodeToolContext {
    pub model: String,
    pub provider: String,
    pub parent_session_id: Option<String>,
    pub directory: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZCodeSessionContext {
    pub parent_session_id: Option<String>,
    pub directory: PathBuf,
}

/// Session ancestry is independent of the current model selection.
pub fn resolve_session_context(
    path: &Path,
    session_id: &str,
) -> Result<Option<ZCodeSessionContext>, StreamError> {
    let conn = open_readonly(path)?;
    conn.query_row(
        "SELECT parent_id, directory FROM session WHERE id=?1",
        [session_id],
        |row| {
            Ok(ZCodeSessionContext {
                parent_session_id: row.get(0)?,
                directory: PathBuf::from(row.get::<_, String>(1)?),
            })
        },
    )
    .optional()
    .map_err(sql_error)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ZCodeBashTerminal {
    PermissionDenied,
    Cancelled,
}

/// Read metadata for exactly one observed call. The source interval must
/// contain its frozen pre, and conflicting/reused call records fail closed.
pub(crate) fn terminal_bash_call(
    path: &Path,
    session_id: &str,
    call_id: &str,
    pre_started_at_millis: i64,
    observed_at_millis: i64,
    trace_id: Option<&str>,
    turn_id: Option<&str>,
) -> Result<Option<ZCodeBashTerminal>, StreamError> {
    let conn = open_readonly(path)?;
    let mut stmt = conn
        .prepare(
            "SELECT trace_id, turn_id, started_at, completed_at,
        tool_name,status,error_type,cancelled_by_user FROM tool_usage
        WHERE session_id=?1 AND tool_call_id=?2 LIMIT 2",
        )
        .map_err(sql_error)?;
    let rows = stmt
        .query_map(params![session_id, call_id], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, bool>(7)?,
            ))
        })
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?;
    let [(actual_trace, actual_turn, started, Some(completed), tool, status, error, cancelled)] =
        rows.as_slice()
    else {
        return Ok(None);
    };
    if tool != "Bash"
        || status != "error"
        || *started > pre_started_at_millis
        || *completed < pre_started_at_millis
        || *completed > observed_at_millis
        || trace_id.is_some_and(|expected| actual_trace.as_deref() != Some(expected))
        || turn_id.is_some_and(|expected| actual_turn.as_deref() != Some(expected))
    {
        return Ok(None);
    }
    Ok(match error.as_deref() {
        Some("permission_denied") => Some(ZCodeBashTerminal::PermissionDenied),
        Some("tool_cancelled") if *cancelled => Some(ZCodeBashTerminal::Cancelled),
        _ => None,
    })
}

fn sql_error(error: rusqlite::Error) -> StreamError {
    if let rusqlite::Error::SqliteFailure(ref failure, _) = error
        && matches!(
            failure.code,
            rusqlite::ffi::ErrorCode::DatabaseBusy | rusqlite::ffi::ErrorCode::DatabaseLocked
        )
    {
        return StreamError::Transient {
            message: format!("ZCode database busy: {}", error),
            retry_after: Duration::from_secs(2),
        };
    }
    StreamError::Fatal {
        message: format!("ZCode database read failed: {}", error),
    }
}

fn open_readonly(path: &Path) -> Result<Connection, StreamError> {
    let conn =
        crate::sqlite::open_with_flags_and_memory_limits(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(sql_error)?;
    conn.busy_timeout(Duration::from_secs(2))
        .map_err(sql_error)?;
    Ok(conn)
}

/// Read the same configured DB as the installed ZCode runtime. Do not derive
/// this path from ZCODE_DATA_BASE_DIR: that variable is not the DB path contract.
pub fn database_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    database_path_from(
        &home,
        std::env::var_os("GIT_AI_ZCODE_STORAGE_PATH").as_deref(),
        std::env::var_os("GIT_AI_ZCODE_CONFIG_PATH").as_deref(),
    )
}

fn database_path_from(
    home: &Path,
    storage_override: Option<&std::ffi::OsStr>,
    config_override: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    let nonempty = |path: &&std::ffi::OsStr| {
        !path.is_empty() && !path.to_str().is_some_and(|value| value.trim().is_empty())
    };
    if let Some(path) = storage_override.filter(nonempty) {
        return expanded_absolute_path(home, Path::new(path));
    }
    let config_path = match config_override.filter(nonempty) {
        Some(path) => expanded_absolute_path(home, Path::new(path))?,
        None => home.join(".zcode/cli/config.json"),
    };
    database_path_from_config(home, &config_path)
}

fn expanded_absolute_path(home: &Path, path: &Path) -> Option<PathBuf> {
    let expanded = path
        .to_str()
        .and_then(|value| {
            value
                .strip_prefix("~/")
                .or_else(|| value.strip_prefix("~\\"))
        })
        .map(|relative| home.join(relative))
        .unwrap_or_else(|| path.to_path_buf());
    // The CLI hook and daemon can have different working directories. Never
    // allow either side to resolve a relative override against its own cwd.
    expanded.is_absolute().then_some(expanded)
}

fn database_path_from_config(home: &Path, config_path: &Path) -> Option<PathBuf> {
    if !config_path.is_absolute() {
        return None;
    }
    let config = match std::fs::read(config_path) {
        Ok(bytes) => Some(serde_json::from_slice::<Value>(&bytes).ok()?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return None,
    };
    let Some(configured) = config
        .as_ref()
        .and_then(|v| v.pointer("/storage/sessionDbPath"))
    else {
        return Some(home.join(".zcode/cli/db/db.sqlite"));
    };
    expanded_absolute_path(home, Path::new(configured.as_str()?.trim()))
}

/// Bind a tool to its exact generating assistant message and model request.
/// JSON extraction occurs inside SQLite; bodies, inputs and outputs are never
/// selected or copied into Git AI. Multiple conflicting matches fail closed.
pub fn resolve_tool_context(
    path: &Path,
    session_id: &str,
    tool_call_id: &str,
    trace_id: Option<&str>,
    turn_id: Option<&str>,
) -> Result<Option<ZCodeToolContext>, StreamError> {
    let conn = open_readonly(path)?;
    let tx = conn.unchecked_transaction().map_err(sql_error)?;
    let mut stmt = tx
        .prepare(
            "SELECT DISTINCT message_id FROM part
        WHERE session_id=?1 AND json_valid(data) AND json_extract(data, '$.type')='tool'
          AND json_extract(data, '$.callID')=?2 LIMIT 2",
        )
        .map_err(sql_error)?;
    let messages = stmt
        .query_map(params![session_id, tool_call_id], |row| {
            row.get::<_, String>(0)
        })
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?;
    if messages.len() != 1 {
        return Ok(None);
    }
    let message_id = &messages[0];
    let message = tx
        .query_row(
            "SELECT s.parent_id, s.directory,
        json_extract(m.data, '$.modelId'), json_extract(m.data, '$.providerId'),
        json_extract(m.data, '$.anchor.turnId')
        FROM message m JOIN session s ON s.id=m.session_id
        WHERE m.id=?1 AND m.session_id=?2 AND json_valid(m.data)
          AND json_extract(m.data, '$.role')='assistant'",
            params![message_id, session_id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                ))
            },
        )
        .optional()
        .map_err(sql_error)?;
    let Some((parent_session_id, directory, message_model, message_provider, anchor_turn)) =
        message
    else {
        return Ok(None);
    };
    if turn_id
        .zip(anchor_turn.as_deref())
        .is_some_and(|(expected, actual)| expected != actual)
    {
        return Ok(None);
    }
    let mut stmt = tx
        .prepare(
            "SELECT DISTINCT model_id, provider_id, trace_id, turn_id FROM model_usage
        WHERE session_id=?1 AND assistant_message_id=?2 LIMIT 2",
        )
        .map_err(sql_error)?;
    let usages = stmt
        .query_map(params![session_id, message_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(sql_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(sql_error)?;
    if usages.len() > 1 {
        return Ok(None);
    }
    let (model, provider) = if let Some((model, provider, trace, turn)) = usages.into_iter().next()
    {
        if trace_id.is_some_and(|expected| trace.as_deref() != Some(expected))
            || turn_id.is_some_and(|expected| turn.as_deref() != Some(expected))
            || message_model.as_deref().is_some_and(|value| value != model)
            || message_provider
                .as_deref()
                .is_some_and(|value| value != provider)
        {
            return Ok(None);
        }
        (model, provider)
    } else {
        // Streaming tools can run before ModelComplete writes model_usage.
        // The exact generating assistant row already records the execution
        // model (persistAssistantMessage receives the resolved Model object).
        let tool_trace = tx
            .query_row(
                "SELECT trace_id, turn_id FROM tool_usage
            WHERE session_id=?1 AND tool_call_id=?2",
                params![session_id, tool_call_id],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .optional()
            .map_err(sql_error)?;
        if let Some((trace, turn)) = tool_trace
            && (trace_id.is_some_and(|expected| trace.as_deref() != Some(expected))
                || turn_id.is_some_and(|expected| turn.as_deref() != Some(expected)))
        {
            return Ok(None);
        }
        let (Some(model), Some(provider)) = (message_model, message_provider) else {
            return Ok(None);
        };
        (model, provider)
    };
    if model.trim().is_empty() || provider.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(ZCodeToolContext {
        model,
        provider,
        parent_session_id,
        directory: PathBuf::from(directory),
    }))
}

pub struct ZCodeAgent {
    batch_size: usize,
}

impl ZCodeAgent {
    pub fn new() -> Self {
        Self { batch_size: 1000 }
    }
}

impl Default for ZCodeAgent {
    fn default() -> Self {
        Self::new()
    }
}

/// Only the immutable identity and normalized usage projection leave SQLite.
fn read_usage_page(
    conn: &Connection,
    session: &str,
    watermark: &ZCodeWatermark,
    limit: usize,
) -> Result<Vec<Value>, StreamError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, logical_request_id, session_id, turn_id, trace_id,
        assistant_message_id, parent_user_message_id, provider_id, model_id, status, started_at,
        input_tokens, output_tokens, cache_read_input_tokens, cache_creation_input_tokens
        FROM model_usage WHERE session_id=?1 AND started_at>=?2
          AND status IN ('completed', 'error', 'cancelled')
          AND (started_at>?3 OR (started_at=?3 AND id>?4))
        ORDER BY started_at, id LIMIT ?5",
        )
        .map_err(sql_error)?;
    stmt.query_map(
        params![
            session,
            watermark.activated_at_millis,
            watermark.scan_started_at,
            watermark.scan_id,
            limit as i64
        ],
        |row| {
            Ok(json!({"zcode_usage": {
                "id": row.get::<_, String>(0)?,
                "logical_request_id": row.get::<_, String>(1)?,
                "session_id": row.get::<_, String>(2)?,
                "turn_id": row.get::<_, Option<String>>(3)?,
                "trace_id": row.get::<_, Option<String>>(4)?,
                "assistant_message_id": row.get::<_, Option<String>>(5)?,
                "parent_user_message_id": row.get::<_, Option<String>>(6)?,
                "provider_id": row.get::<_, String>(7)?,
                "model_id": row.get::<_, String>(8)?,
                "status": row.get::<_, String>(9)?,
                "started_at": row.get::<_, i64>(10)?,
                "input_tokens": row.get::<_, i64>(11)?.max(0),
                "output_tokens": row.get::<_, i64>(12)?.max(0),
                "cache_read_input_tokens": row.get::<_, i64>(13)?.max(0),
                "cache_creation_input_tokens": row.get::<_, i64>(14)?.max(0),
            }}))
        },
    )
    .map_err(sql_error)?
    .collect::<Result<Vec<_>, _>>()
    .map_err(sql_error)
}

impl Agent for ZCodeAgent {
    fn batch_size_hint(&self) -> usize {
        self.batch_size
    }
    fn sweep_strategy(&self) -> SweepStrategy {
        SweepStrategy::Periodic(Duration::from_secs(30 * 60))
    }
    fn polls_tracked_sessions(&self) -> bool {
        true
    }
    fn discover_sessions(&self) -> Result<Vec<DiscoveredSession>, StreamError> {
        // Only sessions enrolled by hooks are eligible; never discover history.
        Ok(Vec::new())
    }
    fn read_incremental(
        &self,
        path: &Path,
        watermark: Box<dyn WatermarkStrategy>,
        session_id: &str,
    ) -> Result<StreamBatch, StreamError> {
        let mut next = watermark
            .as_any()
            .downcast_ref::<ZCodeWatermark>()
            .ok_or_else(|| StreamError::Fatal {
                message: "ZCode reader requires ZCodeUsage watermark".to_string(),
            })?
            .clone();
        if next.cycle_complete {
            next.cycle_complete = false;
            next.generation = next.generation.wrapping_add(1);
            next.scan_started_at = next.activated_at_millis;
            next.scan_id.clear();
            return Ok(StreamBatch {
                events: Vec::new(),
                new_watermark: Box::new(next),
            });
        }
        let conn = open_readonly(path)?;
        let mut events = Vec::new();
        loop {
            let page = read_usage_page(&conn, session_id, &next, self.batch_size)?;
            let page_len = page.len();
            for event in page {
                let usage = &event["zcode_usage"];
                let id = usage["id"].as_str().unwrap_or_default();
                let started_at = usage["started_at"].as_i64().unwrap_or_default();
                next.scan_started_at = started_at;
                next.scan_id = id.to_string();
                let digest = format!("{:x}", Sha256::digest(event.to_string().as_bytes()));
                if next
                    .seen
                    .get(id)
                    .is_none_or(|previous| previous.digest != digest)
                {
                    next.seen.insert(
                        id.to_string(),
                        ZCodeSeenUsage {
                            generation: next.generation,
                            digest,
                        },
                    );
                    events.push(event);
                } else if let Some(previous) = next.seen.get_mut(id) {
                    previous.generation = next.generation;
                }
            }
            if page_len < self.batch_size {
                // A complete scan proves which rows still exist. Prune hashes
                // only after that proof, even if upstream retention is delayed.
                next.seen
                    .retain(|_, usage| usage.generation == next.generation);
                next.cycle_complete = !events.is_empty();
                if events.is_empty() {
                    next.generation = next.generation.wrapping_add(1);
                    next.scan_started_at = next.activated_at_millis;
                    next.scan_id.clear();
                }
                break;
            }
            if !events.is_empty() {
                break;
            }
        }
        Ok(StreamBatch {
            events,
            new_watermark: Box::new(next),
        })
    }
    fn extract_event_ids(&self, event: &Value) -> (Option<String>, Option<String>, Option<String>) {
        (
            event
                .pointer("/zcode_usage/id")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            event
                .pointer("/zcode_usage/parent_user_message_id")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            None,
        )
    }
    fn extract_event_timestamp(
        &self,
        event: &Value,
        file_meta: &std::fs::Metadata,
        first: bool,
    ) -> u32 {
        event
            .pointer("/zcode_usage/started_at")
            .and_then(Value::as_i64)
            .map(|ms| (ms / 1000).clamp(0, u32::MAX as i64) as u32)
            .unwrap_or_else(|| crate::streams::agent::file_time_fallback(file_meta, first))
    }
    fn streams(&self) -> Vec<StreamDescriptor> {
        vec![StreamDescriptor {
            stream_kind: "transcript",
            format: StreamFormat::ZCodeSqlite,
            watermark_type: WatermarkType::ZCodeUsage,
            path_resolver: PathResolverKind::Identity,
            shared: false,
            watermark_type_resolver: None,
            format_resolver: None,
        }]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use serde_json::json;

    #[test]
    fn zcode_terminal_metadata_requires_unique_call_and_exact_pre_interval() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("terminal.sqlite");
        let conn = crate::sqlite::open_with_memory_limits(&path).unwrap();
        conn.execute_batch("CREATE TABLE tool_usage(session_id TEXT,tool_call_id TEXT,trace_id TEXT,turn_id TEXT,
            tool_name TEXT,status TEXT,error_type TEXT,started_at INTEGER,completed_at INTEGER,cancelled_by_user INTEGER);
            INSERT INTO tool_usage VALUES('s','call','trace','turn','Bash','error','permission_denied',100,200,0);").unwrap();
        let read = |session, call, pre, observed, trace, turn| {
            terminal_bash_call(&path, session, call, pre, observed, trace, turn).unwrap()
        };
        assert_eq!(
            read("s", "call", 150, 250, Some("trace"), Some("turn")),
            Some(ZCodeBashTerminal::PermissionDenied)
        );
        for (session, call, pre, observed, trace, turn) in [
            ("other", "call", 150, 250, Some("trace"), Some("turn")),
            ("s", "other", 150, 250, Some("trace"), Some("turn")),
            ("s", "call", 99, 250, Some("trace"), Some("turn")),
            ("s", "call", 201, 250, Some("trace"), Some("turn")),
            ("s", "call", 150, 199, Some("trace"), Some("turn")),
            ("s", "call", 150, 250, Some("different"), Some("turn")),
            ("s", "call", 150, 250, Some("trace"), Some("different")),
        ] {
            assert_eq!(read(session, call, pre, observed, trace, turn), None);
        }
        conn.execute_batch(
            "UPDATE tool_usage SET error_type='tool_cancelled',cancelled_by_user=1;",
        )
        .unwrap();
        assert_eq!(
            read("s", "call", 150, 250, None, None),
            Some(ZCodeBashTerminal::Cancelled)
        );
        conn.execute_batch("UPDATE tool_usage SET cancelled_by_user=0;")
            .unwrap();
        assert_eq!(read("s", "call", 150, 250, None, None), None);
        conn.execute_batch("UPDATE tool_usage SET error_type='generic_error',cancelled_by_user=1;")
            .unwrap();
        assert_eq!(read("s", "call", 150, 250, None, None), None);
        conn.execute_batch("UPDATE tool_usage SET error_type='permission_denied';
            INSERT INTO tool_usage VALUES('s','call','trace','turn','Bash','completed',NULL,100,200,0);").unwrap();
        assert_eq!(
            read("s", "call", 150, 250, None, None),
            None,
            "conflicting success must not be filtered out before uniqueness validation"
        );
        conn.execute_batch("DROP TABLE tool_usage;").unwrap();
        assert!(
            terminal_bash_call(&path, "s", "call", 150, 250, None, None).is_err(),
            "schema failure must not manufacture a terminal"
        );
    }

    fn fixture(path: &Path) -> Connection {
        let conn = crate::sqlite::open_with_memory_limits(path).unwrap();
        conn.execute_batch("CREATE TABLE session(id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT);
            CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, data TEXT);
            CREATE TABLE part(id TEXT PRIMARY KEY, session_id TEXT, message_id TEXT, data TEXT);
            CREATE TABLE tool_usage(session_id TEXT, tool_call_id TEXT, trace_id TEXT, turn_id TEXT);
            CREATE TABLE model_usage(id TEXT PRIMARY KEY, logical_request_id TEXT, session_id TEXT,
              turn_id TEXT, trace_id TEXT, assistant_message_id TEXT, parent_user_message_id TEXT,
              provider_id TEXT, model_id TEXT, status TEXT, started_at INTEGER, completed_at INTEGER,
              input_tokens INTEGER, output_tokens INTEGER, cache_read_input_tokens INTEGER,
              cache_creation_input_tokens INTEGER);
            INSERT INTO session VALUES('s', 'parent', '/repo');").unwrap();
        conn
    }

    fn usage(conn: &Connection, id: &str, started: i64, status: &str, model: &str) {
        conn.execute(
            "INSERT OR REPLACE INTO model_usage VALUES(?1, 'logical', 's', 'turn', 'trace', ?1,
            'user', 'provider', ?2, ?3, ?4, ?4+1, 100, 20, 30, 10)",
            params![id, model, status, started],
        )
        .unwrap();
    }

    #[test]
    fn zcode_path_overrides_expand_home_and_reject_relative_paths_without_env_mutation() {
        use std::ffi::OsStr;
        let home = tempfile::tempdir().unwrap();
        let config_dir = home.path().join("custom");
        std::fs::create_dir(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.json"),
            r#"{"storage":{"sessionDbPath":"~/actual/db.sqlite"}}"#,
        )
        .unwrap();
        assert_eq!(
            database_path_from(home.path(), None, Some(OsStr::new("~/custom/config.json")))
                .unwrap(),
            home.path().join("actual/db.sqlite")
        );
        assert_eq!(
            database_path_from(home.path(), Some(OsStr::new("~/override.sqlite")), None).unwrap(),
            home.path().join("override.sqlite")
        );
        assert_eq!(
            database_path_from(home.path(), Some(OsStr::new("~\\override.sqlite")), None).unwrap(),
            home.path().join("override.sqlite")
        );
        let absolute = home.path().join("absolute.sqlite");
        assert_eq!(
            database_path_from(
                home.path(),
                Some(absolute.as_os_str()),
                Some(OsStr::new("bad-relative-config"))
            )
            .unwrap(),
            absolute
        );
        assert!(
            database_path_from(home.path(), Some(OsStr::new("relative.sqlite")), None).is_none()
        );
        assert!(
            database_path_from(home.path(), None, Some(OsStr::new("relative-config.json")))
                .is_none()
        );
        assert!(
            database_path_from_config(home.path(), Path::new("relative-config.json")).is_none()
        );
        assert_eq!(
            database_path_from(home.path(), Some(OsStr::new("")), Some(OsStr::new("  "))).unwrap(),
            home.path().join(".zcode/cli/db/db.sqlite")
        );
    }

    #[test]
    fn zcode_configured_database_path_and_missing_database_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.json");
        assert_eq!(
            database_path_from_config(dir.path(), &config).unwrap(),
            dir.path().join(".zcode/cli/db/db.sqlite")
        );
        std::fs::write(
            &config,
            r#"{"storage":{"sessionDbPath":"~/custom/db.sqlite"}}"#,
        )
        .unwrap();
        assert_eq!(
            database_path_from_config(dir.path(), &config).unwrap(),
            dir.path().join("custom/db.sqlite")
        );
        std::fs::write(
            &config,
            r#"{"storage":{"sessionDbPath":"relative.sqlite"}}"#,
        )
        .unwrap();
        assert!(database_path_from_config(dir.path(), &config).is_none());
        std::fs::write(&config, "bad json").unwrap();
        assert!(database_path_from_config(dir.path(), &config).is_none());
        let absent = dir.path().join("absent.sqlite");
        assert!(resolve_tool_context(&absent, "s", "call", None, None).is_err());
        assert!(
            !absent.exists(),
            "read-only access must never create the source DB"
        );
    }

    #[test]
    fn zcode_bounded_batches_rescan_without_duplicates_and_prune_only_missing_sources() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        let conn = fixture(&path);
        for id in ["a", "b", "c", "d", "e"] {
            usage(&conn, id, 100, "completed", "model");
        }
        conn.execute("INSERT INTO session VALUES('other', NULL, '/other')", [])
            .unwrap();
        conn.execute("UPDATE model_usage SET session_id='other' WHERE id='e'", [])
            .unwrap();
        let reader = ZCodeAgent { batch_size: 2 };
        let mut wm: Box<dyn WatermarkStrategy> = Box::new(ZCodeWatermark::at(100));
        let mut ids = Vec::new();
        loop {
            let batch = reader.read_incremental(&path, wm, "s").unwrap();
            assert!(batch.events.len() <= 2);
            ids.extend(
                batch
                    .events
                    .iter()
                    .map(|e| e["zcode_usage"]["id"].as_str().unwrap().to_string()),
            );
            wm = WatermarkType::ZCodeUsage
                .deserialize(&batch.new_watermark.serialize())
                .unwrap();
            if batch.events.is_empty() {
                break;
            }
        }
        assert_eq!(ids, ["a", "b", "c", "d"]);
        let batch = reader.read_incremental(&path, wm, "s").unwrap();
        assert!(batch.events.is_empty());
        conn.execute("DELETE FROM model_usage WHERE id='a'", [])
            .unwrap();
        let batch = reader
            .read_incremental(&path, batch.new_watermark, "s")
            .unwrap();
        assert!(batch.events.is_empty());
        let cursor = batch
            .new_watermark
            .as_any()
            .downcast_ref::<ZCodeWatermark>()
            .unwrap();
        assert_eq!(cursor.seen.len(), 3);
    }

    #[test]
    fn zcode_no_backfill_and_same_millisecond_late_rows_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        let conn = fixture(&path);
        usage(&conn, "old", 99, "completed", "old-model");
        usage(&conn, "z", 100, "completed", "model-a");
        let reader = ZCodeAgent { batch_size: 1 };
        let batch = reader
            .read_incremental(&path, Box::new(ZCodeWatermark::at(100)), "s")
            .unwrap();
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0]["zcode_usage"]["id"], "z");
        let mut wm = batch.new_watermark;
        loop {
            let batch = reader.read_incremental(&path, wm, "s").unwrap();
            wm = batch.new_watermark;
            if batch.events.is_empty() {
                break;
            }
        }
        usage(&conn, "a", 100, "completed", "model-b");
        let wm = WatermarkType::ZCodeUsage
            .deserialize(&wm.serialize())
            .unwrap();
        let batch = reader.read_incremental(&path, wm, "s").unwrap();
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0]["zcode_usage"]["id"], "a");
        let serialized = serde_json::to_string(&batch.events).unwrap();
        assert!(!serialized.contains("data"));
        assert!(!serialized.contains("raw_usage"));
    }

    #[test]
    fn zcode_running_completion_and_updates_are_not_lost() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        let conn = fixture(&path);
        usage(&conn, "a", 100, "running", "model-a");
        let reader = ZCodeAgent::new();
        let batch = reader
            .read_incremental(&path, Box::new(ZCodeWatermark::at(100)), "s")
            .unwrap();
        assert!(batch.events.is_empty());
        usage(&conn, "a", 100, "error", "model-a");
        let batch = reader
            .read_incremental(&path, batch.new_watermark, "s")
            .unwrap();
        assert_eq!(batch.events.len(), 1);
        let batch = reader
            .read_incremental(&path, batch.new_watermark, "s")
            .unwrap();
        assert!(batch.events.is_empty());
        conn.execute("UPDATE model_usage SET output_tokens=25 WHERE id='a'", [])
            .unwrap();
        let batch = reader
            .read_incremental(&path, batch.new_watermark, "s")
            .unwrap();
        assert_eq!(batch.events[0]["zcode_usage"]["output_tokens"], 25);
    }

    #[test]
    fn zcode_streaming_tool_resolves_exact_execution_model_before_usage_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        let conn = fixture(&path);
        conn.execute("INSERT INTO message VALUES('streaming', 's', ?1)", [json!({"role":"assistant", "modelId":"glm-stream", "providerId":"zai-plan", "anchor":{"turnId":"turn"}}).to_string()]).unwrap();
        conn.execute(
            "INSERT INTO part VALUES('p', 's', 'streaming', ?1)",
            [json!({"type":"tool", "callID":"call"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tool_usage VALUES('s','call','trace','turn')",
            [],
        )
        .unwrap();
        let context = resolve_tool_context(&path, "s", "call", Some("trace"), Some("turn"))
            .unwrap()
            .unwrap();
        assert_eq!(context.model, "glm-stream");
        assert_eq!(context.provider, "zai-plan");
        assert!(
            resolve_tool_context(&path, "s", "call", Some("other"), Some("turn"))
                .unwrap()
                .is_none()
        );
        assert!(
            resolve_tool_context(&path, "s", "call", Some("trace"), Some("other"))
                .unwrap()
                .is_none()
        );
        usage(&conn, "newer", 200, "completed", "different-latest-model");
        assert_eq!(
            resolve_tool_context(&path, "s", "call", Some("trace"), Some("turn"))
                .unwrap()
                .unwrap()
                .model,
            "glm-stream"
        );
    }

    #[test]
    fn zcode_exact_tool_message_keeps_model_switch_and_ambiguity_safe() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        let conn = fixture(&path);
        usage(&conn, "a", 100, "completed", "model-a");
        usage(&conn, "b", 200, "completed", "model-b");
        conn.execute(
            "INSERT INTO message VALUES('a', 's', ?1)",
            [
                json!({"role":"assistant", "modelId":"model-a", "providerId":"provider"})
                    .to_string(),
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part VALUES('part', 's', 'a', ?1)",
            [
                json!({"type":"tool", "callID":"call", "state":{"input":{"secret":"never-read"}}})
                    .to_string(),
            ],
        )
        .unwrap();
        let context = resolve_tool_context(&path, "s", "call", Some("trace"), Some("turn"))
            .unwrap()
            .unwrap();
        assert_eq!(context.model, "model-a");
        assert_eq!(context.provider, "provider");
        assert_eq!(context.parent_session_id.as_deref(), Some("parent"));
        assert_eq!(context.directory, PathBuf::from("/repo"));
        let session = resolve_session_context(&path, "s").unwrap().unwrap();
        assert_eq!(session.parent_session_id.as_deref(), Some("parent"));
        assert_eq!(session.directory, context.directory);
        assert!(
            resolve_tool_context(&path, "s", "call", Some("wrong"), None)
                .unwrap()
                .is_none()
        );
        conn.execute(
            "INSERT INTO part VALUES('other', 's', 'b', ?1)",
            [json!({"type":"tool", "callID":"call"}).to_string()],
        )
        .unwrap();
        assert!(
            resolve_tool_context(&path, "s", "call", None, None)
                .unwrap()
                .is_none()
        );
    }
}
