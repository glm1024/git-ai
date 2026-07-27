//! Codex agent implementation with sweep discovery.

use crate::authorship::authorship_log_serialization::generate_session_id;
use crate::mdm::utils::codex_home_dir;
use crate::streams::agent::{Agent, PathResolverKind, StreamDescriptor};
use crate::streams::sweep::{DiscoveredSession, StreamFormat, SweepStrategy};
use crate::streams::types::{StreamBatch, StreamError, TOKEN_BASELINE_ONLY_FIELD};
use crate::streams::watermark::{CodexByteOffsetWatermark, WatermarkStrategy};
use chrono::DateTime;
use serde_json::Value;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

struct CodexForkPrefix {
    boundary_offset: u64,
    baseline_event: Option<Value>,
}

enum InitialRead {
    Regular,
    IncompleteSubagent,
    ReadySubagent(CodexForkPrefix),
}

/// Codex agent that reads Codex JSONL transcript files.
pub struct CodexAgent {
    batch_size: usize,
}

impl CodexAgent {
    pub fn new() -> Self {
        Self { batch_size: 1000 }
    }

    #[cfg(test)]
    pub fn with_batch_size(batch_size: usize) -> Self {
        Self { batch_size }
    }

    /// Search for a rollout file matching the given session ID in the Codex home directory.
    ///
    /// Looks in both `sessions` and `archived_sessions` subdirectories for files
    /// matching `rollout-*{session_id}*.jsonl`. Returns the newest match by
    /// modification time.
    pub fn find_rollout_path_for_session_in_home(
        session_id: &str,
        codex_home: &Path,
    ) -> Result<Option<PathBuf>, StreamError> {
        let mut candidates: Vec<PathBuf> = Vec::new();

        for subdir in &["sessions", "archived_sessions"] {
            let search_dir = codex_home.join(subdir);
            if !search_dir.exists() {
                continue;
            }

            let pattern = format!("{}/**/rollout-*{}*.jsonl", search_dir.display(), session_id);

            let entries = glob::glob(&pattern).map_err(|e| StreamError::Fatal {
                message: format!("Invalid glob pattern for Codex session search: {}", e),
            })?;

            for entry in entries {
                let path = entry.map_err(|e| StreamError::Fatal {
                    message: format!("Error reading glob entry: {}", e),
                })?;
                candidates.push(path);
            }
        }

        if candidates.is_empty() {
            return Ok(None);
        }

        // Return the newest by modification time
        let newest = candidates
            .into_iter()
            .filter_map(|p| {
                p.metadata()
                    .ok()
                    .and_then(|m| m.modified().ok())
                    .map(|t| (p, t))
            })
            .max_by_key(|(_, t)| *t)
            .map(|(p, _)| p);

        Ok(newest)
    }

    fn scan_session_files() -> Vec<PathBuf> {
        let mut paths = Vec::new();

        let codex_home = codex_home_dir();

        for subdir in &["sessions", "archived_sessions"] {
            let search_dir = codex_home.join(subdir);
            if search_dir.exists() {
                Self::scan_rollout_recursive(&search_dir, &mut paths);
            }
        }

        paths
    }

