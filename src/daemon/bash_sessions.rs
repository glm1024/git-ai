use crate::authorship::working_log::AgentId;
use crate::commands::checkpoint_agent::bash_tool::StatSnapshot;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const STALE_SESSION_SECS: u64 = 300;

#[derive(Clone)]
pub struct BashSession {
    pub repo_work_dir: String,
    pub agent_id: AgentId,
    pub metadata: HashMap<String, String>,
    pub stat_snapshot: StatSnapshot,
    pub start_trace_id: String,
    pub started_at_ns: u128,
    pub command: Option<String>,
    pub started_at: Instant,
}

pub struct BashSessionStart {
    pub session_id: String,
    pub tool_use_id: String,
    pub repo_work_dir: String,
    pub agent_id: AgentId,
    pub metadata: HashMap<String, String>,
    pub stat_snapshot: StatSnapshot,
    pub start_trace_id: String,
    pub started_at_ns: u128,
    pub command: Option<String>,
}

/// Identity-only view of a live, frozen pre snapshot. No file or command body.
pub(crate) struct ZCodeBashPre {
    pub has_snapshot: bool,
    pub tool_use_id: String,
    pub agent_id: AgentId,
    pub metadata: HashMap<String, String>,
    pub start_trace_id: String,
    pub started_at_ns: u128,
}

#[derive(Default)]
pub struct BashSessionState {
    sessions: HashMap<(String, String), BashSession>,
}

impl BashSessionState {
    pub fn new() -> Self {
        Self::default()
    }

    fn prune_stale_sessions(&mut self) {
        self.sessions
            .retain(|_, s| s.started_at.elapsed() < Duration::from_secs(STALE_SESSION_SECS));
    }

    pub fn start_session(&mut self, session: BashSessionStart) {
        self.prune_stale_sessions();
        self.sessions.insert(
            (session.session_id, session.tool_use_id),
            BashSession {
                repo_work_dir: session.repo_work_dir,
                agent_id: session.agent_id,
                metadata: session.metadata,
                stat_snapshot: session.stat_snapshot,
                start_trace_id: session.start_trace_id,
                started_at_ns: session.started_at_ns,
                command: session.command,
                started_at: Instant::now(),
            },
        );
    }

    pub(crate) fn zcode_pres_for_session(&self, repo: &str, session: &str) -> Vec<ZCodeBashPre> {
        self.sessions
            .iter()
            .filter(|((sid, call), pre)| {
                sid == session
                    && pre.agent_id.tool == "zcode"
                    && pre.agent_id.id == session
                    && pre.repo_work_dir == repo
                    && pre.stat_snapshot.repo_root == std::path::Path::new(repo)
                    && pre.stat_snapshot.invocation_key == format!("{sid}:{call}")
            })
            .take(65)
            .map(|((_, call), pre)| ZCodeBashPre {
                has_snapshot: true,
                tool_use_id: call.clone(),
                agent_id: pre.agent_id.clone(),
                metadata: pre.metadata.clone(),
                start_trace_id: pre.start_trace_id.clone(),
                started_at_ns: pre.started_at_ns,
            })
            .collect()
    }

    pub(crate) fn get_session(&self, session_id: &str, call: &str) -> Option<&BashSession> {
        self.sessions.get(&(session_id.into(), call.into()))
    }

    pub fn end_session(&mut self, session_id: &str, tool_use_id: &str) -> Option<BashSession> {
        self.sessions
            .remove(&(session_id.to_string(), tool_use_id.to_string()))
    }

    pub fn query_active_for_repo(
        &self,
        repo_work_dir: &str,
    ) -> Option<(&(String, String), &BashSession)> {
        self.sessions
            .iter()
            .find(|(_, s)| s.repo_work_dir == repo_work_dir)
    }

    pub fn get_snapshot(&self, session_id: &str, tool_use_id: &str) -> Option<&StatSnapshot> {
        self.sessions
            .get(&(session_id.to_string(), tool_use_id.to_string()))
            .map(|s| &s.stat_snapshot)
    }
}

#[cfg(test)]
mod zcode_tests {
    use super::*;
    #[test]
    fn zcode_live_pre_selection_excludes_other_sessions_worktrees_and_bad_snapshot_identity() {
        let mut state = BashSessionState::new();
        let make = |session: &str, call: &str, repo: &str| BashSessionStart {
            session_id: session.into(),
            tool_use_id: call.into(),
            repo_work_dir: repo.into(),
            agent_id: AgentId {
                tool: "zcode".into(),
                id: session.into(),
                model: "pre-model".into(),
            },
            metadata: HashMap::new(),
            start_trace_id: format!("trace-{call}"),
            started_at_ns: 1000,
            command: None,
            stat_snapshot: StatSnapshot {
                entries: HashMap::new(),
                taken_at: None,
                invocation_key: format!("{session}:{call}"),
                repo_root: repo.into(),
                head: None,
                effective_worktree_wm: None,
                per_file_wm: HashMap::new(),
            },
        };
        state.start_session(make("s", "shared-call", "/repo"));
        state.start_session(make("another", "shared-call", "/repo"));
        state.start_session(make("s", "other-worktree", "/other"));
        let mut bad = make("s", "bad-snapshot", "/repo");
        bad.stat_snapshot.invocation_key = "unrelated:call".into();
        state.start_session(bad);
        let selected = state.zcode_pres_for_session("/repo", "s");
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].tool_use_id, "shared-call");
        assert!(selected[0].has_snapshot);
        assert_eq!(selected[0].start_trace_id, "trace-shared-call");
    }
}
