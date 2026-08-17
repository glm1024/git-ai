use crate::authorship::authorship_log_serialization::generate_trace_id;
use crate::authorship::working_log::{AgentId, CheckpointKind};
use crate::checkpoint_content_budget::CheckpointContentBudget;
use crate::commands::checkpoint_agent::presets::{
    KnownHumanEdit, ParsedHookEvent, PostBashCall, PostFileEdit, PreBashCall, PreFileEdit,
    StreamSource, UntrackedEdit,
};
use crate::config;
use crate::daemon::checkpoint::PreparedPathRole;
use crate::error::GitAiError;
use crate::git::repo_state::{read_head_state_for_worktree, worktree_root_for_path};
use crate::git::repository::discover_repository_in_path_no_git_exec;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BaseCommit {
    Sha(String),
    Initial,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointFile {
    pub path: PathBuf,
    pub content: Option<String>,
    pub repo_work_dir: PathBuf,
    pub base_commit: BaseCommit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointRequest {
    pub trace_id: String,
    pub checkpoint_kind: CheckpointKind,
    pub agent_id: Option<AgentId>,
    pub files: Vec<CheckpointFile>,
    pub path_role: PreparedPathRole,
    pub stream_source: Option<StreamSource>,
    pub metadata: HashMap<String, String>,
}

#[derive(Serialize)]
struct CheckpointDebugLogEntry<'a> {
    timestamp: String,
    preset_name: &'a str,
    hook_input: &'a str,
    trace_id: &'a str,
    event_count: usize,
    requests: &'a [CheckpointRequest],
}

struct RepoContext {
    repo_work_dir: PathBuf,
    base_commit: BaseCommit,
}

const MAX_CHECKPOINT_FILES: usize = 1000;

fn checkpoint_content_error(path: &Path, reason: impl std::fmt::Display) -> GitAiError {
    GitAiError::PresetError(format!(
        "checkpoint content unavailable for {}: {}",
        path.display(),
        reason
    ))
}

fn reject_nul_binary_content(path: &Path, content: &str) -> Result<(), GitAiError> {
    if content.as_bytes().contains(&0) {
        return Err(checkpoint_content_error(
            path,
            "binary content contains a NUL byte and cannot be checkpointed",
        ));
    }
    Ok(())
}

fn metadata_error_means_file_is_missing(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound
}

fn apply_checkpoint_content_budget(
    files: &mut [CheckpointFile],
    strict_errors: bool,
) -> Result<(), GitAiError> {
    let mut budget = CheckpointContentBudget::from_config(config::Config::get());
    for file in files {
        let Some(content) = file.content.as_ref() else {
            if strict_errors {
                return Err(checkpoint_content_error(
                    &file.path,
                    "file content could not be read",
                ));
            }
            continue;
        };
        if strict_errors {
            reject_nul_binary_content(&file.path, content)?;
        }
        if let Err(error) = budget.try_reserve(file.path.display(), content) {
            if strict_errors {
                return Err(checkpoint_content_error(&file.path, error));
            }
            file.content = None;
        }
    }
    Ok(())
}

fn apply_dirty_file_overrides(
    files: &mut [CheckpointFile],
    dirty_files: &HashMap<PathBuf, String>,
    strict_errors: bool,
) -> Result<(), GitAiError> {
    for file in &mut *files {
        if let Some(override_content) = dirty_files.get(&file.path) {
            file.content = Some(override_content.clone());
        }
    }
    apply_checkpoint_content_budget(files, strict_errors)
}

fn build_checkpoint_files(
    file_paths: &[PathBuf],
    dirty_files: Option<&HashMap<PathBuf, String>>,
    strict_errors: bool,
) -> Result<Vec<CheckpointFile>, GitAiError> {
    build_checkpoint_files_with_budget(
        file_paths,
        dirty_files,
        strict_errors,
        CheckpointContentBudget::from_config(config::Config::get()),
    )
}

fn build_checkpoint_files_with_budget(
    file_paths: &[PathBuf],
    dirty_files: Option<&HashMap<PathBuf, String>>,
    strict_errors: bool,
    mut content_budget: CheckpointContentBudget,
) -> Result<Vec<CheckpointFile>, GitAiError> {
    let perf = std::env::var("GIT_AI_DEBUG_PERFORMANCE").is_ok_and(|v| !v.is_empty() && v != "0");

    if file_paths.len() > MAX_CHECKPOINT_FILES {
        tracing::warn!(
            "build_checkpoint_files called with {} paths (max {}); truncating",
            file_paths.len(),
            MAX_CHECKPOINT_FILES,
        );
    }
    let capped_paths = &file_paths[..file_paths.len().min(MAX_CHECKPOINT_FILES)];

    let mut repo_cache: HashMap<PathBuf, RepoContext> = HashMap::new();
    let mut files = Vec::new();
    let max_size = content_budget.max_file_size_bytes();

    for path in capped_paths {
        if !path.is_absolute() {
            return Err(GitAiError::PresetError(format!(
                "file path must be absolute: {}",
                path.display()
            )));
        }

        let ctx = {
            let t_discover = std::time::Instant::now();
            let repo_work_dir = worktree_root_for_path(path).ok_or_else(|| {
                GitAiError::Generic(format!(
                    "No git repository found for path: {}",
                    path.display()
                ))
            })?;
            if !repo_cache.contains_key(&repo_work_dir) {
                let t_head = std::time::Instant::now();
                let base_commit = match read_head_state_for_worktree(&repo_work_dir) {
                    Some(state) => match state.head {
                        Some(sha) => BaseCommit::Sha(sha),
                        None => BaseCommit::Initial,
                    },
                    None => BaseCommit::Initial,
                };
                let head_ms = t_head.elapsed().as_secs_f64() * 1000.0;

                if perf {
                    eprintln!(
                        "[perf] build_checkpoint_files: discover={:.1}ms head={:.1}ms (repo={})",
                        t_discover.elapsed().as_secs_f64() * 1000.0,
                        head_ms,
                        repo_work_dir.display(),
                    );
                }

                let key = repo_work_dir.clone();
                repo_cache.insert(
                    key,
                    RepoContext {
                        repo_work_dir: repo_work_dir.clone(),
                        base_commit,
                    },
                );
            }
            repo_cache.get(&repo_work_dir).unwrap()
        };

        let t_read = std::time::Instant::now();
        let override_available = dirty_files.is_some_and(|files| files.contains_key(path));
        let content = match fs::metadata(path) {
            Ok(meta) => {
                if meta.len() as usize > max_size {
                    let reason = format!(
                        "file has {} bytes, exceeding the per-file checkpoint limit of {} bytes",
                        meta.len(),
                        max_size
                    );
                    tracing::warn!(
                        "skipping file larger than max_checkpoint_file_size_bytes: {} ({} bytes)",
                        path.display(),
                        meta.len(),
                    );
                    if strict_errors {
                        return Err(checkpoint_content_error(path, reason));
                    }
                    continue;
                }
                match fs::read_to_string(path) {
                    Ok(content) => Some(content),
                    Err(error) => {
                        if strict_errors && !override_available {
                            let reason = if error.kind() == std::io::ErrorKind::InvalidData {
                                format!("file is binary or not valid UTF-8: {error}")
                            } else {
                                format!("failed to read file content: {error}")
                            };
                            return Err(checkpoint_content_error(path, reason));
                        }
                        None
                    }
                }
            }
            Err(error) if metadata_error_means_file_is_missing(&error) => Some(String::new()),
            Err(error) => {
                if strict_errors && !override_available {
                    return Err(checkpoint_content_error(
                        path,
                        format!("failed to read file metadata: {error}"),
                    ));
                }
                // Preserve the historical non-strict behavior: an unavailable
                // metadata lookup is represented as an empty file snapshot.
                Some(String::new())
            }
        };
        if perf {
            eprintln!(
                "[perf] build_checkpoint_files: read_file={:.1}ms (path={}, size={})",
                t_read.elapsed().as_secs_f64() * 1000.0,
                path.display(),
                content.as_ref().map(|c| c.len()).unwrap_or(0),
            );
        }

        if strict_errors
            && !override_available
            && let Some(content) = content.as_ref()
        {
            reject_nul_binary_content(path, content)?;
        }

        let content = match content {
            Some(content) => match content_budget.try_reserve(path.display(), &content) {
                Ok(()) => Some(content),
                Err(error) => {
                    if strict_errors && !override_available {
                        return Err(checkpoint_content_error(path, error));
                    }
                    None
                }
            },
            None => None,
        };

        files.push(CheckpointFile {
            path: path.clone(),
            content,
            repo_work_dir: ctx.repo_work_dir.clone(),
            base_commit: ctx.base_commit.clone(),
        });
    }

    Ok(files)
}

pub fn execute_preset_checkpoint(
    preset_name: &str,
    hook_input: &str,
) -> Result<Vec<CheckpointRequest>, GitAiError> {
    execute_preset_checkpoint_with_mode(preset_name, hook_input, false)
}

pub fn execute_preset_checkpoint_strict(
    preset_name: &str,
    hook_input: &str,
) -> Result<Vec<CheckpointRequest>, GitAiError> {
    execute_preset_checkpoint_with_mode(preset_name, hook_input, true)
}

fn execute_preset_checkpoint_with_mode(
    preset_name: &str,
    hook_input: &str,
    strict_errors: bool,
) -> Result<Vec<CheckpointRequest>, GitAiError> {
    let perf = std::env::var("GIT_AI_DEBUG_PERFORMANCE").is_ok_and(|v| !v.is_empty() && v != "0");
    let t0 = std::time::Instant::now();

    let trace_id = generate_trace_id();
    let preset = super::presets::resolve_preset(preset_name)?;
    let events = preset.parse(hook_input, &trace_id)?;
    let events_len = events.len();

    if perf {
        eprintln!(
            "[perf] orchestrator: parse={:.1}ms (events={})",
            t0.elapsed().as_secs_f64() * 1000.0,
            events_len,
        );
    }

    let mut requests = Vec::new();
    for event in events {
        let t_event = std::time::Instant::now();
        let event_name = format!("{:?}", std::mem::discriminant(&event));
        let new_requests = execute_event(event, preset_name, strict_errors)?;
        if perf {
            eprintln!(
                "[perf] orchestrator: execute_event({})={:.1}ms (requests={})",
                event_name,
                t_event.elapsed().as_secs_f64() * 1000.0,
                new_requests.len(),
            );
        }
        requests.extend(new_requests);
    }

    if config::Config::get()
        .get_feature_flags()
        .checkpoint_debug_log
    {
        write_checkpoint_debug_log(preset_name, hook_input, &trace_id, events_len, &requests);
    }

    Ok(requests)
}

fn write_checkpoint_debug_log(
    preset_name: &str,
    hook_input: &str,
    trace_id: &str,
    event_count: usize,
    requests: &[CheckpointRequest],
) {
    let Some(internal_dir) = config::internal_dir_path() else {
        return;
    };

    let log_dir = internal_dir.join("checkpoint-debug-logs");
    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let log_path = log_dir.join(format!("{}.log", date));

    if let Err(e) = fs::create_dir_all(&log_dir) {
        eprintln!("[checkpoint_debug_log] failed to create dir: {}", e);
        return;
    }

    cleanup_old_debug_logs(&log_dir);

    let entry = CheckpointDebugLogEntry {
        timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        preset_name,
        hook_input,
        trace_id,
        event_count,
        requests,
    };

    let Ok(line) = serde_json::to_string(&entry) else {
        return;
    };

    let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    else {
        return;
    };

    let _ = file
        .write_all(line.as_bytes())
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.flush());
}

