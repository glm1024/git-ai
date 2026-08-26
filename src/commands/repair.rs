//! Explicit operator recovery for unverifiable durable checkpoint evidence.
//!
//! This command is deliberately offline and two-step. It first prints an
//! immutable impact preview. Only a second invocation carrying that preview's
//! repair id may archive the broken working log and terminally abandon the
//! affected repository FIFO.

use crate::daemon::DaemonConfig;
use crate::error::GitAiError;
use crate::git::repo_state::{
    common_dir_for_git_dir, git_dir_for_worktree, worktree_root_for_path,
};
use crate::git::repo_storage::PersistedWorkingLog;
use crate::git::repository::worktree_storage_ai_dir;
use crate::metrics::deferred_checkpoint_jobs::{
    ManualCheckpointRepairPlan, manual_repair_plan_global, manually_abandon_repo_fifo_global,
    repository_identity,
};
use crate::utils::LockFile;
use serde::Serialize;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

const CONFIRM_FLAG: &str = "--confirm";
const UNAVAILABLE_REPOSITORY_REPAIR_SUFFIX: &str = "-unavailable";
const FROZEN_EVIDENCE_ONLY_REPAIR_SUFFIX: &str = "-evidence-only";

#[derive(Debug, Clone, PartialEq, Eq)]
struct RepairOptions {
    job_key: String,
    confirmation: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CheckpointRepairMode {
    RepositoryWorkingLog,
    FrozenEvidenceOnly,
    UnavailableRepository,
}

impl CheckpointRepairMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::RepositoryWorkingLog => "repository_working_log",
            Self::FrozenEvidenceOnly => "frozen_evidence_only",
            Self::UnavailableRepository => "unavailable_repository",
        }
    }
}

#[derive(Debug)]
struct CheckpointRepairTarget {
    mode: CheckpointRepairMode,
    backup_root: PathBuf,
    backup_dir: PathBuf,
    repository_workdir: Option<PathBuf>,
    source_working_log: Option<PathBuf>,
    archived_working_log: Option<PathBuf>,
    mode_reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct RepairCompletion<'a> {
    version: u32,
    mode: &'a str,
    repair_id: &'a str,
    target_job_key: &'a str,
    repo_identity: &'a str,
    repository_workdir: &'a str,
    base_commit: &'a str,
    affected_job_count: usize,
    evidence_file: String,
    archived_working_log: Option<String>,
    unavailable_repository_reason: Option<&'a str>,
    frozen_evidence_only_reason: Option<&'a str>,
    working_log_boundary: &'a str,
}

pub(crate) fn handle_repair(args: &[String]) {
    match run_repair(args) {
        Ok(()) => {}
        Err(error) => {
            eprintln!("repair: {error}");
            std::process::exit(1);
        }
    }
}

fn run_repair(args: &[String]) -> Result<(), GitAiError> {
    if args.is_empty() || matches!(args[0].as_str(), "--help" | "-h" | "help") {
        print_help();
        return Ok(());
    }
    if args[0] != "checkpoint-baseline" {
        return Err(GitAiError::Generic(format!(
            "unknown repair subcommand {}; run `git-ai repair --help`",
            args[0]
        )));
    }
    if args[1..]
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h" | "help"))
    {
        print_checkpoint_baseline_help();
        return Ok(());
    }
    let options = parse_checkpoint_baseline_options(&args[1..])?;
    repair_checkpoint_baseline(&options)
}

fn parse_checkpoint_baseline_options(args: &[String]) -> Result<RepairOptions, GitAiError> {
    let mut job_key = None;
    let mut confirmation = None;
    let mut index = 0usize;
    while index < args.len() {
        match args[index].as_str() {
            "--job-key" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| GitAiError::Generic("--job-key requires a value".to_string()))?;
                if job_key.replace(value.clone()).is_some() {
                    return Err(GitAiError::Generic(
                        "--job-key may only be specified once".to_string(),
                    ));
                }
                index += 2;
            }
            CONFIRM_FLAG => {
                let value = args.get(index + 1).ok_or_else(|| {
                    GitAiError::Generic(format!("{CONFIRM_FLAG} requires a repair id"))
                })?;
                if confirmation.replace(value.clone()).is_some() {
                    return Err(GitAiError::Generic(format!(
                        "{CONFIRM_FLAG} may only be specified once"
                    )));
                }
                index += 2;
            }
            other => {
                return Err(GitAiError::Generic(format!(
                    "unknown checkpoint-baseline argument {other}"
                )));
            }
        }
    }
    let job_key = job_key.ok_or_else(|| {
        GitAiError::Generic("checkpoint-baseline requires --job-key <64-hex-key>".to_string())
    })?;
    if job_key.len() != 64 || !job_key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(GitAiError::Generic(
            "--job-key must be the 64-character hexadecimal key printed by `git-ai await`"
                .to_string(),
        ));
    }
    Ok(RepairOptions {
        job_key,
        confirmation,
    })
}