    fn scan_rollout_recursive(dir: &Path, paths: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                Self::scan_rollout_recursive(&path, paths);
            } else if path.is_file()
                && path.extension().map(|ext| ext == "jsonl").unwrap_or(false)
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("rollout-"))
                    .unwrap_or(false)
            {
                paths.push(path);
            }
        }
    }

    /// Detect if a Codex JSONL file is a subagent transcript by reading its first line.
    ///
    /// Subagent transcripts have a `session_meta` first event with
    /// `payload.thread_source == "subagent"` and `payload.forked_from_id` containing
    /// the parent session UUID.
    pub fn detect_subagent_parent(path: &Path) -> Option<String> {
        let file = File::open(path).ok()?;
        let reader = BufReader::new(file);
        let first_line = reader.lines().next()?.ok()?;
        if first_line.trim().is_empty() {
            return None;
        }

        let obj: serde_json::Value = serde_json::from_str(&first_line).ok()?;

        if obj.get("type").and_then(|t| t.as_str()) != Some("session_meta") {
            return None;
        }

        let payload = obj.get("payload")?;

        if payload.get("thread_source").and_then(|t| t.as_str()) != Some("subagent") {
            return None;
        }

        payload
            .get("forked_from_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    fn prepare_initial_read(path: &Path) -> Result<InitialRead, StreamError> {
        let file = File::open(path).map_err(|e| StreamError::Transient {
            message: format!("Failed to open Codex transcript {}: {}", path.display(), e),
            retry_after: Duration::from_secs(5),
        })?;
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        let mut current_offset = 0u64;

        let first_bytes = match crate::streams::types::read_jsonl_line(&mut reader, &mut line)
            .map_err(|e| StreamError::Transient {
                message: format!("I/O error reading Codex session metadata: {}", e),
                retry_after: Duration::from_secs(5),
            })? {
            crate::streams::types::JsonlLineState::Complete(bytes) => bytes,
            crate::streams::types::JsonlLineState::Eof
            | crate::streams::types::JsonlLineState::Partial => {
                return Ok(InitialRead::IncompleteSubagent);
            }
        };
        current_offset += first_bytes as u64;
        let first: Value = serde_json::from_str(&line).map_err(|e| StreamError::Parse {
            line: 1,
            message: format!("Invalid Codex session metadata: {}", e),
        })?;
        let payload = first.get("payload");
        let is_subagent = first.get("type").and_then(Value::as_str) == Some("session_meta")
            && payload
                .and_then(|value| value.get("thread_source"))
                .and_then(Value::as_str)
                == Some("subagent")
            && payload
                .and_then(|value| value.get("forked_from_id"))
                .and_then(Value::as_str)
                .is_some();
        if !is_subagent {
            return Ok(InitialRead::Regular);
        }

        let child_id = payload
            .and_then(|value| value.get("id"))
            .and_then(Value::as_str)
            .and_then(uuid_v7_timestamp_millis);
        let child_started_at = first
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.timestamp());
        let (Some(child_id_millis), Some(child_started_at)) = (child_id, child_started_at) else {
            tracing::warn!(
                path = %path.display(),
                "Codex subagent metadata lacks a usable UUIDv7 or timestamp; token accounting waits for a recognized fork boundary"
            );
            return Ok(InitialRead::IncompleteSubagent);
        };

        let mut last_token_event = None;
        let mut candidate: Option<(u64, String, Option<Value>, bool)> = None;
        let mut line_number = 1usize;
        loop {
            let line_start = current_offset;
            match crate::streams::types::read_jsonl_line(&mut reader, &mut line).map_err(|e| {
                StreamError::Transient {
                    message: format!("I/O error scanning Codex fork prefix: {}", e),
                    retry_after: Duration::from_secs(5),
                }
            })? {
                crate::streams::types::JsonlLineState::Eof
                | crate::streams::types::JsonlLineState::Partial => break,
                crate::streams::types::JsonlLineState::Complete(bytes) => {
                    current_offset += bytes as u64;
                    line_number += 1;
                }
            }
            if line.trim().is_empty() {
                continue;
            }
            let event: Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(error) => {
                    tracing::warn!(
                        line = line_number,
                        path = %path.display(),
                        error = %error,
                        "skipping malformed JSON while scanning Codex fork prefix"
                    );
                    continue;
                }
            };

            if let Some((boundary_offset, turn_id, baseline_event, saw_turn_context)) =
                candidate.as_mut()
            {
                if event.get("type").and_then(Value::as_str) == Some("turn_context")
                    && event.pointer("/payload/turn_id").and_then(Value::as_str)
                        == Some(turn_id.as_str())
                {
                    *saw_turn_context = true;
                } else if *saw_turn_context
                    && event.get("type").and_then(Value::as_str)
                        == Some("inter_agent_communication_metadata")
                    && event
                        .pointer("/payload/trigger_turn")
                        .and_then(Value::as_bool)
                        == Some(true)
                {
                    let mut baseline_event = baseline_event.clone();
                    if let Some(Value::Object(values)) = baseline_event.as_mut() {
                        values.insert(TOKEN_BASELINE_ONLY_FIELD.to_string(), Value::Bool(true));
                    }
                    return Ok(InitialRead::ReadySubagent(CodexForkPrefix {
                        boundary_offset: *boundary_offset,
                        baseline_event,
                    }));
                }
                continue;
            }

            if is_cumulative_token_event(&event) {
                last_token_event = Some(event.clone());
            }

            if event.pointer("/payload/type").and_then(Value::as_str) == Some("task_started") {
                let started_at = event.pointer("/payload/started_at").and_then(Value::as_i64);
                let turn_id = event.pointer("/payload/turn_id").and_then(Value::as_str);
                let turn_millis = turn_id.and_then(uuid_v7_timestamp_millis);
                if started_at.is_some_and(|value| value >= child_started_at)
                    && turn_millis.is_some_and(|value| value >= child_id_millis)
                {
                    candidate = Some((
                        line_start,
                        turn_id.unwrap_or_default().to_string(),
                        last_token_event.clone(),
                        false,
                    ));
                }
            }
        }

        tracing::debug!(
            path = %path.display(),
            "Codex subagent fork boundary is incomplete; leaving watermark unchanged"
        );
        Ok(InitialRead::IncompleteSubagent)
    }
}