fn cleanup_old_debug_logs(log_dir: &Path) {
    let Ok(entries) = fs::read_dir(log_dir) else {
        return;
    };

    let cutoff = chrono::Utc::now() - chrono::Duration::days(14);

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if let Ok(file_date) = chrono::NaiveDate::parse_from_str(stem, "%Y-%m-%d")
            && file_date < cutoff.date_naive()
        {
            let _ = fs::remove_file(&path);
        }
    }
}

fn execute_event(
    event: ParsedHookEvent,
    preset_name: &str,
    strict_errors: bool,
) -> Result<Vec<CheckpointRequest>, GitAiError> {
    match event {
        ParsedHookEvent::PreFileEdit(e) => execute_pre_file_edit(e, strict_errors),
        ParsedHookEvent::PostFileEdit(e) => execute_post_file_edit(e, preset_name, strict_errors),
        ParsedHookEvent::PreBashCall(e) => execute_pre_bash_call(e, strict_errors),
        ParsedHookEvent::PostBashCall(e) => execute_post_bash_call(e, strict_errors),
        ParsedHookEvent::KnownHumanEdit(e) => execute_known_human_edit(e),
        ParsedHookEvent::UntrackedEdit(e) => execute_untracked_edit(e),
    }
}

fn split_files_into_requests(
    all_files: Vec<CheckpointFile>,
    trace_id: String,
    checkpoint_kind: CheckpointKind,
    agent_id: Option<AgentId>,
    path_role: PreparedPathRole,
    stream_source: Option<StreamSource>,
    metadata: HashMap<String, String>,
) -> Vec<CheckpointRequest> {
    let all_files: Vec<CheckpointFile> = all_files
        .into_iter()
        .filter(|f| f.content.is_some())
        .collect();
    let mut by_repo: HashMap<PathBuf, Vec<CheckpointFile>> = HashMap::new();
    for f in all_files {
        by_repo.entry(f.repo_work_dir.clone()).or_default().push(f);
    }

    by_repo
        .into_values()
        .map(|files| CheckpointRequest {
            trace_id: trace_id.clone(),
            checkpoint_kind,
            agent_id: agent_id.clone(),
            files,
            path_role,
            stream_source: stream_source.clone(),
            metadata: metadata.clone(),
        })
        .collect()
}

