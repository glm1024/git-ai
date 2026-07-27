//! Crash-safe deferred computation for formal committed metrics.
//!
//! Post-commit authorship notes must be written promptly, but computing the
//! complete Event 1 payload can be expensive for very large commits and merge
//! conflict resolutions.  This module keeps that work in a durable SQLite job
//! table.  A completed event and the job's `done` transition are committed in
//! the same transaction, so a crash can expose neither an incomplete event nor
//! a completed job without its event.

use crate::authorship::authorship_log::LineRange;
use crate::authorship::authorship_log_serialization::AuthorshipLog;
use crate::authorship::diff_base::single_commit_diff_base;
use crate::error::GitAiError;
use crate::git::repository::{
    InternalGitProfile, Repository, discover_repository_in_path_no_git_exec, exec_git_with_profile,
    from_bare_repository,
};
use crate::metrics::PosEncoded;
use crate::metrics::db::MetricsDatabase;
use crate::metrics::events::committed_pos;
use crate::metrics::types::{MetricEvent, MetricEventId, MetricsBatch, SparseArray};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const DEFERRED_COMMIT_JOBS_SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS deferred_commit_metric_jobs (
    job_key TEXT PRIMARY KEY NOT NULL,
    job_kind TEXT NOT NULL CHECK (job_kind IN ('single_parent', 'merge_novel')),
    repo_identity TEXT NOT NULL,
    repository_workdir TEXT NOT NULL,
    git_dir TEXT NOT NULL,
    git_common_dir TEXT NOT NULL,
    commit_sha TEXT NOT NULL,
    parent_sha TEXT NOT NULL,
    human_author TEXT NOT NULL,
    authorship_note TEXT NOT NULL,
    parent_authorship_note TEXT NOT NULL DEFAULT '',
    first_checkpoint_ts INTEGER,
    attrs_json TEXT NOT NULL,
    ignore_patterns_json TEXT NOT NULL,
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

CREATE UNIQUE INDEX IF NOT EXISTS deferred_commit_metric_jobs_repo_commit_kind
    ON deferred_commit_metric_jobs (repo_identity, commit_sha, job_kind);

CREATE INDEX IF NOT EXISTS deferred_commit_metric_jobs_due
    ON deferred_commit_metric_jobs (state, next_retry_at, created_at)
    WHERE state != 'done';
"#;

const PROCESSING_LEASE_SECS: u64 = 10 * 60;
const INITIAL_RETRY_BACKOFF_SECS: u64 = 5;
const MAX_RETRY_BACKOFF_SECS: u64 = 60 * 60;
const MAX_PERIODIC_JOBS_PER_PASS: usize = 1;
const MAX_HUNKS_PER_BUNDLE_CHUNK: usize = 512;
const MAX_COMMIT_BUNDLE_CHUNKS: usize = 128;
const MAX_HUNKS_PER_COMMIT_BUNDLE: usize = MAX_HUNKS_PER_BUNDLE_CHUNK * MAX_COMMIT_BUNDLE_CHUNKS;
const MAX_COMMIT_BUNDLE_HUNKS_JSON_BYTES: usize = 64 * 1024 * 1024;
const MAX_COMMIT_BUNDLE_EVENT_JSON_BYTES: usize = 64 * 1024 * 1024;
const DONE_JOB_COMPACTION_BATCH_SIZE: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeferredCommitMetricKind {
    SingleParent,
    MergeNovel,
}

impl DeferredCommitMetricKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::SingleParent => "single_parent",
            Self::MergeNovel => "merge_novel",
        }
    }

    fn parse(value: &str) -> Result<Self, GitAiError> {
        match value {
            "single_parent" => Ok(Self::SingleParent),
            "merge_novel" => Ok(Self::MergeNovel),
            other => Err(GitAiError::Generic(format!(
                "unknown deferred commit metric job kind: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DeferredCommitMetricJobSpec {
    pub kind: DeferredCommitMetricKind,
    pub repo_identity: String,
    pub repository_workdir: String,
    pub git_dir: String,
    pub git_common_dir: String,
    pub commit_sha: String,
    pub parent_sha: String,
    pub human_author: String,
    pub authorship_note: String,
    pub parent_authorship_note: String,
    pub first_checkpoint_ts: Option<u64>,
    pub attrs_json: String,
    pub ignore_patterns_json: String,
    pub event_ts: u32,
}

impl DeferredCommitMetricJobSpec {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_commit(
        repo: &Repository,
        kind: DeferredCommitMetricKind,
        commit_sha: &str,
        parent_sha: &str,
        human_author: &str,
        authorship_note: &str,
        parent_authorship_note: &str,
        first_checkpoint_ts: Option<u64>,
        attrs: &crate::metrics::EventAttributes,
        ignore_patterns: &[String],
    ) -> Result<Self, GitAiError> {
        let repository_workdir = repo.workdir()?;
        let git_dir = repo.path().to_path_buf();
        let git_common_dir = repo.common_dir().to_path_buf();
        let repo_identity = repository_identity(&git_common_dir);
        let attrs_json = serde_json::to_string(&attrs.to_sparse())?;
        let ignore_patterns_json = serde_json::to_string(ignore_patterns)?;

        Ok(Self {
            kind,
            repo_identity,
            repository_workdir: repository_workdir.to_string_lossy().to_string(),
            git_dir: git_dir.to_string_lossy().to_string(),
            git_common_dir: git_common_dir.to_string_lossy().to_string(),
            commit_sha: commit_sha.to_string(),
            parent_sha: parent_sha.to_string(),
            human_author: human_author.to_string(),
            authorship_note: authorship_note.to_string(),
            parent_authorship_note: parent_authorship_note.to_string(),
            first_checkpoint_ts,
            attrs_json,
            ignore_patterns_json,
            event_ts: unix_now().min(u64::from(u32::MAX)) as u32,
        })
    }

    fn job_key(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.repo_identity.as_bytes());
        hasher.update(b"\0");
        hasher.update(self.commit_sha.as_bytes());
        hasher.update(b"\0");
        hasher.update(self.kind.as_str().as_bytes());
        format!("{:x}", hasher.finalize())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ClaimedDeferredCommitMetricJob {
    pub job_key: String,
    pub lease_token: String,
    pub kind: DeferredCommitMetricKind,
    pub repo_identity: String,
    pub repository_workdir: String,
    pub git_dir: String,
    pub git_common_dir: String,
    pub commit_sha: String,
    pub parent_sha: String,
    pub authorship_note: String,
    pub parent_authorship_note: String,
    pub first_checkpoint_ts: Option<u64>,
    pub attrs_json: String,
    pub ignore_patterns_json: String,
    pub event_ts: u32,
    pub attempts: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeferredCommitMetricProcessSummary {
    pub completed: usize,
    pub failed: usize,
}

pub(crate) fn enqueue(spec: &DeferredCommitMetricJobSpec) -> Result<bool, GitAiError> {
    let db = MetricsDatabase::global()?;
    let mut db = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    enqueue_on_connection(db.deferred_jobs_connection(), spec, unix_now())
}

pub(crate) fn count_outstanding() -> Result<usize, GitAiError> {
    let db = MetricsDatabase::global()?;
    let mut db = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    count_outstanding_on_connection(db.deferred_jobs_connection())
}

pub(crate) fn process_periodic_jobs() -> DeferredCommitMetricProcessSummary {
    compact_done_jobs_global();
    process_due_jobs(MAX_PERIODIC_JOBS_PER_PASS)
}

pub(crate) fn process_jobs_for_await() -> DeferredCommitMetricProcessSummary {
    // `git-ai await` promises to drain all work that is currently executable.
    // Failed jobs move to a future backoff deadline, so this loop still stops
    // promptly once no due job remains.
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
        tracing::warn!(%error, "deferred commit metrics: failed to compact completed job payloads");
    }
}

fn process_due_jobs(limit: usize) -> DeferredCommitMetricProcessSummary {
    let mut summary = DeferredCommitMetricProcessSummary::default();

    for _ in 0..limit {
        let claimed = match claim_one_global(unix_now()) {
            Ok(Some(job)) => job,
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(%error, "deferred commit metrics: failed to claim job");
                summary.failed += 1;
                break;
            }
        };

        match compute_complete_events(&claimed) {
            Ok(events) => match complete_global(&claimed, &events, unix_now()) {
                Ok(true) => summary.completed += 1,
                Ok(false) => {
                    tracing::debug!(
                        job_key = %claimed.job_key,
                        "deferred commit metrics: lease was superseded before completion"
                    );
                }
                Err(error) => {
                    let message = error.to_string();
                    if let Err(mark_error) = mark_failed_global(&claimed, &message, unix_now()) {
                        tracing::warn!(
                            job_key = %claimed.job_key,
                            %mark_error,
                            "deferred commit metrics: failed to persist retry state after completion error"
                        );
                    }
                    tracing::warn!(
                        job_key = %claimed.job_key,
                        %error,
                        "deferred commit metrics: failed to atomically complete job"
                    );
                    summary.failed += 1;
                }
            },
            Err(error) => {
                let message = error.to_string();
                if let Err(mark_error) = mark_failed_global(&claimed, &message, unix_now()) {
                    tracing::warn!(
                        job_key = %claimed.job_key,
                        %mark_error,
                        "deferred commit metrics: failed to persist retry state"
                    );
                }
                tracing::warn!(
                    job_key = %claimed.job_key,
                    attempt = claimed.attempts,
                    %error,
                    "deferred commit metrics: computation failed and was deferred"
                );
                summary.failed += 1;
            }
        }
    }

    summary
}

fn claim_one_global(now: u64) -> Result<Option<ClaimedDeferredCommitMetricJob>, GitAiError> {
    let db = MetricsDatabase::global()?;
    let mut db = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    claim_due_on_connection(db.deferred_jobs_connection(), now, PROCESSING_LEASE_SECS)
}

fn complete_global(
    job: &ClaimedDeferredCommitMetricJob,
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

fn mark_failed_global(
    job: &ClaimedDeferredCommitMetricJob,
    error: &str,
    now: u64,
) -> Result<bool, GitAiError> {
    let db = MetricsDatabase::global()?;
    let mut db = db
        .lock()
        .map_err(|_| GitAiError::Generic("metrics DB lock poisoned".to_string()))?;
    mark_failed_on_connection(db.deferred_jobs_connection(), job, error, now)
}

fn compute_complete_events(
    job: &ClaimedDeferredCommitMetricJob,
) -> Result<Vec<MetricEvent>, GitAiError> {
    let repo = reopen_repository(job)?;
    let authorship_log =
        AuthorshipLog::deserialize_from_string(&job.authorship_note).map_err(|error| {
            GitAiError::Generic(format!("invalid persisted authorship note: {error}"))
        })?;
    let parent_authorship_log = if job.kind == DeferredCommitMetricKind::SingleParent
        && !job.parent_authorship_note.is_empty()
    {
        match AuthorshipLog::deserialize_from_string(&job.parent_authorship_note) {
            Ok(note) => Some(note),
            Err(error) => {
                tracing::warn!(
                    commit_sha = %job.commit_sha,
                    parent_sha = %job.parent_sha,
                    %error,
                    "persisted parent authorship note is invalid; deletion provenance remains unknown"
                );
                None
            }
        }
    } else {
        None
    };
    let ignore_patterns: Vec<String> = serde_json::from_str(&job.ignore_patterns_json)?;

    let diff_hunks = match job.kind {
        DeferredCommitMetricKind::SingleParent => {
            let diff_base = single_commit_diff_base(&job.parent_sha, &job.commit_sha);
            crate::commands::diff::get_commit_metric_diff_with_line_numbers(
                &repo,
                &diff_base,
                &job.commit_sha,
            )?
        }
        DeferredCommitMetricKind::MergeNovel => {
            merge_novel_ai_hunks(&repo, &job.commit_sha, &authorship_log)?
        }
    };

    if job.kind == DeferredCommitMetricKind::MergeNovel && diff_hunks.is_empty() {
        // A clean merge, or a merge whose novel result lines have no AI
        // attestation, must not emit Event 1.
        return Ok(Vec::new());
    }

    let stats = if job.kind == DeferredCommitMetricKind::MergeNovel {
        crate::authorship::stats::stats_for_commit_stats_from_hunks_with_merge_flag(
            &ignore_patterns,
            &diff_hunks,
            Some(&authorship_log),
            false,
        )
    } else {
        crate::authorship::stats::stats_for_commit_stats_from_hunks(
            &repo,
            &job.commit_sha,
            &ignore_patterns,
            &diff_hunks,
            Some(&authorship_log),
        )?
    };

    let mut artifacts = if job.kind == DeferredCommitMetricKind::SingleParent {
        crate::commands::diff::build_diff_artifacts_from_hunks_with_parent_note_and_ignore_patterns(
            &repo,
            diff_hunks,
            &job.commit_sha,
            Some(&authorship_log),
            parent_authorship_log
                .as_ref()
                .map(|parent_note| (job.parent_sha.as_str(), parent_note)),
            &ignore_patterns,
        )?
    } else {
        crate::commands::diff::build_diff_artifacts_from_hunks_with_ignore_patterns(
            &repo,
            diff_hunks,
            &job.commit_sha,
            Some(&authorship_log),
            &ignore_patterns,
        )?
    };
    sort_json_hunks_stably(&mut artifacts.json_hunks);
    let full_hunks_sha256 = full_hunks_digest(&artifacts.json_hunks)?;
    let bundle_id = deferred_bundle_id(&job.repo_identity, &job.commit_sha, &full_hunks_sha256);
    let base_values = crate::authorship::post_commit::build_commit_metric_values(
        &repo,
        &job.commit_sha,
        &job.authorship_note,
        &stats,
        job.first_checkpoint_ts,
        "[]",
    );
    let Some(base_values) = base_values else {
        return Ok(Vec::new());
    };

    let attrs: SparseArray = serde_json::from_str(&job.attrs_json)?;
    let attrs = normalize_deferred_commit_attrs(attrs, &job.parent_sha);
    build_bundled_events(
        base_values,
        attrs,
        job.event_ts,
        &bundle_id,
        &full_hunks_sha256,
        artifacts.json_hunks,
    )
}

fn normalize_deferred_commit_attrs(mut attrs: SparseArray, parent_sha: &str) -> SparseArray {
    if parent_sha == "initial" {
        attrs.insert(
            crate::metrics::attrs::attr_pos::BASE_COMMIT_SHA.to_string(),
            Value::Null,
        );
    }
    attrs
}

fn build_bundled_events(
    base_values: crate::metrics::CommittedValues,
    attrs: SparseArray,
    event_ts: u32,
    bundle_id: &str,
    full_hunks_sha256: &str,
    hunks: Vec<crate::commands::diff::DiffJsonHunk>,
) -> Result<Vec<MetricEvent>, GitAiError> {
    let total_hunk_count = hunks.len();
    let mut chunks: Vec<Vec<crate::commands::diff::DiffJsonHunk>> = Vec::new();
    let mut current = Vec::new();

    for hunk in hunks {
        current.push(hunk);
        let within_count = current.len() <= MAX_HUNKS_PER_BUNDLE_CHUNK;
        let within_bytes = if within_count {
            let candidate = bundled_event(
                &base_values,
                &attrs,
                event_ts,
                bundle_id,
                u32::MAX,
                u32::MAX,
                full_hunks_sha256,
                &current,
            )?;
            serialized_single_event_batch_bytes(&candidate)?
                < crate::metrics::db::MAX_METRICS_UPLOAD_BODY_BYTES
        } else {
            false
        };
        if within_count && within_bytes {
            continue;
        }

        let last = current
            .pop()
            .expect("the current chunk contains the hunk just appended");
        if !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
        }
        current.push(last);

        let single = bundled_event(
            &base_values,
            &attrs,
            event_ts,
            bundle_id,
            u32::MAX,
            u32::MAX,
            full_hunks_sha256,
            &current,
        )?;
        let single_bytes = serialized_single_event_batch_bytes(&single)?;
        if single_bytes >= crate::metrics::db::MAX_METRICS_UPLOAD_BODY_BYTES {
            return Err(GitAiError::Generic(format!(
                "deferred commit metric bundle {bundle_id} contains a hunk that cannot fit in one upload event ({single_bytes} bytes, limit is strictly below {} bytes)",
                crate::metrics::db::MAX_METRICS_UPLOAD_BODY_BYTES
            )));
        }
    }

    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        chunks.push(Vec::new());
    }
    validate_bundle_totals(bundle_id, chunks.len(), total_hunk_count, 0, 0)?;

    let chunk_count = u32::try_from(chunks.len()).map_err(|_| {
        GitAiError::Generic(format!(
            "deferred commit metric bundle {bundle_id} has too many chunks"
        ))
    })?;
    let mut events = Vec::with_capacity(chunks.len());
    let mut total_event_json_bytes = 0usize;
    let mut total_hunks_json_bytes = 0usize;
    for (index, chunk) in chunks.iter().enumerate() {
        let chunk_index = u32::try_from(index).map_err(|_| {
            GitAiError::Generic(format!(
                "deferred commit metric bundle {bundle_id} has too many chunks"
            ))
        })?;
        let event = bundled_event(
            &base_values,
            &attrs,
            event_ts,
            bundle_id,
            chunk_index,
            chunk_count,
            full_hunks_sha256,
            chunk,
        )?;
        let event_bytes = serialized_single_event_batch_bytes(&event)?;
        if event_bytes >= crate::metrics::db::MAX_METRICS_UPLOAD_BODY_BYTES {
            return Err(GitAiError::Generic(format!(
                "deferred commit metric bundle {bundle_id} chunk {chunk_index}/{chunk_count} is too large ({event_bytes} bytes, limit is strictly below {} bytes)",
                crate::metrics::db::MAX_METRICS_UPLOAD_BODY_BYTES
            )));
        }
        let raw_event_bytes = serde_json::to_vec(&event)?.len();
        total_event_json_bytes = total_event_json_bytes
            .checked_add(raw_event_bytes)
            .ok_or_else(|| {
                GitAiError::Generic(format!(
                    "deferred commit metric bundle {bundle_id} event payload size overflowed"
                ))
            })?;
        let hunks_json_bytes = required_bundle_string(&event, committed_pos::HUNKS, "hunks")?.len();
        total_hunks_json_bytes = total_hunks_json_bytes
            .checked_add(hunks_json_bytes)
            .ok_or_else(|| {
                GitAiError::Generic(format!(
                    "deferred commit metric bundle {bundle_id} hunk JSON size overflowed"
                ))
            })?;
        validate_bundle_totals(
            bundle_id,
            chunks.len(),
            total_hunk_count,
            total_event_json_bytes,
            total_hunks_json_bytes,
        )?;
        events.push(event);
    }
    Ok(events)
}

#[allow(clippy::too_many_arguments)]
fn bundled_event(
    base_values: &crate::metrics::CommittedValues,
    attrs: &SparseArray,
    event_ts: u32,
    bundle_id: &str,
    bundle_index: u32,
    bundle_count: u32,
    full_hunks_sha256: &str,
    hunks: &[crate::commands::diff::DiffJsonHunk],
) -> Result<MetricEvent, GitAiError> {
    let hunks_json = serde_json::to_string(hunks)?;
    let values = base_values
        .clone()
        .hunks(hunks_json)
        .bundle_id(bundle_id)
        .bundle_index(bundle_index)
        .bundle_count(bundle_count)
        .bundle_hunks_sha256(full_hunks_sha256);
    Ok(MetricEvent::from_values_with_timestamp(
        values,
        attrs.clone(),
        Some(event_ts),
    ))
}

fn serialized_single_event_batch_bytes(event: &MetricEvent) -> Result<usize, GitAiError> {
    Ok(serde_json::to_vec(&MetricsBatch::new(vec![event.clone()]))?.len())
}

fn sort_json_hunks_stably(hunks: &mut [crate::commands::diff::DiffJsonHunk]) {
    hunks.sort_by(|left, right| {
        (
            &left.file_path,
            left.start_line,
            left.end_line,
            &left.hunk_kind,
            &left.content_hash,
            &left.commit_sha,
            &left.original_commit_sha,
            &left.prompt_id,
            &left.session_id,
            &left.human_id,
        )
            .cmp(&(
                &right.file_path,
                right.start_line,
                right.end_line,
                &right.hunk_kind,
                &right.content_hash,
                &right.commit_sha,
                &right.original_commit_sha,
                &right.prompt_id,
                &right.session_id,
                &right.human_id,
            ))
    });
}

fn deferred_bundle_id(repo_identity: &str, commit_sha: &str, full_hunks_sha256: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(repo_identity.as_bytes());
    hasher.update(b"\0");
    hasher.update(commit_sha.as_bytes());
    hasher.update(b"\0");
    hasher.update(full_hunks_sha256.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

struct BoundedSha256Writer {
    hasher: Sha256,
    written: usize,
    limit: usize,
    label: &'static str,
}

impl Write for BoundedSha256Writer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self
            .written
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other(format!("{} size overflowed", self.label)))?;
        if next > self.limit {
            return Err(io::Error::other(format!(
                "{} exceeds {} bytes",
                self.label, self.limit
            )));
        }
        self.hasher.update(bytes);
        self.written = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn sha256_json_with_limit<T: serde::Serialize>(
    value: &T,
    limit: usize,
    label: &'static str,
) -> Result<(String, usize), GitAiError> {
    let mut writer = BoundedSha256Writer {
        hasher: Sha256::new(),
        written: 0,
        limit,
        label,
    };
    serde_json::to_writer(&mut writer, value)?;
    Ok((format!("{:x}", writer.hasher.finalize()), writer.written))
}

fn full_hunks_digest(hunks: &[crate::commands::diff::DiffJsonHunk]) -> Result<String, GitAiError> {
    if hunks.len() > MAX_HUNKS_PER_COMMIT_BUNDLE {
        return Err(GitAiError::Generic(format!(
            "deferred commit metric has {} hunks; bundle limit is {}",
            hunks.len(),
            MAX_HUNKS_PER_COMMIT_BUNDLE
        )));
    }
    let (digest, _) =
        sha256_json_with_limit(&hunks, MAX_COMMIT_BUNDLE_HUNKS_JSON_BYTES, "full hunk JSON")?;
    Ok(digest)
}

fn validate_bundle_totals(
    bundle_id: &str,
    chunk_count: usize,
    hunk_count: usize,
    event_json_bytes: usize,
    hunks_json_bytes: usize,
) -> Result<(), GitAiError> {
    if chunk_count > MAX_COMMIT_BUNDLE_CHUNKS
        || hunk_count > MAX_HUNKS_PER_COMMIT_BUNDLE
        || event_json_bytes > MAX_COMMIT_BUNDLE_EVENT_JSON_BYTES
        || hunks_json_bytes > MAX_COMMIT_BUNDLE_HUNKS_JSON_BYTES
    {
        return Err(GitAiError::Generic(format!(
            "deferred commit metric bundle {bundle_id} exceeds total limits \
             ({chunk_count}/{} chunks, {hunk_count}/{} hunks, event JSON \
             {event_json_bytes}/{} bytes, hunk JSON {hunks_json_bytes}/{} bytes)",
            MAX_COMMIT_BUNDLE_CHUNKS,
            MAX_HUNKS_PER_COMMIT_BUNDLE,
            MAX_COMMIT_BUNDLE_EVENT_JSON_BYTES,
            MAX_COMMIT_BUNDLE_HUNKS_JSON_BYTES
        )));
    }
    Ok(())
}

fn reopen_repository(job: &ClaimedDeferredCommitMetricJob) -> Result<Repository, GitAiError> {
    let mut attempted = HashSet::new();
    for raw in [&job.repository_workdir, &job.git_dir] {
        let path = PathBuf::from(raw);
        if !path.exists() || !attempted.insert(path.clone()) {
            continue;
        }
        if let Ok(repo) = discover_repository_in_path_no_git_exec(&path)
            && commit_is_available(&repo, &job.commit_sha)
        {
            return Ok(repo);
        }
    }

    let common_dir = PathBuf::from(&job.git_common_dir);
    if common_dir.exists()
        && let Ok(repo) = from_bare_repository(&common_dir)
        && commit_is_available(&repo, &job.commit_sha)
    {
        return Ok(repo);
    }

    Err(GitAiError::Generic(format!(
        "repository for deferred commit {} is unavailable (workdir={}, git_dir={}, common_dir={})",
        job.commit_sha, job.repository_workdir, job.git_dir, job.git_common_dir
    )))
}

fn commit_is_available(repo: &Repository, commit_sha: &str) -> bool {
    repo.revparse_single(commit_sha)
        .and_then(|object| object.peel_to_commit())
        .is_ok()
}

fn merge_novel_ai_hunks(
    repo: &Repository,
    commit_sha: &str,
    authorship_log: &AuthorshipLog,
) -> Result<Vec<crate::commands::diff::DiffHunk>, GitAiError> {
    let commit = repo.find_commit(commit_sha.to_string())?;
    let parent_count = commit.parent_count()?;
    if parent_count < 2 {
        return Err(GitAiError::Generic(format!(
            "merge-novel metric job references non-merge commit {commit_sha}"
        )));
    }

    let mut args = repo.global_args_for_exec();
    args.extend([
        "diff-tree".to_string(),
        "--no-commit-id".to_string(),
        "-c".to_string(),
        "--combined-all-paths".to_string(),
        "-p".to_string(),
        "-U0".to_string(),
        "--no-color".to_string(),
        "--no-ext-diff".to_string(),
        "--no-textconv".to_string(),
        // Formal commit evidence uses the same explicit 50% rename threshold
        // as the ordinary committed diff. Without rename detection, a file
        // renamed only while resolving a merge appears absent from every
        // parent and all unchanged AI lines can be misclassified as novel.
        "--find-renames=50%".to_string(),
        commit_sha.to_string(),
        "--".to_string(),
    ]);
    let output = exec_git_with_profile(&args, InternalGitProfile::PatchParse)?;
    let combined_diff = String::from_utf8(output.stdout)?;
    let hunks = parse_combined_novel_result_hunks(&combined_diff, parent_count)?;
    Ok(intersect_hunks_with_ai_attestations(hunks, authorship_log))
}

fn parse_combined_novel_result_hunks(
    combined_diff: &str,
    parent_count: usize,
) -> Result<Vec<crate::commands::diff::DiffHunk>, GitAiError> {
    if parent_count < 2 {
        return Err(GitAiError::Generic(
            "combined diff requires at least two parents".to_string(),
        ));
    }

    let mut result = Vec::new();
    let mut current_file = String::new();
    let mut current_hunk: Option<crate::commands::diff::DiffHunk> = None;
    let mut result_line = 0u32;

    let flush = |result: &mut Vec<crate::commands::diff::DiffHunk>,
                 current_hunk: &mut Option<crate::commands::diff::DiffHunk>| {
        if let Some(hunk) = current_hunk.take()
            && !hunk.added_lines.is_empty()
        {
            result.push(hunk);
        }
    };

    for line in combined_diff.lines() {
        if line.starts_with("diff --combined ") || line.starts_with("diff --cc ") {
            flush(&mut result, &mut current_hunk);
            current_file.clear();
            continue;
        }

        if current_hunk.is_none()
            && let Some(path) =
                crate::commands::diff::parse_new_file_path_from_plus_header_line(line)
        {
            current_file = path.unwrap_or_default();
            continue;
        }

        if line.starts_with('@') {
            let marker_width = line.chars().take_while(|ch| *ch == '@').count();
            if marker_width < 3 {
                continue;
            }
            if marker_width != parent_count + 1 {
                return Err(GitAiError::Generic(format!(
                    "combined diff parent-column mismatch: expected {}, found {} in {line}",
                    parent_count,
                    marker_width.saturating_sub(1)
                )));
            }

            flush(&mut result, &mut current_hunk);
            result_line = parse_combined_result_start(line, marker_width, parent_count)?;
            current_hunk = Some(crate::commands::diff::DiffHunk {
                file_path: current_file.clone(),
                old_file_path: None,
                old_start: 0,
                old_count: 0,
                new_start: result_line,
                new_count: 0,
                deleted_lines: Vec::new(),
                added_lines: Vec::new(),
                deleted_contents: Vec::new(),
                added_contents: Vec::new(),
            });
            continue;
        }

        let Some(hunk) = current_hunk.as_mut() else {
            continue;
        };
        if line.starts_with('\\') || line.len() < parent_count {
            continue;
        }

        let prefix = &line.as_bytes()[..parent_count];
        if !prefix
            .iter()
            .all(|marker| matches!(*marker, b' ' | b'+' | b'-'))
        {
            continue;
        }

        let appears_in_result = !prefix.contains(&b'-');
        if appears_in_result && prefix.iter().all(|marker| *marker == b'+') {
            hunk.added_lines.push(result_line);
            hunk.added_contents.push(line[parent_count..].to_string());
        }
        if appears_in_result {
            result_line = result_line.saturating_add(1);
        }
    }

    flush(&mut result, &mut current_hunk);
    Ok(result)
}

fn parse_combined_result_start(
    header: &str,
    marker_width: usize,
    parent_count: usize,
) -> Result<u32, GitAiError> {
    let rest = header
        .get(marker_width..)
        .ok_or_else(|| GitAiError::Generic(format!("invalid combined diff header: {header}")))?;
    let ranges = rest.split_whitespace().collect::<Vec<_>>();
    if ranges.len() < parent_count + 1 {
        return Err(GitAiError::Generic(format!(
            "combined diff header has too few ranges: {header}"
        )));
    }
    for parent in &ranges[..parent_count] {
        if !parent.starts_with('-') {
            return Err(GitAiError::Generic(format!(
                "invalid combined diff parent range in: {header}"
            )));
        }
    }
    let result = ranges[parent_count]
        .strip_prefix('+')
        .ok_or_else(|| GitAiError::Generic(format!("missing result range in: {header}")))?;
    let start = result.split(',').next().unwrap_or_default();
    start
        .parse::<u32>()
        .map_err(|error| GitAiError::Generic(format!("invalid result range in {header}: {error}")))
}

fn intersect_hunks_with_ai_attestations(
    hunks: Vec<crate::commands::diff::DiffHunk>,
    authorship_log: &AuthorshipLog,
) -> Vec<crate::commands::diff::DiffHunk> {
    let ai_ranges = ai_attestation_ranges_by_file(authorship_log);
    let mut result = Vec::new();

    for mut hunk in hunks {
        let Some(ranges) = ai_ranges.get(&hunk.file_path) else {
            continue;
        };
        let mut lines = Vec::new();
        let mut contents = Vec::new();
        for (line, content) in hunk.added_lines.into_iter().zip(hunk.added_contents) {
            if ranges.iter().any(|range| range.contains(line)) {
                lines.push(line);
                contents.push(content);
            }
        }
        if lines.is_empty() {
            continue;
        }
        hunk.new_start = lines[0];
        hunk.new_count = lines.len() as u32;
        hunk.added_lines = lines;
        hunk.added_contents = contents;
        result.push(hunk);
    }

    result
}

fn ai_attestation_ranges_by_file(
    authorship_log: &AuthorshipLog,
) -> HashMap<String, Vec<LineRange>> {
    let mut by_file: HashMap<String, Vec<LineRange>> = HashMap::new();
    for file in &authorship_log.attestations {
        for entry in &file.entries {
            let is_ai = if entry.hash.starts_with("s_") {
                let session_key = entry.hash.split("::").next().unwrap_or(&entry.hash);
                authorship_log.metadata.sessions.contains_key(session_key)
            } else if entry.hash.starts_with("h_") {
                false
            } else {
                authorship_log.metadata.prompts.contains_key(&entry.hash)
            };
            if is_ai {
                by_file
                    .entry(file.file_path.clone())
                    .or_default()
                    .extend(entry.line_ranges.iter().cloned());
            }
        }
    }
    by_file
}

fn repository_identity(git_common_dir: &Path) -> String {
    let stable_path = git_common_dir
        .canonicalize()
        .unwrap_or_else(|_| git_common_dir.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(stable_path.to_string_lossy().as_bytes());
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

pub(crate) fn enqueue_on_connection(
    conn: &mut Connection,
    spec: &DeferredCommitMetricJobSpec,
    now: u64,
) -> Result<bool, GitAiError> {
    let inserted = conn.execute(
        r#"
        INSERT INTO deferred_commit_metric_jobs (
            job_key, job_kind, repo_identity, repository_workdir, git_dir, git_common_dir,
            commit_sha, parent_sha, human_author, authorship_note, parent_authorship_note,
            first_checkpoint_ts, attrs_json, ignore_patterns_json, event_ts, state, attempts,
            next_retry_at, created_at, updated_at
        ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
            ?15, 'pending', 0, 0, ?16, ?16
        )
        ON CONFLICT(job_key) DO NOTHING
        "#,
        params![
            spec.job_key(),
            spec.kind.as_str(),
            spec.repo_identity,
            spec.repository_workdir,
            spec.git_dir,
            spec.git_common_dir,
            spec.commit_sha,
            spec.parent_sha,
            spec.human_author,
            spec.authorship_note,
            spec.parent_authorship_note,
            spec.first_checkpoint_ts.map(u64_to_sqlite),
            spec.attrs_json,
            spec.ignore_patterns_json,
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
) -> Result<Option<ClaimedDeferredCommitMetricJob>, GitAiError> {
    let tx = conn.transaction()?;
    let expired_before = now.saturating_sub(lease_secs);
    let job_key: Option<String> = tx
        .query_row(
            r#"
            SELECT job_key
            FROM deferred_commit_metric_jobs
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
        UPDATE deferred_commit_metric_jobs
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
) -> Result<ClaimedDeferredCommitMetricJob, GitAiError> {
    tx.query_row(
        r#"
        SELECT job_kind, repo_identity, repository_workdir, git_dir, git_common_dir,
               commit_sha, parent_sha, authorship_note, parent_authorship_note,
               first_checkpoint_ts, attrs_json, ignore_patterns_json, event_ts, attempts
        FROM deferred_commit_metric_jobs
        WHERE job_key = ?1 AND state = 'processing' AND lease_token = ?2
        "#,
        params![job_key, lease_token],
        |row| {
            let kind: String = row.get(0)?;
            let first_checkpoint_ts: Option<i64> = row.get(9)?;
            let event_ts: i64 = row.get(12)?;
            let attempts: i64 = row.get(13)?;
            Ok((
                kind,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                first_checkpoint_ts,
                row.get::<_, String>(10)?,
                row.get::<_, String>(11)?,
                event_ts,
                attempts,
            ))
        },
    )
    .map_err(GitAiError::from)
    .and_then(
        |(
            kind,
            repo_identity,
            repository_workdir,
            git_dir,
            git_common_dir,
            commit_sha,
            parent_sha,
            authorship_note,
            parent_authorship_note,
            first_checkpoint_ts,
            attrs_json,
            ignore_patterns_json,
            event_ts,
            attempts,
        )| {
            Ok(ClaimedDeferredCommitMetricJob {
                job_key: job_key.to_string(),
                lease_token: lease_token.to_string(),
                kind: DeferredCommitMetricKind::parse(&kind)?,
                repo_identity,
                repository_workdir,
                git_dir,
                git_common_dir,
                commit_sha,
                parent_sha,
                authorship_note,
                parent_authorship_note,
                first_checkpoint_ts: first_checkpoint_ts.map(|value| value.max(0) as u64),
                attrs_json,
                ignore_patterns_json,
                event_ts: event_ts.max(0).min(i64::from(u32::MAX)) as u32,
                attempts: attempts.max(0).min(i64::from(u32::MAX)) as u32,
            })
        },
    )
}

pub(crate) fn mark_failed_on_connection(
    conn: &mut Connection,
    job: &ClaimedDeferredCommitMetricJob,
    error: &str,
    now: u64,
) -> Result<bool, GitAiError> {
    let next_retry_at = now.saturating_add(retry_backoff_seconds(job.attempts));
    let updated = conn.execute(
        r#"
        UPDATE deferred_commit_metric_jobs
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
    job: &ClaimedDeferredCommitMetricJob,
    event_jsons: &[String],
    now: u64,
) -> Result<bool, GitAiError> {
    validate_event_bundle(job, event_jsons)?;

    let tx = conn.transaction()?;
    let owns_lease: bool = tx.query_row(
        r#"
        SELECT EXISTS(
            SELECT 1
            FROM deferred_commit_metric_jobs
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
        let value: Value = serde_json::from_str(event_json)?;
        let event_ts = value.get("t").and_then(Value::as_u64).unwrap_or(0);
        let event_kind = value.get("e").and_then(Value::as_u64).unwrap_or(0);
        if event_kind != u64::from(MetricEventId::Committed as u16) {
            return Err(GitAiError::Generic(format!(
                "deferred commit job produced non-committed event kind {event_kind}"
            )));
        }
        tx.execute(
            "INSERT INTO metrics (event_json, event_ts, event_kind) VALUES (?1, ?2, ?3)",
            params![
                event_json,
                u64_to_sqlite(event_ts),
                u64_to_sqlite(event_kind)
            ],
        )?;
        metric_ids.push(tx.last_insert_rowid());
    }

    let metric_ids_json = serde_json::to_string(&metric_ids)?;
    let updated = tx.execute(
        r#"
        UPDATE deferred_commit_metric_jobs
        SET state = 'done',
            processing_started_at = NULL,
            lease_token = NULL,
            last_error = NULL,
            metric_ids_json = ?3,
            repository_workdir = '',
            git_dir = '',
            git_common_dir = '',
            parent_sha = '',
            human_author = '',
            authorship_note = '',
            parent_authorship_note = '',
            first_checkpoint_ts = NULL,
            attrs_json = '',
            ignore_patterns_json = '',
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

/// Keep the idempotency tombstone for completed jobs while removing paths,
/// authorship evidence, and projection inputs that are no longer needed.
///
/// This is deliberately bounded so opening an older database cannot turn into
/// an unbounded migration pause. Startup and periodic processing repeat the
/// batch until every historical `done` row is compact.
pub(crate) fn compact_done_payloads_on_connection(
    conn: &mut Connection,
    limit: usize,
) -> Result<usize, GitAiError> {
    if limit == 0 {
        return Ok(0);
    }
    let compacted = conn.execute(
        r#"
        UPDATE deferred_commit_metric_jobs
        SET repository_workdir = '',
            git_dir = '',
            git_common_dir = '',
            parent_sha = '',
            human_author = '',
            authorship_note = '',
            parent_authorship_note = '',
            first_checkpoint_ts = NULL,
            attrs_json = '',
            ignore_patterns_json = '',
            event_ts = 0
        WHERE job_key IN (
            SELECT job_key
            FROM deferred_commit_metric_jobs
            WHERE state = 'done'
              AND (
                  repository_workdir != ''
                  OR git_dir != ''
                  OR git_common_dir != ''
                  OR parent_sha != ''
                  OR human_author != ''
                  OR authorship_note != ''
                  OR parent_authorship_note != ''
                  OR first_checkpoint_ts IS NOT NULL
                  OR attrs_json != ''
                  OR ignore_patterns_json != ''
                  OR event_ts != 0
              )
            ORDER BY completed_at ASC, job_key ASC
            LIMIT ?1
        )
        "#,
        params![i64::try_from(limit).unwrap_or(i64::MAX)],
    )?;
    Ok(compacted)
}

fn validate_event_bundle(
    job: &ClaimedDeferredCommitMetricJob,
    event_jsons: &[String],
) -> Result<(), GitAiError> {
    if event_jsons.is_empty() {
        return Ok(());
    }
    if event_jsons.len() > MAX_COMMIT_BUNDLE_CHUNKS {
        return Err(GitAiError::Generic(format!(
            "deferred commit metric job {} produced {} bundle chunks; limit is {}",
            job.job_key,
            event_jsons.len(),
            MAX_COMMIT_BUNDLE_CHUNKS
        )));
    }
    let total_event_json_bytes = event_jsons.iter().try_fold(0usize, |total, event_json| {
        total.checked_add(event_json.len()).ok_or_else(|| {
            GitAiError::Generic("deferred commit metric bundle event size overflowed".to_string())
        })
    })?;
    if total_event_json_bytes > MAX_COMMIT_BUNDLE_EVENT_JSON_BYTES {
        return Err(GitAiError::Generic(format!(
            "deferred commit metric bundle event JSON is {total_event_json_bytes} bytes; limit is {}",
            MAX_COMMIT_BUNDLE_EVENT_JSON_BYTES
        )));
    }

    let expected_count = u32::try_from(event_jsons.len()).map_err(|_| {
        GitAiError::Generic(format!(
            "deferred commit metric job {} produced too many bundle chunks",
            job.job_key
        ))
    })?;
    let mut expected_bundle_id: Option<String> = None;
    let mut expected_hunks_sha256: Option<String> = None;
    let mut expected_timestamp: Option<u32> = None;
    let mut expected_attrs: Option<SparseArray> = None;
    let mut expected_common_values: Option<SparseArray> = None;
    let mut total_hunks = 0usize;
    let mut total_hunks_json_bytes = 0usize;
    let mut chunks: Vec<Option<Vec<crate::commands::diff::DiffJsonHunk>>> =
        vec![None; event_jsons.len()];

    for event_json in event_jsons {
        let event: MetricEvent = serde_json::from_str(event_json)?;
        if event.event_id != MetricEventId::Committed as u16 {
            return Err(GitAiError::Generic(format!(
                "deferred commit job produced non-committed event kind {}",
                event.event_id
            )));
        }
        let event_bytes = serialized_single_event_batch_bytes(&event)?;
        if event_bytes >= crate::metrics::db::MAX_METRICS_UPLOAD_BODY_BYTES {
            return Err(GitAiError::Generic(format!(
                "deferred commit metric event is too large ({event_bytes} bytes, limit is strictly below {} bytes)",
                crate::metrics::db::MAX_METRICS_UPLOAD_BODY_BYTES
            )));
        }

        let bundle_id = required_bundle_string(&event, committed_pos::BUNDLE_ID, "bundle_id")?;
        let bundle_index =
            required_bundle_u32(&event, committed_pos::BUNDLE_INDEX, "bundle_index")?;
        let bundle_count =
            required_bundle_u32(&event, committed_pos::BUNDLE_COUNT, "bundle_count")?;
        let hunks_sha256 = required_bundle_string(
            &event,
            committed_pos::BUNDLE_HUNKS_SHA256,
            "bundle_hunks_sha256",
        )?;
        if bundle_count != expected_count || bundle_index >= expected_count {
            return Err(GitAiError::Generic(format!(
                "deferred commit metric bundle has invalid index/count {bundle_index}/{bundle_count}; expected count {expected_count}"
            )));
        }
        if let Some(expected) = &expected_bundle_id {
            if expected != bundle_id {
                return Err(GitAiError::Generic(
                    "deferred commit metric bundle ids do not match".to_string(),
                ));
            }
        } else {
            expected_bundle_id = Some(bundle_id.to_string());
        }
        if let Some(expected) = &expected_hunks_sha256 {
            if expected != hunks_sha256 {
                return Err(GitAiError::Generic(
                    "deferred commit metric full-hunk digests do not match".to_string(),
                ));
            }
        } else {
            expected_hunks_sha256 = Some(hunks_sha256.to_string());
        }
        if let Some(expected) = expected_timestamp {
            if expected != event.timestamp {
                return Err(GitAiError::Generic(
                    "deferred commit metric bundle timestamps do not match".to_string(),
                ));
            }
        } else {
            expected_timestamp = Some(event.timestamp);
        }
        if let Some(expected) = &expected_attrs {
            if expected != &event.attrs {
                return Err(GitAiError::Generic(
                    "deferred commit metric bundle attributes do not match".to_string(),
                ));
            }
        } else {
            expected_attrs = Some(event.attrs.clone());
        }

        let mut common_values = event.values.clone();
        common_values.remove(&committed_pos::HUNKS.to_string());
        common_values.remove(&committed_pos::BUNDLE_INDEX.to_string());
        if let Some(expected) = &expected_common_values {
            if expected != &common_values {
                return Err(GitAiError::Generic(
                    "deferred commit metric bundle headline values do not match".to_string(),
                ));
            }
        } else {
            expected_common_values = Some(common_values);
        }

        let hunks_json = required_bundle_string(&event, committed_pos::HUNKS, "hunks")?;
        let chunk: Vec<crate::commands::diff::DiffJsonHunk> = serde_json::from_str(hunks_json)
            .map_err(|error| {
                GitAiError::Generic(format!(
                    "deferred commit metric bundle has invalid hunk JSON: {error}"
                ))
            })?;
        if chunk.len() > MAX_HUNKS_PER_BUNDLE_CHUNK {
            return Err(GitAiError::Generic(format!(
                "deferred commit metric bundle chunk has {} hunks; limit is {}",
                chunk.len(),
                MAX_HUNKS_PER_BUNDLE_CHUNK
            )));
        }
        total_hunks = total_hunks.checked_add(chunk.len()).ok_or_else(|| {
            GitAiError::Generic("deferred commit metric bundle hunk count overflowed".to_string())
        })?;
        total_hunks_json_bytes = total_hunks_json_bytes
            .checked_add(hunks_json.len())
            .ok_or_else(|| {
                GitAiError::Generic(
                    "deferred commit metric bundle hunk JSON size overflowed".to_string(),
                )
            })?;
        if total_hunks > MAX_HUNKS_PER_COMMIT_BUNDLE
            || total_hunks_json_bytes > MAX_COMMIT_BUNDLE_HUNKS_JSON_BYTES
        {
            return Err(GitAiError::Generic(format!(
                "deferred commit metric bundle exceeds total hunk limits \
                 ({total_hunks}/{} hunks, {total_hunks_json_bytes}/{} bytes)",
                MAX_HUNKS_PER_COMMIT_BUNDLE, MAX_COMMIT_BUNDLE_HUNKS_JSON_BYTES
            )));
        }
        if chunk.iter().any(|hunk| hunk.commit_sha != job.commit_sha) {
            return Err(GitAiError::Generic(format!(
                "deferred commit metric bundle contains a hunk for a commit other than {}",
                job.commit_sha
            )));
        }
        let slot = chunks
            .get_mut(bundle_index as usize)
            .expect("bundle index was range-checked");
        if slot.replace(chunk).is_some() {
            return Err(GitAiError::Generic(format!(
                "deferred commit metric bundle repeats chunk index {bundle_index}"
            )));
        }
    }

    let ordered_hunks =
        chunks
            .into_iter()
            .enumerate()
            .try_fold(Vec::new(), |mut all, (index, chunk)| {
                let chunk = chunk.ok_or_else(|| {
                    GitAiError::Generic(format!(
                        "deferred commit metric bundle is missing chunk index {index}"
                    ))
                })?;
                all.extend(chunk);
                Ok::<_, GitAiError>(all)
            })?;
    let actual_hunks_sha256 = sha256_hex(&serde_json::to_vec(&ordered_hunks)?);
    let expected_hunks_sha256 =
        expected_hunks_sha256.expect("a non-empty event bundle always has a full-hunk digest");
    if actual_hunks_sha256 != expected_hunks_sha256 {
        return Err(GitAiError::Generic(format!(
            "deferred commit metric full-hunk digest mismatch: expected {expected_hunks_sha256}, computed {actual_hunks_sha256}"
        )));
    }
    let expected_bundle =
        deferred_bundle_id(&job.repo_identity, &job.commit_sha, &expected_hunks_sha256);
    if expected_bundle_id.as_deref() != Some(expected_bundle.as_str()) {
        return Err(GitAiError::Generic(format!(
            "deferred commit metric bundle id does not match repository/commit evidence for {}",
            job.commit_sha
        )));
    }

    Ok(())
}

fn required_bundle_string<'a>(
    event: &'a MetricEvent,
    position: usize,
    name: &str,
) -> Result<&'a str, GitAiError> {
    event
        .values
        .get(&position.to_string())
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            GitAiError::Generic(format!(
                "deferred commit metric event is missing required {name}"
            ))
        })
}

fn required_bundle_u32(
    event: &MetricEvent,
    position: usize,
    name: &str,
) -> Result<u32, GitAiError> {
    let value = event
        .values
        .get(&position.to_string())
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            GitAiError::Generic(format!(
                "deferred commit metric event is missing required {name}"
            ))
        })?;
    u32::try_from(value)
        .map_err(|_| GitAiError::Generic(format!("deferred commit metric {name} exceeds u32")))
}

pub(crate) fn count_outstanding_on_connection(conn: &mut Connection) -> Result<usize, GitAiError> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM deferred_commit_metric_jobs WHERE state != 'done'",
        [],
        |row| row.get(0),
    )?;
    Ok(count.max(0) as usize)
}

fn u64_to_sqlite(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorship::authorship_log::PromptRecord;
    use crate::authorship::authorship_log_serialization::AttestationEntry;
    use crate::authorship::working_log::AgentId;
    use crate::metrics::EventAttributes;
    use rusqlite::Connection;

    fn test_connection(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
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
        .unwrap();
        conn.execute_batch(DEFERRED_COMMIT_JOBS_SCHEMA_SQL).unwrap();
        conn
    }

    fn spec(kind: DeferredCommitMetricKind) -> DeferredCommitMetricJobSpec {
        DeferredCommitMetricJobSpec {
            kind,
            repo_identity: "repo-id".to_string(),
            repository_workdir: "/tmp/repo".to_string(),
            git_dir: "/tmp/repo/.git".to_string(),
            git_common_dir: "/tmp/repo/.git".to_string(),
            commit_sha: "commit".to_string(),
            parent_sha: "parent".to_string(),
            human_author: "dev@example.com".to_string(),
            authorship_note: "file.rs\n---\n{}".to_string(),
            parent_authorship_note: "parent.rs\n---\n{}".to_string(),
            first_checkpoint_ts: Some(12),
            attrs_json: serde_json::to_string(&EventAttributes::with_version("test").to_sparse())
                .unwrap(),
            ignore_patterns_json: "[]".to_string(),
            event_ts: 100,
        }
    }

    #[test]
    fn root_job_normalizes_legacy_initial_base_commit_to_null() {
        let attrs = EventAttributes::with_version("test")
            .base_commit_sha("initial")
            .to_sparse();

        let normalized = normalize_deferred_commit_attrs(attrs, "initial");

        assert_eq!(
            normalized.get(&crate::metrics::attrs::attr_pos::BASE_COMMIT_SHA.to_string()),
            Some(&serde_json::Value::Null)
        );
    }

    fn json_hunk(commit_sha: &str, index: u32) -> crate::commands::diff::DiffJsonHunk {
        crate::commands::diff::DiffJsonHunk {
            commit_sha: commit_sha.to_string(),
            content_hash: format!("content-{index:04}"),
            hunk_kind: "addition".to_string(),
            original_commit_sha: None,
            start_line: index + 1,
            end_line: index + 1,
            file_path: format!("src/file-{index:04}.rs"),
            prompt_id: Some("prompt".to_string()),
            session_id: None,
            human_id: None,
        }
    }

    fn bundle_events_for_job(
        job: &ClaimedDeferredCommitMetricJob,
        hunk_count: u32,
    ) -> Vec<MetricEvent> {
        let hunks = (0..hunk_count)
            .map(|index| json_hunk(&job.commit_sha, index))
            .collect::<Vec<_>>();
        let digest = sha256_hex(&serde_json::to_vec(&hunks).unwrap());
        let bundle_id = deferred_bundle_id(&job.repo_identity, &job.commit_sha, &digest);
        build_bundled_events(
            crate::metrics::CommittedValues::new().human_additions(hunk_count),
            SparseArray::new(),
            100,
            &bundle_id,
            &digest,
            hunks,
        )
        .unwrap()
    }

    fn committed_event_json(job: &ClaimedDeferredCommitMetricJob) -> String {
        serde_json::to_string(&bundle_events_for_job(job, 0)[0]).unwrap()
    }

    #[test]
    fn enqueue_is_idempotent_and_done_job_is_not_reopened() {
        let temp = tempfile::tempdir().unwrap();
        let mut conn = test_connection(&temp.path().join("metrics.db"));
        let job = spec(DeferredCommitMetricKind::SingleParent);

        assert!(enqueue_on_connection(&mut conn, &job, 10).unwrap());
        assert!(!enqueue_on_connection(&mut conn, &job, 11).unwrap());
        assert_eq!(count_outstanding_on_connection(&mut conn).unwrap(), 1);

        let claimed = claim_due_on_connection(&mut conn, 12, 60).unwrap().unwrap();
        assert_eq!(
            &claimed.parent_authorship_note, &job.parent_authorship_note,
            "the enqueue boundary must snapshot parent-side deletion provenance"
        );
        assert!(
            complete_on_connection(&mut conn, &claimed, &[committed_event_json(&claimed)], 13)
                .unwrap()
        );
        assert!(!enqueue_on_connection(&mut conn, &job, 14).unwrap());
        assert_eq!(count_outstanding_on_connection(&mut conn).unwrap(), 0);
        let metrics: i64 = conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(metrics, 1);
    }

    #[test]
    fn crash_reopen_reclaims_expired_processing_job_without_emitting_event() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("metrics.db");
        {
            let mut conn = test_connection(&path);
            enqueue_on_connection(&mut conn, &spec(DeferredCommitMetricKind::SingleParent), 10)
                .unwrap();
            let first = claim_due_on_connection(&mut conn, 20, 60).unwrap().unwrap();
            assert_eq!(first.attempts, 1);
            assert!(
                claim_due_on_connection(&mut conn, 79, 60)
                    .unwrap()
                    .is_none()
            );
            let metrics: i64 = conn
                .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
                .unwrap();
            assert_eq!(metrics, 0, "claiming never exposes an incomplete event");
        }

        let mut reopened = Connection::open(&path).unwrap();
        let reclaimed = claim_due_on_connection(&mut reopened, 80, 60)
            .unwrap()
            .unwrap();
        assert_eq!(reclaimed.attempts, 2);
        assert_ne!(reclaimed.lease_token, "");
    }

    #[test]
    fn failed_computation_uses_persisted_exponential_backoff() {
        let temp = tempfile::tempdir().unwrap();
        let mut conn = test_connection(&temp.path().join("metrics.db"));
        enqueue_on_connection(&mut conn, &spec(DeferredCommitMetricKind::SingleParent), 10)
            .unwrap();

        let first = claim_due_on_connection(&mut conn, 20, 60).unwrap().unwrap();
        assert!(mark_failed_on_connection(&mut conn, &first, "boom", 20).unwrap());
        assert!(
            claim_due_on_connection(&mut conn, 24, 60)
                .unwrap()
                .is_none()
        );
        let second = claim_due_on_connection(&mut conn, 25, 60).unwrap().unwrap();
        assert_eq!(second.attempts, 2);
        assert!(mark_failed_on_connection(&mut conn, &second, "again", 25).unwrap());
        assert!(
            claim_due_on_connection(&mut conn, 34, 60)
                .unwrap()
                .is_none()
        );
        assert!(
            claim_due_on_connection(&mut conn, 35, 60)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn stale_lease_cannot_insert_duplicate_event() {
        let temp = tempfile::tempdir().unwrap();
        let mut conn = test_connection(&temp.path().join("metrics.db"));
        enqueue_on_connection(&mut conn, &spec(DeferredCommitMetricKind::SingleParent), 10)
            .unwrap();
        let stale = claim_due_on_connection(&mut conn, 20, 10).unwrap().unwrap();
        let current = claim_due_on_connection(&mut conn, 30, 10).unwrap().unwrap();

        assert!(
            !complete_on_connection(&mut conn, &stale, &[committed_event_json(&stale)], 31)
                .unwrap()
        );
        assert!(
            complete_on_connection(&mut conn, &current, &[committed_event_json(&current)], 32)
                .unwrap()
        );
        let metrics: i64 = conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(metrics, 1);
    }

    #[test]
    fn more_than_one_thousand_hunks_are_stably_chunked_below_upload_limit() {
        let temp = tempfile::tempdir().unwrap();
        let mut conn = test_connection(&temp.path().join("metrics.db"));
        enqueue_on_connection(&mut conn, &spec(DeferredCommitMetricKind::SingleParent), 10)
            .unwrap();
        let claimed = claim_due_on_connection(&mut conn, 20, 60).unwrap().unwrap();

        let events = bundle_events_for_job(&claimed, 1001);
        assert_eq!(events.len(), 2);
        for (index, event) in events.iter().enumerate() {
            assert_eq!(
                required_bundle_u32(event, committed_pos::BUNDLE_INDEX, "bundle_index").unwrap(),
                index as u32
            );
            assert_eq!(
                required_bundle_u32(event, committed_pos::BUNDLE_COUNT, "bundle_count").unwrap(),
                2
            );
            let hunks: Vec<crate::commands::diff::DiffJsonHunk> = serde_json::from_str(
                required_bundle_string(event, committed_pos::HUNKS, "hunks").unwrap(),
            )
            .unwrap();
            assert!(hunks.len() <= MAX_HUNKS_PER_BUNDLE_CHUNK);
            assert!(
                serialized_single_event_batch_bytes(event).unwrap()
                    < crate::metrics::db::MAX_METRICS_UPLOAD_BODY_BYTES
            );
        }

        let event_jsons = events
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(complete_on_connection(&mut conn, &claimed, &event_jsons, 21).unwrap());
        let metrics: i64 = conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(metrics, 2);
        let stored_ids: String = conn
            .query_row(
                "SELECT metric_ids_json FROM deferred_commit_metric_jobs WHERE job_key = ?1",
                params![claimed.job_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<i64>>(&stored_ids).unwrap().len(),
            2
        );
    }

    #[test]
    #[allow(clippy::type_complexity)]
    fn completed_job_keeps_only_idempotency_tombstone_and_compacts_legacy_done_rows() {
        let temp = tempfile::tempdir().unwrap();
        let mut conn = test_connection(&temp.path().join("metrics.db"));
        let job = spec(DeferredCommitMetricKind::SingleParent);
        enqueue_on_connection(&mut conn, &job, 10).unwrap();
        let claimed = claim_due_on_connection(&mut conn, 20, 60).unwrap().unwrap();
        assert!(
            complete_on_connection(&mut conn, &claimed, &[committed_event_json(&claimed)], 21)
                .unwrap()
        );

        let tombstone: (
            String,
            String,
            String,
            String,
            String,
            String,
            String,
            String,
            String,
            String,
            Option<i64>,
            String,
            String,
        ) = conn
            .query_row(
                r#"
                SELECT job_key, job_kind, repo_identity, commit_sha, metric_ids_json,
                       repository_workdir, git_dir, git_common_dir, parent_sha,
                       authorship_note, first_checkpoint_ts, attrs_json,
                       ignore_patterns_json
                FROM deferred_commit_metric_jobs
                WHERE job_key = ?1
                "#,
                params![claimed.job_key],
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
                        row.get(8)?,
                        row.get(9)?,
                        row.get(10)?,
                        row.get(11)?,
                        row.get(12)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(tombstone.0, claimed.job_key);
        assert_eq!(tombstone.1, DeferredCommitMetricKind::SingleParent.as_str());
        assert_eq!(tombstone.2, job.repo_identity);
        assert_eq!(tombstone.3, job.commit_sha);
        assert_eq!(
            serde_json::from_str::<Vec<i64>>(&tombstone.4)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            (&tombstone.5, &tombstone.6, &tombstone.7),
            (&"".to_string(), &"".to_string(), &"".to_string())
        );
        assert_eq!(
            (&tombstone.8, &tombstone.9),
            (&"".to_string(), &"".to_string())
        );
        let compacted_parent_note: String = conn
            .query_row(
                "SELECT parent_authorship_note FROM deferred_commit_metric_jobs WHERE job_key = ?1",
                params![claimed.job_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(compacted_parent_note, "");
        assert!(tombstone.10.is_none());
        assert_eq!(
            (&tombstone.11, &tombstone.12),
            (&"".to_string(), &"".to_string())
        );
        assert!(
            !enqueue_on_connection(&mut conn, &job, 22).unwrap(),
            "the compact tombstone must still deduplicate the same commit job"
        );

        let historical = spec(DeferredCommitMetricKind::MergeNovel);
        enqueue_on_connection(&mut conn, &historical, 30).unwrap();
        conn.execute(
            "UPDATE deferred_commit_metric_jobs SET state = 'done', completed_at = 31 WHERE job_key = ?1",
            params![historical.job_key()],
        )
        .unwrap();
        assert_eq!(
            compact_done_payloads_on_connection(&mut conn, 1).unwrap(),
            1
        );
        let compacted_note: String = conn
            .query_row(
                "SELECT authorship_note FROM deferred_commit_metric_jobs WHERE job_key = ?1",
                params![historical.job_key()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(compacted_note, "");
    }

    #[test]
    fn bundle_total_limits_fail_before_unbounded_chunk_or_payload_accumulation() {
        assert!(
            validate_bundle_totals(
                "bundle",
                MAX_COMMIT_BUNDLE_CHUNKS,
                MAX_HUNKS_PER_COMMIT_BUNDLE,
                MAX_COMMIT_BUNDLE_EVENT_JSON_BYTES,
                MAX_COMMIT_BUNDLE_HUNKS_JSON_BYTES,
            )
            .is_ok()
        );
        for result in [
            validate_bundle_totals("bundle", MAX_COMMIT_BUNDLE_CHUNKS + 1, 0, 0, 0),
            validate_bundle_totals("bundle", 1, MAX_HUNKS_PER_COMMIT_BUNDLE + 1, 0, 0),
            validate_bundle_totals("bundle", 1, 1, MAX_COMMIT_BUNDLE_EVENT_JSON_BYTES + 1, 0),
            validate_bundle_totals("bundle", 1, 1, 0, MAX_COMMIT_BUNDLE_HUNKS_JSON_BYTES + 1),
        ] {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("exceeds total limits")
            );
        }

        let job = ClaimedDeferredCommitMetricJob {
            job_key: "job".to_string(),
            lease_token: "lease".to_string(),
            kind: DeferredCommitMetricKind::SingleParent,
            repo_identity: "repo".to_string(),
            repository_workdir: String::new(),
            git_dir: String::new(),
            git_common_dir: String::new(),
            commit_sha: "commit".to_string(),
            parent_sha: String::new(),
            authorship_note: String::new(),
            parent_authorship_note: String::new(),
            first_checkpoint_ts: None,
            attrs_json: String::new(),
            ignore_patterns_json: String::new(),
            event_ts: 0,
            attempts: 1,
        };
        let too_many_chunks = vec!["{}".to_string(); MAX_COMMIT_BUNDLE_CHUNKS + 1];
        let error = validate_event_bundle(&job, &too_many_chunks).unwrap_err();
        assert!(error.to_string().contains("bundle chunks; limit"));
    }

    #[test]
    fn incomplete_bundle_is_not_exposed_to_upload() {
        let temp = tempfile::tempdir().unwrap();
        let mut conn = test_connection(&temp.path().join("metrics.db"));
        enqueue_on_connection(&mut conn, &spec(DeferredCommitMetricKind::SingleParent), 10)
            .unwrap();
        let claimed = claim_due_on_connection(&mut conn, 20, 60).unwrap().unwrap();
        let events = bundle_events_for_job(&claimed, 1001);
        let only_first = vec![serde_json::to_string(&events[0]).unwrap()];

        let error = complete_on_connection(&mut conn, &claimed, &only_first, 21).unwrap_err();
        assert!(error.to_string().contains("expected count"));
        let metrics: i64 = conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(metrics, 0);
        let state: String = conn
            .query_row(
                "SELECT state FROM deferred_commit_metric_jobs WHERE job_key = ?1",
                params![claimed.job_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "processing");
    }

    #[test]
    fn single_oversized_hunk_keeps_job_deferred_with_diagnostic_error() {
        let temp = tempfile::tempdir().unwrap();
        let mut conn = test_connection(&temp.path().join("metrics.db"));
        enqueue_on_connection(&mut conn, &spec(DeferredCommitMetricKind::SingleParent), 10)
            .unwrap();
        let claimed = claim_due_on_connection(&mut conn, 20, 60).unwrap().unwrap();
        let mut hunk = json_hunk(&claimed.commit_sha, 0);
        hunk.file_path = "x".repeat(crate::metrics::db::MAX_METRICS_UPLOAD_BODY_BYTES);
        let hunks = vec![hunk];
        let digest = sha256_hex(&serde_json::to_vec(&hunks).unwrap());
        let bundle_id = deferred_bundle_id(&claimed.repo_identity, &claimed.commit_sha, &digest);

        let error = build_bundled_events(
            crate::metrics::CommittedValues::new(),
            SparseArray::new(),
            100,
            &bundle_id,
            &digest,
            hunks,
        )
        .unwrap_err();
        assert!(error.to_string().contains("cannot fit in one upload event"));
        assert!(mark_failed_on_connection(&mut conn, &claimed, &error.to_string(), 21).unwrap());

        let (state, last_error): (String, String) = conn
            .query_row(
                "SELECT state, last_error FROM deferred_commit_metric_jobs WHERE job_key = ?1",
                params![claimed.job_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "pending");
        assert!(last_error.contains("cannot fit in one upload event"));
        let metrics: i64 = conn
            .query_row("SELECT COUNT(*) FROM metrics", [], |row| row.get(0))
            .unwrap();
        assert_eq!(metrics, 0);
    }

    #[test]
    fn combined_diff_parser_keeps_only_lines_absent_from_every_parent() {
        let diff = concat!(
            "diff --combined src/lib.rs\n",
            "index 111,222..333\n",
            "--- a/src/lib.rs\n",
            "--- a/src/lib.rs\n",
            "+++ b/src/lib.rs\n",
            "@@@ -1,2 -1,2 +1,4 @@@\n",
            "  shared\n",
            "+ from-second-only\n",
            " +from-first-only\n",
            "++novel resolution\n",
        );

        let hunks = parse_combined_novel_result_hunks(diff, 2).unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].file_path, "src/lib.rs");
        assert_eq!(hunks[0].added_lines, vec![4]);
        assert_eq!(hunks[0].added_contents, vec!["novel resolution"]);
    }

    #[test]
    fn clean_combined_diff_has_no_novel_result_hunks() {
        let diff = concat!(
            "diff --combined src/lib.rs\n",
            "--- a/src/lib.rs\n",
            "--- a/src/lib.rs\n",
            "+++ b/src/lib.rs\n",
            "@@@ -1,1 -1,2 +1,2 @@@\n",
            "  shared\n",
            " +from-first\n",
        );
        assert!(
            parse_combined_novel_result_hunks(diff, 2)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn merge_resolution_rename_does_not_turn_existing_ai_lines_into_novel_lines() {
        let tmp = crate::git::test_utils::TmpRepo::new().expect("tmp repo");
        tmp.write_file("old.rs", "existing ai one\nexisting ai two\n", false)
            .expect("write base file");
        tmp.commit_all("base").expect("base commit");

        tmp.git_command(&["switch", "-c", "feature"])
            .expect("create feature");
        tmp.write_file("feature.txt", "feature\n", false)
            .expect("write feature");
        tmp.commit_all("feature").expect("feature commit");

        tmp.git_command(&["switch", "main"]).expect("switch main");
        tmp.write_file("main.txt", "main\n", false)
            .expect("write main");
        tmp.commit_all("main").expect("main commit");
        tmp.git_command(&["merge", "--no-ff", "--no-commit", "feature"])
            .expect("prepare merge");
        tmp.git_command(&["mv", "old.rs", "new.rs"])
            .expect("rename while resolving merge");
        let merge_sha = tmp
            .commit_all("merge with resolution rename")
            .expect("merge commit");

        let mut note = AuthorshipLog::new();
        let prompt_id = "rename-resolution-prompt";
        note.metadata.prompts.insert(
            prompt_id.to_string(),
            PromptRecord {
                agent_id: AgentId {
                    tool: "codex".to_string(),
                    id: "session".to_string(),
                    model: "gpt-5".to_string(),
                },
                human_author: Some("dev@example.com".to_string()),
                messages_url: None,
                total_additions: 0,
                total_deletions: 0,
                accepted_lines: 0,
                overriden_lines: 0,
                custom_attributes: None,
            },
        );
        note.get_or_create_file("new.rs")
            .add_entry(AttestationEntry::new(
                prompt_id.to_string(),
                vec![LineRange::Single(1), LineRange::Single(2)],
            ));

        assert!(
            merge_novel_ai_hunks(tmp.gitai_repo(), &merge_sha, &note)
                .expect("formal combined diff")
                .is_empty(),
            "a resolution-only rename must preserve existing-line identity"
        );
    }

    #[test]
    fn merge_novel_lines_are_intersected_with_ai_attestations() {
        let mut note = AuthorshipLog::new();
        let prompt_id = "0123456789abcdef";
        note.metadata.prompts.insert(
            prompt_id.to_string(),
            PromptRecord {
                agent_id: AgentId {
                    tool: "codex".to_string(),
                    id: "session".to_string(),
                    model: "gpt-5".to_string(),
                },
                human_author: Some("dev@example.com".to_string()),
                messages_url: None,
                total_additions: 0,
                total_deletions: 0,
                accepted_lines: 0,
                overriden_lines: 0,
                custom_attributes: None,
            },
        );
        let file = note.get_or_create_file("src/lib.rs");
        file.add_entry(AttestationEntry::new(
            prompt_id.to_string(),
            vec![LineRange::Single(4)],
        ));

        let hunks = vec![crate::commands::diff::DiffHunk {
            file_path: "src/lib.rs".to_string(),
            old_file_path: None,
            old_start: 0,
            old_count: 0,
            new_start: 3,
            new_count: 2,
            deleted_lines: Vec::new(),
            added_lines: vec![3, 4],
            deleted_contents: Vec::new(),
            added_contents: vec!["human".to_string(), "ai".to_string()],
        }];
        let filtered = intersect_hunks_with_ai_attestations(hunks, &note);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].added_lines, vec![4]);
        assert_eq!(filtered[0].added_contents, vec!["ai"]);
    }
}