fn repair_checkpoint_baseline(options: &RepairOptions) -> Result<(), GitAiError> {
    let daemon_config = DaemonConfig::from_env_or_default_paths()?;
    daemon_config.ensure_parent_dirs()?;
    let _daemon_offline_guard = match LockFile::try_acquire_result(&daemon_config.lock_path)? {
        Some(lock) => lock,
        None => {
            return Err(GitAiError::Generic(
                "the git-ai background service is running. Run `git-ai daemon shutdown`, verify it has stopped, then repeat the repair preview"
                    .to_string(),
            ));
        }
    };

    let plan = manual_repair_plan_global(&options.job_key)?;
    let (plan, target) = resolve_checkpoint_repair_target(plan, &daemon_config)?;
    reject_symlink_if_present(&target.backup_root, "repair backup root")?;
    reject_symlink_if_present(&target.backup_dir, "repair backup directory")?;

    let evidence_file = target.backup_dir.join("deferred-checkpoint-evidence.json");
    let completion_file = target.backup_dir.join("REPAIR-COMPLETE.json");
    if plan.already_terminal {
        match target.mode {
            CheckpointRepairMode::RepositoryWorkingLog => {
                let archived = target
                    .archived_working_log
                    .as_deref()
                    .expect("repository repair has an archive path");
                let source = target
                    .source_working_log
                    .as_deref()
                    .expect("repository repair has a source path");
                let workdir = target
                    .repository_workdir
                    .as_deref()
                    .expect("repository repair has a worktree");
                if archived.is_dir() {
                    validate_archived_corrupt_initial(&plan, archived, workdir)?;
                    validate_recreated_active_baseline(&plan, source, workdir)?;
                    // On Windows Rust cannot fsync a directory entry. If power
                    // loss drops a backup filename after SQLite reached its
                    // terminal tombstone, rebuild it from retained frozen rows.
                    ensure_private_backup_directory(&target)?;
                    preserve_deferred_evidence(&evidence_file, &plan)?;
                    write_repair_completion(&completion_file, &plan, &target, &evidence_file)?;
                    print_preview(&plan, &target);
                    eprintln!(
                        "Repair is already complete; no additional repair action was performed."
                    );
                    return Ok(());
                }
            }
            CheckpointRepairMode::FrozenEvidenceOnly
            | CheckpointRepairMode::UnavailableRepository => {
                ensure_private_backup_directory(&target)?;
                preserve_deferred_evidence(&evidence_file, &plan)?;
                write_repair_completion(&completion_file, &plan, &target, &evidence_file)?;
                print_preview(&plan, &target);
                eprintln!("Repair is already complete; no additional repair action was performed.");
                return Ok(());
            }
        }
    }

    if target.mode == CheckpointRepairMode::RepositoryWorkingLog {
        validate_repairable_initial(
            &plan,
            target
                .source_working_log
                .as_deref()
                .expect("repository repair has a source path"),
            target
                .archived_working_log
                .as_deref()
                .expect("repository repair has an archive path"),
            target
                .repository_workdir
                .as_deref()
                .expect("repository repair has a worktree"),
        )?;
    }

    print_preview(&plan, &target);
    let Some(confirmation) = options.confirmation.as_deref() else {
        eprintln!();
        eprintln!("No checkpoint repair action was performed.");
        eprintln!(
            "After reviewing the impact, rerun:\n  git-ai repair checkpoint-baseline --job-key {} {} {}",
            plan.target_job_key, CONFIRM_FLAG, plan.repair_id
        );
        return Ok(());
    };
    if confirmation != plan.repair_id {
        return Err(GitAiError::Generic(format!(
            "confirmation id does not match the current impact preview; rerun without {CONFIRM_FLAG} and use {}",
            plan.repair_id
        )));
    }

    ensure_private_backup_directory(&target)?;
    preserve_deferred_evidence(&evidence_file, &plan)?;

    let backup_path_string = target.backup_dir.to_string_lossy().to_string();
    let abandoned = manually_abandon_repo_fifo_global(&plan, &backup_path_string)?;
    if target.mode == CheckpointRepairMode::RepositoryWorkingLog {
        let source = target
            .source_working_log
            .as_deref()
            .expect("repository repair has a source path");
        let archived = target
            .archived_working_log
            .as_deref()
            .expect("repository repair has an archive path");
        if let Err(error) = archive_working_log(source, archived) {
            return Err(GitAiError::EvidenceError(format!(
                "repair {} terminally preserved {} deferred job(s), but could not finish archiving the working log: {error}. The original working-log evidence remains at {}; rerun the same confirmed command",
                plan.repair_id,
                abandoned,
                source.display()
            )));
        }
    }

    write_repair_completion(&completion_file, &plan, &target, &evidence_file)?;
    eprintln!();
    eprintln!(
        "Repair complete: {} checkpoint job(s) are terminally marked manual_abandoned.",
        plan.affected_jobs.len()
    );
    eprintln!("Evidence backup: {}", target.backup_dir.display());
    match target.mode {
        CheckpointRepairMode::RepositoryWorkingLog => eprintln!(
            "The repaired base will be recreated as an empty attribution baseline if used again, and future checkpoints are no longer held behind the abandoned FIFO. No archived evidence was deleted."
        ),
        CheckpointRepairMode::FrozenEvidenceOnly => eprintln!(
            "The complete frozen SQLite evidence is preserved and the FIFO is released. The verified repository working log was intentionally not archived, modified, or reset because the blocking evidence failure was not its INITIAL baseline."
        ),
        CheckpointRepairMode::UnavailableRepository => eprintln!(
            "The frozen SQLite evidence is preserved and the FIFO is released. The unavailable repository's working log could not be archived or recovered by this repair. No path at the frozen repository location was modified."
        ),
    }
    Ok(())
}

fn resolve_checkpoint_repair_target(
    mut plan: ManualCheckpointRepairPlan,
    daemon_config: &DaemonConfig,
) -> Result<(ManualCheckpointRepairPlan, CheckpointRepairTarget), GitAiError> {
    let global_root = daemon_config
        .internal_dir
        .join("repair-backups")
        .join("checkpoint-baseline");

    if plan.already_terminal {
        let stored = plan.repair_backup_path.as_deref().ok_or_else(|| {
            GitAiError::EvidenceError(format!(
                "manually abandoned checkpoint {} is missing its immutable backup path",
                plan.target_job_key
            ))
        })?;
        let expected_global = global_root.join(&plan.repair_id);
        if Path::new(stored) == expected_global {
            let mode = if plan
                .repair_id
                .ends_with(UNAVAILABLE_REPOSITORY_REPAIR_SUFFIX)
            {
                CheckpointRepairMode::UnavailableRepository
            } else if plan.repair_id.ends_with(FROZEN_EVIDENCE_ONLY_REPAIR_SUFFIX) {
                CheckpointRepairMode::FrozenEvidenceOnly
            } else {
                return Err(GitAiError::EvidenceError(format!(
                    "repair {} records a global evidence backup without a recognized mode-bound repair id",
                    plan.repair_id
                )));
            };
            return Ok((
                plan,
                CheckpointRepairTarget {
                    mode,
                    backup_root: global_root,
                    backup_dir: expected_global,
                    repository_workdir: None,
                    source_working_log: None,
                    archived_working_log: None,
                    mode_reason: Some(match mode {
                        CheckpointRepairMode::FrozenEvidenceOnly => {
                            "the blocking failure was outside the INITIAL working-log baseline when the repair was confirmed"
                                .to_string()
                        }
                        CheckpointRepairMode::UnavailableRepository => {
                            "the original repository was unavailable or could not be verified when the repair was confirmed"
                                .to_string()
                        }
                        CheckpointRepairMode::RepositoryWorkingLog => unreachable!(),
                    }),
                },
            ));
        }

        let paths = exact_repository_repair_paths(&plan).map_err(|reason| {
            GitAiError::EvidenceError(format!(
                "repair {} records a repository-local working-log archive at {}, but the original repository can no longer be verified: {reason}; refusing to replace that completed repair with a different backup mode",
                plan.repair_id, stored
            ))
        })?;
        let expected = paths.repair_backups_root.join(&plan.repair_id);
        if Path::new(stored) != expected {
            return Err(GitAiError::EvidenceError(format!(
                "repair {} records backup {}, but the verified repository resolves it as {}; refusing to split evidence",
                plan.repair_id,
                stored,
                expected.display()
            )));
        }
        return Ok((
            plan,
            CheckpointRepairTarget {
                mode: CheckpointRepairMode::RepositoryWorkingLog,
                backup_root: paths.repair_backups_root,
                backup_dir: expected.clone(),
                repository_workdir: Some(paths.repository_workdir),
                source_working_log: Some(paths.source_working_log),
                archived_working_log: Some(expected.join("working-log")),
                mode_reason: None,
            },
        ));
    }

    match exact_repository_repair_paths(&plan) {
        Ok(paths) => {
            let backup_dir = paths.repair_backups_root.join(&plan.repair_id);
            let archived = backup_dir.join("working-log");
            if active_initial_failure_matches_block(
                &plan,
                &paths.source_working_log,
                &paths.repository_workdir,
            ) {
                Ok((
                    plan,
                    CheckpointRepairTarget {
                        mode: CheckpointRepairMode::RepositoryWorkingLog,
                        backup_root: paths.repair_backups_root,
                        backup_dir,
                        repository_workdir: Some(paths.repository_workdir),
                        source_working_log: Some(paths.source_working_log),
                        archived_working_log: Some(archived),
                        mode_reason: None,
                    },
                ))
            } else {
                // A prepared payload, request, blob, or other frozen SQLite
                // evidence failure does not authorize resetting a valid (or
                // unrelatedly damaged) working log. Bind confirmation to a
                // global evidence-only mode that never touches repository
                // storage.
                plan.repair_id.push_str(FROZEN_EVIDENCE_ONLY_REPAIR_SUFFIX);
                let backup_dir = global_root.join(&plan.repair_id);
                Ok((
                    plan,
                    CheckpointRepairTarget {
                        mode: CheckpointRepairMode::FrozenEvidenceOnly,
                        backup_root: global_root,
                        backup_dir,
                        repository_workdir: None,
                        source_working_log: None,
                        archived_working_log: None,
                        mode_reason: Some(
                            "the repository identity is verified, but the blocking evidence failure does not match the active working log's INITIAL failure"
                                .to_string(),
                        ),
                    },
                ))
            }
        }
        Err(reason) => {
            // Bind the two-step confirmation to this more destructive mode.
            // If the repository reappears (or disappears) between preview and
            // confirmation, the repair id changes and confirmation fails.
            plan.repair_id
                .push_str(UNAVAILABLE_REPOSITORY_REPAIR_SUFFIX);
            let backup_dir = global_root.join(&plan.repair_id);
            Ok((
                plan,
                CheckpointRepairTarget {
                    mode: CheckpointRepairMode::UnavailableRepository,
                    backup_root: global_root,
                    backup_dir,
                    repository_workdir: None,
                    source_working_log: None,
                    archived_working_log: None,
                    mode_reason: Some(reason),
                },
            ))
        }
    }
}