fn uuid_v7_timestamp_millis(value: &str) -> Option<u64> {
    if value.as_bytes().get(14).copied()? != b'7' {
        return None;
    }
    let prefix: String = value.chars().filter(|ch| *ch != '-').take(12).collect();
    if prefix.len() != 12 {
        return None;
    }
    u64::from_str_radix(&prefix, 16).ok()
}

fn is_cumulative_token_event(event: &Value) -> bool {
    event.pointer("/payload/type").and_then(Value::as_str) == Some("token_count")
        && event
            .pointer("/payload/info/total_token_usage")
            .is_some_and(Value::is_object)
}

impl Default for CodexAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl Agent for CodexAgent {
    fn batch_size_hint(&self) -> usize {
        self.batch_size
    }

    fn sweep_strategy(&self) -> SweepStrategy {
        SweepStrategy::Periodic(Duration::from_secs(30 * 60))
    }

    fn discover_sessions(&self) -> Result<Vec<DiscoveredSession>, StreamError> {
        let paths = Self::scan_session_files();
        let mut sessions = Vec::new();

        for path in paths {
            // Codex filename: rollout-2026-02-06T20-35-49-019c35bd-ad8e-7422-834c-3605bc4ee7ac
            // The hook payload sends the UUID as session_id/thread_id (last 36 chars)
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if stem.len() < 36 {
                continue;
            }
            let external_session_id = stem[stem.len() - 36..].to_string();
            let session_id = generate_session_id(&external_session_id, "codex");
            let external_parent_session_id = Self::detect_subagent_parent(&path);

            sessions.push(DiscoveredSession {
                session_id,
                tool: "codex".to_string(),
                stream_path: path,
                external_session_id,
                external_parent_session_id,
            });
        }

        Ok(sessions)
    }

