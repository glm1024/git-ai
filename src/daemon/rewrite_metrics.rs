use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

use crate::authorship::authorship_log_serialization::AuthorshipLog;
use crate::authorship::ignore::effective_ignore_patterns;
use crate::authorship::post_commit::metric_tool_model_breakdown;
use crate::authorship::rewrite::{DiffTreeResult, RewriteMetricCommit};
use crate::config::Config;
use crate::error::GitAiError;
use crate::git::repository::Repository;
use crate::metrics::types::SparseArray;
use crate::metrics::{
    EventAttributes, LifecycleTransitionValues, MetricEvent, PosEncoded, RewriteCommittedValues,
};

const MAX_LIFECYCLE_COMMITS_PER_CHUNK: usize = 512;
pub(crate) const MAX_LIFECYCLE_CHUNKS: usize = 64;
pub(crate) const MAX_LIFECYCLE_UNIQUE_COMMITS: usize = 32_768;
pub(crate) const MAX_LIFECYCLE_CHUNK_EVENT_BYTES: usize = 256 * 1024;
pub(crate) const MAX_LIFECYCLE_BUNDLE_EVENT_BYTES: usize = 8 * 1024 * 1024;

pub(crate) async fn persist_rewrite_commit_metrics(
    repo: &Repository,
    metric_commits: Vec<RewriteMetricCommit>,
) -> Result<(), GitAiError> {
    if !crate::authorship::rewrite::rewrite_metrics_enabled() {
        return Ok(());
    }
    if metric_commits.is_empty() {
        return Ok(());
    }

    let repo = repo.clone();
    build_and_persist_events(
        move || build_rewrite_metric_events(&repo, &metric_commits),
        |events| crate::daemon::telemetry_worker::persist_metrics_to_db_blocking(&events),
    )
    .await
}

async fn build_and_persist_events<Build, Persist>(
    build: Build,
    persist: Persist,
) -> Result<(), GitAiError>
where
    Build: FnOnce() -> Result<Vec<MetricEvent>, GitAiError> + Send + 'static,
    Persist: FnOnce(Vec<MetricEvent>) -> Result<(), GitAiError> + Send + 'static,
{
    let events = tokio::task::spawn_blocking(build).await.map_err(|error| {
        GitAiError::Generic(format!(
            "rewrite metrics builder failed before persistence: {error}"
        ))
    })??;
    if events.is_empty() {
        return Ok(());
    }
    tokio::task::spawn_blocking(move || persist(events))
        .await
        .map_err(|error| {
            GitAiError::Generic(format!(
                "rewrite metrics persistence worker failed before acknowledgement: {error}"
            ))
        })?
}

pub(crate) fn dedupe_metric_commits(
    metric_commits: Vec<RewriteMetricCommit>,
) -> Vec<RewriteMetricCommit> {
    #[derive(Hash, PartialEq, Eq)]
    struct MetricCommitKey {
        new_sha: String,
        original_shas: Vec<String>,
        operation: crate::authorship::rewrite::RewriteMetricOperation,
        branch: Option<String>,
    }

    let mut deduped = Vec::new();
    let mut indices_by_key: HashMap<MetricCommitKey, usize> = HashMap::new();
    for commit in metric_commits {
        if commit.new_sha.is_empty() {
            continue;
        }
        let key = MetricCommitKey {
            new_sha: commit.new_sha.clone(),
            original_shas: commit.original_shas.clone(),
            operation: commit.operation,
            branch: commit.branch.clone(),
        };
        if let Some(index) = indices_by_key.get(&key).copied() {
            merge_metric_commit_context(&mut deduped[index], commit);
        } else {
            indices_by_key.insert(key, deduped.len());
            deduped.push(commit);
        }
    }
    deduped
}

fn merge_metric_commit_context(target: &mut RewriteMetricCommit, source: RewriteMetricCommit) {
    if target.parent_sha.is_none() {
        target.parent_sha = source.parent_sha;
    }
    if target.authorship_note.is_none() {
        target.authorship_note = source.authorship_note;
    }
    if target.parent_diff.is_none() {
        target.parent_diff = source.parent_diff;
    }
}

fn build_rewrite_metric_events(
    repo: &Repository,
    metric_commits: &[RewriteMetricCommit],
) -> Result<Vec<MetricEvent>, GitAiError> {
    let mut metric_commits = dedupe_metric_commits(metric_commits.to_vec());
    hydrate_missing_parent_shas(repo, &mut metric_commits);
    hydrate_missing_parent_diffs(repo, &mut metric_commits);
    let batch_context = RewriteMetricBatchContext::new(repo);

    let mut events = Vec::new();
    for metric_commit in &metric_commits {
        // Persist the strong replacement fact before the derived replacement
        // commit. The metrics DB preserves insertion order and retries pending
        // rows in that order.
        events.extend(build_mapped_lifecycle_events(
            metric_commit,
            &batch_context,
        )?);
        match build_rewrite_committed_metric_event(repo, metric_commit, &batch_context) {
            Ok(Some(event)) => events.push(event),
            Ok(None) => {}
            Err(err) => {
                tracing::debug!(
                    %err,
                    commit_sha = %metric_commit.new_sha,
                    operation_kind = metric_commit.operation.as_str(),
                    "skipping rewrite committed metric"
                );
            }
        }
    }
    Ok(events)
}

/// Durably capture a named-branch ref transition using facts already observed
/// by trace2/ref analysis. The caller awaits the SQLite intent insert before
/// acknowledging the command side effect; repository enumeration and outbox
/// construction are retryable work and may finish immediately or later.
pub(crate) async fn persist_ref_lifecycle_transition_metrics(
    repo: &Repository,
    operation_kind: impl Into<String>,
    old_tip: impl Into<String>,
    new_tip: impl Into<String>,
    branch: Option<String>,
    semantics: impl Into<String>,
) -> Result<(), GitAiError> {
    if !crate::authorship::rewrite::rewrite_metrics_enabled() {
        return Ok(());
    }
    let operation_kind = operation_kind.into();
    let old_tip = old_tip.into();
    let new_tip = new_tip.into();
    let semantics = semantics.into();
    let branch = resolve_lifecycle_branch(repo, branch);
    let Some(branch) = branch else {
        // Event 8 is intentionally scoped to one named branch. A detached HEAD
        // transition does not move a branch pointer, and the backend rejects an
        // unscoped lifecycle bundle. Treat it as out of scope instead of
        // creating a permanently retrying/dead-lettered metric.
        tracing::debug!(
            operation_kind,
            old_tip,
            new_tip,
            "skipping lifecycle ref transition without a named branch"
        );
        return Ok(());
    };
    let context = RewriteMetricBatchContext::new(repo);
    let attrs = lifecycle_attrs(&new_tip, Some(&branch), &context).to_sparse();
    let spec =
        crate::metrics::deferred_lifecycle_jobs::DeferredLifecycleMetricJobSpec::from_transition(
            repo,
            &operation_kind,
            &old_tip,
            &new_tip,
            Some(branch),
            &semantics,
            &attrs,
        )?;

    // The immutable old/new ref intent enters SQLite before rev-list or event
    // construction. Immediate processing is only a latency optimization; a
    // computation/outbox failure leaves the job pending for periodic retry.
    tokio::task::spawn_blocking(move || {
        crate::metrics::deferred_lifecycle_jobs::enqueue_and_try_process(&spec)
    })
    .await
    .map_err(|error| {
        GitAiError::Generic(format!(
            "deferred lifecycle enqueue worker failed before durable acknowledgement: {error}"
        ))
    })?
}