struct ExactRepositoryRepairPaths {
    repository_workdir: PathBuf,
    repair_backups_root: PathBuf,
    source_working_log: PathBuf,
}

fn exact_repository_repair_paths(
    plan: &ManualCheckpointRepairPlan,
) -> Result<ExactRepositoryRepairPaths, String> {
    let frozen = Path::new(&plan.repository_workdir);
    let frozen_canonical = frozen.canonicalize().map_err(|error| {
        format!(
            "frozen worktree {} is unavailable: {error}",
            frozen.display()
        )
    })?;
    let discovered = worktree_root_for_path(frozen).ok_or_else(|| {
        format!(
            "frozen worktree {} is not a resolvable Git worktree",
            frozen.display()
        )
    })?;
    let discovered_canonical = discovered.canonicalize().map_err(|error| {
        format!(
            "resolved worktree {} cannot be canonicalized: {error}",
            discovered.display()
        )
    })?;
    if discovered_canonical != frozen_canonical {
        return Err(format!(
            "frozen path {} now resolves inside a different worktree {}",
            frozen.display(),
            discovered.display()
        ));
    }
    let git_dir = git_dir_for_worktree(&discovered).ok_or_else(|| {
        format!(
            "Git metadata for frozen worktree {} cannot be resolved",
            discovered.display()
        )
    })?;
    let common_dir = common_dir_for_git_dir(&git_dir).ok_or_else(|| {
        format!(
            "Git common directory for frozen worktree {} cannot be resolved",
            discovered.display()
        )
    })?;
    let actual_identity = repository_identity(&common_dir);
    if actual_identity != plan.repo_identity {
        return Err(format!(
            "the existing directory at {} belongs to repository identity {}, not frozen identity {}; it will not be read, modified, or used as the original repository",
            frozen.display(),
            actual_identity,
            plan.repo_identity
        ));
    }

    // Derive storage paths without constructing RepoStorage: a preview and an
    // evidence-only repair must remain read-only with respect to the verified
    // repository.
    let worktree_ai_dir = worktree_storage_ai_dir(&git_dir, &common_dir);
    Ok(ExactRepositoryRepairPaths {
        repository_workdir: discovered,
        repair_backups_root: worktree_ai_dir.join("repair-backups"),
        source_working_log: worktree_ai_dir.join("working_logs").join(&plan.base_commit),
    })
}

fn active_initial_failure_matches_block(
    plan: &ManualCheckpointRepairPlan,
    source: &Path,
    workdir: &Path,
) -> bool {
    if reject_symlink_if_present(source, "active working log").is_err() || !source.is_dir() {
        return false;
    }
    let canonical_workdir = workdir
        .canonicalize()
        .unwrap_or_else(|_| workdir.to_path_buf());
    let working_log = PersistedWorkingLog::new(
        source.to_path_buf(),
        &plan.base_commit,
        workdir.to_path_buf(),
        canonical_workdir,
        None,
    );
    matches!(
        working_log.read_initial_attributions(),
        Err(GitAiError::EvidenceError(reason))
            if plan.original_block_reason.contains(&reason)
    )
}

fn ensure_private_backup_directory(target: &CheckpointRepairTarget) -> Result<(), GitAiError> {
    create_private_directory(&target.backup_root)?;
    create_private_directory(&target.backup_dir)
}

fn write_repair_completion(
    completion_file: &Path,
    plan: &ManualCheckpointRepairPlan,
    target: &CheckpointRepairTarget,
    evidence_file: &Path,
) -> Result<(), GitAiError> {
    let archived_working_log = target
        .archived_working_log
        .as_ref()
        .map(|path| path.to_string_lossy().to_string());
    let completion = RepairCompletion {
        version: 2,
        mode: target.mode.as_str(),
        repair_id: &plan.repair_id,
        target_job_key: &plan.target_job_key,
        repo_identity: &plan.repo_identity,
        repository_workdir: &plan.repository_workdir,
        base_commit: &plan.base_commit,
        affected_job_count: plan.affected_jobs.len(),
        evidence_file: evidence_file.to_string_lossy().to_string(),
        archived_working_log,
        unavailable_repository_reason: match target.mode {
            CheckpointRepairMode::RepositoryWorkingLog
            | CheckpointRepairMode::FrozenEvidenceOnly => None,
            CheckpointRepairMode::UnavailableRepository => Some(
                "the original repository was unavailable or its frozen identity could not be verified at confirmation",
            ),
        },
        frozen_evidence_only_reason: match target.mode {
            CheckpointRepairMode::RepositoryWorkingLog
            | CheckpointRepairMode::UnavailableRepository => None,
            CheckpointRepairMode::FrozenEvidenceOnly => Some(
                "the blocking evidence failure did not match a corrupt INITIAL working-log baseline at confirmation",
            ),
        },
        working_log_boundary: match target.mode {
            CheckpointRepairMode::RepositoryWorkingLog => "archived_complete_working_log",
            CheckpointRepairMode::FrozenEvidenceOnly => {
                "verified_repository_working_log_intentionally_not_archived_or_modified"
            }
            CheckpointRepairMode::UnavailableRepository => {
                "unavailable_not_archived_or_recoverable_by_this_repair"
            }
        },
    };
    write_json_once_or_verify(completion_file, &completion)?;
    Ok(())
}