fn ensure_strict_file_coverage(
    expected_paths: &[PathBuf],
    requests: &[CheckpointRequest],
    phase: &str,
) -> Result<(), GitAiError> {
    let prepared: HashSet<&Path> = requests
        .iter()
        .flat_map(|request| request.files.iter().map(|file| file.path.as_path()))
        .collect();
    let missing: Vec<String> = expected_paths
        .iter()
        .filter(|path| !prepared.contains(path.as_path()))
        .map(|path| path.display().to_string())
        .collect();
    if missing.is_empty() {
        return Ok(());
    }

    Err(GitAiError::PresetError(format!(
        "{phase} checkpoint could not prepare every file: {}",
        missing.join(", ")
    )))
}

fn execute_pre_file_edit(
    e: PreFileEdit,
    strict_errors: bool,
) -> Result<Vec<CheckpointRequest>, GitAiError> {
    let expected_paths = e.file_paths.clone();
    let mut files = build_checkpoint_files(&e.file_paths, e.dirty_files.as_ref(), strict_errors)?;
    if let Some(ref dirty) = e.dirty_files {
        apply_dirty_file_overrides(&mut files, dirty, strict_errors)?;
    }
    let mut metadata = e.context.metadata;
    if let Some(tuid) = e.tool_use_id {
        metadata.entry("tool_use_id".to_string()).or_insert(tuid);
    }
    let requests = split_files_into_requests(
        files,
        e.context.trace_id,
        CheckpointKind::Human,
        Some(e.context.agent_id),
        PreparedPathRole::WillEdit,
        None,
        metadata,
    );
    if strict_errors {
        ensure_strict_file_coverage(&expected_paths, &requests, "Pre-tool")?;
    }
    Ok(requests)
}