/// Build a ref transition only when both exclusive commit sets were observed.
/// A tip-only event is not a safe fallback because downstream consumers may
/// interpret its old/new tips as authoritative lifecycle replacements.
#[allow(dead_code)]
fn build_ref_lifecycle_transition_events(
    repo: &Repository,
    operation_kind: &str,
    old_tip: &str,
    new_tip: &str,
    branch: Option<String>,
    semantics: &str,
) -> Result<Vec<MetricEvent>, GitAiError> {
    let context = RewriteMetricBatchContext::new(repo);
    let branch = resolve_lifecycle_branch(repo, branch);
    let attrs = lifecycle_attrs(new_tip, branch.as_deref(), &context).to_sparse();
    let event_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(u64::from(u32::MAX)) as u32;
    build_ref_lifecycle_transition_events_from_snapshot(
        repo,
        operation_kind,
        old_tip,
        new_tip,
        branch.as_deref(),
        semantics,
        attrs,
        event_ts,
    )
}

/// Rebuild a persisted ref-transition intent without consulting mutable
/// configuration or metadata. Only repository reachability is recomputed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_ref_lifecycle_transition_events_from_snapshot(
    repo: &Repository,
    operation_kind: &str,
    old_tip: &str,
    new_tip: &str,
    branch: Option<&str>,
    semantics: &str,
    attrs: SparseArray,
    event_ts: u32,
) -> Result<Vec<MetricEvent>, GitAiError> {
    // One detached plumbing call returns both sides of the ref move. This is
    // outside trace2 ingestion and avoids per-commit subprocesses while still
    // sending the complete reset/rebase/drop set.
    let (invalidated, replacements) = exclusive_commits_for_transition(repo, old_tip, new_tip)?;
    if invalidated.is_empty() && replacements.is_empty() {
        return Ok(Vec::new());
    }
    build_lifecycle_events_from_snapshot(
        operation_kind,
        old_tip,
        new_tip,
        branch,
        &invalidated,
        &replacements,
        semantics,
        attrs,
        event_ts,
    )
}

fn resolve_lifecycle_branch(repo: &Repository, branch: Option<String>) -> Option<String> {
    branch
        .or_else(|| repo.head().ok().and_then(|head| head.shorthand().ok()))
        .and_then(|branch| {
            let branch = branch.trim();
            let branch = branch.strip_prefix("refs/heads/").unwrap_or(branch);
            (!branch.is_empty() && !branch.eq_ignore_ascii_case("HEAD")).then(|| branch.to_string())
        })
}

fn exclusive_commits_for_transition(
    repo: &Repository,
    old_tip: &str,
    new_tip: &str,
) -> Result<(Vec<String>, Vec<String>), GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "rev-list".to_string(),
        "--left-right".to_string(),
        "--topo-order".to_string(),
        format!("{old_tip}...{new_tip}"),
    ]);
    let output = crate::git::repository::exec_git_allow_nonzero(&args)?;
    if !output.status.success() {
        return Err(GitAiError::Generic(
            "unable to enumerate lifecycle ref transition".to_string(),
        ));
    }
    let mut invalidated = Vec::new();
    let mut replacements = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if let Some(sha) = line.strip_prefix('<') {
            invalidated.push(sha.trim().to_string());
        } else if let Some(sha) = line.strip_prefix('>') {
            replacements.push(sha.trim().to_string());
        }
    }
    Ok((invalidated, replacements))
}