fn validate_repairable_initial(
    plan: &ManualCheckpointRepairPlan,
    source: &Path,
    archived: &Path,
    workdir: &Path,
) -> Result<(), GitAiError> {
    reject_symlink_if_present(source, "active working log")?;
    reject_symlink_if_present(archived, "archived working log")?;
    match (source.exists(), archived.exists()) {
        (true, true) => Err(GitAiError::EvidenceError(format!(
            "both active and archived working logs exist for repair {}; refusing ambiguous overwrite",
            plan.repair_id
        ))),
        (false, true) if archived.is_dir() => {
            validate_archived_corrupt_initial(plan, archived, workdir)
        }
        (false, true) => Err(GitAiError::EvidenceError(format!(
            "repair archive {} exists but is not a directory",
            archived.display()
        ))),
        (false, false) => Err(GitAiError::EvidenceError(format!(
            "the frozen working log {} is missing and no repair archive exists",
            source.display()
        ))),
        (true, false) => {
            if !source.is_dir() {
                return Err(GitAiError::EvidenceError(format!(
                    "the frozen working log {} is not a directory",
                    source.display()
                )));
            }
            let canonical_workdir = workdir
                .canonicalize()
                .unwrap_or_else(|_| workdir.to_path_buf());
            let working_log = PersistedWorkingLog::new(
                source.to_path_buf(),
                &plan.base_commit,
                workdir.to_path_buf(),
                canonical_workdir,
                None,
            );
            match working_log.read_initial_attributions() {
                Err(GitAiError::EvidenceError(reason))
                    if plan.original_block_reason.contains(&reason) =>
                {
                    Ok(())
                }
                Err(error) => Err(GitAiError::EvidenceError(format!(
                    "checkpoint {} is blocked, but its active baseline is not an INITIAL evidence failure: {error}",
                    plan.target_job_key
                ))),
                Ok(_) => Err(GitAiError::Generic(format!(
                    "checkpoint {} no longer has a corrupt INITIAL baseline; refusing destructive reset",
                    plan.target_job_key
                ))),
            }
        }
    }
}

fn print_preview(plan: &ManualCheckpointRepairPlan, target: &CheckpointRepairTarget) {
    eprintln!("Checkpoint baseline repair preview");
    eprintln!("  repair id: {}", plan.repair_id);
    eprintln!("  repair mode: {}", target.mode.as_str());
    eprintln!("  blocked job: {}", plan.target_job_key);
    eprintln!("  repository identity: {}", plan.repo_identity);
    eprintln!("  frozen worktree: {}", plan.repository_workdir);
    eprintln!("  baseline: {}", plan.base_commit);
    eprintln!("  affected FIFO jobs: {}", plan.affected_jobs.len());
    match target.mode {
        CheckpointRepairMode::RepositoryWorkingLog => {
            eprintln!(
                "  active working log: {}",
                target
                    .source_working_log
                    .as_deref()
                    .expect("repository repair has a source path")
                    .display()
            );
            eprintln!(
                "  archived working log: {}",
                target
                    .archived_working_log
                    .as_deref()
                    .expect("repository repair has an archive path")
                    .display()
            );
        }
        CheckpointRepairMode::FrozenEvidenceOnly => {
            eprintln!(
                "  evidence-only reason: {}",
                target.mode_reason.as_deref().unwrap_or("unverified")
            );
            eprintln!(
                "  working-log boundary: the verified repository working log will not be archived, modified, or reset"
            );
            eprintln!(
                "  frozen path action: no file or directory at the frozen repository path will be modified"
            );
        }
        CheckpointRepairMode::UnavailableRepository => {
            eprintln!(
                "  unavailable repository: {}",
                target.mode_reason.as_deref().unwrap_or("unverified")
            );
            eprintln!(
                "  working-log boundary: the original working log is unavailable and cannot be archived or recovered by this repair"
            );
            eprintln!(
                "  frozen path action: no file or directory at the frozen repository path will be modified"
            );
        }
    }
    eprintln!("  evidence backup: {}", target.backup_dir.display());
    eprintln!("  original block: {}", plan.original_block_reason);
    eprintln!("  jobs to mark manual_abandoned:");
    for job in &plan.affected_jobs {
        eprintln!(
            "    - job_key={} phase={} worktree={} base={}",
            job.job_key,
            job.phase,
            job.repository_workdir,
            repair_row_base_for_display(job)
        );
    }
}

fn repair_row_base_for_display(
    row: &crate::metrics::deferred_checkpoint_jobs::ManualCheckpointRepairEvidenceRow,
) -> String {
    let Ok(request) = serde_json::from_str::<
        crate::commands::checkpoint_agent::orchestrator::CheckpointRequest,
    >(&row.request_json) else {
        return "<unreadable frozen request>".to_string();
    };
    let mut bases = request
        .files
        .iter()
        .map(|file| match &file.base_commit {
            crate::commands::checkpoint_agent::orchestrator::BaseCommit::Sha(value) => {
                value.clone()
            }
            crate::commands::checkpoint_agent::orchestrator::BaseCommit::Initial => {
                "initial".to_string()
            }
        })
        .collect::<std::collections::BTreeSet<_>>();
    match bases.len() {
        0 => "<no frozen file evidence>".to_string(),
        1 => bases.pop_first().expect("one base"),
        _ => bases.into_iter().collect::<Vec<_>>().join(","),
    }
}

fn validate_archived_corrupt_initial(
    plan: &ManualCheckpointRepairPlan,
    archived: &Path,
    workdir: &Path,
) -> Result<(), GitAiError> {
    reject_symlink_if_present(archived, "archived working log")?;
    if !archived.is_dir() {
        return Err(GitAiError::EvidenceError(format!(
            "repair {} records a terminal archive, but {} is not a directory",
            plan.repair_id,
            archived.display()
        )));
    }
    let canonical_workdir = workdir
        .canonicalize()
        .unwrap_or_else(|_| workdir.to_path_buf());
    let archived_log = PersistedWorkingLog::new(
        archived.to_path_buf(),
        &plan.base_commit,
        workdir.to_path_buf(),
        canonical_workdir,
        None,
    );
    match archived_log.read_initial_attributions() {
        Err(GitAiError::EvidenceError(reason)) if reason.contains("INITIAL") => Ok(()),
        Err(error) => Err(GitAiError::EvidenceError(format!(
            "cannot verify the archived INITIAL evidence for repair {}: {error}",
            plan.repair_id
        ))),
        Ok(_) => Err(GitAiError::EvidenceError(format!(
            "repair {} archive no longer contains the corrupt INITIAL evidence that was approved",
            plan.repair_id
        ))),
    }
}

fn validate_recreated_active_baseline(
    plan: &ManualCheckpointRepairPlan,
    source: &Path,
    workdir: &Path,
) -> Result<(), GitAiError> {
    reject_symlink_if_present(source, "recreated active working log")?;
    if !source.exists() {
        return Ok(());
    }
    if !source.is_dir() {
        return Err(GitAiError::EvidenceError(format!(
            "recreated active working log {} is not a directory",
            source.display()
        )));
    }
    let canonical_workdir = workdir
        .canonicalize()
        .unwrap_or_else(|_| workdir.to_path_buf());
    let active = PersistedWorkingLog::new(
        source.to_path_buf(),
        &plan.base_commit,
        workdir.to_path_buf(),
        canonical_workdir,
        None,
    );
    match active.read_initial_attributions() {
        Err(GitAiError::EvidenceError(reason)) if reason.contains("INITIAL") => {
            Err(GitAiError::EvidenceError(format!(
                "repair {} has an archive but the active baseline is also corrupt ({reason}); refusing to guess whether a Windows crash duplicated the old directory",
                plan.repair_id
            )))
        }
        Err(error) => Err(GitAiError::EvidenceError(format!(
            "cannot verify the active baseline after repair {}: {error}",
            plan.repair_id
        ))),
        Ok(_) => Ok(()),
    }
}