fn execute_post_file_edit(
    e: PostFileEdit,
    preset_name: &str,
    strict_errors: bool,
) -> Result<Vec<CheckpointRequest>, GitAiError> {
    let expected_paths = e.file_paths.clone();
    let mut files = build_checkpoint_files(&e.file_paths, e.dirty_files.as_ref(), strict_errors)?;
    if let Some(ref dirty) = e.dirty_files {
        apply_dirty_file_overrides(&mut files, dirty, strict_errors)?;
    }
    let checkpoint_kind = match preset_name {
        "ai_tab" => CheckpointKind::AiTab,
        _ => CheckpointKind::AiAgent,
    };
    let mut metadata = e.context.metadata;
    if let Some(tuid) = e.tool_use_id {
        metadata.entry("tool_use_id".to_string()).or_insert(tuid);
    }
    metadata
        .entry("edit_kind".to_string())
        .or_insert_with(|| "file_edit".to_string());
    let requests = split_files_into_requests(
        files,
        e.context.trace_id,
        checkpoint_kind,
        Some(e.context.agent_id),
        PreparedPathRole::Edited,
        e.stream_source,
        metadata,
    );
    if strict_errors {
        ensure_strict_file_coverage(&expected_paths, &requests, "Post-tool")?;
    }
    Ok(requests)
}