    fn read_incremental(
        &self,
        path: &Path,
        watermark: Box<dyn WatermarkStrategy>,
        session_id: &str,
    ) -> Result<StreamBatch, StreamError> {
        let codex_watermark = watermark
            .as_any()
            .downcast_ref::<CodexByteOffsetWatermark>()
            .ok_or_else(|| StreamError::Fatal {
                message: format!(
                    "Codex reader requires CodexByteOffsetWatermark, got incompatible type for session {}",
                    session_id
                ),
            })?;

        if codex_watermark.offset > 0 && !codex_watermark.fork_prefix_complete {
            return Err(StreamError::Fatal {
                message: format!(
                    "Codex session {} has a legacy watermark without fork-prefix state; replay it from offset zero",
                    session_id
                ),
            });
        }

        let mut start_offset = codex_watermark.offset;
        let mut fork_prefix_complete = codex_watermark.fork_prefix_complete;
        let mut baseline_event = None;
        if start_offset == 0 && !fork_prefix_complete {
            match Self::prepare_initial_read(path)? {
                InitialRead::Regular => {
                    fork_prefix_complete = true;
                }
                InitialRead::IncompleteSubagent => {
                    return Ok(StreamBatch {
                        events: Vec::new(),
                        new_watermark: Box::new(CodexByteOffsetWatermark::initial()),
                    });
                }
                InitialRead::ReadySubagent(prefix) => {
                    start_offset = prefix.boundary_offset;
                    baseline_event = prefix.baseline_event;
                    fork_prefix_complete = true;
                }
            }
        }

        // Open file
        let file = File::open(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StreamError::Fatal {
                    message: format!("Transcript file not found: {}", path.display()),
                }
            } else if e.kind() == std::io::ErrorKind::PermissionDenied {
                StreamError::Fatal {
                    message: format!("Permission denied reading transcript: {}", path.display()),
                }
            } else {
                StreamError::Transient {
                    message: format!("Failed to open transcript file: {}", e),
                    retry_after: Duration::from_secs(5),
                }
            }
        })?;

        let mut reader = BufReader::new(file);

        // Seek to watermark position
        reader
            .seek(SeekFrom::Start(start_offset))
            .map_err(|e| StreamError::Transient {
                message: format!("Failed to seek to offset {}: {}", start_offset, e),
                retry_after: Duration::from_secs(5),
            })?;

        let batch_limit = self.batch_size_hint();
        let mut events = Vec::with_capacity(batch_limit);
        if let Some(event) = baseline_event {
            events.push(event);
        }
        let mut current_offset = start_offset;
        let mut line_number = 0;
        let mut line = String::new();

        loop {
            match crate::streams::types::read_jsonl_line(&mut reader, &mut line).map_err(|e| {
                StreamError::Transient {
                    message: format!("I/O error reading line: {}", e),
                    retry_after: Duration::from_secs(5),
                }
            })? {
                crate::streams::types::JsonlLineState::Eof => break,
                crate::streams::types::JsonlLineState::Partial => break,
                crate::streams::types::JsonlLineState::Complete(bytes_read) => {
                    line_number += 1;
                    current_offset += bytes_read as u64;
                }
            }

            if line.trim().is_empty() {
                continue;
            }

            let entry: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        line = line_number,
                        path = %path.display(),
                        error = %e,
                        "skipping malformed JSON line"
                    );
                    continue;
                }
            };

            events.push(entry);
            if events.len() >= batch_limit {
                break;
            }
        }

        let new_watermark = Box::new(CodexByteOffsetWatermark::new(
            current_offset,
            fork_prefix_complete,
        ));

        Ok(StreamBatch {
            events,
            new_watermark,
        })
    }

    fn extract_event_timestamp(
        &self,
        event: &serde_json::Value,
        file_meta: &std::fs::Metadata,
        is_first_event: bool,
    ) -> u32 {
        crate::daemon::stream_worker::extract_event_timestamp(event)
            .unwrap_or_else(|| crate::streams::agent::file_time_fallback(file_meta, is_first_event))
    }

    fn infer_cwd(&self, stream_path: &Path) -> Option<PathBuf> {
        use std::fs::File;
        use std::io::{BufRead, BufReader};

        let file = File::open(stream_path).ok()?;
        let reader = BufReader::new(file);

        // Codex has cwd in session_meta or turn_context payload events
        for line in reader.lines().take(20) {
            let Ok(line) = line else { continue };
            if line.is_empty() {
                continue;
            }
            if let Ok(obj) = serde_json::from_str::<serde_json::Value>(&line) {
                // Check payload.cwd (session_meta and turn_context)
                if let Some(cwd) = obj
                    .get("payload")
                    .and_then(|p| p.get("cwd"))
                    .and_then(|v| v.as_str())
                    && !cwd.is_empty()
                {
                    return Some(PathBuf::from(cwd));
                }
            }
        }
        None
    }

    fn streams(&self) -> Vec<StreamDescriptor> {
        let format = StreamFormat::CodexJsonl;
        vec![StreamDescriptor {
            stream_kind: "transcript",
            format,
            watermark_type: format.watermark_type(),
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
    use crate::streams::watermark::CodexByteOffsetWatermark;

    #[test]
    fn test_sweep_strategy() {
        let agent = CodexAgent::new();
        assert_eq!(
            agent.sweep_strategy(),
            SweepStrategy::Periodic(Duration::from_secs(30 * 60))
        );
    }

    fn make_jsonl_line(i: usize) -> String {
        format!(
            r#"{{"type":"response_item","id":{},"payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"msg-{}"}}]}}}}"#,
            i, i
        )
    }

    fn drain_all(
        agent: &CodexAgent,
        path: &Path,
    ) -> (Vec<serde_json::Value>, Box<dyn WatermarkStrategy>) {
        let mut all = Vec::new();
        let mut wm: Box<dyn WatermarkStrategy> = Box::new(CodexByteOffsetWatermark::initial());
        loop {
            let batch = agent.read_incremental(path, wm, "test").unwrap();
            if batch.events.is_empty() {
                wm = batch.new_watermark;
                break;
            }
            all.extend(batch.events);
            wm = batch.new_watermark;
        }
        (all, wm)
    }

    #[test]
    fn test_batch_resume_no_loss_or_repeat() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        for i in 0..5 {
            writeln!(file, "{}", make_jsonl_line(i)).unwrap();
        }
        file.flush().unwrap();

        let agent = CodexAgent::with_batch_size(2);
        let (events, _) = drain_all(&agent, file.path());

        assert_eq!(events.len(), 5);
        let ids: Vec<u64> = events.iter().map(|e| e["id"].as_u64().unwrap()).collect();
        assert_eq!(ids, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn test_append_one_record_after_full_read() {
        use std::fs::OpenOptions;
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        for i in 0..3 {
            writeln!(file, "{}", make_jsonl_line(i)).unwrap();
        }
        file.flush().unwrap();

        let agent = CodexAgent::with_batch_size(2);
        let (all, wm) = drain_all(&agent, file.path());
        assert_eq!(all.len(), 3);

        let mut f = OpenOptions::new().append(true).open(file.path()).unwrap();
        writeln!(f, "{}", make_jsonl_line(3)).unwrap();
        f.flush().unwrap();

        let batch = agent.read_incremental(file.path(), wm, "test").unwrap();
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0]["id"].as_u64().unwrap(), 3);
    }

    #[test]
    fn test_append_several_records_after_full_read() {
        use std::fs::OpenOptions;
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        for i in 0..3 {
            writeln!(file, "{}", make_jsonl_line(i)).unwrap();
        }
        file.flush().unwrap();

        let agent = CodexAgent::with_batch_size(2);
        let (_, mut wm) = drain_all(&agent, file.path());

        let mut f = OpenOptions::new().append(true).open(file.path()).unwrap();
        for i in 3..6 {
            writeln!(f, "{}", make_jsonl_line(i)).unwrap();
        }
        f.flush().unwrap();

        let mut new_events = Vec::new();
        loop {
            let batch = agent.read_incremental(file.path(), wm, "test").unwrap();
            wm = batch.new_watermark;
            if batch.events.is_empty() {
                break;
            }
            new_events.extend(batch.events);
        }
        assert_eq!(new_events.len(), 3);
        let ids: Vec<u64> = new_events
            .iter()
            .map(|e| e["id"].as_u64().unwrap())
            .collect();
        assert_eq!(ids, vec![3, 4, 5]);
    }

    #[test]
    fn test_read_incremental_basic() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"turn_context","payload":{{"model":"gpt-4o"}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"response_item","payload":{{"type":"message","role":"assistant","content":[{{"type":"output_text","text":"Hello"}}]}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let agent = CodexAgent::new();
        let watermark = Box::new(CodexByteOffsetWatermark::initial());
        let result = agent
            .read_incremental(file.path(), watermark, "test")
            .unwrap();

        // Both JSONL lines are returned as raw JSON
        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0]["type"], "turn_context");
        assert_eq!(result.events[0]["payload"]["model"], "gpt-4o");
        assert_eq!(result.events[1]["type"], "response_item");
        assert_eq!(result.events[1]["payload"]["role"], "assistant");
    }

    #[test]
    fn test_read_incremental_legacy_format() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"event_msg","payload":{{"type":"user_message","message":"Hello"}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"event_msg","payload":{{"type":"agent_message","message":"Hi there"}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let agent = CodexAgent::new();
        let watermark = Box::new(CodexByteOffsetWatermark::initial());
        let result = agent
            .read_incremental(file.path(), watermark, "test")
            .unwrap();

        // Both JSONL lines are returned as raw JSON
        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0]["type"], "event_msg");
        assert_eq!(result.events[0]["payload"]["type"], "user_message");
        assert_eq!(result.events[1]["payload"]["type"], "agent_message");
    }

    #[test]
    fn test_subagent_skips_inherited_history_and_emits_one_token_baseline() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        let lines = [
            r#"{"timestamp":"2026-07-23T07:00:46.994Z","type":"session_meta","payload":{"id":"019f8dc6-d84b-7e42-88f6-980f148cdbbb","forked_from_id":"019f8dc4-717b-7d02-bdb3-6f234e32f2e3","thread_source":"subagent"}}"#,
            r#"{"timestamp":"2026-07-23T07:00:46.995Z","type":"event_msg","payload":{"type":"task_started","turn_id":"019f8dc4-8000-7000-8000-000000000000","started_at":1784789900}}"#,
            r#"{"timestamp":"2026-07-23T07:00:46.996Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":100,"output_tokens":10,"cached_input_tokens":40}}}}"#,
            r#"{"timestamp":"2026-07-23T07:00:46.996Z","type":"inter_agent_communication_metadata","payload":{"trigger_turn":true}}"#,
            r#"{"timestamp":"2026-07-23T07:00:46.997Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":200,"output_tokens":20,"cached_input_tokens":80}}}}"#,
            r#"{"timestamp":"2026-07-23T07:00:47.080Z","type":"event_msg","payload":{"type":"task_started","turn_id":"019f8dc6-d95d-71e1-ab3b-9fbc9bd26a23","started_at":1784790047}}"#,
            r#"{"timestamp":"2026-07-23T07:00:49.974Z","type":"turn_context","payload":{"turn_id":"019f8dc6-d95d-71e1-ab3b-9fbc9bd26a23"}}"#,
            r#"{"timestamp":"2026-07-23T07:00:49.999Z","type":"inter_agent_communication_metadata","payload":{"trigger_turn":true}}"#,
            r#"{"timestamp":"2026-07-23T07:00:56.444Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":230,"output_tokens":25,"cached_input_tokens":90}}}}"#,
        ];
        for line in lines {
            writeln!(file, "{line}").unwrap();
        }
        file.flush().unwrap();

        let agent = CodexAgent::with_batch_size(2);
        let mut watermark: Box<dyn WatermarkStrategy> =
            Box::new(CodexByteOffsetWatermark::initial());
        let mut events = Vec::new();
        loop {
            let batch = agent
                .read_incremental(file.path(), watermark, "child")
                .unwrap();
            watermark = batch.new_watermark;
            if batch.events.is_empty() {
                break;
            }
            events.extend(batch.events);
        }

        let token_events: Vec<_> = events
            .iter()
            .filter(|event| {
                event.pointer("/payload/type").and_then(Value::as_str) == Some("token_count")
            })
            .collect();
        assert_eq!(token_events.len(), 2);
        assert_eq!(
            token_events[0]
                .pointer("/payload/info/total_token_usage/input_tokens")
                .and_then(Value::as_u64),
            Some(200)
        );
        assert_eq!(
            token_events[0]
                .get("_git_ai_token_baseline_only")
                .and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            token_events[1]
                .pointer("/payload/info/total_token_usage/input_tokens")
                .and_then(Value::as_u64),
            Some(230)
        );
        assert!(token_events[1].get("_git_ai_token_baseline_only").is_none());
        assert!(
            watermark
                .as_any()
                .downcast_ref::<CodexByteOffsetWatermark>()
                .unwrap()
                .fork_prefix_complete
        );
    }

    #[test]
    fn test_incomplete_subagent_boundary_keeps_initial_watermark() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"timestamp":"2026-07-23T07:00:46.994Z","type":"session_meta","payload":{{"id":"019f8dc6-d84b-7e42-88f6-980f148cdbbb","forked_from_id":"019f8dc4-717b-7d02-bdb3-6f234e32f2e3","thread_source":"subagent"}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"timestamp":"2026-07-23T07:00:47.080Z","type":"event_msg","payload":{{"type":"task_started","turn_id":"019f8dc6-d95d-71e1-ab3b-9fbc9bd26a23","started_at":1784790047}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let agent = CodexAgent::with_batch_size(2);
        let batch = agent
            .read_incremental(
                file.path(),
                Box::new(CodexByteOffsetWatermark::initial()),
                "child",
            )
            .unwrap();

        assert!(batch.events.is_empty());
        assert_eq!(batch.new_watermark.serialize(), "0|0");
    }

    #[test]
    fn test_detect_subagent_parent_found() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"timestamp":"2026-05-12T20:40:23.849Z","type":"session_meta","payload":{{"id":"019e1deb-59ab-7ec3-8731-5825d1566f6d","forked_from_id":"019e1dea-c02a-71b3-b87f-67812459e1d9","source":{{"subagent":{{"thread_spawn":{{"parent_thread_id":"019e1dea-c02a-71b3-b87f-67812459e1d9","depth":1,"agent_nickname":"Carson"}}}}}},"thread_source":"subagent","agent_nickname":"Carson"}}}}"#).unwrap();
        writeln!(
            file,
            r#"{{"type":"event_msg","payload":{{"type":"user_message","message":"do something"}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let result = CodexAgent::detect_subagent_parent(file.path());
        assert_eq!(
            result,
            Some("019e1dea-c02a-71b3-b87f-67812459e1d9".to_string())
        );
    }

    #[test]
    fn test_detect_subagent_parent_user_session() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, r#"{{"timestamp":"2026-05-12T20:38:00.000Z","type":"session_meta","payload":{{"id":"019e1dea-c02a-71b3-b87f-67812459e1d9","thread_source":"user"}}}}"#).unwrap();
        file.flush().unwrap();

        let result = CodexAgent::detect_subagent_parent(file.path());
        assert_eq!(result, None);
    }

    #[test]
    fn test_detect_subagent_parent_no_session_meta() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut file = NamedTempFile::new().unwrap();
        writeln!(
            file,
            r#"{{"type":"event_msg","payload":{{"type":"user_message","message":"hello"}}}}"#
        )
        .unwrap();
        file.flush().unwrap();

        let result = CodexAgent::detect_subagent_parent(file.path());
        assert_eq!(result, None);
    }

    #[test]
    fn test_detect_subagent_parent_empty_file() {
        use tempfile::NamedTempFile;

        let file = NamedTempFile::new().unwrap();

        let result = CodexAgent::detect_subagent_parent(file.path());
        assert_eq!(result, None);
    }

    #[test]
    fn test_detect_subagent_parent_nonexistent_file() {
        let result =
            CodexAgent::detect_subagent_parent(Path::new("/nonexistent/path/rollout.jsonl"));
        assert_eq!(result, None);
    }
}