fn preserve_deferred_evidence(
    path: &Path,
    plan: &ManualCheckpointRepairPlan,
) -> Result<(), GitAiError> {
    if path.exists() {
        let bytes = fs::read(path)?;
        let preserved: ManualCheckpointRepairPlan =
            serde_json::from_slice(&bytes).map_err(|e| {
                GitAiError::EvidenceError(format!(
                    "existing deferred evidence backup {} is unreadable: {e}",
                    path.display()
                ))
            })?;
        let preserved_jobs = preserved
            .affected_jobs
            .iter()
            .map(|row| (&row.job_key, &row.request_evidence_sha256))
            .collect::<Vec<_>>();
        let current_jobs = plan
            .affected_jobs
            .iter()
            .map(|row| (&row.job_key, &row.request_evidence_sha256))
            .collect::<Vec<_>>();
        if preserved.repair_id != plan.repair_id
            || preserved.target_job_key != plan.target_job_key
            || preserved.repo_identity != plan.repo_identity
            || preserved.base_commit != plan.base_commit
            || preserved_jobs != current_jobs
        {
            return Err(GitAiError::EvidenceError(format!(
                "existing deferred evidence backup {} does not match repair {}",
                path.display(),
                plan.repair_id
            )));
        }
        return Ok(());
    }
    write_json_atomic(path, plan)
}

fn write_json_once_or_verify<T: Serialize>(path: &Path, value: &T) -> Result<(), GitAiError> {
    let expected = serde_json::to_vec_pretty(value)?;
    if path.exists() {
        let current = fs::read(path)?;
        if current == expected {
            return Ok(());
        }
        return Err(GitAiError::EvidenceError(format!(
            "existing repair marker {} has unexpected content",
            path.display()
        )));
    }
    write_bytes_atomic(path, &expected)
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), GitAiError> {
    let bytes = serde_json::to_vec_pretty(value)?;
    write_bytes_atomic(path, &bytes)
}

fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> Result<(), GitAiError> {
    let parent = path.parent().ok_or_else(|| {
        GitAiError::Generic(format!(
            "repair backup path has no parent: {}",
            path.display()
        ))
    })?;
    create_private_directory(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    {
        let mut writer = BufWriter::new(temp.as_file_mut());
        writer.write_all(bytes)?;
        writer.flush()?;
    }
    temp.as_file().sync_all()?;
    temp.persist_noclobber(path).map_err(|error| {
        GitAiError::IoError(std::io::Error::new(
            error.error.kind(),
            format!(
                "failed to publish repair backup {}: {}",
                path.display(),
                error.error
            ),
        ))
    })?;
    sync_directory(parent)?;
    Ok(())
}

fn create_private_directory(path: &Path) -> Result<(), GitAiError> {
    reject_symlink_if_present(path, "repair backup directory")?;
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn archive_working_log(source: &Path, destination: &Path) -> Result<(), GitAiError> {
    reject_symlink_if_present(source, "working-log source")?;
    reject_symlink_if_present(destination, "working-log destination")?;
    match (source.exists(), destination.exists()) {
        (false, true) if destination.is_dir() => return Ok(()),
        (false, true) => {
            return Err(GitAiError::EvidenceError(format!(
                "working-log archive {} is not a directory",
                destination.display()
            )));
        }
        (false, false) => {
            return Err(GitAiError::EvidenceError(format!(
                "working-log evidence {} disappeared before it could be archived",
                source.display()
            )));
        }
        (true, true) => {
            return Err(GitAiError::EvidenceError(format!(
                "working-log source {} and destination {} both exist",
                source.display(),
                destination.display()
            )));
        }
        (true, false) => {}
    }
    let destination_parent = destination.parent().ok_or_else(|| {
        GitAiError::Generic(format!(
            "working-log archive path has no parent: {}",
            destination.display()
        ))
    })?;
    create_private_directory(destination_parent)?;
    fs::rename(source, destination)?;
    if let Some(source_parent) = source.parent() {
        sync_directory(source_parent)?;
    }
    sync_directory(destination_parent)?;
    Ok(())
}

fn reject_symlink_if_present(path: &Path, label: &str) -> Result<(), GitAiError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(GitAiError::EvidenceError(format!(
                "{label} {} is a symbolic link; refusing repair",
                path.display()
            )))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), GitAiError> {
    fs::File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), GitAiError> {
    // Rust exposes no portable Windows directory fsync. The repair protocol
    // therefore relies on ordered, independently recoverable artifacts: the
    // SQLite manual_abandoned tombstone retains un-compacted frozen rows, and
    // every retry revalidates source/archive/evidence before reporting success.
    Ok(())
}

fn print_help() {
    eprintln!("git-ai repair - Explicit recovery for preserved local evidence");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  git-ai repair checkpoint-baseline --job-key <key> [{CONFIRM_FLAG} <repair-id>]");
    eprintln!();
    eprintln!("Run without {CONFIRM_FLAG} first to inspect the immutable impact preview.");
}

fn print_checkpoint_baseline_help() {
    eprintln!("git-ai repair checkpoint-baseline");
    eprintln!();
    eprintln!(
        "Preserve frozen evidence and terminally abandon every unverifiable deferred checkpoint in that repository FIFO; only a matching corrupt INITIAL baseline is archived."
    );
    eprintln!("The background service must already be stopped.");
    eprintln!(
        "Other evidence failures use a global evidence-only backup; an unavailable or mismatched repository is never modified."
    );
    eprintln!();
    eprintln!("Preview:");
    eprintln!("  git-ai repair checkpoint-baseline --job-key <64-hex-key>");
    eprintln!("Confirm using the exact repair id printed by the preview:");
    eprintln!(
        "  git-ai repair checkpoint-baseline --job-key <64-hex-key> {CONFIRM_FLAG} <repair-id>"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorship::working_log::CheckpointKind;
    use crate::commands::checkpoint_agent::orchestrator::{
        BaseCommit, CheckpointFile, CheckpointRequest,
    };
    use crate::daemon::checkpoint::PreparedPathRole;
    use crate::git::repo_storage::RepoStorage;
    use crate::metrics::deferred_checkpoint_jobs::{
        DEFERRED_CHECKPOINT_JOBS_SCHEMA_SQL, DeferredCheckpointJobSpec,
        DeferredCheckpointJobStatus, claim_specific_on_connection,
        compact_done_payloads_on_connection, count_outstanding_on_connection,
        enqueue_on_connection, manual_repair_plan_on_connection,
        manually_abandon_repo_fifo_on_connection, mark_blocked_on_connection, status_on_connection,
    };
    use rusqlite::Connection;
    use std::collections::HashMap;

    #[test]
    fn checkpoint_baseline_parser_requires_exact_job_key_and_confirmation_value() {
        let key = "a".repeat(64);
        assert_eq!(
            parse_checkpoint_baseline_options(&[
                "--job-key".to_string(),
                key.clone(),
                "--confirm".to_string(),
                "checkpoint-baseline-deadbeef".to_string(),
            ])
            .unwrap(),
            RepairOptions {
                job_key: key,
                confirmation: Some("checkpoint-baseline-deadbeef".to_string()),
            }
        );
        assert!(parse_checkpoint_baseline_options(&[]).is_err());
        assert!(
            parse_checkpoint_baseline_options(&["--job-key".to_string(), "not-a-key".to_string(),])
                .is_err()
        );
    }

    fn repair_spec(
        job_key: char,
        repo_identity: &str,
        workdir: &Path,
        base_commit: &str,
        tool_use_id: &str,
    ) -> DeferredCheckpointJobSpec {
        let request = CheckpointRequest {
            trace_id: format!("checkpoint-job:{}", job_key.to_string().repeat(64)),
            checkpoint_kind: CheckpointKind::Human,
            agent_id: Some(crate::authorship::working_log::AgentId {
                tool: "kilo".to_string(),
                model: "test".to_string(),
                id: "session".to_string(),
            }),
            files: vec![CheckpointFile {
                path: workdir.join("tracked.txt"),
                content: Some("new\n".to_string()),
                repo_work_dir: workdir.to_path_buf(),
                base_commit: BaseCommit::Sha(base_commit.to_string()),
            }],
            path_role: PreparedPathRole::WillEdit,
            stream_source: None,
            metadata: HashMap::from([
                ("integration".to_string(), "kilo-v7".to_string()),
                ("tool_use_id".to_string(), tool_use_id.to_string()),
            ]),
        };
        DeferredCheckpointJobSpec {
            job_key: job_key.to_string().repeat(64),
            repo_identity: repo_identity.to_string(),
            repository_workdir: workdir.to_string_lossy().to_string(),
            integration: "kilo-v7".to_string(),
            external_session_id: "session".to_string(),
            external_tool_use_id: tool_use_id.to_string(),
            phase: "pre".to_string(),
            request_shape_sha256: format!("shape-{tool_use_id}"),
            request_evidence_sha256: format!("evidence-{tool_use_id}"),
            request_json: serde_json::to_string(&request).unwrap(),
            metrics_context_json: "{}".to_string(),
            path_scope_json: r#"{"kind":"files","paths":["tracked.txt"]}"#.to_string(),
            admission_owner: None,
            observed_at_ms: 1,
        }
    }

    #[test]
    fn corrupt_initial_repair_preserves_evidence_releases_fifo_and_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let workdir = temp.path().join("worktree");
        fs::create_dir_all(&workdir).unwrap();
        let ai_dir = temp.path().join("git-ai-state");
        let storage = RepoStorage::for_isolated_worktree_storage(&ai_dir, &workdir).unwrap();
        let base_commit = "1".repeat(40);
        let working_log = storage.working_log_for_base_commit(&base_commit).unwrap();
        fs::write(
            &working_log.initial_file,
            r#"{"files":{"tracked.txt":[]},"prompts":{},"file_blobs":{}}"#,
        )
        .unwrap();
        let initial_bytes = fs::read(&working_log.initial_file).unwrap();
        assert!(working_log.read_initial_attributions().is_err());

        let mut conn = crate::sqlite::open_in_memory_with_memory_limits().unwrap();
        conn.execute_batch(DEFERRED_CHECKPOINT_JOBS_SCHEMA_SQL)
            .unwrap();
        let repo_identity = "repo-identity";
        let blocked_spec = repair_spec('a', repo_identity, &workdir, &base_commit, "call-1");
        let later_spec = repair_spec('b', repo_identity, &workdir, &base_commit, "call-2");
        enqueue_on_connection(&mut conn, &blocked_spec, 1).unwrap();
        let claimed = claim_specific_on_connection(&mut conn, &blocked_spec.job_key, 2, 600)
            .unwrap()
            .unwrap();
        mark_blocked_on_connection(
            &mut conn,
            &claimed,
            "Evidence error: INITIAL missing persisted file snapshot for tracked.txt",
            3,
        )
        .unwrap();
        enqueue_on_connection(&mut conn, &later_spec, 4).unwrap();
        assert_eq!(count_outstanding_on_connection(&mut conn).unwrap(), 2);

        let plan = manual_repair_plan_on_connection(&mut conn, &blocked_spec.job_key).unwrap();
        assert!(!plan.already_terminal);
        assert_eq!(plan.affected_jobs.len(), 2);
        let backup_dir = ai_dir.join("repair-backups").join(&plan.repair_id);
        let evidence_file = backup_dir.join("deferred-checkpoint-evidence.json");
        let archived = backup_dir.join("working-log");
        validate_repairable_initial(&plan, &working_log.dir, &archived, &workdir).unwrap();
        let ambiguous_archive = temp.path().join("ambiguous-archive");
        fs::create_dir(&ambiguous_archive).unwrap();
        assert!(
            validate_repairable_initial(&plan, &working_log.dir, &ambiguous_archive, &workdir,)
                .is_err(),
            "before the DB tombstone, source+archive must fail closed"
        );
        fs::remove_dir(&ambiguous_archive).unwrap();
        create_private_directory(&backup_dir).unwrap();
        archive_working_log(&working_log.dir, &archived).unwrap();
        validate_repairable_initial(&plan, &working_log.dir, &archived, &workdir).unwrap();
        fs::rename(&archived, &working_log.dir).unwrap();
        preserve_deferred_evidence(&evidence_file, &plan).unwrap();
        let original_backup: ManualCheckpointRepairPlan =
            serde_json::from_slice(&fs::read(&evidence_file).unwrap()).unwrap();
        assert_eq!(original_backup.affected_jobs[0].state, "pending");
        let abandoned = manually_abandon_repo_fifo_on_connection(
            &mut conn,
            &plan,
            &backup_dir.to_string_lossy(),
            5,
        )
        .unwrap();
        assert_eq!(abandoned, 2);

        let resumed_before_archive =
            manual_repair_plan_on_connection(&mut conn, &blocked_spec.job_key).unwrap();
        assert!(resumed_before_archive.already_terminal);
        validate_repairable_initial(
            &resumed_before_archive,
            &working_log.dir,
            &archived,
            &workdir,
        )
        .unwrap();
        assert_eq!(
            manually_abandon_repo_fifo_on_connection(
                &mut conn,
                &resumed_before_archive,
                &backup_dir.to_string_lossy(),
                5,
            )
            .unwrap(),
            0,
            "a crash after the DB tombstone but before archive must be retryable"
        );
        let temporarily_missing = temp.path().join("temporarily-missing-working-log");
        fs::rename(&working_log.dir, &temporarily_missing).unwrap();
        assert!(
            validate_repairable_initial(
                &resumed_before_archive,
                &working_log.dir,
                &archived,
                &workdir,
            )
            .is_err(),
            "source+archive both absent must never be reported as repaired"
        );
        fs::rename(&temporarily_missing, &working_log.dir).unwrap();
        archive_working_log(&working_log.dir, &archived).unwrap();
        validate_archived_corrupt_initial(&resumed_before_archive, &archived, &workdir).unwrap();

        fs::remove_file(&evidence_file).unwrap();
        preserve_deferred_evidence(&evidence_file, &resumed_before_archive).unwrap();
        let reconstructed: ManualCheckpointRepairPlan =
            serde_json::from_slice(&fs::read(&evidence_file).unwrap()).unwrap();
        assert_eq!(
            reconstructed.affected_jobs[0].request_json, blocked_spec.request_json,
            "a lost Windows directory entry must be recoverable from un-compacted SQLite evidence"
        );
        let completion_file = backup_dir.join("REPAIR-COMPLETE.json");
        assert!(!completion_file.exists());
        let target = CheckpointRepairTarget {
            mode: CheckpointRepairMode::RepositoryWorkingLog,
            backup_root: ai_dir.join("repair-backups"),
            backup_dir: backup_dir.clone(),
            repository_workdir: Some(workdir.clone()),
            source_working_log: Some(working_log.dir.clone()),
            archived_working_log: Some(archived.clone()),
            mode_reason: None,
        };
        write_repair_completion(
            &completion_file,
            &resumed_before_archive,
            &target,
            &evidence_file,
        )
        .unwrap();
        write_repair_completion(
            &completion_file,
            &resumed_before_archive,
            &target,
            &evidence_file,
        )
        .unwrap();
        assert!(completion_file.is_file());

        assert_eq!(count_outstanding_on_connection(&mut conn).unwrap(), 0);
        for job_key in [&blocked_spec.job_key, &later_spec.job_key] {
            assert!(matches!(
                status_on_connection(&mut conn, job_key).unwrap(),
                Some(DeferredCheckpointJobStatus::ManuallyAbandoned(_))
            ));
        }
        assert_eq!(
            compact_done_payloads_on_connection(&mut conn, 100).unwrap(),
            0
        );
        let retained_request: String = conn
            .query_row(
                "SELECT request_json FROM deferred_checkpoint_jobs WHERE job_key = ?1",
                rusqlite::params![blocked_spec.job_key],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained_request, blocked_spec.request_json);
        let preserved: ManualCheckpointRepairPlan =
            serde_json::from_slice(&fs::read(&evidence_file).unwrap()).unwrap();
        assert_eq!(
            preserved.affected_jobs[0].terminal_resolution,
            "manual_abandoned"
        );
        assert_eq!(fs::read(archived.join("INITIAL")).unwrap(), initial_bytes);
        assert!(!working_log.dir.exists());

        let fresh = storage.working_log_for_base_commit(&base_commit).unwrap();
        assert!(fresh.read_initial_attributions().unwrap().files.is_empty());
        validate_recreated_active_baseline(&resumed_before_archive, &fresh.dir, &workdir).unwrap();
        let future_spec = repair_spec('c', repo_identity, &workdir, &base_commit, "call-3");
        enqueue_on_connection(&mut conn, &future_spec, 6).unwrap();
        assert!(
            claim_specific_on_connection(&mut conn, &future_spec.job_key, 7, 600)
                .unwrap()
                .is_some(),
            "manual abandonment must release the repository FIFO for future checkpoints"
        );

        let resumed = manual_repair_plan_on_connection(&mut conn, &blocked_spec.job_key).unwrap();
        assert!(resumed.already_terminal);
        preserve_deferred_evidence(&evidence_file, &resumed).unwrap();
        assert_eq!(
            manually_abandon_repo_fifo_on_connection(
                &mut conn,
                &resumed,
                &backup_dir.to_string_lossy(),
                8,
            )
            .unwrap(),
            0
        );
        assert!(archived.is_dir());
        assert!(fresh.dir.is_dir());

        fs::write(
            &fresh.initial_file,
            r#"{"files":{"tracked.txt":[]},"prompts":{},"file_blobs":{}}"#,
        )
        .unwrap();
        assert!(
            validate_recreated_active_baseline(&resumed, &fresh.dir, &workdir).is_err(),
            "archive+corrupt active source must fail closed instead of guessing that source is new"
        );
    }

    fn blocked_repair_plan(
        workdir: &Path,
        repo_identity: &str,
    ) -> (
        Connection,
        DeferredCheckpointJobSpec,
        ManualCheckpointRepairPlan,
    ) {
        blocked_repair_plan_with_reason(
            workdir,
            repo_identity,
            "Evidence error: frozen repository path cannot be resolved",
        )
    }

    fn blocked_repair_plan_with_reason(
        workdir: &Path,
        repo_identity: &str,
        reason: &str,
    ) -> (
        Connection,
        DeferredCheckpointJobSpec,
        ManualCheckpointRepairPlan,
    ) {
        let mut conn = crate::sqlite::open_in_memory_with_memory_limits().unwrap();
        conn.execute_batch(DEFERRED_CHECKPOINT_JOBS_SCHEMA_SQL)
            .unwrap();
        let blocked = repair_spec('d', repo_identity, workdir, &"2".repeat(40), "call-blocked");
        enqueue_on_connection(&mut conn, &blocked, 1).unwrap();
        let claimed = claim_specific_on_connection(&mut conn, &blocked.job_key, 2, 600)
            .unwrap()
            .unwrap();
        mark_blocked_on_connection(&mut conn, &claimed, reason, 3).unwrap();
        let plan = manual_repair_plan_on_connection(&mut conn, &blocked.job_key).unwrap();
        (conn, blocked, plan)
    }

    #[test]
    fn valid_initial_with_prepared_evidence_failure_uses_global_evidence_only_mode() {
        let temp = tempfile::tempdir().unwrap();
        let workdir = temp.path().join("verified-worktree");
        let git_dir = workdir.join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let repo_identity = repository_identity(&git_dir);
        let prepared_reason =
            "Evidence error: durable checkpoint has corrupt prepared checkpoint evidence";
        let (mut conn, blocked, plan) =
            blocked_repair_plan_with_reason(&workdir, &repo_identity, prepared_reason);
        let storage = RepoStorage::for_repo_path(&git_dir, &workdir).unwrap();
        let working_log = storage
            .working_log_for_base_commit(&plan.base_commit)
            .unwrap();
        let valid_initial = br#"{"files":{},"prompts":{},"file_blobs":{}}"#;
        fs::write(&working_log.initial_file, valid_initial).unwrap();
        assert!(working_log.read_initial_attributions().is_ok());
        let daemon_config = DaemonConfig::from_home(&temp.path().join("daemon-home"));

        let original_repair_id = plan.repair_id.clone();
        let (plan, target) = resolve_checkpoint_repair_target(plan, &daemon_config).unwrap();
        assert_eq!(target.mode, CheckpointRepairMode::FrozenEvidenceOnly);
        assert_eq!(
            plan.repair_id,
            format!("{original_repair_id}{FROZEN_EVIDENCE_ONLY_REPAIR_SUFFIX}")
        );
        assert!(target.source_working_log.is_none());
        assert!(target.archived_working_log.is_none());
        assert!(
            target
                .backup_dir
                .starts_with(daemon_config.internal_dir.join("repair-backups"))
        );
        assert!(
            !storage.ai_dir.join("repair-backups").exists(),
            "the evidence-only preview must not create a repository-local repair directory"
        );

        ensure_private_backup_directory(&target).unwrap();
        let evidence_file = target.backup_dir.join("deferred-checkpoint-evidence.json");
        preserve_deferred_evidence(&evidence_file, &plan).unwrap();
        assert_eq!(
            manually_abandon_repo_fifo_on_connection(
                &mut conn,
                &plan,
                &target.backup_dir.to_string_lossy(),
                4,
            )
            .unwrap(),
            1
        );
        assert_eq!(fs::read(&working_log.initial_file).unwrap(), valid_initial);
        assert!(working_log.dir.is_dir());
        assert!(matches!(
            status_on_connection(&mut conn, &blocked.job_key).unwrap(),
            Some(DeferredCheckpointJobStatus::ManuallyAbandoned(_))
        ));
    }

    #[test]
    fn missing_repository_uses_global_evidence_only_repair_mode() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("permanently-missing-worktree");
        let (_conn, _blocked, plan) = blocked_repair_plan(&missing, "missing-repo-identity");
        let daemon_config = DaemonConfig::from_home(&temp.path().join("daemon-home"));

        let original_repair_id = plan.repair_id.clone();
        let (plan, target) = resolve_checkpoint_repair_target(plan, &daemon_config).unwrap();
        assert_eq!(target.mode, CheckpointRepairMode::UnavailableRepository);
        assert_eq!(
            plan.repair_id,
            format!("{original_repair_id}{UNAVAILABLE_REPOSITORY_REPAIR_SUFFIX}")
        );
        assert!(target.source_working_log.is_none());
        assert!(target.archived_working_log.is_none());
        assert!(
            target
                .mode_reason
                .as_deref()
                .unwrap()
                .contains("unavailable")
        );
        assert!(
            target
                .backup_dir
                .starts_with(daemon_config.internal_dir.join("repair-backups"))
        );
        assert!(
            !target.backup_dir.exists(),
            "a preview must not publish the global evidence directory"
        );
        assert!(!missing.exists());
    }

    #[test]
    fn identity_mismatch_uses_global_mode_without_touching_existing_repository() {
        let temp = tempfile::tempdir().unwrap();
        let occupied = temp.path().join("occupied-worktree");
        fs::create_dir_all(occupied.join(".git")).unwrap();
        fs::write(occupied.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(occupied.join("keep.txt"), "do not modify\n").unwrap();
        let (_conn, _blocked, plan) =
            blocked_repair_plan(&occupied, "frozen-different-repository-identity");
        let daemon_config = DaemonConfig::from_home(&temp.path().join("daemon-home"));

        let (_plan, target) = resolve_checkpoint_repair_target(plan, &daemon_config).unwrap();
        assert_eq!(target.mode, CheckpointRepairMode::UnavailableRepository);
        assert!(
            target
                .mode_reason
                .as_deref()
                .unwrap()
                .contains("not frozen identity")
        );
        assert_eq!(
            fs::read_to_string(occupied.join("keep.txt")).unwrap(),
            "do not modify\n"
        );
        assert!(
            !occupied.join(".git/ai").exists(),
            "identity probing must not create Git AI storage in the unrelated repository"
        );
    }

    #[test]
    fn unavailable_repository_repair_is_atomic_idempotent_and_keeps_full_rows() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing-worktree");
        let (mut conn, blocked, _plan) = blocked_repair_plan(&missing, "missing-repo-identity");
        let later = repair_spec(
            'e',
            "missing-repo-identity",
            &missing,
            &"2".repeat(40),
            "call-later",
        );
        enqueue_on_connection(&mut conn, &later, 4).unwrap();
        let plan = manual_repair_plan_on_connection(&mut conn, &blocked.job_key).unwrap();
        let daemon_config = DaemonConfig::from_home(&temp.path().join("daemon-home"));
        let (plan, target) = resolve_checkpoint_repair_target(plan, &daemon_config).unwrap();
        ensure_private_backup_directory(&target).unwrap();
        let evidence_file = target.backup_dir.join("deferred-checkpoint-evidence.json");
        let completion_file = target.backup_dir.join("REPAIR-COMPLETE.json");
        preserve_deferred_evidence(&evidence_file, &plan).unwrap();
        let evidence_before = fs::read(&evidence_file).unwrap();
        assert_eq!(
            manually_abandon_repo_fifo_on_connection(
                &mut conn,
                &plan,
                &target.backup_dir.to_string_lossy(),
                5,
            )
            .unwrap(),
            2
        );
        write_repair_completion(&completion_file, &plan, &target, &evidence_file).unwrap();
        let completion_before = fs::read(&completion_file).unwrap();

        let resumed = manual_repair_plan_on_connection(&mut conn, &blocked.job_key).unwrap();
        assert!(resumed.already_terminal);
        let (resumed, resumed_target) =
            resolve_checkpoint_repair_target(resumed, &daemon_config).unwrap();
        assert_eq!(
            resumed_target.mode,
            CheckpointRepairMode::UnavailableRepository
        );
        ensure_private_backup_directory(&resumed_target).unwrap();
        preserve_deferred_evidence(&evidence_file, &resumed).unwrap();
        write_repair_completion(&completion_file, &resumed, &resumed_target, &evidence_file)
            .unwrap();
        assert_eq!(fs::read(&evidence_file).unwrap(), evidence_before);
        assert_eq!(fs::read(&completion_file).unwrap(), completion_before);
        assert_eq!(
            manually_abandon_repo_fifo_on_connection(
                &mut conn,
                &resumed,
                &resumed_target.backup_dir.to_string_lossy(),
                6,
            )
            .unwrap(),
            0
        );
        assert_eq!(count_outstanding_on_connection(&mut conn).unwrap(), 0);
        assert!(!missing.exists());

        let preserved: ManualCheckpointRepairPlan =
            serde_json::from_slice(&evidence_before).unwrap();
        assert_eq!(preserved.affected_jobs.len(), 2);
        assert_eq!(
            preserved.affected_jobs[0].request_json,
            blocked.request_json
        );
        assert_eq!(preserved.affected_jobs[1].request_json, later.request_json);
        let receipt: serde_json::Value = serde_json::from_slice(&completion_before).unwrap();
        assert_eq!(receipt["mode"], "unavailable_repository");
        assert!(receipt["archived_working_log"].is_null());
        assert_eq!(
            receipt["working_log_boundary"],
            "unavailable_not_archived_or_recoverable_by_this_repair"
        );
    }

    #[test]
    fn repair_confirmation_rejects_fifo_impact_that_changed_after_preview() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing-worktree");
        let (mut conn, blocked, preview) = blocked_repair_plan(&missing, "missing-repo-identity");
        let later = repair_spec(
            'f',
            "missing-repo-identity",
            &missing,
            &"2".repeat(40),
            "call-arrived-after-preview",
        );
        enqueue_on_connection(&mut conn, &later, 4).unwrap();

        let error = manually_abandon_repo_fifo_on_connection(
            &mut conn,
            &preview,
            &temp.path().join("backup").to_string_lossy(),
            5,
        )
        .unwrap_err();
        assert!(error.to_string().contains("impact changed after preview"));
        assert_eq!(count_outstanding_on_connection(&mut conn).unwrap(), 2);
        assert!(matches!(
            status_on_connection(&mut conn, &blocked.job_key).unwrap(),
            Some(DeferredCheckpointJobStatus::Blocked(_))
        ));
    }
}