fn build_mapped_lifecycle_events(
    commit: &RewriteMetricCommit,
    context: &RewriteMetricBatchContext,
) -> Result<Vec<MetricEvent>, GitAiError> {
    let (invalidated, semantics) = match commit.operation {
        crate::authorship::rewrite::RewriteMetricOperation::Rebase
        | crate::authorship::rewrite::RewriteMetricOperation::Amend
        | crate::authorship::rewrite::RewriteMetricOperation::UpdateRef
        | crate::authorship::rewrite::RewriteMetricOperation::NonFastForward => {
            (commit.original_shas.as_slice(), "replacement")
        }
        // Copy-like operations create a new attribution-bearing commit but do
        // not supersede their source commit.
        crate::authorship::rewrite::RewriteMetricOperation::SquashMerge
        | crate::authorship::rewrite::RewriteMetricOperation::CherryPick
        | crate::authorship::rewrite::RewriteMetricOperation::CherryPickNoCommit
        | crate::authorship::rewrite::RewriteMetricOperation::Revert => return Ok(Vec::new()),
    };
    build_lifecycle_events_checked(
        commit.operation.as_str(),
        invalidated.first().map(String::as_str).unwrap_or(""),
        &commit.new_sha,
        commit.branch.as_deref(),
        invalidated,
        std::slice::from_ref(&commit.new_sha),
        semantics,
        context,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_lifecycle_events_checked(
    operation_kind: &str,
    old_tip: &str,
    new_tip: &str,
    branch: Option<&str>,
    invalidated: &[String],
    replacements: &[String],
    semantics: &str,
    context: &RewriteMetricBatchContext,
) -> Result<Vec<MetricEvent>, GitAiError> {
    let attrs = lifecycle_attrs(new_tip, branch, context).to_sparse();
    let event_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(u64::from(u32::MAX)) as u32;
    build_lifecycle_events_from_snapshot(
        operation_kind,
        old_tip,
        new_tip,
        branch,
        invalidated,
        replacements,
        semantics,
        attrs,
        event_ts,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn build_lifecycle_events_from_snapshot(
    operation_kind: &str,
    old_tip: &str,
    new_tip: &str,
    branch: Option<&str>,
    invalidated: &[String],
    replacements: &[String],
    semantics: &str,
    attrs: SparseArray,
    event_ts: u32,
) -> Result<Vec<MetricEvent>, GitAiError> {
    let unique_commit_count = invalidated
        .len()
        .checked_add(replacements.len())
        .ok_or_else(|| GitAiError::Generic("lifecycle commit count overflowed".to_string()))?;
    if unique_commit_count > MAX_LIFECYCLE_UNIQUE_COMMITS {
        return Err(GitAiError::Generic(format!(
            "lifecycle bundle has {unique_commit_count} commits; limit is {MAX_LIFECYCLE_UNIQUE_COMMITS}"
        )));
    }
    let operation_id = lifecycle_operation_id(operation_kind, old_tip, new_tip, branch);
    if semantics == "replacement" && !invalidated.is_empty() && !replacements.is_empty() {
        return build_replacement_lifecycle_events(
            operation_kind,
            old_tip,
            new_tip,
            invalidated,
            replacements,
            semantics,
            &operation_id,
            &attrs,
            event_ts,
        );
    }
    let mut tagged = Vec::with_capacity(invalidated.len() + replacements.len());
    tagged.extend(invalidated.iter().cloned().map(|sha| (true, sha)));
    tagged.extend(replacements.iter().cloned().map(|sha| (false, sha)));
    if tagged.is_empty() {
        tagged.push((true, String::new()));
    }
    let chunk_count = tagged.len().div_ceil(MAX_LIFECYCLE_COMMITS_PER_CHUNK) as u32;
    if chunk_count as usize > MAX_LIFECYCLE_CHUNKS {
        return Err(GitAiError::Generic(format!(
            "lifecycle bundle has {chunk_count} chunks; limit is {MAX_LIFECYCLE_CHUNKS}"
        )));
    }
    let events = tagged
        .chunks(MAX_LIFECYCLE_COMMITS_PER_CHUNK)
        .enumerate()
        .map(|(index, chunk)| {
            let invalidated = chunk
                .iter()
                .filter(|(is_old, sha)| *is_old && !sha.is_empty())
                .map(|(_, sha)| sha.clone())
                .collect();
            let replacements = chunk
                .iter()
                .filter(|(is_old, _)| !*is_old)
                .map(|(_, sha)| sha.clone())
                .collect();
            let values = LifecycleTransitionValues::new()
                .operation_id(operation_id.clone())
                .operation_kind(operation_kind)
                .old_tip(old_tip)
                .new_tip(new_tip)
                .invalidated_commit_shas(invalidated)
                .replacement_commit_shas(replacements)
                .chunk_index(index as u32)
                .chunk_count(chunk_count)
                .semantics(semantics);
            MetricEvent::from_values_with_timestamp(values, attrs.clone(), Some(event_ts))
        })
        .collect::<Vec<_>>();
    validate_lifecycle_event_bundle_size(&events)?;
    Ok(events)
}

#[allow(clippy::too_many_arguments)]
fn build_replacement_lifecycle_events(
    operation_kind: &str,
    old_tip: &str,
    new_tip: &str,
    invalidated: &[String],
    replacements: &[String],
    semantics: &str,
    operation_id: &str,
    attrs: &SparseArray,
    event_ts: u32,
) -> Result<Vec<MetricEvent>, GitAiError> {
    let old_chunk_count = invalidated.len().div_ceil(MAX_LIFECYCLE_COMMITS_PER_CHUNK);
    let new_chunk_count = replacements.len().div_ceil(MAX_LIFECYCLE_COMMITS_PER_CHUNK);
    let chunk_count = old_chunk_count.max(new_chunk_count);
    if chunk_count > MAX_LIFECYCLE_CHUNKS {
        return Err(GitAiError::Generic(format!(
            "lifecycle replacement bundle has {chunk_count} chunks; limit is {MAX_LIFECYCLE_CHUNKS}"
        )));
    }
    let old_anchor = invalidated[0].clone();
    let new_anchor = replacements[0].clone();

    let events = (0..chunk_count)
        .map(|index| {
            let old_start = index * MAX_LIFECYCLE_COMMITS_PER_CHUNK;
            let new_start = index * MAX_LIFECYCLE_COMMITS_PER_CHUNK;
            let old_chunk = invalidated
                .get(
                    old_start
                        ..invalidated
                            .len()
                            .min(old_start + MAX_LIFECYCLE_COMMITS_PER_CHUNK),
                )
                .filter(|chunk| !chunk.is_empty())
                .map(<[String]>::to_vec)
                .unwrap_or_else(|| vec![old_anchor.clone()]);
            let new_chunk = replacements
                .get(
                    new_start
                        ..replacements
                            .len()
                            .min(new_start + MAX_LIFECYCLE_COMMITS_PER_CHUNK),
                )
                .filter(|chunk| !chunk.is_empty())
                .map(<[String]>::to_vec)
                .unwrap_or_else(|| vec![new_anchor.clone()]);
            let values = LifecycleTransitionValues::new()
                .operation_id(operation_id)
                .operation_kind(operation_kind)
                .old_tip(old_tip)
                .new_tip(new_tip)
                .invalidated_commit_shas(old_chunk)
                .replacement_commit_shas(new_chunk)
                .chunk_index(index as u32)
                .chunk_count(chunk_count as u32)
                .semantics(semantics);
            MetricEvent::from_values_with_timestamp(values, attrs.clone(), Some(event_ts))
        })
        .collect::<Vec<_>>();
    validate_lifecycle_event_bundle_size(&events)?;
    Ok(events)
}

fn validate_lifecycle_event_bundle_size(events: &[MetricEvent]) -> Result<(), GitAiError> {
    let mut total = 0usize;
    for event in events {
        let event_bytes = serde_json::to_vec(event)?.len();
        if event_bytes > MAX_LIFECYCLE_CHUNK_EVENT_BYTES {
            return Err(GitAiError::Generic(format!(
                "lifecycle event chunk is {event_bytes} bytes; limit is {MAX_LIFECYCLE_CHUNK_EVENT_BYTES}"
            )));
        }
        total = total.checked_add(event_bytes).ok_or_else(|| {
            GitAiError::Generic("lifecycle event bundle byte size overflowed".to_string())
        })?;
        if total > MAX_LIFECYCLE_BUNDLE_EVENT_BYTES {
            return Err(GitAiError::Generic(format!(
                "lifecycle event bundle is {total} bytes; limit is {MAX_LIFECYCLE_BUNDLE_EVENT_BYTES}"
            )));
        }
    }
    Ok(())
}

fn lifecycle_operation_id(
    operation_kind: &str,
    old_tip: &str,
    new_tip: &str,
    branch: Option<&str>,
) -> String {
    let mut hash = Sha256::new();
    for value in [operation_kind, old_tip, new_tip, branch.unwrap_or("")] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value.as_bytes());
    }
    format!("sha256:{:x}", hash.finalize())
}

fn lifecycle_attrs(
    new_tip: &str,
    branch: Option<&str>,
    context: &RewriteMetricBatchContext,
) -> EventAttributes {
    let mut attrs = EventAttributes::with_version(env!("CARGO_PKG_VERSION")).commit_sha(new_tip);
    attrs = attrs.author(&context.author);
    if let Some(branch) = branch {
        attrs = attrs.branch(branch);
    }
    if let Some(repo_url) = context.repo_url.as_deref() {
        attrs = attrs.repo_url(repo_url);
    }
    if let Some(custom) = context.custom_attributes_json.as_deref() {
        attrs = attrs.custom_attributes(custom);
    }
    attrs
}

fn hydrate_missing_parent_shas(repo: &Repository, metric_commits: &mut [RewriteMetricCommit]) {
    let mut new_shas = Vec::new();
    let mut seen = HashSet::new();
    for metric_commit in metric_commits.iter() {
        if metric_commit.parent_sha.is_some() {
            continue;
        }
        if seen.insert(metric_commit.new_sha.clone()) {
            new_shas.push(metric_commit.new_sha.clone());
        }
    }
    if new_shas.is_empty() {
        return;
    }

    let Some(parent_by_commit) = parent_shas_for_commits(repo, &new_shas) else {
        return;
    };
    for metric_commit in metric_commits {
        if metric_commit.parent_sha.is_none()
            && let Some(parent_sha) = parent_by_commit.get(&metric_commit.new_sha)
        {
            metric_commit.parent_sha = Some(parent_sha.clone());
        }
    }
}

fn parent_shas_for_commits(
    repo: &Repository,
    commit_shas: &[String],
) -> Option<HashMap<String, String>> {
    if commit_shas.is_empty() {
        return Some(HashMap::new());
    }

    let mut args = repo.global_args_for_exec();
    args.extend([
        "show".to_string(),
        "-s".to_string(),
        "--format=%H %P".to_string(),
        "--no-walk".to_string(),
    ]);
    args.extend(commit_shas.iter().cloned());

    let output = crate::git::repository::exec_git(&args).ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut parent_by_commit = HashMap::new();
    for line in stdout.lines() {
        let mut parts = line.split_whitespace();
        let Some(commit_sha) = parts.next() else {
            continue;
        };
        let parents = parts.collect::<Vec<_>>();
        match parents.as_slice() {
            [] => {
                parent_by_commit.insert(commit_sha.to_string(), "initial".to_string());
            }
            [parent_sha] => {
                parent_by_commit.insert(commit_sha.to_string(), (*parent_sha).to_string());
            }
            _ => {
                // Existing rewrite metrics skip merge commits.
            }
        }
    }
    Some(parent_by_commit)
}

struct RewriteMetricBatchContext {
    ignore_patterns: Vec<String>,
    repo_url: Option<String>,
    author: String,
    custom_attributes_json: Option<String>,
}

impl RewriteMetricBatchContext {
    fn new(repo: &Repository) -> Self {
        Self {
            ignore_patterns: effective_ignore_patterns(repo, &[], &[]),
            repo_url: rewrite_metric_repo_url(repo),
            author: repo.effective_author_identity().formatted_or_unknown(),
            custom_attributes_json: rewrite_metric_custom_attributes_json(),
        }
    }
}

fn rewrite_metric_repo_url(repo: &Repository) -> Option<String> {
    let remotes = repo.remotes_with_urls().ok()?;
    let (_, url) = remotes
        .iter()
        .find(|(name, _)| name == "origin")
        .or_else(|| remotes.first())?;
    crate::repo_url::normalize_repo_url(url).ok()
}

fn rewrite_metric_custom_attributes_json() -> Option<String> {
    let config = Config::fresh();
    let attrs = config.metrics_custom_attributes();
    if attrs.is_empty() {
        None
    } else {
        serde_json::to_string(attrs).ok()
    }
}

fn hydrate_missing_parent_diffs(repo: &Repository, metric_commits: &mut [RewriteMetricCommit]) {
    let mut indices = Vec::new();
    let mut pairs = Vec::new();
    for (index, metric_commit) in metric_commits.iter().enumerate() {
        if metric_commit.parent_diff.is_some() {
            continue;
        }
        let Some(parent_sha) = metric_commit.parent_sha.as_ref() else {
            continue;
        };
        indices.push(index);
        pairs.push((parent_sha.clone(), metric_commit.new_sha.clone()));
    }
    if pairs.is_empty() {
        return;
    }

    let Ok(results) = crate::authorship::rewrite::compute_diff_trees_batch(repo, &pairs) else {
        return;
    };
    for (index, result) in indices.into_iter().zip(results) {
        metric_commits[index].parent_diff = Some(result);
    }
}

fn build_rewrite_committed_metric_event(
    repo: &Repository,
    metric_commit: &RewriteMetricCommit,
    batch_context: &RewriteMetricBatchContext,
) -> Result<Option<MetricEvent>, GitAiError> {
    let Some(raw_note) = metric_commit.authorship_note.as_ref() else {
        return Ok(None);
    };
    let authorship_log = match AuthorshipLog::deserialize_from_string(raw_note) {
        Ok(log) => log,
        Err(_) => return Ok(None),
    };

    let Some(parent_diff) = metric_commit.parent_diff.as_ref() else {
        return Ok(None);
    };

    let diff_hunks = diff_hunks_from_diff_tree_result(parent_diff);
    if should_skip_rewrite_metric_stats(&diff_hunks, &batch_context.ignore_patterns) {
        return Ok(None);
    }
    let stats = crate::authorship::stats::stats_for_commit_stats_from_hunks_with_merge_flag(
        &batch_context.ignore_patterns,
        &diff_hunks,
        Some(&authorship_log),
        false,
    );
    let Some(breakdown) = metric_tool_model_breakdown(&stats) else {
        return Ok(None);
    };

    let mut values = RewriteCommittedValues::new()
        .human_additions(stats.human_additions)
        .git_diff_deleted_lines(stats.git_diff_deleted_lines)
        .git_diff_added_lines(stats.git_diff_added_lines)
        .tool_model_pairs(breakdown.tool_model_pairs)
        .ai_additions(breakdown.ai_additions)
        .ai_accepted(breakdown.ai_accepted)
        .authorship_note(raw_note.clone())
        .operation_kind(metric_commit.operation.as_str())
        .original_commit_shas(metric_commit.original_shas.clone());

    let hunks_json = crate::commands::diff::build_diff_artifacts_from_hunks(
        repo,
        diff_hunks,
        &metric_commit.new_sha,
        Some(&authorship_log),
    )
    .ok()
    .and_then(|artifacts| serde_json::to_string(&artifacts.json_hunks).ok());

    values = values.commit_subject_null().commit_body_null();
    values = match hunks_json {
        Some(hunks) => values.hunks(hunks),
        None => values.hunks_null(),
    };

    let attrs = rewrite_metric_attrs(metric_commit, batch_context);

    Ok(Some(MetricEvent::from_values(values, attrs.to_sparse())))
}

fn diff_hunks_from_diff_tree_result(
    result: &DiffTreeResult,
) -> Vec<crate::commands::diff::DiffHunk> {
    let mut hunks = Vec::new();
    for (file_path, file_hunks) in &result.hunks_by_file {
        for (index, hunk) in file_hunks.iter().enumerate() {
            let contents = result
                .hunk_contents_by_file
                .get(file_path)
                .and_then(|file_contents| file_contents.get(index));
            hunks.push(crate::commands::diff::DiffHunk {
                file_path: file_path.clone(),
                old_file_path: None,
                old_start: hunk.old_start,
                old_count: hunk.old_count,
                new_start: hunk.new_start,
                new_count: hunk.new_count,
                deleted_lines: line_numbers(hunk.old_start, hunk.old_count),
                added_lines: line_numbers(hunk.new_start, hunk.new_count),
                deleted_contents: contents.map(|c| c.deleted.clone()).unwrap_or_default(),
                added_contents: contents.map(|c| c.added.clone()).unwrap_or_default(),
            });
        }
    }
    hunks
}

fn line_numbers(start: u32, count: u32) -> Vec<u32> {
    if count == 0 {
        return Vec::new();
    }
    (start..start.saturating_add(count))
        .filter(|line| *line > 0)
        .collect()
}

fn should_skip_rewrite_metric_stats(
    hunks: &[crate::commands::diff::DiffHunk],
    ignore_patterns: &[String],
) -> bool {
    let ignore_matcher = crate::authorship::ignore::build_ignore_matcher(ignore_patterns);
    let mut files_with_additions = std::collections::HashSet::new();
    let mut added_lines = 0usize;
    let mut deleted_lines = 0usize;
    let mut hunk_ranges = 0usize;

    for hunk in hunks {
        if crate::authorship::ignore::should_ignore_file_with_matcher(
            &hunk.file_path,
            &ignore_matcher,
        ) {
            continue;
        }
        if !hunk.added_lines.is_empty() {
            files_with_additions.insert(hunk.file_path.as_str());
            hunk_ranges += 1;
        }
        added_lines += hunk.added_lines.len();
        deleted_lines += hunk.deleted_lines.len();
    }

    hunk_ranges >= crate::authorship::post_commit::STATS_SKIP_MAX_HUNKS
        || added_lines >= crate::authorship::post_commit::STATS_SKIP_MAX_ADDED_LINES
        || files_with_additions.len()
            >= crate::authorship::post_commit::STATS_SKIP_MAX_FILES_WITH_ADDITIONS
        || deleted_lines >= crate::authorship::post_commit::STATS_SKIP_MAX_DELETED_LINES
}

fn rewrite_metric_attrs(
    metric_commit: &RewriteMetricCommit,
    batch_context: &RewriteMetricBatchContext,
) -> EventAttributes {
    let base_commit_sha = metric_commit.parent_sha.as_deref().unwrap_or("initial");
    let mut attrs = EventAttributes::with_version(env!("CARGO_PKG_VERSION"))
        .commit_sha(metric_commit.new_sha.clone())
        .author(&batch_context.author);
    attrs = if base_commit_sha == "initial" {
        attrs.base_commit_sha_null()
    } else {
        attrs.base_commit_sha(base_commit_sha)
    };

    attrs = apply_rewrite_metric_branch(attrs, metric_commit);

    if let Some(repo_url) = batch_context.repo_url.as_deref() {
        attrs = attrs.repo_url(repo_url);
    }

    attrs = apply_rewrite_metric_custom_attributes(
        attrs,
        batch_context.custom_attributes_json.as_deref(),
    );

    attrs
}

fn apply_rewrite_metric_custom_attributes(
    attrs: EventAttributes,
    custom_attributes_json: Option<&str>,
) -> EventAttributes {
    if let Some(custom_attributes_json) = custom_attributes_json {
        // `custom_attributes_map` serializes the map and stores this same string field.
        // Rewrite metrics pre-serialize once per batch to avoid repeated serde work.
        attrs.custom_attributes(custom_attributes_json)
    } else {
        attrs
    }
}

fn apply_rewrite_metric_branch(
    attrs: EventAttributes,
    metric_commit: &RewriteMetricCommit,
) -> EventAttributes {
    if let Some(branch) = metric_commit.branch.as_deref() {
        return attrs.branch(branch);
    }

    attrs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorship::authorship_log::LineRange;
    use crate::authorship::authorship_log_serialization::AttestationEntry;
    use crate::authorship::rewrite::RewriteMetricOperation;
    use crate::authorship::working_log::AgentId;
    use crate::metrics::EventValues;
    use crate::metrics::events::rewrite_committed_pos;
    use std::collections::HashMap;
    use std::sync::mpsc;
    use std::time::Duration;

    fn metric_commit(
        new_sha: &str,
        originals: &[&str],
        operation: RewriteMetricOperation,
    ) -> RewriteMetricCommit {
        RewriteMetricCommit::new(
            new_sha.to_string(),
            originals.iter().map(|s| s.to_string()).collect(),
            operation,
        )
    }

    fn note_for_ai_line(file_path: &str, line: u32) -> String {
        let prompt_id = "prompt1".to_string();
        let mut log = AuthorshipLog::new();
        log.metadata.prompts.insert(
            prompt_id.clone(),
            crate::authorship::authorship_log::PromptRecord {
                agent_id: AgentId {
                    tool: "codex".to_string(),
                    id: "session".to_string(),
                    model: "gpt-5".to_string(),
                },
                human_author: None,
                messages_url: None,
                total_additions: 0,
                total_deletions: 0,
                accepted_lines: 0,
                overriden_lines: 0,
                custom_attributes: None,
            },
        );
        log.get_or_create_file(file_path)
            .add_entry(AttestationEntry::new(
                prompt_id,
                vec![LineRange::Single(line)],
            ));
        log.serialize_to_string().expect("serialize note")
    }

    #[tokio::test]
    async fn durable_metric_task_waits_for_builder_and_persistence() {
        let (release_tx, release_rx) = mpsc::channel();
        let (persisted_tx, persisted_rx) = mpsc::channel();
        let mut task = tokio::spawn(build_and_persist_events(
            move || {
                release_rx.recv().expect("release builder");
                Ok(vec![MetricEvent {
                    timestamp: 1,
                    event_id: 8,
                    values: Default::default(),
                    attrs: Default::default(),
                }])
            },
            move |events| {
                assert_eq!(events.len(), 1);
                persisted_tx.send(()).expect("record persistence");
                Ok(())
            },
        ));

        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut task)
                .await
                .is_err(),
            "the tracked task must not acknowledge while its builder is blocked"
        );
        assert!(persisted_rx.try_recv().is_err());

        release_tx.send(()).expect("release builder");
        task.await
            .expect("tracked task join")
            .expect("durable task result");
        persisted_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("persistence completed before acknowledgement");
    }

    #[tokio::test]
    async fn durable_metric_task_propagates_persistence_failure() {
        let error = build_and_persist_events(
            || {
                Ok(vec![MetricEvent {
                    timestamp: 1,
                    event_id: 8,
                    values: Default::default(),
                    attrs: Default::default(),
                }])
            },
            |_| Err(GitAiError::Generic("simulated sqlite failure".to_string())),
        )
        .await
        .expect_err("persistence failure must prevent acknowledgement");

        assert!(error.to_string().contains("simulated sqlite failure"));
    }

    #[test]
    fn dedupe_metric_commits_keeps_distinct_original_sets() {
        let first = metric_commit("new", &["old1"], RewriteMetricOperation::Rebase);
        let second = metric_commit("new", &["old1"], RewriteMetricOperation::Rebase);
        let squash = metric_commit(
            "new",
            &["old1", "old2"],
            RewriteMetricOperation::SquashMerge,
        );

        let result = dedupe_metric_commits(vec![first.clone(), second, squash.clone()]);

        assert_eq!(result, vec![first, squash]);
    }

    #[test]
    fn rewrite_event_schema_does_not_emit_position_10() {
        let values = RewriteCommittedValues::new()
            .human_additions(1)
            .git_diff_deleted_lines(2)
            .git_diff_added_lines(3)
            .tool_model_pairs(vec!["all".to_string()])
            .ai_additions(vec![1])
            .ai_accepted(vec![1])
            .commit_subject("subject")
            .commit_body_null()
            .authorship_note("note")
            .hunks("[]")
            .operation_kind("rebase")
            .original_commit_shas(vec!["old".to_string()]);

        let sparse = PosEncoded::to_sparse(&values);

        assert_eq!(
            RewriteCommittedValues::event_id(),
            crate::metrics::types::MetricEventId::RewriteCommitted
        );
        assert!(!sparse.contains_key("10"));
        assert_eq!(
            sparse.get(&rewrite_committed_pos::OPERATION_KIND.to_string()),
            Some(&serde_json::json!("rebase"))
        );
        assert_eq!(
            sparse.get(&rewrite_committed_pos::ORIGINAL_COMMIT_SHAS.to_string()),
            Some(&serde_json::json!(["old"]))
        );
    }

    #[test]
    fn rewrite_metric_branch_overrides_head_branch_attr() {
        let commit = metric_commit("new", &["old"], RewriteMetricOperation::NonFastForward)
            .with_branch("feature");
        let attrs = apply_rewrite_metric_branch(
            crate::metrics::EventAttributes::with_version("test").branch("main"),
            &commit,
        );
        let sparse = attrs.to_sparse();

        assert_eq!(
            sparse.get(&crate::metrics::attrs::attr_pos::BRANCH.to_string()),
            Some(&serde_json::json!("feature"))
        );
    }

    #[test]
    fn rewrite_metric_custom_attributes_match_map_builder_wire_format() {
        let mut custom_attributes = HashMap::new();
        custom_attributes.insert("team".to_string(), "metrics".to_string());
        let custom_attributes_json =
            serde_json::to_string(&custom_attributes).expect("serialize custom attributes");

        let sparse_from_batch_json = apply_rewrite_metric_custom_attributes(
            crate::metrics::EventAttributes::with_version("test"),
            Some(&custom_attributes_json),
        )
        .to_sparse();
        let sparse_from_map = crate::metrics::EventAttributes::with_version("test")
            .custom_attributes_map(&custom_attributes)
            .to_sparse();

        assert_eq!(
            sparse_from_batch_json
                .get(&crate::metrics::attrs::attr_pos::CUSTOM_ATTRIBUTES.to_string()),
            sparse_from_map.get(&crate::metrics::attrs::attr_pos::CUSTOM_ATTRIBUTES.to_string())
        );
    }

    #[test]
    fn rewrite_metric_event_uses_supplied_note_and_parent_diff() {
        let tmp = crate::git::test_utils::TmpRepo::new().expect("tmp repo");
        let note = note_for_ai_line("file.txt", 1);

        let mut hunks_by_file = HashMap::new();
        hunks_by_file.insert(
            "file.txt".to_string(),
            vec![crate::authorship::hunk_shift::DiffHunk {
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 1,
            }],
        );
        let parent_diff = DiffTreeResult {
            hunks_by_file,
            hunk_contents_by_file: HashMap::new(),
            added_lines_by_file: HashMap::new(),
            renames: Vec::new(),
        };
        let commit = metric_commit("new", &["old"], RewriteMetricOperation::Rebase)
            .with_branch("feature")
            .with_parent_sha("parent")
            .with_authorship_note(note.clone())
            .with_parent_diff(parent_diff);

        let batch_context = RewriteMetricBatchContext::new(tmp.gitai_repo());
        let event = build_rewrite_committed_metric_event(tmp.gitai_repo(), &commit, &batch_context)
            .expect("metric build")
            .expect("event");

        assert_eq!(
            event
                .values
                .get(&rewrite_committed_pos::AUTHORSHIP_NOTE.to_string()),
            Some(&serde_json::json!(note))
        );
        assert_eq!(
            event
                .values
                .get(&rewrite_committed_pos::GIT_DIFF_ADDED_LINES.to_string()),
            Some(&serde_json::json!(1))
        );
        assert_eq!(
            event
                .values
                .get(&rewrite_committed_pos::TOOL_MODEL_PAIRS.to_string()),
            Some(&serde_json::json!(["all", "codex::gpt-5"]))
        );
        assert_eq!(
            event
                .attrs
                .get(&crate::metrics::attrs::attr_pos::BRANCH.to_string()),
            Some(&serde_json::json!("feature"))
        );
        assert_eq!(
            event
                .attrs
                .get(&crate::metrics::attrs::attr_pos::BASE_COMMIT_SHA.to_string()),
            Some(&serde_json::json!("parent"))
        );
    }

    #[test]
    fn rewrite_metric_worker_hydrates_missing_parent_and_diff() {
        let tmp = crate::git::test_utils::TmpRepo::new().expect("tmp repo");
        tmp.write_file("file.txt", "base\n", false)
            .expect("write base");
        let parent_sha = tmp.commit_all("base").expect("base commit");
        tmp.write_file("file.txt", "base\nai\n", false)
            .expect("write update");
        let new_sha = tmp.commit_all("update").expect("update commit");
        let note = note_for_ai_line("file.txt", 2);

        let commit = metric_commit(
            &new_sha,
            &[&parent_sha],
            RewriteMetricOperation::NonFastForward,
        )
        .with_authorship_note(note);

        let events =
            build_rewrite_metric_events(tmp.gitai_repo(), &[commit]).expect("rewrite events");

        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].event_id,
            crate::metrics::types::MetricEventId::LifecycleTransition as u16
        );
        assert_eq!(
            events[1]
                .attrs
                .get(&crate::metrics::attrs::attr_pos::BASE_COMMIT_SHA.to_string()),
            Some(&serde_json::json!(parent_sha))
        );
        assert_eq!(
            events[1]
                .values
                .get(&rewrite_committed_pos::GIT_DIFF_ADDED_LINES.to_string()),
            Some(&serde_json::json!(1))
        );
        let hunks = events[1]
            .values
            .get(&rewrite_committed_pos::HUNKS.to_string())
            .and_then(|value| value.as_str())
            .expect("rewrite event hunks");
        let hunks: serde_json::Value = serde_json::from_str(hunks).expect("valid hunk json");
        assert!(hunks.as_array().is_some_and(|items| !items.is_empty()));
        let expected_content_hash = format!("{:x}", Sha256::digest(b"ai"));
        assert!(
            hunks
                .as_array()
                .is_some_and(|items| items.iter().any(|item| {
                    item.get("hunk_kind").and_then(|value| value.as_str()) == Some("addition")
                        && item.get("content_hash").and_then(|value| value.as_str())
                            == Some(expected_content_hash.as_str())
                }))
        );
    }

    #[test]
    fn rewrite_metric_worker_hydrates_initial_parent_diff() {
        let tmp = crate::git::test_utils::TmpRepo::new().expect("tmp repo");
        tmp.write_file("file.txt", "ai\n", false)
            .expect("write root");
        let root_sha = tmp.commit_all("root").expect("root commit");
        let note = note_for_ai_line("file.txt", 1);

        let commit = metric_commit(&root_sha, &["old"], RewriteMetricOperation::Amend)
            .with_authorship_note(note);

        let events =
            build_rewrite_metric_events(tmp.gitai_repo(), &[commit]).expect("rewrite events");

        assert_eq!(events.len(), 2);
        assert_eq!(
            events[1]
                .attrs
                .get(&crate::metrics::attrs::attr_pos::BASE_COMMIT_SHA.to_string()),
            Some(&serde_json::Value::Null)
        );
        assert_eq!(
            events[1]
                .values
                .get(&rewrite_committed_pos::GIT_DIFF_ADDED_LINES.to_string()),
            Some(&serde_json::json!(1))
        );
    }

    #[test]
    fn lifecycle_operation_id_is_stable_and_branch_sensitive() {
        let first = lifecycle_operation_id("reset", "old", "new", Some("main"));
        assert_eq!(
            first,
            lifecycle_operation_id("reset", "old", "new", Some("main"))
        );
        assert_ne!(
            first,
            lifecycle_operation_id("reset", "old", "new", Some("feature"))
        );
    }

    #[test]
    fn lifecycle_events_chunk_at_512_and_never_invalidate_copy_sources() {
        let tmp = crate::git::test_utils::TmpRepo::new().expect("tmp repo");
        let context = RewriteMetricBatchContext::new(tmp.gitai_repo());
        let invalidated = (0..513).map(|i| format!("old-{i}")).collect::<Vec<_>>();
        let events = build_lifecycle_events_checked(
            "rebase",
            "old-tip",
            "new-tip",
            Some("main"),
            &invalidated,
            &[],
            "ref_transition",
            &context,
        )
        .expect("bounded lifecycle events");
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|event| event.event_id == 8));
        assert_eq!(
            events[0].values
                [&crate::metrics::events::lifecycle_transition_pos::INVALIDATED_COMMIT_SHAS
                    .to_string()]
                .as_array()
                .map(Vec::len),
            Some(512)
        );

        let cherry = metric_commit("copy", &["source"], RewriteMetricOperation::CherryPick);
        assert!(
            build_mapped_lifecycle_events(&cherry, &context)
                .expect("copy-like lifecycle")
                .is_empty()
        );
    }

    #[test]
    fn replacement_chunks_keep_both_sides_with_anchor_after_one_side_exhausts() {
        let tmp = crate::git::test_utils::TmpRepo::new().expect("tmp repo");
        let context = RewriteMetricBatchContext::new(tmp.gitai_repo());
        let invalidated = (0..513).map(|i| format!("old-{i}")).collect::<Vec<_>>();
        let replacements = vec!["new-anchor".to_string()];
        let events = build_lifecycle_events_checked(
            "rebase",
            "old-tip",
            "new-tip",
            Some("main"),
            &invalidated,
            &replacements,
            "replacement",
            &context,
        )
        .expect("bounded replacement lifecycle events");

        assert_eq!(events.len(), 2);
        for event in &events {
            let old = event.values
                [&crate::metrics::events::lifecycle_transition_pos::INVALIDATED_COMMIT_SHAS
                    .to_string()]
                .as_array()
                .expect("old side");
            let new = event.values
                [&crate::metrics::events::lifecycle_transition_pos::REPLACEMENT_COMMIT_SHAS
                    .to_string()]
                .as_array()
                .expect("new side");
            assert!(!old.is_empty());
            assert!(!new.is_empty());
            assert!(old.len() <= 512);
            assert!(new.len() <= 512);
            assert_eq!(new, &vec![serde_json::json!("new-anchor")]);
        }
    }

    #[test]
    fn lifecycle_bundle_uses_one_timestamp_and_rejects_aggregate_overflow() {
        let invalidated = (0..513).map(|i| format!("old-{i}")).collect::<Vec<_>>();
        let events = build_lifecycle_events_from_snapshot(
            "reset",
            "old-tip",
            "new-tip",
            Some("main"),
            &invalidated,
            &[],
            "ref_transition",
            SparseArray::new(),
            1_700_000_123,
        )
        .expect("bounded lifecycle bundle");
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|event| event.timestamp == 1_700_000_123));

        let oversized = (0..=MAX_LIFECYCLE_UNIQUE_COMMITS)
            .map(|i| format!("old-{i}"))
            .collect::<Vec<_>>();
        let error = build_lifecycle_events_from_snapshot(
            "reset",
            "old-tip",
            "new-tip",
            Some("main"),
            &oversized,
            &[],
            "ref_transition",
            SparseArray::new(),
            1_700_000_123,
        )
        .expect_err("oversized lifecycle bundle must be rejected");
        assert!(error.to_string().contains("limit is 32768"));
    }

    #[test]
    fn lifecycle_bundle_rejects_cumulative_serialized_payload_overflow() {
        let mut attrs = SparseArray::new();
        attrs.insert(
            "30".to_string(),
            serde_json::Value::String("x".repeat(MAX_LIFECYCLE_BUNDLE_EVENT_BYTES)),
        );
        let error = build_lifecycle_events_from_snapshot(
            "reset",
            "old-tip",
            "new-tip",
            Some("main"),
            &["old".to_string()],
            &[],
            "ref_transition",
            attrs,
            1_700_000_123,
        )
        .expect_err("oversized serialized lifecycle payload must be rejected");
        assert!(error.to_string().contains("bytes"));
    }

    #[test]
    fn amend_rebase_and_reset_emit_strong_replacement_or_ref_semantics() {
        let tmp = crate::git::test_utils::TmpRepo::new().expect("tmp repo");
        let context = RewriteMetricBatchContext::new(tmp.gitai_repo());

        for operation in [
            RewriteMetricOperation::Amend,
            RewriteMetricOperation::Rebase,
        ] {
            let commit = metric_commit("new", &["old"], operation);
            let events =
                build_mapped_lifecycle_events(&commit, &context).expect("mapped lifecycle");
            assert_eq!(events.len(), 1);
            assert_eq!(
                events[0].values
                    [&crate::metrics::events::lifecycle_transition_pos::INVALIDATED_COMMIT_SHAS
                        .to_string()],
                serde_json::json!(["old"])
            );
            assert_eq!(
                events[0].values
                    [&crate::metrics::events::lifecycle_transition_pos::REPLACEMENT_COMMIT_SHAS
                        .to_string()],
                serde_json::json!(["new"])
            );
        }

        let reset = build_lifecycle_events_checked(
            "reset",
            "old-tip",
            "new-tip",
            Some("main"),
            &[],
            &[],
            "ref_transition",
            &context,
        )
        .expect("bounded reset lifecycle events");
        assert_eq!(reset.len(), 1);
        assert_eq!(
            reset[0].values[&crate::metrics::events::lifecycle_transition_pos::OLD_TIP.to_string()],
            serde_json::json!("old-tip")
        );
        assert_eq!(
            reset[0].values[&crate::metrics::events::lifecycle_transition_pos::NEW_TIP.to_string()],
            serde_json::json!("new-tip")
        );
    }

    #[test]
    fn mapped_lifecycle_limit_failure_is_not_silently_dropped() {
        let tmp = crate::git::test_utils::TmpRepo::new().expect("tmp repo");
        let context = RewriteMetricBatchContext::new(tmp.gitai_repo());
        let originals = (0..=MAX_LIFECYCLE_UNIQUE_COMMITS)
            .map(|index| format!("old-{index}"))
            .collect::<Vec<_>>();
        let commit =
            RewriteMetricCommit::new("new".to_string(), originals, RewriteMetricOperation::Rebase);

        let error = build_mapped_lifecycle_events(&commit, &context)
            .expect_err("oversized strong lifecycle evidence must fail the tracked task");
        assert!(error.to_string().contains("limit is 32768"));
    }

    #[test]
    fn reset_transition_enumeration_captures_all_backward_and_forward_commits() {
        let tmp = crate::git::test_utils::TmpRepo::new().expect("tmp repo");
        tmp.write_file("file.txt", "a\n", false).expect("write a");
        let first = tmp.commit_all("first").expect("first");
        tmp.write_file("file.txt", "a\nb\n", false)
            .expect("write b");
        let second = tmp.commit_all("second").expect("second");
        tmp.write_file("file.txt", "a\nb\nc\n", false)
            .expect("write c");
        let third = tmp.commit_all("third").expect("third");

        let (invalidated, replacements) =
            exclusive_commits_for_transition(tmp.gitai_repo(), &third, &first)
                .expect("backward reset range");
        assert_eq!(invalidated, vec![third.clone(), second.clone()]);
        assert!(replacements.is_empty());

        let (invalidated, replacements) =
            exclusive_commits_for_transition(tmp.gitai_repo(), &first, &third)
                .expect("forward reset range");
        assert!(invalidated.is_empty());
        assert_eq!(replacements, vec![third, second]);
    }

    #[test]
    fn failed_ref_transition_enumeration_does_not_build_tip_only_lifecycle_event() {
        let tmp = crate::git::test_utils::TmpRepo::new().expect("tmp repo");
        tmp.write_file("file.txt", "a\n", false).expect("write");
        tmp.commit_all("first").expect("commit");

        let result = build_ref_lifecycle_transition_events(
            tmp.gitai_repo(),
            "reset",
            "missing-old-tip",
            "missing-new-tip",
            Some("main".to_string()),
            "ref_transition",
        );

        assert!(result.is_err());
    }

    #[test]
    fn no_op_ref_transition_does_not_build_tip_only_lifecycle_event() {
        let tmp = crate::git::test_utils::TmpRepo::new().expect("tmp repo");
        tmp.write_file("file.txt", "a\n", false).expect("write");
        let tip = tmp.commit_all("first").expect("commit");

        let events = build_ref_lifecycle_transition_events(
            tmp.gitai_repo(),
            "reset",
            &tip,
            &tip,
            Some("main".to_string()),
            "ref_transition",
        )
        .expect("enumerate no-op transition");

        assert!(events.is_empty());
    }

    #[test]
    fn reset_lifecycle_branch_falls_back_to_current_head_off_ingestion_path() {
        let tmp = crate::git::test_utils::TmpRepo::new().expect("tmp repo");
        tmp.write_file("file.txt", "a\n", false).expect("write");
        tmp.commit_all("first").expect("commit");
        let expected = tmp
            .gitai_repo()
            .head()
            .expect("head")
            .shorthand()
            .expect("branch")
            .to_string();

        assert_eq!(
            resolve_lifecycle_branch(tmp.gitai_repo(), None),
            Some(expected)
        );
        assert_eq!(
            resolve_lifecycle_branch(tmp.gitai_repo(), Some("captured".to_string())),
            Some("captured".to_string())
        );
        assert_eq!(
            resolve_lifecycle_branch(tmp.gitai_repo(), Some("refs/heads/captured".to_string())),
            Some("captured".to_string())
        );
        assert_eq!(
            resolve_lifecycle_branch(tmp.gitai_repo(), Some("HEAD".to_string())),
            None
        );
    }

    #[test]
    fn detached_head_is_not_fabricated_as_a_lifecycle_branch() {
        let tmp = crate::git::test_utils::TmpRepo::new().expect("tmp repo");
        tmp.write_file("file.txt", "a\n", false).expect("write");
        let tip = tmp.commit_all("first").expect("commit");
        tmp.git_command(&["checkout", "--detach", &tip])
            .expect("detach HEAD");

        assert_eq!(resolve_lifecycle_branch(tmp.gitai_repo(), None), None);
    }
}
