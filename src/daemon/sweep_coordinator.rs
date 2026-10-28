use crate::streams::agent::{Agent, StreamDescriptor, get_all_agents};
use crate::streams::db::{StreamRecord, StreamsDatabase};
use crate::streams::sweep::{DiscoveredSession, SweepStrategy};
use crate::streams::types::StreamError;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// Work items discovered by the sweep.
#[derive(Debug, Clone)]
pub enum SweepItem {
    /// A session has at least one owned (non-shared) stream with new data.
    /// The worker should expand all streams for this session.
    Session {
        session_id: String,
        tool: String,
        canonical_path: PathBuf,
        external_session_id: String,
        external_parent_session_id: Option<String>,
    },
    /// A shared stream has new data. Process just this stream.
    SharedStream {
        tool: String,
        stream_kind: String,
        canonical_path: PathBuf,
    },
}

/// Orchestrates periodic sweeps across all registered agents.
///
/// Discovers sessions via each agent's filesystem scan, then checks staleness
/// separately for owned streams (per-session) and shared streams (once per agent).
pub struct SweepCoordinator {
    streams_db: Arc<StreamsDatabase>,
    agent_registry: Vec<(String, Box<dyn Agent>)>,
    lookback_days: Option<u32>,
}

impl SweepCoordinator {
    pub fn new(streams_db: Arc<StreamsDatabase>) -> Self {
        let lookback_days = crate::config::Config::get().transcript_streaming_lookback_days();
        Self {
            streams_db,
            agent_registry: get_all_agents(),
            lookback_days,
        }
    }

    #[cfg(test)]
    pub fn with_lookback(streams_db: Arc<StreamsDatabase>, lookback_days: Option<u32>) -> Self {
        Self {
            streams_db,
            agent_registry: get_all_agents(),
            lookback_days,
        }
    }

    /// Run a full sweep across all agents.
    pub fn run_sweep(&self) -> Result<Vec<SweepItem>, StreamError> {
        let mut items = Vec::new();

        for (agent_type, agent) in &self.agent_registry {
            if !matches!(agent.sweep_strategy(), SweepStrategy::Periodic(_)) {
                continue;
            }

            if agent.polls_tracked_sessions() {
                // Enrollment is persisted by the hook, not inferred from old
                // source rows. Always poll these sources: SQLite WAL updates
                // and late request finalization need not change main DB mtime.
                let owned_kinds: HashSet<_> = agent
                    .streams()
                    .into_iter()
                    .filter(|stream| !stream.shared)
                    .map(|stream| stream.stream_kind)
                    .collect();
                let mut seen = HashSet::new();
                for record in self.streams_db.all_streams()? {
                    if record.tool != *agent_type
                        || !owned_kinds.contains(record.stream_kind.as_str())
                    {
                        continue;
                    }
                    let path = Self::canonicalize_path(&PathBuf::from(&record.stream_path));
                    if path.exists() && seen.insert((record.session_id.clone(), path.clone())) {
                        items.push(SweepItem::Session {
                            session_id: record.session_id,
                            tool: record.tool,
                            canonical_path: path,
                            external_session_id: record.external_session_id,
                            external_parent_session_id: record.external_parent_session_id,
                        });
                    }
                }
                continue;
            }

            let discovered = match agent.discover_sessions() {
                Ok(sessions) => sessions,
                Err(e) => {
                    tracing::error!(
                        agent_type = %agent_type,
                        error = %e,
                        "agent discovery failed during sweep, skipping"
                    );
                    continue;
                }
            };

            let streams = agent.streams();
            let (shared, owned): (Vec<_>, Vec<_>) = streams.into_iter().partition(|s| s.shared);

            // Per-session: only check owned streams for staleness
            for session in &discovered {
                let canonical = Self::canonicalize_path(&session.stream_path);
                if self.any_stream_stale(session, &canonical, &owned)? {
                    items.push(SweepItem::Session {
                        session_id: session.session_id.clone(),
                        tool: session.tool.clone(),
                        canonical_path: canonical,
                        external_session_id: session.external_session_id.clone(),
                        external_parent_session_id: session.external_parent_session_id.clone(),
                    });
                }
            }

            // Shared streams: resolve from each distinct path (e.g., Code vs Code Insiders
            // have separate globalStorage directories with independent OTEL DBs)
            let mut seen_shared_paths = HashSet::new();
            for session in &discovered {
                let canonical = Self::canonicalize_path(&session.stream_path);
                for stream in &shared {
                    let Some(item) = self.check_shared_stream(stream, &canonical, &session.tool)?
                    else {
                        continue;
                    };
                    let SweepItem::SharedStream {
                        canonical_path: ref p,
                        ..
                    } = item
                    else {
                        continue;
                    };
                    if seen_shared_paths.insert(p.clone()) {
                        items.push(item);
                    }
                }
            }
        }

        Ok(items)
    }

