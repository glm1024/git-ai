//! Crash-recoverable checkpoint side effects.
//!
//! A checkpoint spans two durability domains: the repository working log and
//! the metrics SQLite outbox.  They cannot share one transaction.  This queue
//! therefore persists the immutable request first, prepares the exact working
//! log checkpoint and metric JSON before either side effect is published, and
//! uses a stable request id to make the file publication replay-safe.  Metrics
//! insertion and the final `done` transition remain one SQLite transaction.

use crate::authorship::working_log::CheckpointKind;
use crate::commands::checkpoint_agent::orchestrator::CheckpointRequest;
use crate::daemon::checkpoint::{FrozenCheckpointMetricsContext, PreparedPathRole};
use crate::error::GitAiError;
use crate::git::repository::discover_repository_in_path_no_git_exec;
use crate::metrics::db::MetricsDatabase;
use crate::metrics::types::MetricEvent;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const DEFERRED_CHECKPOINT_JOBS_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS deferred_checkpoint_jobs (
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
    terminal_resolution TEXT NOT NULL DEFAULT 'normal'
        CHECK (terminal_resolution IN ('normal', 'manual_abandoned')),
    repair_id TEXT,
    repair_backup_path TEXT,
    metric_ids_json TEXT NOT NULL DEFAULT '[]',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    completed_at INTEGER
);

CREATE INDEX IF NOT EXISTS deferred_checkpoint_jobs_due
    ON deferred_checkpoint_jobs (state, next_retry_at, id)
    WHERE state != 'done' AND blocked_evidence = 0;

CREATE INDEX IF NOT EXISTS deferred_checkpoint_jobs_repo_order
    ON deferred_checkpoint_jobs (repo_identity, state, id);
"#;

pub(crate) const DEFERRED_CHECKPOINT_RECOVERY_INDEX_SQL: &str = r#"
DROP INDEX IF EXISTS deferred_checkpoint_jobs_due;
CREATE INDEX deferred_checkpoint_jobs_due
    ON deferred_checkpoint_jobs (state, next_retry_at, id)
    WHERE state != 'done' AND blocked_evidence = 0;
"#;

pub(crate) const JOB_TRACE_PREFIX: &str = "checkpoint-job:";
const PROCESSING_LEASE_SECS: u64 = 10 * 60;
const INITIAL_RETRY_BACKOFF_SECS: u64 = 1;
const MAX_RETRY_BACKOFF_SECS: u64 = 60;
const MAX_REQUEST_JSON_BYTES: usize = 64 * 1024 * 1024;
const MAX_PREPARED_JSON_BYTES: usize = 128 * 1024 * 1024;
const DONE_JOB_COMPACTION_BATCH_SIZE: usize = 100;