fn execute_known_human_edit(e: KnownHumanEdit) -> Result<Vec<CheckpointRequest>, GitAiError> {
    let mut files = build_checkpoint_files(&e.file_paths, e.dirty_files.as_ref(), false)?;
    if let Some(ref dirty) = e.dirty_files {
        apply_dirty_file_overrides(&mut files, dirty, false)?;
    }
    Ok(split_files_into_requests(
        files,
        e.trace_id,
        CheckpointKind::KnownHuman,
        None,
        PreparedPathRole::Edited,
        None,
        e.editor_metadata,
    ))
}

fn execute_untracked_edit(e: UntrackedEdit) -> Result<Vec<CheckpointRequest>, GitAiError> {
    let files = build_checkpoint_files(&e.file_paths, None, false)?;
    Ok(split_files_into_requests(
        files,
        e.trace_id,
        CheckpointKind::Human,
        None,
        PreparedPathRole::WillEdit,
        None,
        HashMap::new(),
    ))
}

fn execute_pre_bash_call(
    e: PreBashCall,
    strict_errors: bool,
) -> Result<Vec<CheckpointRequest>, GitAiError> {
    use crate::commands::checkpoint_agent::bash_tool::{
        self, BashHookAttemptPhase, BashHookAttemptSignal,
    };

    let started_at_ns = crate::daemon::bash_history_db::unix_time_ns();
    let repo_work_dir = match discover_repository_in_path_no_git_exec(e.context.cwd.as_path())
        .and_then(|repo| repo.workdir())
    {
        Ok(repo_work_dir) => repo_work_dir,
        Err(error) => {
            let error_message = error.to_string();
            let _ = bash_tool::signal_daemon_bash_hook_attempt(
                BashHookAttemptPhase::Start,
                BashHookAttemptSignal {
                    original_cwd: e.context.cwd.as_path(),
                    discovered_repo_work_dir: None,
                    repo_discovery_error: Some(&error_message),
                    session_id: &e.context.external_session_id,
                    tool_use_id: &e.tool_use_id,
                    agent_id: &e.context.agent_id,
                    metadata: &e.context.metadata,
                    trace_id: &e.context.trace_id,
                    timestamp_ns: started_at_ns,
                    command: e.command.as_deref(),
                },
            );
            if strict_errors {
                return Err(GitAiError::PresetError(format!(
                    "Bash pre-hook repository discovery failed: {}",
                    error
                )));
            }
            return Ok(vec![]);
        }
    };

    if config::Config::get()
        .get_feature_flags()
        .bash_checkpoints_v2
    {
        let signal_result = bash_tool::signal_daemon_bash_hook_attempt(
            BashHookAttemptPhase::Start,
            BashHookAttemptSignal {
                original_cwd: e.context.cwd.as_path(),
                discovered_repo_work_dir: Some(&repo_work_dir),
                repo_discovery_error: None,
                session_id: &e.context.external_session_id,
                tool_use_id: &e.tool_use_id,
                agent_id: &e.context.agent_id,
                metadata: &e.context.metadata,
                trace_id: &e.context.trace_id,
                timestamp_ns: started_at_ns,
                command: e.command.as_deref(),
            },
        );
        if let Err(error) = signal_result {
            if strict_errors {
                return Err(error);
            }
            tracing::debug!("Bash pre-hook attempt signal failed: {}", error);
        }
        return Ok(vec![]);
    }

    let dirty_paths = match bash_tool::handle_bash_pre_tool_use_with_context_and_cwd(
        &repo_work_dir,
        e.context.cwd.as_path(),
        bash_tool::BashToolHookContext {
            session_id: &e.context.external_session_id,
            tool_use_id: &e.tool_use_id,
            agent_id: &e.context.agent_id,
            agent_metadata: Some(&e.context.metadata),
            trace_id: &e.context.trace_id,
            command: e.command.as_deref(),
        },
    ) {
        Ok(result) => result.dirty_paths,
        Err(error) => {
            tracing::debug!(
                "Bash pre-hook snapshot failed for {} session {}: {}",
                e.context.agent_id.tool,
                e.context.external_session_id,
                error
            );
            if strict_errors {
                return Err(error);
            }
            return Ok(vec![]);
        }
    };

    let mut metadata = e.context.metadata;
    metadata
        .entry("tool_use_id".to_string())
        .or_insert(e.tool_use_id);
    metadata
        .entry("edit_kind".to_string())
        .or_insert_with(|| "bash".to_string());
    if dirty_paths.is_empty() {
        let base_commit =
            match read_head_state_for_worktree(&repo_work_dir).and_then(|state| state.head) {
                Some(sha) => BaseCommit::Sha(sha),
                None => BaseCommit::Initial,
            };
        return Ok(vec![CheckpointRequest {
            trace_id: e.context.trace_id,
            checkpoint_kind: CheckpointKind::Human,
            agent_id: Some(e.context.agent_id),
            files: vec![CheckpointFile {
                path: repo_work_dir.clone(),
                content: None,
                repo_work_dir,
                base_commit,
            }],
            path_role: PreparedPathRole::WillEdit,
            stream_source: None,
            metadata,
        }]);
    }

    let files = build_checkpoint_files(&dirty_paths, None, strict_errors)?;
    let requests = split_files_into_requests(
        files,
        e.context.trace_id,
        CheckpointKind::Human,
        Some(e.context.agent_id),
        PreparedPathRole::WillEdit,
        None,
        metadata,
    );
    if strict_errors {
        ensure_strict_file_coverage(&dirty_paths, &requests, "Bash pre-tool")?;
    }
    Ok(requests)
}