    /// Returns true if any owned (non-shared) stream file is new or has changed.
    fn any_stream_stale(
        &self,
        session: &DiscoveredSession,
        canonical_path: &Path,
        streams: &[StreamDescriptor],
    ) -> Result<bool, StreamError> {
        for stream in streams {
            let Some(path) = stream.resolve_path(canonical_path) else {
                continue;
            };
            if !path.exists() {
                continue;
            }

            let path_str = path.display().to_string();
            match self
                .streams_db
                .get_stream(&session.session_id, stream.stream_kind, &path_str)?
            {
                None => {
                    if !self.is_within_lookback(&path) {
                        continue;
                    }
                    return Ok(true);
                }
                Some(existing) => {
                    if Self::is_file_stale(&path, &existing)? {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    /// Check a single shared stream for processing.
    ///
    /// Shared streams (e.g., SQLite DBs in WAL mode) bypass file-metadata
    /// staleness checks because WAL writes don't update the main file's
    /// size/mtime. Instead, we always return them if the file exists —
    /// the watermark cursor inside the processing logic prevents re-processing.
    fn check_shared_stream(
        &self,
        stream: &StreamDescriptor,
        canonical_path: &Path,
        tool: &str,
    ) -> Result<Option<SweepItem>, StreamError> {
        let Some(path) = stream.resolve_path(canonical_path) else {
            return Ok(None);
        };
        if !path.exists() {
            return Ok(None);
        }

        Ok(Some(SweepItem::SharedStream {
            tool: tool.to_string(),
            stream_kind: stream.stream_kind.to_string(),
            canonical_path: path,
        }))
    }

    fn is_within_lookback(&self, path: &Path) -> bool {
        let Some(days) = self.lookback_days else {
            return true;
        };
        let cutoff =
            SystemTime::now() - std::time::Duration::from_secs(u64::from(days) * 24 * 60 * 60);
        match std::fs::metadata(path) {
            Ok(meta) => meta.modified().map(|mtime| mtime >= cutoff).unwrap_or(true),
            Err(_) => true,
        }
    }

    fn is_file_stale(path: &Path, existing: &StreamRecord) -> Result<bool, StreamError> {
        let metadata = std::fs::metadata(path).map_err(|e| StreamError::Transient {
            message: format!("failed to stat {}: {}", path.display(), e),
            retry_after: std::time::Duration::from_secs(5),
        })?;
        let file_size = metadata.len() as i64;
        let modified = Self::get_modified_timestamp(&metadata);
        Ok(file_size != existing.last_known_size
            || (modified.is_some() && modified != existing.last_modified))
    }

    fn canonicalize_path(path: &PathBuf) -> PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.clone())
    }

    fn get_modified_timestamp(metadata: &std::fs::Metadata) -> Option<i64> {
        metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streams::watermark::{WatermarkStrategy, ZCodeWatermark};

    #[test]
    fn zcode_sweep_only_polls_enrolled_sessions_including_wal_and_restart() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("zcode.sqlite");
        let source_conn = crate::sqlite::open_with_memory_limits(&source).unwrap();
        source_conn
            .execute_batch(
                "PRAGMA journal_mode=WAL; CREATE TABLE session(id TEXT);
            INSERT INTO session VALUES('historical-unenrolled');",
            )
            .unwrap();
        let tracking_path = dir.path().join("tracked.sqlite");
        let db = Arc::new(StreamsDatabase::open(&tracking_path).unwrap());
        let coordinator = SweepCoordinator {
            streams_db: db.clone(),
            agent_registry: vec![(
                "zcode".to_string(),
                Box::new(crate::streams::agents::ZCodeAgent::new()),
            )],
            lookback_days: Some(0),
        };
        assert!(
            coordinator.run_sweep().unwrap().is_empty(),
            "must not enroll source history"
        );
        let metadata = source.metadata().unwrap();
        for session in ["child-a", "child-b"] {
            db.insert_stream(&StreamRecord {
                session_id: session.to_string(),
                stream_kind: "transcript".to_string(),
                tool: "zcode".to_string(),
                stream_path: source.display().to_string(),
                stream_format: "ZCodeSqlite".to_string(),
                watermark_type: "ZCodeUsage".to_string(),
                watermark_value: ZCodeWatermark::at(100).serialize(),
                external_session_id: format!("external-{session}"),
                external_parent_session_id: Some("parent".to_string()),
                first_seen_at: 0,
                last_processed_at: 100,
                last_known_size: metadata.len() as i64,
                last_modified: SweepCoordinator::get_modified_timestamp(&metadata),
                processing_errors: 0,
                last_error: None,
                repo_work_dir: Some("/repo".to_string()),
            })
            .unwrap();
        }
        let first = coordinator.run_sweep().unwrap();
        assert_eq!(
            first.len(),
            2,
            "one shared DB still requires each enrolled session"
        );
        source_conn
            .execute("INSERT INTO session VALUES('late-write')", [])
            .unwrap();
        let reopened = Arc::new(StreamsDatabase::open(&tracking_path).unwrap());
        let recovered = SweepCoordinator {
            streams_db: reopened,
            agent_registry: vec![(
                "zcode".to_string(),
                Box::new(crate::streams::agents::ZCodeAgent::new()),
            )],
            lookback_days: Some(0),
        }
        .run_sweep()
        .unwrap();
        assert_eq!(
            recovered.len(),
            2,
            "poll registered sessions even if main DB mtime/size is unchanged"
        );
        for item in recovered {
            match item {
                SweepItem::Session {
                    tool,
                    external_session_id,
                    external_parent_session_id,
                    ..
                } => {
                    assert_eq!(tool, "zcode");
                    assert!(external_session_id.starts_with("external-child-"));
                    assert_eq!(external_parent_session_id.as_deref(), Some("parent"));
                }
                _ => panic!("ZCode uses per-session onboarding boundaries"),
            }
        }
    }
}