#[derive(Debug, Clone)]
pub(crate) struct DeferredCheckpointJobSpec {
    pub job_key: String,
    pub repo_identity: String,
    pub repository_workdir: String,
    pub integration: String,
    pub external_session_id: String,
    pub external_tool_use_id: String,
    pub phase: String,
    pub request_shape_sha256: String,
    pub request_evidence_sha256: String,
    pub request_json: String,
    pub metrics_context_json: String,
    pub path_scope_json: String,
    pub admission_owner: Option<String>,
    pub observed_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeferredCheckpointRecoveryRequest {
    pub job_key: String,
    pub repo_identity: String,
    pub repository_workdir: String,
    pub observed_at_ms: u64,
    pub working_log_applied: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlockedDeferredCheckpoint {
    pub job_key: String,
    pub repo_identity: String,
    pub reason: String,
}

/// Complete frozen evidence exported before an operator abandons an
/// unverifiable repository FIFO. Manual-repair rows are intentionally kept in
/// SQLite as well; this copy makes the recovery boundary independently visible
/// beside the archived working log.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ManualCheckpointRepairEvidenceRow {
    pub id: i64,
    pub job_key: String,
    pub repo_identity: String,
    pub repository_workdir: String,
    pub integration: String,
    pub external_session_id: String,
    pub external_tool_use_id: String,
    pub phase: String,
    pub request_shape_sha256: String,
    pub request_evidence_sha256: String,
    pub request_json: String,
    pub metrics_context_json: String,
    pub path_scope_json: Option<String>,
    pub admission_owner: Option<String>,
    pub observed_at_ms: i64,
    pub prepared_checkpoint_json: Option<String>,
    pub prepared_metric_events_json: Option<String>,
    pub working_log_applied: i64,
    pub state: String,
    pub attempts: i64,
    pub next_retry_at: i64,
    pub processing_started_at: Option<i64>,
    pub lease_token: Option<String>,
    pub last_error: Option<String>,
    pub blocked_evidence: i64,
    pub blocked_reason: Option<String>,
    pub metric_ids_json: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub completed_at: Option<i64>,
    pub terminal_resolution: String,
    pub repair_id: Option<String>,
    pub repair_backup_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ManualCheckpointRepairPlan {
    pub repair_id: String,
    pub target_job_key: String,
    pub repo_identity: String,
    pub repository_workdir: String,
    pub base_commit: String,
    pub original_block_reason: String,
    pub affected_jobs: Vec<ManualCheckpointRepairEvidenceRow>,
    pub repair_backup_path: Option<String>,
    pub already_terminal: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeferredCheckpointJobExecution<'a> {
    Live {
        admission_owner: &'a str,
    },
    Recovery {
        preflight_evidence_error: Option<&'a str>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ClaimedDeferredCheckpointJob {
    #[allow(dead_code)]
    pub id: i64,
    pub job_key: String,
    pub lease_token: String,
    pub repo_identity: String,
    pub repository_workdir: String,
    pub request_json: String,
    pub metrics_context_json: String,
    pub observed_at_ms: u64,
    pub prepared_checkpoint_json: Option<String>,
    pub prepared_metric_events_json: Option<String>,
    pub working_log_applied: bool,
    pub attempts: u32,
}

pub(crate) struct AgentUsageCandidate {
    pub prompt_id: String,
    pub min_interval_secs: u64,
    pub observed_at_secs: u64,
    pub event: MetricEvent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeferredCheckpointJobStatus {
    Pending,
    Processing,
    Done,
    Blocked(String),
    ManuallyAbandoned(String),
}

type DeferredCheckpointJobStatusRow = (
    String,
    bool,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum DeferredCheckpointPathScope {
    Files { paths: Vec<String> },
    BashWildcard,
}

impl DeferredCheckpointJobSpec {
    pub(crate) fn from_request(
        request: &mut CheckpointRequest,
    ) -> Result<Option<Self>, GitAiError> {
        if !is_durable_checkpoint_request(request) {
            return Ok(None);
        }
        let first_file = request.files.first().ok_or_else(|| {
            GitAiError::Generic("durable checkpoint request has no files".to_string())
        })?;
        let repo = discover_repository_in_path_no_git_exec(&first_file.repo_work_dir)?;
        let repository_workdir = repo.workdir()?;
        let repo_identity = repository_identity(repo.common_dir());
        let path_scope = checkpoint_path_scope(request, &repo_identity)?;
        let metrics_context = FrozenCheckpointMetricsContext::capture(&repo);
        let observed_at_ms = unix_now_millis();
        let agent = request.agent_id.as_ref().ok_or_else(|| {
            GitAiError::Generic("durable checkpoint request has no agent identity".to_string())
        })?;
        let external_tool_use_id = request
            .metadata
            .get("tool_use_id")
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                GitAiError::Generic(
                    "durable checkpoint request has no external tool-use id".to_string(),
                )
            })?
            .to_string();
        let integration = request
            .metadata
            .get("integration")
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .unwrap_or(agent.tool.as_str())
            .to_string();
        let phase = checkpoint_phase(request).to_string();
        let job_key = stable_job_key(
            &repo_identity,
            &integration,
            &agent.id,
            &external_tool_use_id,
            &phase,
        );
        request.trace_id = format!("{JOB_TRACE_PREFIX}{job_key}");

        let request_shape_sha256 = request_shape_digest(request)?;
        let request_evidence_sha256 =
            request_evidence_digest(request, &metrics_context, observed_at_ms)?;
        let request_json = serde_json::to_string(request)?;
        let metrics_context_json = serde_json::to_string(&metrics_context)?;
        let path_scope_json = serde_json::to_string(&path_scope)?;
        if request_json.len() > MAX_REQUEST_JSON_BYTES {
            return Err(GitAiError::Generic(format!(
                "durable checkpoint request is {} bytes; limit is {} bytes",
                request_json.len(),
                MAX_REQUEST_JSON_BYTES
            )));
        }

        Ok(Some(Self {
            job_key,
            repo_identity,
            repository_workdir: repository_workdir.to_string_lossy().to_string(),
            integration,
            external_session_id: agent.id.clone(),
            external_tool_use_id,
            phase,
            request_shape_sha256,
            request_evidence_sha256,
            request_json,
            metrics_context_json,
            path_scope_json,
            admission_owner: None,
            observed_at_ms,
        }))
    }
}

pub(crate) fn is_durable_checkpoint_request(request: &CheckpointRequest) -> bool {
    let Some(agent) = request.agent_id.as_ref() else {
        return false;
    };
    matches!(agent.tool.as_str(), "kilo" | "opencode")
        && !agent.id.trim().is_empty()
        && request
            .metadata
            .get("tool_use_id")
            .is_some_and(|value| !value.trim().is_empty())
        && !request.files.is_empty()
}

pub(crate) fn job_key_from_trace_id(trace_id: &str) -> Option<&str> {
    trace_id
        .strip_prefix(JOB_TRACE_PREFIX)
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn checkpoint_phase(request: &CheckpointRequest) -> &'static str {
    match (request.path_role, request.checkpoint_kind) {
        (PreparedPathRole::WillEdit, CheckpointKind::Human) => "pre",
        (PreparedPathRole::Edited, CheckpointKind::AiAgent | CheckpointKind::AiTab) => "post",
        (PreparedPathRole::Edited, CheckpointKind::KnownHuman) => "known_human",
        (PreparedPathRole::WillEdit, _) => "will_edit",
        (PreparedPathRole::Edited, _) => "edited",
    }
}

fn request_shape_digest(request: &CheckpointRequest) -> Result<String, GitAiError> {
    // `trace_id` has already been replaced with the deterministic job trace.
    // Hash the complete request so every field that can affect checkpoint or
    // metric derivation (path role, stream source, model/version, metadata,
    // file order/content/base) participates. Object keys are sorted because
    // HashMap iteration order is not semantic; array order remains significant.
    let value = serde_json::to_value(request)?;
    let mut hasher = Sha256::new();
    hash_canonical_json(&value, &mut hasher);
    Ok(format!("{:x}", hasher.finalize()))
}

fn request_evidence_digest(
    request: &CheckpointRequest,
    metrics_context: &FrozenCheckpointMetricsContext,
    observed_at_ms: u64,
) -> Result<String, GitAiError> {
    let value = serde_json::json!({
        "request": request,
        "metrics_context": metrics_context,
        "observed_at_ms": observed_at_ms,
    });
    let mut hasher = Sha256::new();
    hash_canonical_json(&value, &mut hasher);
    Ok(format!("{:x}", hasher.finalize()))
}

fn hash_canonical_json(value: &Value, hasher: &mut Sha256) {
    match value {
        Value::Null => hasher.update(b"n"),
        Value::Bool(value) => hasher.update(if *value { b"t" } else { b"f" }),
        Value::Number(value) => {
            hasher.update(b"d");
            hasher.update(value.to_string().as_bytes());
            hasher.update(b";");
        }
        Value::String(value) => {
            hasher.update(b"s");
            hasher.update((value.len() as u64).to_be_bytes());
            hasher.update(value.as_bytes());
        }
        Value::Array(values) => {
            hasher.update(b"[");
            hasher.update((values.len() as u64).to_be_bytes());
            for value in values {
                hash_canonical_json(value, hasher);
            }
            hasher.update(b"]");
        }
        Value::Object(values) => {
            hasher.update(b"{");
            hasher.update((values.len() as u64).to_be_bytes());
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort();
            for key in keys {
                hasher.update((key.len() as u64).to_be_bytes());
                hasher.update(key.as_bytes());
                hash_canonical_json(&values[key], hasher);
            }
            hasher.update(b"}");
        }
    }
}

fn stable_job_key(
    repo_identity: &str,
    integration: &str,
    session_id: &str,
    tool_use_id: &str,
    phase: &str,
) -> String {
    let mut hasher = Sha256::new();
    for value in [repo_identity, integration, session_id, tool_use_id, phase] {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

pub(crate) fn repository_identity(git_common_dir: &Path) -> String {
    let stable = git_common_dir
        .canonicalize()
        .unwrap_or_else(|_| git_common_dir.to_path_buf());
    sha256_hex(stable.to_string_lossy().as_bytes())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn checkpoint_path_scope(
    request: &CheckpointRequest,
    expected_repo_identity: &str,
) -> Result<DeferredCheckpointPathScope, GitAiError> {
    if checkpoint_phase(request) == "pre"
        && request.metadata.get("edit_kind").map(String::as_str) == Some("bash")
    {
        return Ok(DeferredCheckpointPathScope::BashWildcard);
    }

    let mut paths = BTreeSet::new();
    for file in &request.files {
        let repo = discover_repository_in_path_no_git_exec(&file.repo_work_dir)?;
        if repository_identity(repo.common_dir()) != expected_repo_identity {
            return Err(GitAiError::Generic(
                "durable checkpoint path scope spans multiple repositories".to_string(),
            ));
        }
        let workdir = repo.workdir()?;
        paths.insert(normalize_repo_relative_scope_path(&workdir, &file.path)?);
    }
    if paths.is_empty() {
        return Err(GitAiError::Generic(
            "durable checkpoint path scope is empty".to_string(),
        ));
    }
    Ok(DeferredCheckpointPathScope::Files {
        paths: paths.into_iter().collect(),
    })
}

fn normalize_repo_relative_scope_path(
    repo_workdir: &Path,
    file_path: &Path,
) -> Result<String, GitAiError> {
    let relative = if file_path.is_absolute() {
        let canonical_workdir = repo_workdir
            .canonicalize()
            .unwrap_or_else(|_| repo_workdir.to_path_buf());
        if let Ok(canonical_file) = file_path.canonicalize() {
            canonical_file
                .strip_prefix(&canonical_workdir)
                .map(PathBuf::from)
                .map_err(|_| {
                    GitAiError::Generic(format!(
                        "durable checkpoint path is outside repository: {}",
                        file_path.display()
                    ))
                })?
        } else if let Ok(relative) = file_path.strip_prefix(repo_workdir) {
            // Deleted post-edit targets cannot be canonicalized. Their lexical
            // repository-relative scope is still verifiable against the
            // authoritative pre scope below.
            relative.to_path_buf()
        } else {
            return Err(GitAiError::Generic(format!(
                "durable checkpoint path is outside repository: {}",
                file_path.display()
            )));
        }
    } else {
        file_path.to_path_buf()
    };

    let mut normalized = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(GitAiError::Generic(format!(
                        "durable checkpoint path escapes repository: {}",
                        file_path.display()
                    )));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(GitAiError::Generic(format!(
                    "durable checkpoint path is not repository-relative: {}",
                    file_path.display()
                )));
            }
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(GitAiError::Generic(format!(
            "durable checkpoint path does not identify a file: {}",
            file_path.display()
        )));
    }
    Ok(crate::utils::normalize_to_posix(
        &normalized.to_string_lossy(),
    ))
}

pub(crate) fn enqueue_request(
    request: &mut CheckpointRequest,
    admission_owner: &str,
) -> Result<Option<String>, GitAiError> {
    let Some(mut spec) = DeferredCheckpointJobSpec::from_request(request)? else {
        return Ok(None);
    };
    spec.admission_owner = Some(admission_owner.to_string());
    let job_key = spec.job_key.clone();
    let db = MetricsDatabase::global()?;
    let mut db = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    enqueue_on_connection(db.deferred_jobs_connection(), &spec, unix_now())?;
    Ok(Some(job_key))
}

pub(crate) fn count_outstanding() -> Result<usize, GitAiError> {
    let db = MetricsDatabase::global()?;
    let mut db = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    count_outstanding_on_connection(db.deferred_jobs_connection())
}

/// Process one durable row after it has entered the owning repository-family
/// sequencer. This is intentionally the only global entry point that claims a
/// checkpoint job or publishes either checkpoint durability domain.
pub(crate) fn process_specific_job(
    job_key: &str,
    execution: DeferredCheckpointJobExecution<'_>,
) -> Result<(), GitAiError> {
    // Recovery may have to use a synthetic family when the frozen repository
    // path disappears. Keep serialization stable by durable job identity so
    // that route changes cannot overtake a live execution in this daemon.
    let execution_lock = checkpoint_job_execution_lock(job_key)?;
    let _execution_guard = execution_lock
        .lock()
        .map_err(|_| GitAiError::Generic("checkpoint job execution lock poisoned".to_string()))?;
    // A client can retry after losing the ACK while the original daemon call
    // is still completing. Briefly observe that durable row instead of
    // immediately turning successful in-flight work into a false hard failure.
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
    loop {
        match status_global(job_key)? {
            DeferredCheckpointJobStatus::Done => return Ok(()),
            DeferredCheckpointJobStatus::Blocked(reason) => {
                return Err(GitAiError::EvidenceError(format!(
                    "durable checkpoint {job_key} is blocked: {reason}"
                )));
            }
            DeferredCheckpointJobStatus::ManuallyAbandoned(reason) => {
                return Err(GitAiError::EvidenceError(format!(
                    "durable checkpoint {job_key} was manually abandoned after evidence backup: {reason}"
                )));
            }
            DeferredCheckpointJobStatus::Pending | DeferredCheckpointJobStatus::Processing => {
                let bypass_pending_backoff =
                    matches!(execution, DeferredCheckpointJobExecution::Live { .. });
                if let Some(job) =
                    claim_specific_global(job_key, unix_now(), bypass_pending_backoff)?
                {
                    if let DeferredCheckpointJobExecution::Recovery {
                        preflight_evidence_error: Some(reason),
                    } = execution
                        && !job.working_log_applied
                    {
                        let error = GitAiError::EvidenceError(format!(
                            "durable checkpoint {job_key} cannot be replayed safely because {reason}; original evidence is preserved. Stop the background service, then run `git-ai repair checkpoint-baseline --job-key {job_key}` to preview the evidence backup and FIFO reset"
                        ));
                        if let Err(mark_error) =
                            mark_blocked_global(&job, &error.to_string(), unix_now())
                        {
                            tracing::error!(
                                %job_key,
                                %mark_error,
                                "durable checkpoint: failed to persist recovery preflight evidence failure"
                            );
                        }
                        return Err(error);
                    }
                    return process_claimed_job(
                        job,
                        matches!(execution, DeferredCheckpointJobExecution::Recovery { .. }),
                    );
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            if let DeferredCheckpointJobExecution::Live { admission_owner } = execution {
                // A live admission owns the row only until its family entry has
                // had a chance to claim it. If an older FIFO row or active
                // lease prevents that claim, release ownership so the periodic
                // actor recovery pass can resume it later.
                release_admission_owner(job_key, admission_owner)?;
            }
            return Err(GitAiError::Generic(format!(
                "durable checkpoint job {job_key} is safely persisted but still waiting for an earlier checkpoint or active lease"
            )));
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

fn process_claimed_job(
    job: ClaimedDeferredCheckpointJob,
    validate_recovery_base: bool,
) -> Result<(), GitAiError> {
    let _lease_heartbeat = ProcessingLeaseHeartbeat::start(&job);
    let result =
        crate::daemon::process_claimed_durable_checkpoint_job(&job, validate_recovery_base)
            .map_err(|error| checkpoint_repair_guidance(&job.job_key, error));
    if let Err(error) = &result {
        let persist_result = if matches!(error, GitAiError::EvidenceError(_)) {
            mark_blocked_global(&job, &error.to_string(), unix_now())
        } else {
            mark_failed_global(&job, &error.to_string(), unix_now())
        };
        if let Err(mark_error) = persist_result {
            tracing::error!(
                job_key = %job.job_key,
                %mark_error,
                "durable checkpoint: failed to persist failure state"
            );
        }
    }
    result
}

fn checkpoint_repair_guidance(job_key: &str, error: GitAiError) -> GitAiError {
    match error {
        GitAiError::EvidenceError(reason)
            if reason.contains("INITIAL")
                && !reason.contains("git-ai repair checkpoint-baseline") =>
        {
            GitAiError::EvidenceError(format!(
                "{reason}; original evidence is preserved. Stop the background service, then run `git-ai repair checkpoint-baseline --job-key {job_key}` for a two-step impact preview"
            ))
        }
        other => other,
    }
}

pub(crate) fn checkpoint_job_execution_lock(
    job_key: &str,
) -> Result<std::sync::Arc<std::sync::Mutex<()>>, GitAiError> {
    type JobLockRegistry = std::collections::HashMap<String, std::sync::Weak<std::sync::Mutex<()>>>;
    static JOB_LOCKS: std::sync::OnceLock<std::sync::Mutex<JobLockRegistry>> =
        std::sync::OnceLock::new();

    let mut locks = JOB_LOCKS
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
        .lock()
        .map_err(|_| GitAiError::Generic("checkpoint job lock registry poisoned".to_string()))?;
    locks.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = locks.get(job_key).and_then(std::sync::Weak::upgrade) {
        return Ok(lock);
    }
    let lock = std::sync::Arc::new(std::sync::Mutex::new(()));
    locks.insert(job_key.to_string(), std::sync::Arc::downgrade(&lock));
    Ok(lock)
}

struct ProcessingLeaseHeartbeat {
    stop: Option<std::sync::mpsc::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl ProcessingLeaseHeartbeat {
    fn start(job: &ClaimedDeferredCheckpointJob) -> Self {
        let job_key = job.job_key.clone();
        let lease_token = job.lease_token.clone();
        let (stop, stopped) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let interval = std::time::Duration::from_secs((PROCESSING_LEASE_SECS / 3).max(1));
            loop {
                match stopped.recv_timeout(interval) {
                    Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        match renew_processing_lease_global_for(&job_key, &lease_token, unix_now())
                        {
                            Ok(true) => {}
                            Ok(false) => {
                                tracing::warn!(
                                    %job_key,
                                    "durable checkpoint: stopped heartbeat after losing processing lease"
                                );
                                break;
                            }
                            Err(error) => {
                                tracing::warn!(
                                    %error,
                                    %job_key,
                                    "durable checkpoint: processing lease heartbeat failed"
                                );
                            }
                        }
                    }
                }
            }
        });
        Self {
            stop: Some(stop),
            worker: Some(worker),
        }
    }
}

impl Drop for ProcessingLeaseHeartbeat {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(crate) fn compact_done_jobs() {
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
        tracing::warn!(%error, "durable checkpoint: failed to compact completed jobs");
    }
}

pub(crate) fn due_recovery_requests(
    limit: usize,
    recovery_owner: &str,
) -> Result<Vec<DeferredCheckpointRecoveryRequest>, GitAiError> {
    let db = MetricsDatabase::global()?;
    let mut db = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    due_recovery_requests_on_connection(
        db.deferred_jobs_connection(),
        unix_now(),
        PROCESSING_LEASE_SECS,
        limit,
        recovery_owner,
    )
}

pub(crate) fn blocked_jobs() -> Result<Vec<BlockedDeferredCheckpoint>, GitAiError> {
    let db = MetricsDatabase::global()?;
    let mut db = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    blocked_jobs_on_connection(db.deferred_jobs_connection())
}

pub(crate) fn manual_repair_plan_global(
    target_job_key: &str,
) -> Result<ManualCheckpointRepairPlan, GitAiError> {
    let db = MetricsDatabase::global()?;
    let mut db = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    manual_repair_plan_on_connection(db.deferred_jobs_connection(), target_job_key)
}

pub(crate) fn manually_abandon_repo_fifo_global(
    plan: &ManualCheckpointRepairPlan,
    repair_backup_path: &str,
) -> Result<usize, GitAiError> {
    let db = MetricsDatabase::global()?;
    let mut db = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    manually_abandon_repo_fifo_on_connection(
        db.deferred_jobs_connection(),
        plan,
        repair_backup_path,
        unix_now(),
    )
}

fn claim_specific_global(
    job_key: &str,
    now: u64,
    bypass_pending_backoff: bool,
) -> Result<Option<ClaimedDeferredCheckpointJob>, GitAiError> {
    let db = MetricsDatabase::global()?;
    let mut db = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    claim_specific_with_backoff_policy_on_connection(
        db.deferred_jobs_connection(),
        job_key,
        now,
        PROCESSING_LEASE_SECS,
        bypass_pending_backoff,
    )
}

fn status_global(job_key: &str) -> Result<DeferredCheckpointJobStatus, GitAiError> {
    let db = MetricsDatabase::global()?;
    let mut db = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    status_on_connection(db.deferred_jobs_connection(), job_key)?.ok_or_else(|| {
        GitAiError::Generic(format!("durable checkpoint job {job_key} does not exist"))
    })
}

pub(crate) fn release_admission_owner(
    job_key: &str,
    admission_owner: &str,
) -> Result<bool, GitAiError> {
    let db = MetricsDatabase::global()?;
    let mut db = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    release_admission_owner_on_connection(
        db.deferred_jobs_connection(),
        job_key,
        admission_owner,
        unix_now(),
    )
}

pub(crate) fn persist_prepared_global(
    job: &ClaimedDeferredCheckpointJob,
    prepared_checkpoint_json: &str,
    metric_events: &[MetricEvent],
    agent_usage: Option<&AgentUsageCandidate>,
    now: u64,
) -> Result<bool, GitAiError> {
    let metric_event_jsons = metric_events
        .iter()
        .map(serde_json::to_string)
        .collect::<Result<Vec<_>, _>>()?;
    let db = MetricsDatabase::global()?;
    let mut db = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    persist_prepared_on_connection(
        db.deferred_jobs_connection(),
        job,
        prepared_checkpoint_json,
        &metric_event_jsons,
        agent_usage,
        now,
    )
}

pub(crate) fn mark_working_log_applied_global(
    job: &ClaimedDeferredCheckpointJob,
    now: u64,
) -> Result<bool, GitAiError> {
    let db = MetricsDatabase::global()?;
    let mut db = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    mark_working_log_applied_on_connection(db.deferred_jobs_connection(), job, now)
}

pub(crate) fn renew_processing_lease_global(
    job: &ClaimedDeferredCheckpointJob,
    now: u64,
) -> Result<bool, GitAiError> {
    renew_processing_lease_global_for(&job.job_key, &job.lease_token, now)
}

fn renew_processing_lease_global_for(
    job_key: &str,
    lease_token: &str,
    now: u64,
) -> Result<bool, GitAiError> {
    let db = MetricsDatabase::global()?;
    let mut db = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    renew_processing_lease_on_connection(db.deferred_jobs_connection(), job_key, lease_token, now)
}

pub(crate) fn complete_global(
    job: &ClaimedDeferredCheckpointJob,
    now: u64,
) -> Result<bool, GitAiError> {
    let db = MetricsDatabase::global()?;
    let mut db = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    complete_on_connection(db.deferred_jobs_connection(), job, now)
}

fn mark_failed_global(
    job: &ClaimedDeferredCheckpointJob,
    error: &str,
    now: u64,
) -> Result<bool, GitAiError> {
    let db = MetricsDatabase::global()?;
    let mut db = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    mark_failed_on_connection(db.deferred_jobs_connection(), job, error, now)
}

fn mark_blocked_global(
    job: &ClaimedDeferredCheckpointJob,
    reason: &str,
    now: u64,
) -> Result<bool, GitAiError> {
    let db = MetricsDatabase::global()?;
    let mut db = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    mark_blocked_on_connection(db.deferred_jobs_connection(), job, reason, now)
}

pub(crate) fn enqueue_on_connection(
    conn: &mut Connection,
    spec: &DeferredCheckpointJobSpec,
    now: u64,
) -> Result<bool, GitAiError> {
    let tx = conn.transaction()?;
    if spec.phase == "post" {
        // Admission may run ahead of family side effects. A pending or
        // processing pre is sufficient evidence to admit its post because the
        // repository FIFO in claim_on_connection prevents that post from ever
        // executing until every prior row is done. If the pre becomes evidence
        // blocked, the post remains blocked behind it as well.
        let pre_scope_json: Option<Option<String>> = tx
            .query_row(
                r#"
            SELECT path_scope_json
            FROM deferred_checkpoint_jobs
            WHERE repo_identity = ?1
              AND integration = ?2
              AND external_session_id = ?3
              AND external_tool_use_id = ?4
              AND phase = 'pre'
              AND blocked_evidence = 0
            ORDER BY id DESC
            LIMIT 1
            "#,
                params![
                    spec.repo_identity,
                    spec.integration,
                    spec.external_session_id,
                    spec.external_tool_use_id,
                ],
                |row| row.get(0),
            )
            .optional()?;
        let Some(pre_scope_json) = pre_scope_json else {
            return Err(GitAiError::Generic(format!(
                "durable post checkpoint {} has no admitted pre checkpoint for the same repository/session/call",
                spec.job_key
            )));
        };
        let pre_scope_json = pre_scope_json.ok_or_else(|| {
            GitAiError::Generic(format!(
                "durable post checkpoint {} cannot use a legacy completed pre checkpoint without preserved path scope; refusing AI attribution",
                spec.job_key
            ))
        })?;
        validate_post_scope(&pre_scope_json, &spec.path_scope_json, &spec.job_key)?;
    }

    let inserted = tx.execute(
        r#"
        INSERT INTO deferred_checkpoint_jobs (
            job_key, repo_identity, repository_workdir, integration,
            external_session_id, external_tool_use_id, phase,
            request_shape_sha256, request_evidence_sha256, request_json,
            metrics_context_json, path_scope_json, observed_at_ms, state,
            admission_owner, attempts, next_retry_at, created_at, updated_at
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
            ?13, 'pending', ?14, 0, 0, ?15, ?15
        )
        ON CONFLICT(job_key) DO NOTHING
        "#,
        params![
            spec.job_key,
            spec.repo_identity,
            spec.repository_workdir,
            spec.integration,
            spec.external_session_id,
            spec.external_tool_use_id,
            spec.phase,
            spec.request_shape_sha256,
            spec.request_evidence_sha256,
            spec.request_json,
            spec.metrics_context_json,
            spec.path_scope_json,
            u64_to_sqlite(spec.observed_at_ms),
            spec.admission_owner,
            u64_to_sqlite(now),
        ],
    )?;
    if inserted == 1 {
        tx.commit()?;
        return Ok(true);
    }

    let existing_shape: String = tx.query_row(
        "SELECT request_shape_sha256 FROM deferred_checkpoint_jobs WHERE job_key = ?1",
        params![spec.job_key],
        |row| row.get(0),
    )?;
    if existing_shape != spec.request_shape_sha256 {
        return Err(GitAiError::Generic(format!(
            "durable checkpoint identity collision for {}: the same session/call/phase has different file evidence",
            spec.job_key
        )));
    }
    if let Some(admission_owner) = spec.admission_owner.as_deref() {
        // A same-shape live retry atomically takes admission ownership of a
        // pending row. This closes the enqueue-to-family-entry window even for
        // rows created by an older daemon or left in retry backoff.
        tx.execute(
            r#"
            UPDATE deferred_checkpoint_jobs
            SET admission_owner = ?2,
                updated_at = ?3
            WHERE job_key = ?1
              AND state = 'pending'
              AND blocked_evidence = 0
            "#,
            params![spec.job_key, admission_owner, u64_to_sqlite(now)],
        )?;
    }
    tx.commit()?;
    Ok(false)
}

fn validate_post_scope(
    pre_scope_json: &str,
    post_scope_json: &str,
    post_job_key: &str,
) -> Result<(), GitAiError> {
    let pre_scope: DeferredCheckpointPathScope =
        serde_json::from_str(pre_scope_json).map_err(|error| {
            GitAiError::Generic(format!(
                "durable post checkpoint {post_job_key} cannot verify preserved pre path scope: {error}"
            ))
        })?;
    let post_scope: DeferredCheckpointPathScope =
        serde_json::from_str(post_scope_json).map_err(|error| {
            GitAiError::Generic(format!(
                "durable post checkpoint {post_job_key} has invalid path scope: {error}"
            ))
        })?;

    let DeferredCheckpointPathScope::Files { paths: post_paths } = post_scope else {
        return Err(GitAiError::Generic(format!(
            "durable post checkpoint {post_job_key} cannot use wildcard path scope"
        )));
    };
    match pre_scope {
        DeferredCheckpointPathScope::BashWildcard => Ok(()),
        DeferredCheckpointPathScope::Files { paths: pre_paths } => {
            let pre_paths = pre_paths.into_iter().collect::<BTreeSet<_>>();
            let unexpected = post_paths
                .into_iter()
                .filter(|path| !pre_paths.contains(path))
                .collect::<Vec<_>>();
            if unexpected.is_empty() {
                Ok(())
            } else {
                Err(GitAiError::Generic(format!(
                    "durable post checkpoint {post_job_key} expands beyond its completed pre checkpoint path scope: {}",
                    unexpected.join(", ")
                )))
            }
        }
    }
}

pub(crate) fn due_recovery_requests_on_connection(
    conn: &mut Connection,
    now: u64,
    lease_secs: u64,
    limit: usize,
    recovery_owner: &str,
) -> Result<Vec<DeferredCheckpointRecoveryRequest>, GitAiError> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let expired_before = now.saturating_sub(lease_secs);
    let mut stmt = conn.prepare(
        r#"
        SELECT candidate.job_key, candidate.repo_identity,
               candidate.repository_workdir, candidate.observed_at_ms,
               candidate.working_log_applied
        FROM deferred_checkpoint_jobs candidate
        WHERE candidate.blocked_evidence = 0
          AND (candidate.admission_owner IS NULL OR candidate.admission_owner != ?4)
          AND (
              (candidate.state = 'pending' AND candidate.next_retry_at <= ?1)
              OR (candidate.state = 'processing' AND candidate.processing_started_at <= ?2)
          )
          AND NOT EXISTS (
              SELECT 1
              FROM deferred_checkpoint_jobs prior
              WHERE prior.repo_identity = candidate.repo_identity
                AND prior.state != 'done'
                AND prior.id < candidate.id
          )
        ORDER BY candidate.id ASC
        LIMIT ?3
        "#,
    )?;
    let rows = stmt.query_map(
        params![
            u64_to_sqlite(now),
            u64_to_sqlite(expired_before),
            i64::try_from(limit).unwrap_or(i64::MAX),
            recovery_owner,
        ],
        |row| {
            Ok(DeferredCheckpointRecoveryRequest {
                job_key: row.get(0)?,
                repo_identity: row.get(1)?,
                repository_workdir: row.get(2)?,
                observed_at_ms: row.get::<_, i64>(3)?.max(0) as u64,
                working_log_applied: row.get::<_, i64>(4)? != 0,
            })
        },
    )?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(GitAiError::from)
}

pub(crate) fn blocked_jobs_on_connection(
    conn: &mut Connection,
) -> Result<Vec<BlockedDeferredCheckpoint>, GitAiError> {
    let mut stmt = conn.prepare(
        r#"
        SELECT job_key, repo_identity,
               COALESCE(blocked_reason, last_error, 'unknown evidence failure')
        FROM deferred_checkpoint_jobs
        WHERE state != 'done' AND blocked_evidence = 1
        ORDER BY id ASC
        "#,
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(BlockedDeferredCheckpoint {
            job_key: row.get(0)?,
            repo_identity: row.get(1)?,
            reason: row.get(2)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(GitAiError::from)
}

const MANUAL_REPAIR_EVIDENCE_COLUMNS: &str = r#"
    id, job_key, repo_identity, repository_workdir, integration,
    external_session_id, external_tool_use_id, phase,
    request_shape_sha256, request_evidence_sha256, request_json,
    metrics_context_json, path_scope_json, admission_owner, observed_at_ms,
    prepared_checkpoint_json, prepared_metric_events_json,
    working_log_applied, state, attempts, next_retry_at,
    processing_started_at, lease_token, last_error, blocked_evidence,
    blocked_reason, metric_ids_json, created_at, updated_at, completed_at,
    terminal_resolution, repair_id, repair_backup_path
"#;

fn manual_repair_evidence_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<ManualCheckpointRepairEvidenceRow> {
    Ok(ManualCheckpointRepairEvidenceRow {
        id: row.get(0)?,
        job_key: row.get(1)?,
        repo_identity: row.get(2)?,
        repository_workdir: row.get(3)?,
        integration: row.get(4)?,
        external_session_id: row.get(5)?,
        external_tool_use_id: row.get(6)?,
        phase: row.get(7)?,
        request_shape_sha256: row.get(8)?,
        request_evidence_sha256: row.get(9)?,
        request_json: row.get(10)?,
        metrics_context_json: row.get(11)?,
        path_scope_json: row.get(12)?,
        admission_owner: row.get(13)?,
        observed_at_ms: row.get(14)?,
        prepared_checkpoint_json: row.get(15)?,
        prepared_metric_events_json: row.get(16)?,
        working_log_applied: row.get(17)?,
        state: row.get(18)?,
        attempts: row.get(19)?,
        next_retry_at: row.get(20)?,
        processing_started_at: row.get(21)?,
        lease_token: row.get(22)?,
        last_error: row.get(23)?,
        blocked_evidence: row.get(24)?,
        blocked_reason: row.get(25)?,
        metric_ids_json: row.get(26)?,
        created_at: row.get(27)?,
        updated_at: row.get(28)?,
        completed_at: row.get(29)?,
        terminal_resolution: row.get(30)?,
        repair_id: row.get(31)?,
        repair_backup_path: row.get(32)?,
    })
}

fn manual_repair_rows_where(
    conn: &Connection,
    predicate: &str,
    value: &str,
) -> Result<Vec<ManualCheckpointRepairEvidenceRow>, GitAiError> {
    let sql = format!(
        "SELECT {MANUAL_REPAIR_EVIDENCE_COLUMNS} FROM deferred_checkpoint_jobs WHERE {predicate} ORDER BY id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![value], manual_repair_evidence_row)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(GitAiError::from)
}

fn manual_repair_base_commit(
    target: &ManualCheckpointRepairEvidenceRow,
) -> Result<String, GitAiError> {
    let request: CheckpointRequest =
        serde_json::from_str(&target.request_json).map_err(|error| {
            GitAiError::EvidenceError(format!(
                "blocked checkpoint {} has unreadable frozen request evidence: {error}",
                target.job_key
            ))
        })?;
    let first = request.files.first().ok_or_else(|| {
        GitAiError::EvidenceError(format!(
            "blocked checkpoint {} has no frozen file evidence",
            target.job_key
        ))
    })?;
    let base_commit = match &first.base_commit {
        crate::commands::checkpoint_agent::orchestrator::BaseCommit::Sha(value) => {
            if !matches!(value.len(), 40 | 64)
                || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(GitAiError::EvidenceError(format!(
                    "blocked checkpoint {} has an invalid frozen base object id",
                    target.job_key
                )));
            }
            value.clone()
        }
        crate::commands::checkpoint_agent::orchestrator::BaseCommit::Initial => {
            "initial".to_string()
        }
    };
    for file in &request.files {
        let file_base = match &file.base_commit {
            crate::commands::checkpoint_agent::orchestrator::BaseCommit::Sha(value) => {
                value.as_str()
            }
            crate::commands::checkpoint_agent::orchestrator::BaseCommit::Initial => "initial",
        };
        if file_base != base_commit {
            return Err(GitAiError::EvidenceError(format!(
                "blocked checkpoint {} spans multiple frozen baselines",
                target.job_key
            )));
        }
    }
    Ok(base_commit)
}

pub(crate) fn manual_repair_plan_on_connection(
    conn: &mut Connection,
    target_job_key: &str,
) -> Result<ManualCheckpointRepairPlan, GitAiError> {
    let target = manual_repair_rows_where(conn, "job_key = ?1", target_job_key)?
        .into_iter()
        .next()
        .ok_or_else(|| {
            GitAiError::Generic(format!(
                "durable checkpoint job {target_job_key} does not exist"
            ))
        })?;

    let already_terminal = target.terminal_resolution == "manual_abandoned";
    if target.terminal_resolution != "normal" && !already_terminal {
        return Err(GitAiError::EvidenceError(format!(
            "durable checkpoint {} has unsupported terminal resolution {}",
            target.job_key, target.terminal_resolution
        )));
    }
    if !already_terminal && (target.state == "done" || target.blocked_evidence == 0) {
        return Err(GitAiError::Generic(format!(
            "durable checkpoint {} is not an evidence-blocked outstanding job",
            target.job_key
        )));
    }

    let affected_jobs = if already_terminal {
        let repair_id = target.repair_id.as_deref().ok_or_else(|| {
            GitAiError::EvidenceError(format!(
                "manually abandoned checkpoint {} is missing its repair id",
                target.job_key
            ))
        })?;
        manual_repair_rows_where(conn, "repair_id = ?1", repair_id)?
    } else {
        let rows = manual_repair_rows_where(
            conn,
            "repo_identity = ?1 AND state != 'done'",
            &target.repo_identity,
        )?;
        if rows.first().map(|row| row.id) != Some(target.id) {
            return Err(GitAiError::Generic(format!(
                "blocked checkpoint {} is not the first outstanding job for its repository; repair the earlier FIFO entry first",
                target.job_key
            )));
        }
        rows
    };
    if affected_jobs.is_empty() {
        return Err(GitAiError::EvidenceError(format!(
            "checkpoint repair {} has no preserved job evidence",
            target.job_key
        )));
    }

    let base_commit = manual_repair_base_commit(&target)?;
    let repair_id = if already_terminal {
        target.repair_id.clone().expect("checked above")
    } else {
        let mut hasher = Sha256::new();
        hasher.update(target.job_key.as_bytes());
        hasher.update(target.repo_identity.as_bytes());
        hasher.update(base_commit.as_bytes());
        hasher.update(serde_json::to_vec(&affected_jobs)?);
        let digest = format!("{:x}", hasher.finalize());
        format!("checkpoint-baseline-{}", &digest[..16])
    };

    Ok(ManualCheckpointRepairPlan {
        repair_id,
        target_job_key: target.job_key,
        repo_identity: target.repo_identity,
        repository_workdir: target.repository_workdir,
        base_commit,
        original_block_reason: target
            .blocked_reason
            .or(target.last_error)
            .unwrap_or_else(|| "unknown evidence failure".to_string()),
        affected_jobs,
        repair_backup_path: target.repair_backup_path,
        already_terminal,
    })
}

pub(crate) fn manually_abandon_repo_fifo_on_connection(
    conn: &mut Connection,
    plan: &ManualCheckpointRepairPlan,
    repair_backup_path: &str,
    now: u64,
) -> Result<usize, GitAiError> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let target: (String, Option<String>, Option<String>) = tx.query_row(
        "SELECT terminal_resolution, repair_id, repair_backup_path FROM deferred_checkpoint_jobs WHERE job_key = ?1",
        params![plan.target_job_key],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    if target.0 == "manual_abandoned" {
        if target.1.as_deref() == Some(plan.repair_id.as_str())
            && target.2.as_deref() == Some(repair_backup_path)
        {
            tx.commit()?;
            return Ok(0);
        }
        return Err(GitAiError::EvidenceError(format!(
            "checkpoint {} was already repaired with different immutable repair metadata",
            plan.target_job_key
        )));
    }
    if target.0 != "normal" {
        return Err(GitAiError::EvidenceError(format!(
            "checkpoint {} has unsupported terminal resolution {}",
            plan.target_job_key, target.0
        )));
    }

    let current_keys = {
        let mut stmt = tx.prepare(
            "SELECT job_key FROM deferred_checkpoint_jobs WHERE repo_identity = ?1 AND state != 'done' ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![plan.repo_identity], |row| row.get::<_, String>(0))?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let expected_keys = plan
        .affected_jobs
        .iter()
        .map(|row| row.job_key.clone())
        .collect::<Vec<_>>();
    if current_keys != expected_keys {
        return Err(GitAiError::Generic(
            "checkpoint repair impact changed after preview; run the preview again and use its new confirmation id"
                .to_string(),
        ));
    }

    let reason = format!(
        "manually abandoned by repair {}; original evidence backup: {}; original blocking reason: {}",
        plan.repair_id, repair_backup_path, plan.original_block_reason
    );
    let updated = tx.execute(
        r#"
        UPDATE deferred_checkpoint_jobs
        SET state = 'done',
            blocked_evidence = 1,
            blocked_reason = ?2,
            terminal_resolution = 'manual_abandoned',
            repair_id = ?3,
            repair_backup_path = ?4,
            admission_owner = NULL,
            processing_started_at = NULL,
            lease_token = NULL,
            last_error = ?2,
            updated_at = ?5,
            completed_at = ?5
        WHERE repo_identity = ?1 AND state != 'done'
        "#,
        params![
            plan.repo_identity,
            reason,
            plan.repair_id,
            repair_backup_path,
            u64_to_sqlite(now),
        ],
    )?;
    if updated != expected_keys.len() {
        return Err(GitAiError::Generic(format!(
            "checkpoint repair expected to abandon {} job(s) but updated {updated}",
            expected_keys.len()
        )));
    }
    tx.commit()?;
    Ok(updated)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn claim_due_on_connection(
    conn: &mut Connection,
    now: u64,
    lease_secs: u64,
) -> Result<Option<ClaimedDeferredCheckpointJob>, GitAiError> {
    claim_on_connection(conn, None, now, lease_secs, false)
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn claim_specific_on_connection(
    conn: &mut Connection,
    job_key: &str,
    now: u64,
    lease_secs: u64,
) -> Result<Option<ClaimedDeferredCheckpointJob>, GitAiError> {
    claim_specific_with_backoff_policy_on_connection(conn, job_key, now, lease_secs, false)
}

fn claim_specific_with_backoff_policy_on_connection(
    conn: &mut Connection,
    job_key: &str,
    now: u64,
    lease_secs: u64,
    bypass_pending_backoff: bool,
) -> Result<Option<ClaimedDeferredCheckpointJob>, GitAiError> {
    claim_on_connection(conn, Some(job_key), now, lease_secs, bypass_pending_backoff)
}

fn claim_on_connection(
    conn: &mut Connection,
    requested_job_key: Option<&str>,
    now: u64,
    lease_secs: u64,
    bypass_pending_backoff: bool,
) -> Result<Option<ClaimedDeferredCheckpointJob>, GitAiError> {
    let tx = conn.transaction()?;
    let expired_before = now.saturating_sub(lease_secs);
    let job_key: Option<String> = tx
        .query_row(
            r#"
            SELECT candidate.job_key
            FROM deferred_checkpoint_jobs candidate
            WHERE (?1 IS NULL OR candidate.job_key = ?1)
              AND candidate.blocked_evidence = 0
              AND (
                  (candidate.state = 'pending' AND (
                      candidate.next_retry_at <= ?2 OR ?4 = 1
                  ))
                  OR (candidate.state = 'processing' AND candidate.processing_started_at <= ?3)
              )
              AND NOT EXISTS (
                  SELECT 1
                  FROM deferred_checkpoint_jobs prior
                  WHERE prior.repo_identity = candidate.repo_identity
                    AND prior.state != 'done'
                    AND prior.id < candidate.id
              )
            ORDER BY candidate.id ASC
            LIMIT 1
            "#,
            params![
                requested_job_key,
                u64_to_sqlite(now),
                u64_to_sqlite(expired_before),
                i64::from(bypass_pending_backoff),
            ],
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
        UPDATE deferred_checkpoint_jobs
        SET state = 'processing',
            attempts = attempts + 1,
            processing_started_at = ?2,
            lease_token = ?3,
            admission_owner = NULL,
            updated_at = ?2
        WHERE job_key = ?1
          AND blocked_evidence = 0
          AND (
              (state = 'pending' AND (next_retry_at <= ?2 OR ?5 = 1))
              OR (state = 'processing' AND processing_started_at <= ?4)
          )
        "#,
        params![
            job_key,
            u64_to_sqlite(now),
            lease_token,
            u64_to_sqlite(expired_before),
            i64::from(bypass_pending_backoff),
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

pub(crate) fn release_admission_owner_on_connection(
    conn: &mut Connection,
    job_key: &str,
    admission_owner: &str,
    now: u64,
) -> Result<bool, GitAiError> {
    Ok(conn.execute(
        r#"
        UPDATE deferred_checkpoint_jobs
        SET admission_owner = NULL,
            updated_at = ?3
        WHERE job_key = ?1
          AND state = 'pending'
          AND admission_owner = ?2
        "#,
        params![job_key, admission_owner, u64_to_sqlite(now)],
    )? == 1)
}

fn load_claimed_job(
    tx: &Transaction<'_>,
    job_key: &str,
    lease_token: &str,
) -> Result<ClaimedDeferredCheckpointJob, GitAiError> {
    tx.query_row(
        r#"
        SELECT id, repo_identity, repository_workdir, request_json,
               metrics_context_json, observed_at_ms,
               prepared_checkpoint_json, prepared_metric_events_json,
               working_log_applied, attempts
        FROM deferred_checkpoint_jobs
        WHERE job_key = ?1 AND state = 'processing' AND lease_token = ?2
        "#,
        params![job_key, lease_token],
        |row| {
            let attempts: i64 = row.get(9)?;
            Ok(ClaimedDeferredCheckpointJob {
                id: row.get(0)?,
                job_key: job_key.to_string(),
                lease_token: lease_token.to_string(),
                repo_identity: row.get(1)?,
                repository_workdir: row.get(2)?,
                request_json: row.get(3)?,
                metrics_context_json: row.get(4)?,
                observed_at_ms: row.get::<_, i64>(5)?.max(0) as u64,
                prepared_checkpoint_json: row.get(6)?,
                prepared_metric_events_json: row.get(7)?,
                working_log_applied: row.get::<_, i64>(8)? != 0,
                attempts: attempts.max(0).min(i64::from(u32::MAX)) as u32,
            })
        },
    )
    .map_err(GitAiError::from)
}

pub(crate) fn persist_prepared_on_connection(
    conn: &mut Connection,
    job: &ClaimedDeferredCheckpointJob,
    prepared_checkpoint_json: &str,
    metric_event_jsons: &[String],
    agent_usage: Option<&AgentUsageCandidate>,
    now: u64,
) -> Result<bool, GitAiError> {
    if prepared_checkpoint_json.len() > MAX_PREPARED_JSON_BYTES {
        return Err(GitAiError::Generic(format!(
            "prepared checkpoint is {} bytes; limit is {} bytes",
            prepared_checkpoint_json.len(),
            MAX_PREPARED_JSON_BYTES
        )));
    }
    let total_metric_bytes = metric_event_jsons
        .iter()
        .try_fold(0usize, |total, event| total.checked_add(event.len()))
        .ok_or_else(|| GitAiError::Generic("prepared metric size overflowed".to_string()))?;
    if total_metric_bytes > MAX_PREPARED_JSON_BYTES {
        return Err(GitAiError::Generic(format!(
            "prepared checkpoint metrics are {total_metric_bytes} bytes; limit is {MAX_PREPARED_JSON_BYTES} bytes"
        )));
    }
    serde_json::from_str::<Value>(prepared_checkpoint_json)?;
    for event_json in metric_event_jsons {
        serde_json::from_str::<MetricEvent>(event_json)?;
    }
    if let Some(candidate) = agent_usage {
        serde_json::to_string(&candidate.event)?;
    }

    let tx = conn.transaction()?;
    let already_prepared: bool = tx.query_row(
        r#"
        SELECT prepared_checkpoint_json IS NOT NULL
            OR prepared_metric_events_json IS NOT NULL
        FROM deferred_checkpoint_jobs
        WHERE job_key = ?1 AND state = 'processing' AND lease_token = ?2
        "#,
        params![job.job_key, job.lease_token],
        |row| row.get(0),
    )?;
    if already_prepared {
        tx.rollback()?;
        return Ok(false);
    }

    let mut final_event_jsons = metric_event_jsons.to_vec();
    if let Some(candidate) = agent_usage {
        let existing_ts: Option<i64> = tx
            .query_row(
                "SELECT last_sent_ts FROM agent_usage_throttle WHERE prompt_id = ?1",
                params![candidate.prompt_id],
                |row| row.get(0),
            )
            .optional()?;
        let should_emit = existing_ts
            .map(|previous| {
                candidate
                    .observed_at_secs
                    .saturating_sub(previous.max(0) as u64)
                    >= candidate.min_interval_secs
            })
            .unwrap_or(true);
        if should_emit {
            final_event_jsons.push(serde_json::to_string(&candidate.event)?);
            tx.execute(
                r#"
                INSERT INTO agent_usage_throttle (prompt_id, last_sent_ts)
                VALUES (?1, ?2)
                ON CONFLICT(prompt_id) DO UPDATE SET last_sent_ts = excluded.last_sent_ts
                "#,
                params![
                    candidate.prompt_id,
                    u64_to_sqlite(candidate.observed_at_secs)
                ],
            )?;
        }
    }
    let events_json = serde_json::to_string(&final_event_jsons)?;
    let updated = tx.execute(
        r#"
        UPDATE deferred_checkpoint_jobs
        SET prepared_checkpoint_json = ?3,
            prepared_metric_events_json = ?4,
            updated_at = ?5
        WHERE job_key = ?1
          AND state = 'processing'
          AND lease_token = ?2
          AND prepared_checkpoint_json IS NULL
          AND prepared_metric_events_json IS NULL
        "#,
        params![
            job.job_key,
            job.lease_token,
            prepared_checkpoint_json,
            events_json,
            u64_to_sqlite(now),
        ],
    )?;
    tx.commit()?;
    Ok(updated == 1)
}

pub(crate) fn mark_working_log_applied_on_connection(
    conn: &mut Connection,
    job: &ClaimedDeferredCheckpointJob,
    now: u64,
) -> Result<bool, GitAiError> {
    let updated = conn.execute(
        r#"
        UPDATE deferred_checkpoint_jobs
        SET working_log_applied = 1,
            updated_at = ?3
        WHERE job_key = ?1
          AND state = 'processing'
          AND lease_token = ?2
          AND prepared_checkpoint_json IS NOT NULL
          AND prepared_metric_events_json IS NOT NULL
        "#,
        params![job.job_key, job.lease_token, u64_to_sqlite(now)],
    )?;
    Ok(updated == 1)
}

pub(crate) fn renew_processing_lease_on_connection(
    conn: &mut Connection,
    job_key: &str,
    lease_token: &str,
    now: u64,
) -> Result<bool, GitAiError> {
    let updated = conn.execute(
        r#"
        UPDATE deferred_checkpoint_jobs
        SET processing_started_at = ?3,
            updated_at = ?3
        WHERE job_key = ?1
          AND state = 'processing'
          AND lease_token = ?2
          AND blocked_evidence = 0
        "#,
        params![job_key, lease_token, u64_to_sqlite(now)],
    )?;
    Ok(updated == 1)
}

pub(crate) fn complete_on_connection(
    conn: &mut Connection,
    job: &ClaimedDeferredCheckpointJob,
    now: u64,
) -> Result<bool, GitAiError> {
    let tx = conn.transaction()?;
    let row: Option<(i64, String)> = tx
        .query_row(
            r#"
            SELECT working_log_applied, prepared_metric_events_json
            FROM deferred_checkpoint_jobs
            WHERE job_key = ?1 AND state = 'processing' AND lease_token = ?2
            "#,
            params![job.job_key, job.lease_token],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((working_log_applied, metric_events_json)) = row else {
        tx.rollback()?;
        return Ok(false);
    };
    if working_log_applied == 0 {
        return Err(GitAiError::Generic(format!(
            "durable checkpoint {} cannot complete before working-log publication",
            job.job_key
        )));
    }
    let metric_event_jsons: Vec<String> = serde_json::from_str(&metric_events_json)?;
    let metric_ids =
        crate::metrics::db::insert_event_jsons_in_transaction(&tx, &metric_event_jsons, None)?;
    let metric_ids_json = serde_json::to_string(&metric_ids)?;
    let updated = tx.execute(
        r#"
        UPDATE deferred_checkpoint_jobs
        SET state = 'done',
            processing_started_at = NULL,
            lease_token = NULL,
            last_error = NULL,
            metric_ids_json = ?3,
            repository_workdir = '',
            request_json = '',
            metrics_context_json = '',
            prepared_checkpoint_json = NULL,
            prepared_metric_events_json = NULL,
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

pub(crate) fn mark_failed_on_connection(
    conn: &mut Connection,
    job: &ClaimedDeferredCheckpointJob,
    error: &str,
    now: u64,
) -> Result<bool, GitAiError> {
    let next_retry_at = now.saturating_add(retry_backoff_seconds(job.attempts));
    let updated = conn.execute(
        r#"
        UPDATE deferred_checkpoint_jobs
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

pub(crate) fn mark_blocked_on_connection(
    conn: &mut Connection,
    job: &ClaimedDeferredCheckpointJob,
    reason: &str,
    now: u64,
) -> Result<bool, GitAiError> {
    let updated = conn.execute(
        r#"
        UPDATE deferred_checkpoint_jobs
        SET state = 'pending',
            blocked_evidence = 1,
            blocked_reason = ?3,
            last_error = ?3,
            processing_started_at = NULL,
            lease_token = NULL,
            updated_at = ?4
        WHERE job_key = ?1
          AND state = 'processing'
          AND lease_token = ?2
        "#,
        params![job.job_key, job.lease_token, reason, u64_to_sqlite(now),],
    )?;
    Ok(updated == 1)
}

pub(crate) fn status_on_connection(
    conn: &mut Connection,
    job_key: &str,
) -> Result<Option<DeferredCheckpointJobStatus>, GitAiError> {
    let state: Option<DeferredCheckpointJobStatusRow> = conn
        .query_row(
            "SELECT state, blocked_evidence, blocked_reason, terminal_resolution, repair_id, repair_backup_path FROM deferred_checkpoint_jobs WHERE job_key = ?1",
            params![job_key],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()?;
    state
        .map(
            |(state, blocked, blocked_reason, terminal_resolution, repair_id, backup_path)| {
                if terminal_resolution == "manual_abandoned" {
                    return Ok(DeferredCheckpointJobStatus::ManuallyAbandoned(format!(
                        "repair {} preserved evidence at {}",
                        repair_id.unwrap_or_else(|| "<missing>".to_string()),
                        backup_path.unwrap_or_else(|| "<missing>".to_string())
                    )));
                }
                match (state.as_str(), blocked) {
                    (_, true) => Ok(DeferredCheckpointJobStatus::Blocked(
                        blocked_reason.unwrap_or_else(|| "unknown evidence failure".to_string()),
                    )),
                    ("pending", false) => Ok(DeferredCheckpointJobStatus::Pending),
                    ("processing", false) => Ok(DeferredCheckpointJobStatus::Processing),
                    ("done", false) => Ok(DeferredCheckpointJobStatus::Done),
                    (other, false) => Err(GitAiError::Generic(format!(
                        "unknown durable checkpoint state: {other}"
                    ))),
                }
            },
        )
        .transpose()
}

pub(crate) fn count_outstanding_on_connection(conn: &mut Connection) -> Result<usize, GitAiError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM deferred_checkpoint_jobs WHERE state != 'done'",
        [],
        |row| row.get(0),
    )?;
    Ok(count.max(0) as usize)
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
        UPDATE deferred_checkpoint_jobs
        SET repository_workdir = '',
            request_json = '',
            metrics_context_json = '',
            prepared_checkpoint_json = NULL,
            prepared_metric_events_json = NULL
        WHERE id IN (
            SELECT id
            FROM deferred_checkpoint_jobs
            WHERE state = 'done'
              AND terminal_resolution = 'normal'
              AND (
                  repository_workdir != ''
                  OR request_json != ''
                  OR metrics_context_json != ''
                  OR prepared_checkpoint_json IS NOT NULL
                  OR prepared_metric_events_json IS NOT NULL
              )
            ORDER BY completed_at ASC, id ASC
            LIMIT ?1
        )
        "#,
        params![i64::try_from(limit).unwrap_or(i64::MAX)],
    )?;
    Ok(updated)
}

fn retry_backoff_seconds(attempts: u32) -> u64 {
    let shift = attempts.saturating_sub(1).min(20);
    INITIAL_RETRY_BACKOFF_SECS
        .saturating_mul(1u64 << shift)
        .min(MAX_RETRY_BACKOFF_SECS)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn unix_now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

fn u64_to_sqlite(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorship::working_log::AgentId;
    use crate::commands::checkpoint_agent::orchestrator::{BaseCommit, CheckpointFile};
    use crate::metrics::types::{MetricEventId, SparseArray};
    use std::collections::HashMap;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().expect("sqlite");
        conn.execute_batch(
            r#"
            CREATE TABLE metrics (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                event_json TEXT NOT NULL,
                delivered_ts INTEGER,
                event_ts INTEGER,
                event_kind INTEGER,
                trace_id TEXT,
                session_id TEXT,
                parent_session_id TEXT,
                tool TEXT,
                external_session_id TEXT,
                external_parent_session_id TEXT,
                external_event_id TEXT,
                external_parent_event_id TEXT,
                external_tool_use_id TEXT
            );
            CREATE TABLE agent_usage_throttle (
                prompt_id TEXT PRIMARY KEY,
                last_sent_ts INTEGER NOT NULL
            );
            "#,
        )
        .unwrap();
        conn.execute_batch(DEFERRED_CHECKPOINT_JOBS_SCHEMA_SQL)
            .unwrap();
        conn
    }

    fn spec(repo: &str, call: &str, phase: &str, evidence: &str) -> DeferredCheckpointJobSpec {
        let job_key = stable_job_key(repo, "kilo-v7", "session", call, phase);
        DeferredCheckpointJobSpec {
            job_key,
            repo_identity: repo.to_string(),
            repository_workdir: format!("/work/{repo}"),
            integration: "kilo-v7".to_string(),
            external_session_id: "session".to_string(),
            external_tool_use_id: call.to_string(),
            phase: phase.to_string(),
            request_shape_sha256: evidence.to_string(),
            request_evidence_sha256: evidence.to_string(),
            request_json: "{}".to_string(),
            metrics_context_json: r#"{"git_ai_version":"test","custom_attributes":{},"repo_url":null,"branch":null,"author":"Test"}"#.to_string(),
            path_scope_json: serde_json::to_string(&DeferredCheckpointPathScope::Files {
                paths: vec!["src/main.rs".to_string()],
            })
            .unwrap(),
            admission_owner: None,
            observed_at_ms: 1_000,
        }
    }

    fn event_json(instance: &str) -> String {
        serde_json::to_string(&MetricEvent {
            timestamp: 100,
            event_id: MetricEventId::Checkpoint as u16,
            instance_id: Some(instance.to_string()),
            values: SparseArray::new(),
            attrs: SparseArray::new(),
        })
        .unwrap()
    }

    fn metadata_event_json(instance: &str) -> String {
        serde_json::json!({
            "t": 100,
            "e": MetricEventId::Checkpoint as u16,
            "i": instance,
            "v": {"7": "call"},
            "a": {
                "20": "kilo",
                "23": "external-session",
                "24": "local-session",
                "25": "checkpoint-job:trace"
            }
        })
        .to_string()
    }

    #[test]
    fn request_scope_is_repo_relative_sorted_and_deduplicated() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let init = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("init")
            .output()
            .unwrap();
        assert!(init.status.success());
        std::fs::write(repo.join("src/a.rs"), "a\n").unwrap();
        std::fs::write(repo.join("src/b.rs"), "b\n").unwrap();

        let mut request = CheckpointRequest {
            trace_id: "trace".to_string(),
            checkpoint_kind: CheckpointKind::Human,
            agent_id: Some(AgentId {
                tool: "opencode".to_string(),
                id: "session".to_string(),
                model: "test".to_string(),
            }),
            files: vec![
                CheckpointFile {
                    path: repo.join("src/b.rs"),
                    content: Some("b\n".to_string()),
                    repo_work_dir: repo.clone(),
                    base_commit: BaseCommit::Initial,
                },
                CheckpointFile {
                    path: PathBuf::from("src/../src/a.rs"),
                    content: Some("a\n".to_string()),
                    repo_work_dir: repo.clone(),
                    base_commit: BaseCommit::Initial,
                },
                CheckpointFile {
                    path: PathBuf::from("src/a.rs"),
                    content: Some("a\n".to_string()),
                    repo_work_dir: repo.clone(),
                    base_commit: BaseCommit::Initial,
                },
            ],
            path_role: PreparedPathRole::WillEdit,
            stream_source: None,
            metadata: HashMap::from([
                ("tool_use_id".to_string(), "call".to_string()),
                ("integration".to_string(), "opencode".to_string()),
            ]),
        };

        let spec = DeferredCheckpointJobSpec::from_request(&mut request)
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<DeferredCheckpointPathScope>(&spec.path_scope_json).unwrap(),
            DeferredCheckpointPathScope::Files {
                paths: vec!["src/a.rs".to_string(), "src/b.rs".to_string()]
            }
        );
    }

    fn complete_pre(conn: &mut Connection, repo: &str, call: &str) {
        let pre = spec(repo, call, "pre", &format!("pre-{call}"));
        assert!(enqueue_on_connection(conn, &pre, 1).unwrap());
        let claimed = claim_specific_on_connection(conn, &pre.job_key, 2, 60)
            .unwrap()
            .unwrap();
        assert!(
            persist_prepared_on_connection(
                conn,
                &claimed,
                r#"{"base_commit":"abc","checkpoint":null}"#,
                &[],
                None,
                3,
            )
            .unwrap()
        );
        assert!(mark_working_log_applied_on_connection(conn, &claimed, 4).unwrap());
        assert!(complete_on_connection(conn, &claimed, 5).unwrap());
    }

    fn scope(paths: &[&str]) -> String {
        serde_json::to_string(&DeferredCheckpointPathScope::Files {
            paths: paths.iter().map(|path| (*path).to_string()).collect(),
        })
        .unwrap()
    }

    fn complete_pre_with_scope(
        conn: &mut Connection,
        repo: &str,
        call: &str,
        path_scope_json: String,
    ) {
        let mut pre = spec(repo, call, "pre", &format!("pre-{call}"));
        pre.path_scope_json = path_scope_json;
        assert!(enqueue_on_connection(conn, &pre, 1).unwrap());
        let claimed = claim_specific_on_connection(conn, &pre.job_key, 2, 60)
            .unwrap()
            .unwrap();
        assert!(
            persist_prepared_on_connection(
                conn,
                &claimed,
                r#"{"base_commit":"abc","checkpoint":null}"#,
                &[],
                None,
                3,
            )
            .unwrap()
        );
        assert!(mark_working_log_applied_on_connection(conn, &claimed, 4).unwrap());
        assert!(complete_on_connection(conn, &claimed, 5).unwrap());
    }

    #[test]
    fn duplicate_ack_retry_reuses_job_but_rejects_identity_collision() {
        let mut conn = setup();
        complete_pre(&mut conn, "repo", "call");
        let original = spec("repo", "call", "post", "evidence-a");
        assert!(enqueue_on_connection(&mut conn, &original, 10).unwrap());
        assert!(!enqueue_on_connection(&mut conn, &original, 11).unwrap());

        let collision = spec("repo", "call", "post", "evidence-b");
        let error = enqueue_on_connection(&mut conn, &collision, 12).unwrap_err();
        assert!(error.to_string().contains("identity collision"));
        assert_eq!(count_outstanding_on_connection(&mut conn).unwrap(), 1);
    }

    #[test]
    fn duplicate_retry_preserves_first_admission_context_and_time() {
        let mut conn = setup();
        let first = spec("repo", "call", "pre", "shape");
        assert!(enqueue_on_connection(&mut conn, &first, 10).unwrap());

        let mut retry = first.clone();
        retry.request_evidence_sha256 = "different-full-admission-evidence".to_string();
        retry.metrics_context_json = r#"{"profile":"B"}"#.to_string();
        retry.observed_at_ms = 86_401_000;
        assert!(!enqueue_on_connection(&mut conn, &retry, 20).unwrap());

        let stored: (String, String, i64) = conn
            .query_row(
                "SELECT request_evidence_sha256, metrics_context_json, observed_at_ms FROM deferred_checkpoint_jobs WHERE job_key = ?1",
                params![first.job_key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(stored.0, first.request_evidence_sha256);
        assert_eq!(stored.1, first.metrics_context_json);
        assert_eq!(stored.2, first.observed_at_ms as i64);
    }

    #[test]
    fn post_requires_admitted_pre_for_same_repository_session_and_call() {
        let mut conn = setup();
        let post = spec("repo", "call-1", "post", "post");
        let error = enqueue_on_connection(&mut conn, &post, 10).unwrap_err();
        assert!(error.to_string().contains("no admitted pre checkpoint"));

        complete_pre(&mut conn, "other-repo", "call-1");
        let error = enqueue_on_connection(&mut conn, &post, 11).unwrap_err();
        assert!(error.to_string().contains("no admitted pre checkpoint"));

        complete_pre(&mut conn, "repo", "call-1");
        assert!(enqueue_on_connection(&mut conn, &post, 12).unwrap());
    }

    #[test]
    fn post_can_be_admitted_behind_pending_pre_but_cannot_overtake_it() {
        let mut conn = setup();
        let pre = spec("repo", "call-1", "pre", "pre");
        let post = spec("repo", "call-1", "post", "post");

        assert!(enqueue_on_connection(&mut conn, &pre, 10).unwrap());
        assert!(enqueue_on_connection(&mut conn, &post, 11).unwrap());
        assert!(
            claim_specific_on_connection(&mut conn, &post.job_key, 12, 60)
                .unwrap()
                .is_none(),
            "repository FIFO must keep post behind its pending pre"
        );
    }

    #[test]
    fn post_file_scope_must_be_a_subset_of_completed_pre_scope() {
        let mut conn = setup();
        complete_pre_with_scope(&mut conn, "repo", "call", scope(&["src/a.rs", "src/b.rs"]));

        let mut subset = spec("repo", "call", "post", "subset");
        subset.path_scope_json = scope(&["src/b.rs"]);
        assert!(enqueue_on_connection(&mut conn, &subset, 10).unwrap());

        let mut expanded = spec("repo", "call", "post", "expanded");
        expanded.path_scope_json = scope(&["src/b.rs", "src/c.rs"]);
        let error = enqueue_on_connection(&mut conn, &expanded, 11).unwrap_err();
        assert!(error.to_string().contains("expands beyond"));
        assert!(error.to_string().contains("src/c.rs"));
    }

    #[test]
    fn completed_pre_scope_survives_payload_compaction() {
        let mut conn = setup();
        complete_pre_with_scope(&mut conn, "repo", "call", scope(&["src/a.rs"]));
        compact_done_payloads_on_connection(&mut conn, 100).unwrap();

        let mut post = spec("repo", "call", "post", "post");
        post.path_scope_json = scope(&["src/a.rs"]);
        assert!(enqueue_on_connection(&mut conn, &post, 10).unwrap());

        let stored_scope: String = conn
            .query_row(
                "SELECT path_scope_json FROM deferred_checkpoint_jobs WHERE phase = 'pre'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored_scope, scope(&["src/a.rs"]));
    }

    #[test]
    fn legacy_completed_pre_without_scope_rejects_post() {
        let mut conn = setup();
        complete_pre(&mut conn, "repo", "call");
        conn.execute(
            "UPDATE deferred_checkpoint_jobs SET path_scope_json = NULL WHERE phase = 'pre'",
            [],
        )
        .unwrap();

        let post = spec("repo", "call", "post", "post");
        let error = enqueue_on_connection(&mut conn, &post, 10).unwrap_err();
        assert!(error.to_string().contains("without preserved path scope"));
    }

    #[test]
    fn bash_pre_wildcard_allows_post_paths_discovered_from_full_snapshot() {
        let mut conn = setup();
        complete_pre_with_scope(
            &mut conn,
            "repo",
            "call",
            serde_json::to_string(&DeferredCheckpointPathScope::BashWildcard).unwrap(),
        );

        let mut post = spec("repo", "call", "post", "post");
        post.path_scope_json = scope(&["new/file.rs", "renamed.txt"]);
        assert!(enqueue_on_connection(&mut conn, &post, 10).unwrap());
    }

    #[test]
    fn same_repo_jobs_are_fifo_even_when_later_job_is_due() {
        let mut conn = setup();
        complete_pre(&mut conn, "repo", "call-1");
        let post = spec("repo", "call-1", "post", "post");
        enqueue_on_connection(&mut conn, &post, 10).unwrap();
        let next_pre = spec("repo", "call-2", "pre", "next-pre");
        enqueue_on_connection(&mut conn, &next_pre, 10).unwrap();

        assert!(
            claim_specific_on_connection(&mut conn, &next_pre.job_key, 20, 60)
                .unwrap()
                .is_none(),
            "a later pre must not overtake an unfinished post"
        );
        let first = claim_due_on_connection(&mut conn, 20, 60).unwrap().unwrap();
        assert_eq!(first.job_key, post.job_key);
    }

    #[test]
    fn blocked_evidence_is_not_reclaimed_and_still_blocks_only_its_repo_fifo() {
        let mut conn = setup();
        let blocked = spec("repo-a", "call-1", "pre", "blocked");
        let later = spec("repo-a", "call-2", "pre", "later");
        let other = spec("repo-b", "call-1", "pre", "other");
        enqueue_on_connection(&mut conn, &blocked, 1).unwrap();
        enqueue_on_connection(&mut conn, &later, 2).unwrap();
        enqueue_on_connection(&mut conn, &other, 3).unwrap();

        let claimed = claim_specific_on_connection(&mut conn, &blocked.job_key, 10, 60)
            .unwrap()
            .unwrap();
        assert!(
            mark_blocked_on_connection(
                &mut conn,
                &claimed,
                "Evidence error: corrupt INITIAL; back up the repository and reset its baseline manually",
                11,
            )
            .unwrap()
        );

        assert!(
            claim_specific_on_connection(&mut conn, &blocked.job_key, 100, 60)
                .unwrap()
                .is_none()
        );
        assert!(
            claim_specific_on_connection(&mut conn, &later.job_key, 100, 60)
                .unwrap()
                .is_none()
        );
        let due =
            due_recovery_requests_on_connection(&mut conn, 100, 60, 10, "daemon-current").unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].job_key, other.job_key);

        let blocked_rows = blocked_jobs_on_connection(&mut conn).unwrap();
        assert_eq!(blocked_rows.len(), 1);
        assert_eq!(blocked_rows[0].job_key, blocked.job_key);
        assert!(blocked_rows[0].reason.contains("reset its baseline"));
        let attempts: i64 = conn
            .query_row(
                "SELECT attempts FROM deferred_checkpoint_jobs WHERE job_key = ?1",
                params![blocked.job_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(attempts, 1, "blocked evidence must not retry repeatedly");
    }

    #[test]
    fn corrupt_initial_evidence_error_names_the_real_repair_preview_command() {
        let job_key = "a".repeat(64);
        let error = checkpoint_repair_guidance(
            &job_key,
            GitAiError::EvidenceError(
                "INITIAL missing persisted file snapshot for tracked.txt".to_string(),
            ),
        );
        let message = error.to_string();
        assert!(message.contains("original evidence is preserved"));
        assert!(message.contains(&format!(
            "git-ai repair checkpoint-baseline --job-key {job_key}"
        )));
    }

    #[test]
    fn missing_repository_recovery_is_blocked_once_and_remains_await_visible() {
        let mut conn = setup();
        let missing = spec("missing-repo", "call", "pre", "missing");
        enqueue_on_connection(&mut conn, &missing, 1).unwrap();
        let claimed = claim_specific_on_connection(&mut conn, &missing.job_key, 2, 60)
            .unwrap()
            .unwrap();
        let reason = "Evidence error: frozen repository path cannot be resolved; preserve evidence, back up the repository, then reset the checkpoint baseline manually";
        assert!(mark_blocked_on_connection(&mut conn, &claimed, reason, 3).unwrap());

        for now in [4, 10_000] {
            assert!(
                due_recovery_requests_on_connection(&mut conn, now, 60, 10, "current-daemon",)
                    .unwrap()
                    .is_empty(),
                "blocked missing-repository evidence must not be periodically reclaimed"
            );
        }
        let blocked = blocked_jobs_on_connection(&mut conn).unwrap();
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].job_key, missing.job_key);
        assert!(
            blocked[0]
                .reason
                .contains("reset the checkpoint baseline manually")
        );
        let attempts: i64 = conn
            .query_row(
                "SELECT attempts FROM deferred_checkpoint_jobs WHERE job_key = ?1",
                params![missing.job_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(attempts, 1);
    }

    #[test]
    fn recovery_cannot_overtake_fresh_live_admission_window() {
        let mut conn = setup();
        let mut live = spec("repo", "call", "pre", "live");
        live.admission_owner = Some("daemon-a".to_string());
        enqueue_on_connection(&mut conn, &live, 1).unwrap();

        let same_daemon =
            due_recovery_requests_on_connection(&mut conn, 10, 60, 10, "daemon-a").unwrap();
        assert!(
            same_daemon.is_empty(),
            "periodic recovery must not schedule a live row before its family entry is appended"
        );

        let after_restart =
            due_recovery_requests_on_connection(&mut conn, 10, 60, 10, "daemon-b").unwrap();
        assert_eq!(after_restart.len(), 1);
        assert_eq!(after_restart[0].job_key, live.job_key);

        let claimed = claim_specific_on_connection(&mut conn, &live.job_key, 10, 60)
            .unwrap()
            .expect("the live family entry must be able to claim its own admission");
        let owner: Option<String> = conn
            .query_row(
                "SELECT admission_owner FROM deferred_checkpoint_jobs WHERE job_key = ?1",
                params![live.job_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(owner, None);
        assert_eq!(claimed.job_key, live.job_key);
    }

    #[test]
    fn duplicate_live_retry_takes_owner_and_bypasses_pending_backoff() {
        let mut conn = setup();
        let mut original = spec("repo", "call", "pre", "live");
        original.admission_owner = Some("old-daemon".to_string());
        enqueue_on_connection(&mut conn, &original, 1).unwrap();

        let first = claim_specific_on_connection(&mut conn, &original.job_key, 10, 60)
            .unwrap()
            .unwrap();
        assert!(mark_failed_on_connection(&mut conn, &first, "retry", 10).unwrap());

        let mut retry = original.clone();
        retry.admission_owner = Some("current-daemon".to_string());
        assert!(!enqueue_on_connection(&mut conn, &retry, 10).unwrap());

        let same_daemon =
            due_recovery_requests_on_connection(&mut conn, 100, 60, 10, "current-daemon").unwrap();
        assert!(
            same_daemon.is_empty(),
            "periodic recovery must not steal a duplicate live admission"
        );

        assert!(
            claim_specific_on_connection(&mut conn, &retry.job_key, 10, 60)
                .unwrap()
                .is_none(),
            "recovery-style specific claims must still respect retry backoff"
        );
        let claimed = claim_specific_with_backoff_policy_on_connection(
            &mut conn,
            &retry.job_key,
            10,
            60,
            true,
        )
        .unwrap()
        .expect("the live family entry must bypass its pending retry backoff");
        assert_eq!(claimed.attempts, 2);
        let owner: Option<String> = conn
            .query_row(
                "SELECT admission_owner FROM deferred_checkpoint_jobs WHERE job_key = ?1",
                params![retry.job_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(owner, None, "claiming must clear live admission ownership");
    }

    #[test]
    fn live_owner_can_be_released_when_prior_fifo_row_prevents_claim() {
        let mut conn = setup();
        let prior = spec("repo", "call-1", "pre", "prior");
        enqueue_on_connection(&mut conn, &prior, 1).unwrap();
        let mut later = spec("repo", "call-2", "pre", "later");
        later.admission_owner = Some("current-daemon".to_string());
        enqueue_on_connection(&mut conn, &later, 2).unwrap();

        assert!(
            claim_specific_on_connection(&mut conn, &later.job_key, 10, 60)
                .unwrap()
                .is_none(),
            "the live row must not overtake its older repository FIFO dependency"
        );
        assert!(
            release_admission_owner_on_connection(&mut conn, &later.job_key, "current-daemon", 11,)
                .unwrap()
        );
        let owner: Option<String> = conn
            .query_row(
                "SELECT admission_owner FROM deferred_checkpoint_jobs WHERE job_key = ?1",
                params![later.job_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(owner, None);
    }

    #[test]
    fn failed_family_handoff_release_makes_live_row_visible_to_same_daemon_recovery() {
        let mut conn = setup();
        let mut live = spec("repo", "call", "pre", "live");
        live.admission_owner = Some("current-daemon".to_string());
        enqueue_on_connection(&mut conn, &live, 1).unwrap();

        assert!(
            due_recovery_requests_on_connection(&mut conn, 10, 60, 10, "current-daemon",)
                .unwrap()
                .is_empty(),
            "the periodic pass must not race admission before family handoff"
        );

        // Model resolve_family/append/response failure before the family entry
        // can claim the row. The admission wrapper conditionally releases only
        // its own pending row on every such error path.
        assert!(
            release_admission_owner_on_connection(&mut conn, &live.job_key, "current-daemon", 11,)
                .unwrap()
        );
        let due =
            due_recovery_requests_on_connection(&mut conn, 11, 60, 10, "current-daemon").unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].job_key, live.job_key);
    }

    #[test]
    fn processing_lease_renewal_fences_reclaim_and_rejects_stale_tokens() {
        let mut conn = setup();
        let job = spec("repo", "call", "pre", "lease-heartbeat");
        enqueue_on_connection(&mut conn, &job, 1).unwrap();
        let original = claim_specific_on_connection(&mut conn, &job.job_key, 10, 60)
            .unwrap()
            .unwrap();

        assert!(
            !renew_processing_lease_on_connection(&mut conn, &job.job_key, "wrong-token", 50,)
                .unwrap(),
            "a heartbeat must never renew another owner's lease"
        );
        assert!(
            renew_processing_lease_on_connection(
                &mut conn,
                &job.job_key,
                &original.lease_token,
                50,
            )
            .unwrap()
        );
        assert!(
            claim_specific_on_connection(&mut conn, &job.job_key, 109, 60)
                .unwrap()
                .is_none(),
            "the original claim time may expire, but the renewed lease must remain fenced"
        );

        let reclaimed = claim_specific_on_connection(&mut conn, &job.job_key, 110, 60)
            .unwrap()
            .expect("the row must remain crash-recoverable once the renewed lease expires");
        assert_ne!(reclaimed.lease_token, original.lease_token);
        assert!(
            !renew_processing_lease_on_connection(
                &mut conn,
                &job.job_key,
                &original.lease_token,
                111,
            )
            .unwrap(),
            "a reclaimed row must reject its previous owner's heartbeat"
        );
    }

    #[test]
    fn crash_after_prepare_reuses_immutable_payload_after_lease_expiry() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("jobs.db");
        let spec = spec("repo", "call", "post", "evidence");
        {
            let mut conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE metrics (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    event_json TEXT NOT NULL,
                    delivered_ts INTEGER,
                    event_ts INTEGER,
                    event_kind INTEGER,
                    trace_id TEXT,
                    session_id TEXT,
                    parent_session_id TEXT,
                    tool TEXT,
                    external_session_id TEXT,
                    external_parent_session_id TEXT,
                    external_event_id TEXT,
                    external_parent_event_id TEXT,
                    external_tool_use_id TEXT
                );
                CREATE TABLE agent_usage_throttle (
                    prompt_id TEXT PRIMARY KEY,
                    last_sent_ts INTEGER NOT NULL
                );
                "#,
            )
            .unwrap();
            conn.execute_batch(DEFERRED_CHECKPOINT_JOBS_SCHEMA_SQL)
                .unwrap();
            complete_pre(&mut conn, "repo", "call");
            enqueue_on_connection(&mut conn, &spec, 10).unwrap();
            let claimed = claim_due_on_connection(&mut conn, 20, 60).unwrap().unwrap();
            persist_prepared_on_connection(
                &mut conn,
                &claimed,
                r#"{"base_commit":"abc","checkpoint":null}"#,
                &[event_json("event-1")],
                None,
                21,
            )
            .unwrap();
        }

        let mut reopened = Connection::open(&path).unwrap();
        let reclaimed = claim_due_on_connection(&mut reopened, 80, 60)
            .unwrap()
            .unwrap();
        assert_eq!(
            reclaimed.prepared_checkpoint_json.as_deref(),
            Some(r#"{"base_commit":"abc","checkpoint":null}"#)
        );
        let metrics: Vec<String> =
            serde_json::from_str(reclaimed.prepared_metric_events_json.as_deref().unwrap())
                .unwrap();
        assert_eq!(metrics, vec![event_json("event-1")]);
    }

    #[test]
    fn crash_after_working_log_apply_completes_without_repreparing() {
        let mut conn = setup();
        complete_pre(&mut conn, "repo", "call");
        let spec = spec("repo", "call", "post", "evidence");
        enqueue_on_connection(&mut conn, &spec, 10).unwrap();
        let first = claim_due_on_connection(&mut conn, 20, 10).unwrap().unwrap();
        persist_prepared_on_connection(
            &mut conn,
            &first,
            r#"{"base_commit":"abc","checkpoint":null}"#,
            &[event_json("event-1")],
            None,
            21,
        )
        .unwrap();
        mark_working_log_applied_on_connection(&mut conn, &first, 22).unwrap();

        let retry = claim_due_on_connection(&mut conn, 30, 10).unwrap().unwrap();
        assert!(retry.working_log_applied);
        assert!(complete_on_connection(&mut conn, &retry, 31).unwrap());
        assert_eq!(count_outstanding_on_connection(&mut conn).unwrap(), 0);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn outbox_failure_rolls_back_done_transition_and_all_metric_rows() {
        let mut conn = setup();
        complete_pre(&mut conn, "repo", "call");
        let spec = spec("repo", "call", "post", "evidence");
        enqueue_on_connection(&mut conn, &spec, 10).unwrap();
        let claimed = claim_due_on_connection(&mut conn, 20, 60).unwrap().unwrap();
        persist_prepared_on_connection(
            &mut conn,
            &claimed,
            r#"{"base_commit":"abc","checkpoint":null}"#,
            &[event_json("event-1"), event_json("event-2")],
            None,
            21,
        )
        .unwrap();
        mark_working_log_applied_on_connection(&mut conn, &claimed, 22).unwrap();
        conn.execute_batch(
            r#"
            CREATE TRIGGER reject_second_checkpoint_metric
            BEFORE INSERT ON metrics
            WHEN NEW.event_json LIKE '%event-2%'
            BEGIN
                SELECT RAISE(ABORT, 'injected outbox failure');
            END;
            "#,
        )
        .unwrap();

        assert!(complete_on_connection(&mut conn, &claimed, 23).is_err());
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
        assert_eq!(
            status_on_connection(&mut conn, &claimed.job_key).unwrap(),
            Some(DeferredCheckpointJobStatus::Processing)
        );
    }

    #[test]
    fn done_tombstone_deduplicates_ack_loss_after_payload_compaction() {
        let mut conn = setup();
        complete_pre(&mut conn, "repo", "call");
        let spec = spec("repo", "call", "post", "evidence");
        enqueue_on_connection(&mut conn, &spec, 10).unwrap();
        let claimed = claim_due_on_connection(&mut conn, 20, 60).unwrap().unwrap();
        persist_prepared_on_connection(
            &mut conn,
            &claimed,
            r#"{"base_commit":"abc","checkpoint":null}"#,
            &[event_json("event-1")],
            None,
            21,
        )
        .unwrap();
        mark_working_log_applied_on_connection(&mut conn, &claimed, 22).unwrap();
        complete_on_connection(&mut conn, &claimed, 23).unwrap();
        compact_done_payloads_on_connection(&mut conn, 100).unwrap();

        assert!(!enqueue_on_connection(&mut conn, &spec, 24).unwrap());
        assert_eq!(
            status_on_connection(&mut conn, &spec.job_key).unwrap(),
            Some(DeferredCheckpointJobStatus::Done)
        );
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn completion_populates_metric_metadata_columns_in_same_transaction() {
        let mut conn = setup();
        complete_pre(&mut conn, "repo", "call");
        let spec = spec("repo", "call", "post", "evidence");
        enqueue_on_connection(&mut conn, &spec, 10).unwrap();
        let claimed = claim_due_on_connection(&mut conn, 20, 60).unwrap().unwrap();
        persist_prepared_on_connection(
            &mut conn,
            &claimed,
            r#"{"base_commit":"abc","checkpoint":null}"#,
            &[metadata_event_json("event-with-metadata")],
            None,
            21,
        )
        .unwrap();
        mark_working_log_applied_on_connection(&mut conn, &claimed, 22).unwrap();
        complete_on_connection(&mut conn, &claimed, 23).unwrap();

        let cached: (i64, i64, String, String, String, String) = conn
            .query_row(
                "SELECT event_ts, event_kind, trace_id, session_id, tool, external_tool_use_id FROM metrics WHERE event_json LIKE '%event-with-metadata%'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
            )
            .unwrap();
        assert_eq!(cached.0, 100);
        assert_eq!(cached.1, MetricEventId::Checkpoint as i64);
        assert_eq!(cached.2, "checkpoint-job:trace");
        assert_eq!(cached.3, "local-session");
        assert_eq!(cached.4, "kilo");
        assert_eq!(cached.5, "call");
    }
}