fn execute_post_bash_call(
    e: PostBashCall,
    strict_errors: bool,
) -> Result<Vec<CheckpointRequest>, GitAiError> {
    use crate::commands::checkpoint_agent::bash_tool::{
        self, BashHookAttemptPhase, BashHookAttemptSignal,
    };

    let ended_at_ns = crate::daemon::bash_history_db::unix_time_ns();
    let repo_work_dir = match discover_repository_in_path_no_git_exec(e.context.cwd.as_path())
        .and_then(|repo| repo.workdir())
    {
        Ok(repo_work_dir) => repo_work_dir,
        Err(error) => {
            let error_message = error.to_string();
            let _ = bash_tool::signal_daemon_bash_hook_attempt(
                BashHookAttemptPhase::End,
                BashHookAttemptSignal {
                    original_cwd: e.context.cwd.as_path(),
                    discovered_repo_work_dir: None,
                    repo_discovery_error: Some(&error_message),
                    session_id: &e.context.external_session_id,
                    tool_use_id: &e.tool_use_id,
                    agent_id: &e.context.agent_id,
                    metadata: &e.context.metadata,
                    trace_id: &e.context.trace_id,
                    timestamp_ns: ended_at_ns,
                    command: e.command.as_deref(),
                },
            );
            if strict_errors {
                return Err(GitAiError::PresetError(format!(
                    "Bash post-hook repository discovery failed: {}",
                    error
                )));
            }
            return Ok(vec![]);
        }
    };

    if config::Config::get()
        .get_feature_flags()
        .bash_checkpoints_v2
    {
        let signal_result = bash_tool::signal_daemon_bash_hook_attempt(
            BashHookAttemptPhase::End,
            BashHookAttemptSignal {
                original_cwd: e.context.cwd.as_path(),
                discovered_repo_work_dir: Some(&repo_work_dir),
                repo_discovery_error: None,
                session_id: &e.context.external_session_id,
                tool_use_id: &e.tool_use_id,
                agent_id: &e.context.agent_id,
                metadata: &e.context.metadata,
                trace_id: &e.context.trace_id,
                timestamp_ns: ended_at_ns,
                command: e.command.as_deref(),
            },
        );
        if let Err(error) = signal_result {
            if strict_errors {
                return Err(error);
            }
            tracing::debug!("Bash post-hook attempt signal failed: {}", error);
        }
        return Ok(vec![]);
    }

    let bash_result = bash_tool::handle_bash_post_tool_use_with_cwd(
        &repo_work_dir,
        e.context.cwd.as_path(),
        bash_tool::BashToolHookContext {
            session_id: &e.context.external_session_id,
            tool_use_id: &e.tool_use_id,
            agent_id: &e.context.agent_id,
            agent_metadata: Some(&e.context.metadata),
            trace_id: &e.context.trace_id,
            command: e.command.as_deref(),
        },
    );

    let file_paths: Vec<PathBuf> = match bash_result {
        Ok(result) => {
            if strict_errors && let Some(message) = strict_bash_action_error(&result.action) {
                return Err(GitAiError::PresetError(message.to_string()));
            }
            match result.action {
                bash_tool::BashCheckpointAction::Checkpoint(paths) => paths
                    .into_iter()
                    .map(|p| {
                        let joined = repo_work_dir.join(p);
                        fs::canonicalize(&joined).unwrap_or(joined)
                    })
                    .collect(),
                bash_tool::BashCheckpointAction::NoChanges => vec![],
                bash_tool::BashCheckpointAction::HookTimeout
                | bash_tool::BashCheckpointAction::SnapshotFailed
                | bash_tool::BashCheckpointAction::MissingPreSnapshot => vec![],
            }
        }
        Err(err) => {
            tracing::debug!("Bash tool post-hook error: {}", err);
            if strict_errors {
                return Err(err);
            }
            vec![]
        }
    };

    let files = build_checkpoint_files(&file_paths, None, strict_errors)?;
    let mut metadata = e.context.metadata;
    metadata
        .entry("tool_use_id".to_string())
        .or_insert(e.tool_use_id);
    metadata
        .entry("edit_kind".to_string())
        .or_insert_with(|| "bash".to_string());
    let requests = split_files_into_requests(
        files,
        e.context.trace_id,
        CheckpointKind::AiAgent,
        Some(e.context.agent_id),
        PreparedPathRole::Edited,
        e.stream_source,
        metadata,
    );
    if strict_errors {
        ensure_strict_file_coverage(&file_paths, &requests, "Bash post-tool")?;
    }
    Ok(requests)
}

fn strict_bash_action_error(
    action: &crate::commands::checkpoint_agent::bash_tool::BashCheckpointAction,
) -> Option<&'static str> {
    use crate::commands::checkpoint_agent::bash_tool::BashCheckpointAction;

    match action {
        BashCheckpointAction::HookTimeout => {
            Some("Bash post-hook timed out before attribution could be persisted")
        }
        BashCheckpointAction::SnapshotFailed => {
            Some("Bash post-hook snapshot failed before attribution could be persisted")
        }
        BashCheckpointAction::MissingPreSnapshot => {
            Some("Bash post-hook has no acknowledged pre-hook snapshot")
        }
        BashCheckpointAction::Checkpoint(_) | BashCheckpointAction::NoChanges => None,
    }
}

#[cfg(test)]
mod strict_mode_tests {
    use super::*;
    use crate::commands::checkpoint_agent::bash_tool::BashCheckpointAction;
    use crate::git::test_utils::TmpRepo;
    use serde_json::json;

    #[test]
    fn strict_bash_mode_rejects_every_incomplete_post_state() {
        assert!(strict_bash_action_error(&BashCheckpointAction::HookTimeout).is_some());
        assert!(strict_bash_action_error(&BashCheckpointAction::SnapshotFailed).is_some());
        assert!(strict_bash_action_error(&BashCheckpointAction::MissingPreSnapshot).is_some());
        assert!(strict_bash_action_error(&BashCheckpointAction::NoChanges).is_none());
        assert!(strict_bash_action_error(&BashCheckpointAction::Checkpoint(vec![])).is_none());
    }

    #[test]
    fn strict_opencode_mixed_scope_keeps_git_files_in_each_repo() {
        let repo_a = TmpRepo::new().unwrap();
        let repo_b = TmpRepo::new().unwrap();
        let path_a = repo_a.write_file("src/a.rs", "fn a() {}\n", false).unwrap();
        let path_b = repo_b.write_file("src/b.rs", "fn b() {}\n", false).unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("outside.rs");

        let hook_input = json!({
            "hook_event_name": "PreToolUse",
            "session_id": "session-mixed",
            "tool_use_id": "call-mixed",
            "cwd": repo_a.path(),
            "tool_name": "apply_patch",
            "git_ai_file_paths": [&path_a, &path_b],
            "tool_input": {
                "file_paths": [&path_a, &path_b, &outside]
            }
        })
        .to_string();

        let requests = execute_preset_checkpoint_strict("opencode", &hook_input).unwrap();
        assert_eq!(
            requests.len(),
            2,
            "one request must be emitted per repository"
        );

        let prepared_paths: HashSet<PathBuf> = requests
            .iter()
            .flat_map(|request| request.files.iter().map(|file| file.path.clone()))
            .collect();
        assert_eq!(prepared_paths, HashSet::from([path_a, path_b]));
        assert!(!prepared_paths.contains(&outside));

        let repo_dirs: HashSet<PathBuf> = requests
            .iter()
            .map(|request| request.files[0].repo_work_dir.clone())
            .collect();
        assert_eq!(
            repo_dirs.len(),
            2,
            "repositories must not be coalesced or crossed"
        );
    }

    #[test]
    fn strict_opencode_rejects_nul_binary_content_before_daemon_ack() {
        let repo = TmpRepo::new().unwrap();
        let path = repo
            .write_file("binary.dat", "valid utf-8\0binary payload", false)
            .unwrap();
        let hook_input = json!({
            "hook_event_name": "PreToolUse",
            "session_id": "session-binary",
            "tool_use_id": "call-binary",
            "cwd": repo.path(),
            "tool_name": "apply_patch",
            "git_ai_file_paths": [&path],
            "tool_input": {"file_path": &path}
        })
        .to_string();

        let error = execute_preset_checkpoint_strict("opencode", &hook_input)
            .expect_err("strict mode must reject content that the daemon cannot checkpoint");
        let message = error.to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(message.contains("binary"), "{message}");
    }

    #[test]
    fn strict_build_rejects_single_file_over_total_budget_with_path_and_reason() {
        let repo = TmpRepo::new().unwrap();
        let path = repo.write_file("only.txt", &"x".repeat(48), false).unwrap();

        let error = build_checkpoint_files_with_budget(
            std::slice::from_ref(&path),
            None,
            true,
            CheckpointContentBudget::with_limits(1024, 32, 1000),
        )
        .expect_err("strict mode must reject a single file over the aggregate budget");
        let message = error.to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(
            message.contains("total checkpoint byte budget"),
            "{message}"
        );
        assert!(message.contains("32 bytes max"), "{message}");
    }

    #[test]
    fn strict_build_rejects_later_file_that_exhausts_batch_budget() {
        let repo = TmpRepo::new().unwrap();
        let first = repo
            .write_file("first.txt", &"a".repeat(24), false)
            .unwrap();
        let second = repo
            .write_file("second.txt", &"b".repeat(24), false)
            .unwrap();

        let error = build_checkpoint_files_with_budget(
            &[first.clone(), second.clone()],
            None,
            true,
            CheckpointContentBudget::with_limits(1024, 32, 1000),
        )
        .expect_err("strict mode must reject a later file dropped by the batch budget");
        let message = error.to_string();
        assert!(!message.contains(&first.display().to_string()), "{message}");
        assert!(message.contains(&second.display().to_string()), "{message}");
        assert!(message.contains("24 bytes already used"), "{message}");
    }

    #[test]
    fn strict_build_rejects_non_utf8_binary_content_with_path_and_reason() {
        let repo = TmpRepo::new().unwrap();
        let path = repo.path().join("binary.dat");
        fs::write(&path, [0xff, 0xfe, 0xfd]).unwrap();

        let error = build_checkpoint_files_with_budget(
            std::slice::from_ref(&path),
            None,
            true,
            CheckpointContentBudget::with_limits(1024, 1024, 1000),
        )
        .expect_err("strict mode must reject content that cannot be read as UTF-8");
        let message = error.to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(message.contains("binary or not valid UTF-8"), "{message}");
    }

    #[test]
    fn strict_metadata_only_treats_not_found_as_an_empty_file() {
        let missing = std::io::Error::from(std::io::ErrorKind::NotFound);
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let interrupted = std::io::Error::from(std::io::ErrorKind::Interrupted);

        assert!(metadata_error_means_file_is_missing(&missing));
        assert!(!metadata_error_means_file_is_missing(&denied));
        assert!(!metadata_error_means_file_is_missing(&interrupted));
    }

    #[test]
    fn strict_budget_validation_rejects_a_missing_file_body() {
        let path = PathBuf::from("/repo/missing-body.txt");
        let mut files = vec![CheckpointFile {
            path: path.clone(),
            content: None,
            repo_work_dir: PathBuf::from("/repo"),
            base_commit: BaseCommit::Initial,
        }];

        let error = apply_checkpoint_content_budget(&mut files, true)
            .expect_err("strict mode must not prepare a path without a file body");
        let message = error.to_string();
        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(message.contains("could not be read"), "{message}");
    }
}
