use crate::authorship::attribution_tracker::LineAttribution;
use crate::authorship::authorship_log::{HumanRecord, PromptRecord, SessionRecord};
use crate::authorship::authorship_log_serialization::generate_short_hash;
use crate::authorship::working_log::{CHECKPOINT_API_VERSION, Checkpoint, CheckpointKind};
use crate::error::GitAiError;
use crate::utils::normalize_to_posix;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

pub const MAX_CHECKPOINTS_JSONL_BYTES: u64 = 1024 * 1024 * 1024;

#[cfg(feature = "test-support")]
const TEST_CHECKPOINTS_JSONL_MAX_BYTES_ENV: &str = "GIT_AI_TEST_CHECKPOINTS_JSONL_MAX_BYTES";

/// Initial attributions data structure stored in the INITIAL file
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct InitialAttributions {
    /// Map of file path to line attributions
    pub files: HashMap<String, Vec<LineAttribution>>,
    /// Map of author_id (hash) to PromptRecord for prompt tracking
    pub prompts: HashMap<String, PromptRecord>,
    /// Blob snapshot of the file content represented by each entry in `files`.
    ///
    /// The serde default keeps an empty/metadata-only INITIAL readable, but an
    /// INITIAL that contains file attributions must have one durable blob per
    /// file and is rejected otherwise.
    #[serde(default)]
    pub file_blobs: HashMap<String, String>,
    /// Known human records: `h_<hash>` -> HumanRecord
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub humans: std::collections::BTreeMap<String, HumanRecord>,
    /// Session records: `s_<session_id>` -> SessionRecord
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub sessions: std::collections::BTreeMap<String, SessionRecord>,
}

#[derive(Debug, Clone)]
pub struct RepoStorage {
    pub ai_dir: PathBuf,
    pub repo_workdir: PathBuf,
    pub working_logs: PathBuf,
    pub logs: PathBuf,
}

impl RepoStorage {
    pub fn for_repo_path(repo_path: &Path, repo_workdir: &Path) -> Result<RepoStorage, GitAiError> {
        Self::for_ai_dir(&repo_path.join("ai"), repo_workdir)
    }

    pub fn for_isolated_worktree_storage(
        ai_dir: &Path,
        repo_workdir: &Path,
    ) -> Result<RepoStorage, GitAiError> {
        Self::for_ai_dir(ai_dir, repo_workdir)
    }

    fn for_ai_dir(ai_dir: &Path, repo_workdir: &Path) -> Result<RepoStorage, GitAiError> {
        let working_logs_dir = ai_dir.join("working_logs");
        let logs_dir = ai_dir.join("logs");

        let config = RepoStorage {
            ai_dir: ai_dir.to_path_buf(),
            repo_workdir: repo_workdir.to_path_buf(),
            working_logs: working_logs_dir,
            logs: logs_dir,
        };

        config.ensure_config_directory()?;
        Ok(config)
    }

    #[doc(hidden)]
    pub fn ensure_config_directory(&self) -> Result<(), GitAiError> {
        fs::create_dir_all(&self.ai_dir)?;

        // Create working_logs directory
        fs::create_dir_all(&self.working_logs)?;

        // Create logs directory for Sentry events
        fs::create_dir_all(&self.logs)?;

        Ok(())
    }

    /* Working Log Persistance */

    pub fn has_working_log(&self, sha: &str) -> bool {
        self.working_logs.join(sha).exists()
    }

    pub fn working_log_for_base_commit(
        &self,
        sha: &str,
    ) -> Result<PersistedWorkingLog, GitAiError> {
        let working_log_dir = self.working_logs.join(sha);
        fs::create_dir_all(&working_log_dir)?;
        // Always repeat the parent sync. If a prior call created the directory
        // but its sync failed, retry must not mistake `exists()` for proof that
        // the directory entry is durable.
        sync_working_log_parent_directory(&self.working_logs)?;
        let canonical_workdir = self
            .repo_workdir
            .canonicalize()
            .unwrap_or_else(|_| self.repo_workdir.clone());
        Ok(PersistedWorkingLog::new(
            working_log_dir,
            sha,
            self.repo_workdir.clone(),
            canonical_workdir,
            None,
        ))
    }

    pub fn delete_working_log_for_base_commit(&self, sha: &str) -> Result<(), GitAiError> {
        let working_log_dir = self.working_logs.join(sha);
        crate::wltrace::wltrace("working_log.delete", &working_log_dir, String::new);
        if working_log_dir.exists() {
            // Both debug and release: move to old-{sha} for retention
            let old_dir = self.working_logs.join(format!("old-{}", sha));
            // If old-{sha} already exists, remove it first
            if old_dir.exists() {
                fs::remove_dir_all(&old_dir)?;
            }
            fs::rename(&working_log_dir, &old_dir)?;

            // Write a timestamp marker so we know when it was archived
            let marker = old_dir.join(".archived_at");
            let now = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_secs();
            // Best-effort; don't fail the commit if we can't write the marker
            let _ = fs::write(&marker, now.to_string());

            tracing::debug!("Moved checkpoint directory from {} to old-{}", sha, sha);

            // In production builds, prune old working logs that have expired.
            // Debug builds never prune so developers can inspect old state.
            if !cfg!(debug_assertions) {
                self.prune_expired_old_working_logs();
            }
        }
        Ok(())
    }

    /// Number of seconds to retain archived working logs in production builds (7 days).
    const OLD_WORKING_LOG_RETENTION_SECS: u64 = 7 * 24 * 60 * 60;

    /// Remove archived (`old-*`) working log directories whose `.archived_at`
    /// timestamp is older than `OLD_WORKING_LOG_RETENTION_SECS`.
    /// Errors are intentionally swallowed so pruning never breaks the commit flow.
    #[doc(hidden)]
    pub fn prune_expired_old_working_logs(&self) {
        let now_secs = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs();

        let entries = match fs::read_dir(&self.working_logs) {
            Ok(e) => e,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !name_str.starts_with("old-") {
                continue;
            }

            let dir_path = entry.path();
            if !dir_path.is_dir() {
                continue;
            }

            let marker = dir_path.join(".archived_at");
            let archived_at = match fs::read_to_string(&marker) {
                Ok(contents) => contents.trim().parse::<u64>().unwrap_or(0),
                // No marker means this was created before the retention feature;
                // treat it as immediately expired so it gets cleaned up.
                Err(_) => 0,
            };

            if now_secs.saturating_sub(archived_at) >= Self::OLD_WORKING_LOG_RETENTION_SECS {
                tracing::debug!("Pruning expired old working log: {}", name_str);
                let _ = fs::remove_dir_all(&dir_path);
            }
        }
    }

    /// Move a working log directory from one commit SHA to another.
    /// If the destination already has checkpoints, preserve the old-base entries first and
    /// append the destination entries after them.
    pub fn rename_working_log(&self, old_sha: &str, new_sha: &str) -> Result<(), GitAiError> {
        if old_sha == new_sha {
            return Ok(());
        }
        let old_dir = self.working_logs.join(old_sha);
        let new_dir = self.working_logs.join(new_sha);
        if !old_dir.exists() {
            // A retry can arrive after rename/remove succeeded but its parent
            // sync failed. Repeating the sync makes that partial success
            // recoverable without replaying the rename.
            if new_dir.exists() {
                sync_working_log_parent_directory(&self.working_logs)?;
            }
            return Ok(());
        }
        if !new_dir.exists() {
            crate::wltrace::wltrace("working_log.rename", &old_dir, || {
                format!("to={}", new_dir.display())
            });
            fs::rename(&old_dir, &new_dir)?;
            sync_working_log_parent_directory(&self.working_logs)?;
            tracing::debug!("Renamed working log from {} to {}", old_sha, new_sha);
        } else {
            crate::wltrace::wltrace("working_log.merge", &old_dir, || {
                format!("to={}", new_dir.display())
            });
            self.merge_working_log_dirs(old_sha, new_sha, &old_dir, &new_dir)?;
            fs::remove_dir_all(&old_dir)?;
            sync_working_log_parent_directory(&self.working_logs)?;
            tracing::debug!("Merged working log from {} into {}", old_sha, new_sha);
        }
        Ok(())
    }

    fn merge_working_log_dirs(
        &self,
        old_sha: &str,
        new_sha: &str,
        old_dir: &Path,
        new_dir: &Path,
    ) -> Result<(), GitAiError> {
        let canonical = self
            .repo_workdir
            .canonicalize()
            .unwrap_or_else(|_| self.repo_workdir.clone());
        let old_log = PersistedWorkingLog::new(
            old_dir.to_path_buf(),
            old_sha,
            self.repo_workdir.clone(),
            canonical.clone(),
            None,
        );
        let new_log = PersistedWorkingLog::new(
            new_dir.to_path_buf(),
            new_sha,
            self.repo_workdir.clone(),
            canonical,
            None,
        );

        // Preserve OLD-base entries first (per rename_working_log's contract):
        // start from the old INITIAL and only insert a new-base entry when its
        // key is absent, so old wins on any shared key. HashMap::extend would do
        // the opposite (new clobbers old). The checkpoints Vec below is already
        // old-then-new, so it needs no such guard.
        let old_initial = old_log.read_initial_attributions()?;
        let new_initial = new_log.read_initial_attributions()?;
        let old_checkpoints = old_log.read_all_checkpoints()?;
        let new_checkpoints = new_log.read_all_checkpoints()?;
        for checkpoint in &old_checkpoints {
            old_log.validate_checkpoint_blob_references(checkpoint)?;
        }
        for checkpoint in &new_checkpoints {
            new_log.validate_checkpoint_blob_references(checkpoint)?;
        }

        copy_blob_references(
            &old_log,
            &new_log,
            old_initial.file_blobs.values().map(String::as_str).chain(
                old_checkpoints.iter().flat_map(|checkpoint| {
                    checkpoint
                        .entries
                        .iter()
                        .map(|entry| entry.blob_sha.as_str())
                }),
            ),
        )?;

        let mut merged_initial = old_initial;
        for (k, v) in new_initial.files {
            merged_initial.files.entry(k).or_insert(v);
        }
        for (k, v) in new_initial.prompts {
            merged_initial.prompts.entry(k).or_insert(v);
        }
        for (k, v) in new_initial.file_blobs {
            merged_initial.file_blobs.entry(k).or_insert(v);
        }
        for (k, v) in new_initial.humans {
            merged_initial.humans.entry(k).or_insert(v);
        }
        for (k, v) in new_initial.sessions {
            merged_initial.sessions.entry(k).or_insert(v);
        }
        new_log.write_initial(merged_initial)?;

        let mut checkpoints = old_checkpoints;
        checkpoints.extend(new_checkpoints);
        new_log.write_all_checkpoints(&checkpoints)?;
        Ok(())
    }
}

fn copy_blob_references<'a>(
    source_log: &PersistedWorkingLog,
    target_log: &PersistedWorkingLog,
    blob_shas: impl IntoIterator<Item = &'a str>,
) -> Result<(), GitAiError> {
    let mut copied = HashSet::new();
    for blob_sha in blob_shas {
        if !copied.insert(blob_sha) {
            continue;
        }
        let content = source_log.get_file_version(blob_sha)?;
        let persisted_sha = target_log.persist_file_version(&content)?;
        if persisted_sha != blob_sha {
            return Err(GitAiError::Generic(format!(
                "blob {} changed identity while copying working logs",
                blob_sha
            )));
        }
    }
    Ok(())
}

fn sha256_hex(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    format!("{:x}", hasher.finalize())
}

fn validate_blob_sha(sha: &str) -> Result<(), GitAiError> {
    if sha.len() != 64
        || !sha
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(GitAiError::Generic(format!(
            "invalid content-addressed blob SHA: {sha:?}"
        )));
    }
    Ok(())
}

#[derive(Clone)]
pub struct PersistedWorkingLog {
    pub dir: PathBuf,
    #[allow(dead_code)]
    pub base_commit: String,
    pub repo_workdir: PathBuf,
    /// Canonical (absolute, resolved) version of workdir for reliable path comparisons
    /// On Windows, this uses the \\?\ UNC prefix format
    #[allow(dead_code)]
    pub canonical_workdir: PathBuf,
    pub dirty_files: Option<HashMap<String, Arc<str>>>,
    pub initial_file: PathBuf,
}

impl PersistedWorkingLog {
    pub fn new(
        dir: PathBuf,
        base_commit: &str,
        repo_root: PathBuf,
        canonical_workdir: PathBuf,
        dirty_files: Option<HashMap<String, Arc<str>>>,
    ) -> Self {
        let initial_file = dir.join("INITIAL");
        Self {
            dir,
            base_commit: base_commit.to_string(),
            repo_workdir: repo_root,
            canonical_workdir,
            dirty_files,
            initial_file,
        }
    }

    pub fn set_dirty_files(&mut self, dirty_files: Option<HashMap<String, Arc<str>>>) {
        let normalized_dirty_files = dirty_files.map(|map| {
            map.into_iter()
                .map(|(file_path, content)| {
                    let relative_path = self.to_repo_relative_path(&file_path);
                    let normalized_path = normalize_to_posix(&relative_path);
                    (normalized_path, content)
                })
                .collect::<HashMap<_, _>>()
        });

        self.dirty_files = normalized_dirty_files;
    }

    pub fn reset_working_log(&self) -> Result<(), GitAiError> {
        crate::wltrace::wltrace("working_log.reset", &self.dir, String::new);
        // Clear all blobs by removing the blobs directory
        let blobs_dir = self.dir.join("blobs");
        if blobs_dir.exists() {
            fs::remove_dir_all(&blobs_dir)?;
        }

        // Clear checkpoints by truncating the JSONL file
        let checkpoints_file = self.checkpoints_file();
        fs::write(&checkpoints_file, "")?;

        // Clear INITIAL attributions file so stale attributions from a
        // previous working state do not persist across resets
        if self.initial_file.exists() {
            fs::remove_file(&self.initial_file)?;
        }

        Ok(())
    }

    pub fn checkpoints_file(&self) -> PathBuf {
        self.dir.join("checkpoints.jsonl")
    }

    /* blob storage */
    pub fn get_file_version(&self, sha: &str) -> Result<String, GitAiError> {
        let bytes = self.read_verified_blob(sha)?;
        Ok(String::from_utf8(bytes)?)
    }

    pub fn persist_file_version(&self, content: &str) -> Result<String, GitAiError> {
        let content = content.as_bytes();
        let sha = sha256_hex(content);
        let blobs_dir = self.dir.join("blobs");
        fs::create_dir_all(&blobs_dir)?;
        // If create_dir_all published `blobs`, this sync is the durability gate
        // for that directory entry; if it already existed the sync is harmless.
        sync_working_log_parent_directory(&self.dir)?;

        let blob_path = blobs_dir.join(&sha);
        match fs::metadata(&blob_path) {
            Ok(metadata) => {
                if !metadata.is_file() {
                    return Err(GitAiError::Generic(format!(
                        "content-addressed blob path is not a file: {}",
                        blob_path.display()
                    )));
                }
                self.verify_existing_blob(&sha, content)?;
                return Ok(sha);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        let mut temp = tempfile::Builder::new()
            .prefix(".blob-")
            .suffix(".tmp")
            .tempfile_in(&blobs_dir)?;
        temp.write_all(content)?;
        temp.flush()?;
        temp.as_file().sync_all()?;

        match temp.persist_noclobber(&blob_path) {
            Ok(persisted) => {
                // Flush the handle under its final name, then publish the name
                // durably in the blob directory.
                persisted.sync_all()?;
                sync_working_log_parent_directory(&blobs_dir)?;
            }
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                // Another writer won the content-addressed race. Never trust a
                // same-named file without checking both its bytes and SHA.
                error.file.close()?;
                self.verify_existing_blob(&sha, content)?;
            }
            Err(error) => {
                return Err(GitAiError::Generic(format!(
                    "failed to atomically publish blob {}: {}",
                    blob_path.display(),
                    error.error
                )));
            }
        }

        Ok(sha)
    }

    fn blob_path(&self, sha: &str) -> Result<PathBuf, GitAiError> {
        validate_blob_sha(sha)?;
        Ok(self.dir.join("blobs").join(sha))
    }

    fn read_verified_blob(&self, sha: &str) -> Result<Vec<u8>, GitAiError> {
        let blob_path = self.blob_path(sha)?;
        let content = fs::read(&blob_path)?;
        let actual_sha = sha256_hex(&content);
        if actual_sha != sha {
            return Err(GitAiError::Generic(format!(
                "content-addressed blob {} failed SHA verification (actual {})",
                blob_path.display(),
                actual_sha
            )));
        }
        Ok(content)
    }

    fn verify_existing_blob(&self, sha: &str, expected: &[u8]) -> Result<(), GitAiError> {
        let blob_path = self.blob_path(sha)?;
        let actual = self.read_verified_blob(sha)?;
        if actual != expected {
            return Err(GitAiError::Generic(format!(
                "existing content-addressed blob has unexpected content: {}",
                blob_path.display()
            )));
        }
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&blob_path)?
            .sync_all()?;
        sync_working_log_parent_directory(blob_path.parent().ok_or_else(|| {
            GitAiError::Generic(format!(
                "content-addressed blob has no parent: {}",
                blob_path.display()
            ))
        })?)?;
        Ok(())
    }

    pub(crate) fn validate_checkpoint_blob_references(
        &self,
        checkpoint: &Checkpoint,
    ) -> Result<(), GitAiError> {
        for entry in &checkpoint.entries {
            self.get_file_version(&entry.blob_sha).map_err(|error| {
                GitAiError::Generic(format!(
                    "checkpoint blob for {} is missing or corrupt ({}): {}",
                    entry.file, entry.blob_sha, error
                ))
            })?;
        }
        Ok(())
    }

    pub fn to_repo_absolute_path(&self, file_path: &str) -> String {
        if Path::new(file_path).is_absolute() {
            return file_path.to_string();
        }
        self.repo_workdir
            .join(file_path)
            .to_string_lossy()
            .to_string()
    }

    pub fn to_repo_relative_path(&self, file_path: &str) -> String {
        if !Path::new(file_path).is_absolute() {
            return file_path.to_string();
        }
        let path = Path::new(file_path);

        // Try without canonicalizing first
        if path.starts_with(&self.repo_workdir) {
            return path
                .strip_prefix(&self.repo_workdir)
                .unwrap()
                .to_string_lossy()
                .to_string();
        }

        // If we couldn't match yet, try canonicalizing both repo_workdir and the input path
        // On Windows, this uses the canonical_workdir that was pre-computed
        #[cfg(windows)]
        let canonical_workdir = &self.canonical_workdir;

        #[cfg(not(windows))]
        let canonical_workdir = match self.repo_workdir.canonicalize() {
            Ok(p) => p,
            Err(_) => self.repo_workdir.clone(),
        };

        let canonical_path = match path.canonicalize() {
            Ok(p) => p,
            Err(_) => path.to_path_buf(),
        };

        #[cfg(windows)]
        if canonical_path.starts_with(canonical_workdir) {
            return canonical_path
                .strip_prefix(canonical_workdir)
                .unwrap()
                .to_string_lossy()
                .to_string();
        }

        #[cfg(not(windows))]
        if canonical_path.starts_with(&canonical_workdir) {
            return canonical_path
                .strip_prefix(&canonical_workdir)
                .unwrap()
                .to_string_lossy()
                .to_string();
        }

        file_path.to_string()
    }

    pub fn read_current_file_content(&self, file_path: &str) -> Result<Arc<str>, GitAiError> {
        if let Some(ref dirty_files) = self.dirty_files
            && let Some(content) = dirty_files.get(&file_path.to_string())
        {
            return Ok(content.clone());
        }

        Err(GitAiError::Generic(format!(
            "read_current_file_content: file '{}' not found in dirty_files snapshot (filesystem fallback is not allowed in checkpoint flow)",
            file_path
        )))
    }

    /* append checkpoint */
    pub fn append_checkpoint(&self, checkpoint: &Checkpoint) -> Result<(), GitAiError> {
        self.append_checkpoint_idempotent(checkpoint).map(|_| ())
    }

    /// Append a checkpoint once for its stable trace/request id.
    ///
    /// Durable checkpoint jobs can crash after the atomic working-log replace
    /// but before their SQLite phase marker commits.  Replaying the same job
    /// must therefore observe the published trace id and avoid a duplicate.
    pub fn append_checkpoint_idempotent(
        &self,
        checkpoint: &Checkpoint,
    ) -> Result<bool, GitAiError> {
        // Read existing checkpoints
        let mut checkpoints = self.read_all_checkpoints()?;

        if let Some(request_id) = checkpoint.trace_id.as_deref()
            && checkpoints
                .iter()
                .any(|existing| existing.trace_id.as_deref() == Some(request_id))
        {
            return Ok(false);
        }

        self.append_checkpoint_to(&mut checkpoints, checkpoint.clone())?;
        Ok(true)
    }

    /// Append to a checkpoint collection that the caller has already loaded.
    ///
    /// Checkpoint execution needs the prior collection to calculate attribution.
    /// Reusing it here avoids deserializing the entire working log a second time
    /// and avoids cloning the new checkpoint's attribution payload.
    pub fn append_checkpoint_to(
        &self,
        checkpoints: &mut Vec<Checkpoint>,
        checkpoint: Checkpoint,
    ) -> Result<(), GitAiError> {
        crate::wltrace::wltrace("working_log.append_checkpoint", &self.dir, String::new);

        checkpoints.push(checkpoint);

        // Prune char-level attributions from older checkpoints for the same files
        // Only the most recent checkpoint per file needs char-level precision
        self.prune_old_char_attributions(checkpoints);

        // Write all checkpoints back
        self.write_all_checkpoints(checkpoints)
    }

    pub fn read_all_checkpoints(&self) -> Result<Vec<Checkpoint>, GitAiError> {
        self.read_all_checkpoints_with_size_limit(Self::checkpoints_file_size_limit_bytes())
    }

    #[cfg(feature = "test-support")]
    pub fn read_all_checkpoints_with_size_limit_for_test(
        &self,
        max_bytes: u64,
    ) -> Result<Vec<Checkpoint>, GitAiError> {
        self.read_all_checkpoints_with_size_limit(max_bytes)
    }

    pub fn ensure_checkpoints_file_size_limit(&self) -> Result<(), GitAiError> {
        self.reject_oversized_checkpoints_file(Self::checkpoints_file_size_limit_bytes())
    }

    /// Preserve an oversized live checkpoints file for forensic recovery while
    /// allowing a stash operation to continue from an empty checkpoint log.
    /// Ordinary reads and writes remain fail-closed and never call this path.
    pub fn quarantine_oversized_checkpoints_for_stash(
        &self,
    ) -> Result<Option<PathBuf>, GitAiError> {
        self.quarantine_oversized_checkpoints_for_stash_with_limit(
            Self::checkpoints_file_size_limit_bytes(),
        )
    }

    fn quarantine_oversized_checkpoints_for_stash_with_limit(
        &self,
        max_bytes: u64,
    ) -> Result<Option<PathBuf>, GitAiError> {
        let checkpoints_file = self.checkpoints_file();
        let metadata = match fs::metadata(&checkpoints_file) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let size_bytes = metadata.len();
        if size_bytes <= max_bytes {
            return Ok(None);
        }

        let parent = checkpoints_file.parent().ok_or_else(|| {
            GitAiError::Generic(format!(
                "working-log file has no parent directory: {}",
                checkpoints_file.display()
            ))
        })?;
        let quarantine_path = parent.join(format!(
            "checkpoints.jsonl.oversized-{}",
            crate::uuid::generate_v4()
        ));
        fs::rename(&checkpoints_file, &quarantine_path)?;
        sync_working_log_parent_directory(parent)?;
        self.write_all_checkpoints(&[])?;

        tracing::warn!(
            base_commit = %self.base_commit,
            path = %checkpoints_file.display(),
            quarantine_path = %quarantine_path.display(),
            size_bytes,
            max_bytes,
            "oversized checkpoints.jsonl quarantined before stash"
        );
        crate::observability::log_error(
            &GitAiError::Generic(format!(
                "oversized checkpoints.jsonl was quarantined before stash: {}",
                quarantine_path.display()
            )),
            Some(serde_json::json!({
                "event": "checkpoints_jsonl_oversized_quarantined_for_stash",
                "base_commit": self.base_commit,
                "path": checkpoints_file.to_string_lossy(),
                "quarantine_path": quarantine_path.to_string_lossy(),
                "size_bytes": size_bytes,
                "max_bytes": max_bytes,
            })),
        );
        Ok(Some(quarantine_path))
    }

    fn read_all_checkpoints_with_size_limit(
        &self,
        max_bytes: u64,
    ) -> Result<Vec<Checkpoint>, GitAiError> {
        crate::wltrace::wltrace("working_log.read_checkpoints", &self.dir, String::new);
        let checkpoints_file = self.checkpoints_file();

        if !checkpoints_file.exists() {
            return Ok(Vec::new());
        }

        self.reject_oversized_checkpoints_file(max_bytes)?;

        let input = fs::File::open(&checkpoints_file)?;
        let mut checkpoints = Vec::new();

        // Parse JSONL file - each line is a separate JSON object
        for line in BufReader::new(input).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }

            let checkpoint: Checkpoint = serde_json::from_str(&line).map_err(|e| {
                crate::wltrace::wltrace("working_log.read_checkpoints.TORN", &self.dir, || {
                    format!("err={e} line_len={}", line.len())
                });
                std::io::Error::new(std::io::ErrorKind::InvalidData, e)
            })?;

            if checkpoint.api_version != CHECKPOINT_API_VERSION {
                tracing::debug!(
                    "unsupported checkpoint api version: {} (silently skipping checkpoint)",
                    checkpoint.api_version
                );
                continue;
            }

            checkpoints.push(checkpoint);
        }

        // Migrate 7-char prompt hashes to 16-char hashes
        // Step 1: Build mapping from old 7-char hash to new 16-char hash
        let mut old_to_new_hash: HashMap<String, String> = HashMap::new();

        for checkpoint in &checkpoints {
            if let Some(agent_id) = &checkpoint.agent_id {
                let new_hash = generate_short_hash(&agent_id.id, &agent_id.tool);
                let old_hash = new_hash[..7].to_string();
                old_to_new_hash.insert(old_hash, new_hash);
            }
        }

        // Step 2: Replace 7-char author_ids in all checkpoints' attributions and line_attributions
        let mut migrated_checkpoints = Vec::new();
        for mut checkpoint in checkpoints {
            for entry in &mut checkpoint.entries {
                // Replace author_ids in attributions
                for attr in &mut entry.attributions {
                    if attr.author_id.len() == 7
                        && let Some(new_hash) = old_to_new_hash.get(&attr.author_id)
                    {
                        attr.author_id = new_hash.clone();
                    }
                }

                // Replace author_ids in line_attributions
                for line_attr in &mut entry.line_attributions {
                    if line_attr.author_id.len() == 7
                        && let Some(new_hash) = old_to_new_hash.get(&line_attr.author_id)
                    {
                        line_attr.author_id = new_hash.clone();
                    }
                    // Also migrate the overrode field if it contains a 7-char hash
                    if let Some(ref overrode_id) = line_attr.overrode
                        && overrode_id.len() == 7
                        && let Some(new_hash) = old_to_new_hash.get(overrode_id)
                    {
                        line_attr.overrode = Some(new_hash.clone());
                    }
                }
            }
            migrated_checkpoints.push(checkpoint);
        }

        Ok(migrated_checkpoints)
    }

    fn checkpoints_file_size_limit_bytes() -> u64 {
        #[cfg(feature = "test-support")]
        if let Ok(raw) = std::env::var(TEST_CHECKPOINTS_JSONL_MAX_BYTES_ENV)
            && let Ok(value) = raw.parse::<u64>()
            && value > 0
        {
            return value;
        }

        MAX_CHECKPOINTS_JSONL_BYTES
    }

    fn reject_oversized_checkpoints_file(&self, max_bytes: u64) -> Result<(), GitAiError> {
        let checkpoints_file = self.checkpoints_file();
        let metadata = match fs::metadata(&checkpoints_file) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let size_bytes = metadata.len();
        if size_bytes <= max_bytes {
            return Ok(());
        }

        let message = format!(
            "checkpoints.jsonl exceeded maximum size: {} bytes > {} bytes; refusing to modify preserved file {}",
            size_bytes,
            max_bytes,
            checkpoints_file.display()
        );
        tracing::error!(
            base_commit = %self.base_commit,
            path = %checkpoints_file.display(),
            size_bytes,
            max_bytes,
            "checkpoints.jsonl exceeded maximum size; preserving it and failing closed"
        );
        crate::observability::log_error(
            &GitAiError::Generic(message.clone()),
            Some(serde_json::json!({
                "event": "checkpoints_jsonl_oversized_blocked",
                "base_commit": self.base_commit,
                "path": checkpoints_file.to_string_lossy(),
                "size_bytes": size_bytes,
                "max_bytes": max_bytes,
            })),
        );
        Err(GitAiError::Generic(message))
    }

    /// Remove char-level attributions from all but the most recent checkpoint per file.
    /// This reduces storage size while preserving precision for the entries that matter.
    /// Only the most recent checkpoint entry for each file is used when computing new entries.
    fn prune_old_char_attributions(&self, checkpoints: &mut [Checkpoint]) {
        // Track which checkpoint index has the most recent entry for each file
        // Iterate from newest to oldest
        let mut newest_for_file: HashMap<String, usize> = HashMap::new();

        for (checkpoint_idx, checkpoint) in checkpoints.iter().enumerate().rev() {
            for entry in &checkpoint.entries {
                newest_for_file
                    .entry(entry.file.clone())
                    .or_insert(checkpoint_idx);
            }
        }

        // Clear attributions from entries that aren't the most recent for their file
        for (checkpoint_idx, checkpoint) in checkpoints.iter_mut().enumerate() {
            for entry in &mut checkpoint.entries {
                if let Some(&newest_idx) = newest_for_file.get(&entry.file)
                    && checkpoint_idx != newest_idx
                {
                    entry.attributions.clear();
                }
            }
        }
    }

    /// Write all checkpoints to the JSONL file, replacing any existing content
    /// Note: Unlike append_checkpoint(), this preserves transcripts because it's used
    /// by post-commit after transcripts have been refetched and need to be preserved
    /// for from_just_working_log() to read them.
    pub fn write_all_checkpoints(&self, checkpoints: &[Checkpoint]) -> Result<(), GitAiError> {
        crate::wltrace::wltrace("working_log.write_all_checkpoints.begin", &self.dir, || {
            format!("count={}", checkpoints.len())
        });
        let checkpoints_file = self.checkpoints_file();
        let parent = checkpoints_file.parent().ok_or_else(|| {
            GitAiError::Generic(format!(
                "working-log file has no parent directory: {}",
                checkpoints_file.display()
            ))
        })?;
        fs::create_dir_all(parent)?;
        let mut temp = tempfile::Builder::new()
            .prefix(".checkpoints-")
            .suffix(".jsonl.tmp")
            .tempfile_in(parent)?;
        {
            let mut output = BufWriter::new(temp.as_file_mut());
            for checkpoint in checkpoints {
                serde_json::to_writer(&mut output, checkpoint)?;
                output.write_all(b"\n")?;
            }
            output.flush()?;
        }
        temp.as_file().sync_all()?;
        let persisted = temp.persist(&checkpoints_file).map_err(|error| {
            GitAiError::Generic(format!(
                "failed to atomically replace working log {}: {}",
                checkpoints_file.display(),
                error.error
            ))
        })?;
        // Flush the handle under its final name. On Windows this is the
        // portable durability boundary available after MoveFileExW replacement.
        persisted.sync_all()?;
        sync_working_log_parent_directory(parent)?;
        crate::wltrace::wltrace(
            "working_log.write_all_checkpoints.end",
            &self.dir,
            String::new,
        );
        Ok(())
    }

    pub fn mutate_all_checkpoints<F>(&self, mutator: F) -> Result<Vec<Checkpoint>, GitAiError>
    where
        F: FnOnce(&mut Vec<Checkpoint>) -> Result<(), GitAiError>,
    {
        let mut checkpoints = self.read_all_checkpoints()?;
        mutator(&mut checkpoints)?;
        self.write_all_checkpoints(&checkpoints)?;
        Ok(checkpoints)
    }

    pub fn all_touched_files(&self) -> Result<HashSet<String>, GitAiError> {
        let checkpoints = self.read_all_checkpoints()?;
        let mut touched_files = HashSet::new();
        for checkpoint in checkpoints {
            for entry in checkpoint.entries {
                touched_files.insert(entry.file);
            }
        }
        Ok(touched_files)
    }

    pub fn observed_file_snapshot(&self) -> Result<HashMap<String, String>, GitAiError> {
        let initial = self.read_initial_attributions()?;
        let mut snapshot = HashMap::new();

        for file_path in initial.files.keys() {
            let content = self
                .stored_initial_file_content_from(&initial, file_path)?
                .ok_or_else(|| {
                    GitAiError::Generic(format!(
                        "INITIAL missing persisted file snapshot for {}",
                        file_path
                    ))
                })?;
            snapshot.insert(file_path.clone(), content);
        }

        for checkpoint in self.read_all_checkpoints()? {
            for entry in checkpoint.entries {
                let content = self.get_file_version(&entry.blob_sha)?;
                snapshot.insert(entry.file, content);
            }
        }

        Ok(snapshot)
    }

    #[allow(dead_code)]
    pub fn all_ai_touched_files(&self) -> Result<HashSet<String>, GitAiError> {
        let checkpoints = self.read_all_checkpoints()?;
        let mut touched_files = HashSet::new();
        for checkpoint in checkpoints {
            // Only include files from AI checkpoints (AiAgent or AiTab)
            match checkpoint.kind {
                CheckpointKind::AiAgent | CheckpointKind::AiTab => {
                    for entry in checkpoint.entries {
                        touched_files.insert(entry.file);
                    }
                }
                CheckpointKind::Human | CheckpointKind::KnownHuman => {
                    // Skip human checkpoints
                }
            }
        }
        Ok(touched_files)
    }

    /* INITIAL attributions file */

    /// Persist INITIAL attributions plus exact file snapshots for the target working log.
    pub fn write_initial_attributions_with_contents(
        &self,
        attributions: HashMap<String, Vec<LineAttribution>>,
        prompts: HashMap<String, PromptRecord>,
        humans: std::collections::BTreeMap<String, HumanRecord>,
        file_contents: HashMap<String, String>,
        sessions: std::collections::BTreeMap<String, SessionRecord>,
    ) -> Result<(), GitAiError> {
        let filtered: HashMap<String, Vec<LineAttribution>> = attributions
            .into_iter()
            .filter(|(_, attrs)| !attrs.is_empty())
            .collect();
        let mut file_blobs = HashMap::new();
        for file_path in filtered.keys() {
            let content = file_contents.get(file_path).ok_or_else(|| {
                GitAiError::Generic(format!(
                    "INITIAL missing file content snapshot for {}",
                    file_path
                ))
            })?;
            let blob_sha = self.persist_file_version(content)?;
            file_blobs.insert(file_path.clone(), blob_sha);
        }

        self.write_initial(InitialAttributions {
            files: filtered,
            prompts,
            file_blobs,
            humans,
            sessions,
        })
    }

    /// Write a fully-formed INITIAL state, preserving any persisted blob references.
    pub fn write_initial(&self, initial: InitialAttributions) -> Result<(), GitAiError> {
        crate::wltrace::wltrace("working_log.write_initial", &self.dir, String::new);
        let filtered_files: HashMap<String, Vec<LineAttribution>> = initial
            .files
            .into_iter()
            .filter(|(_, attrs)| !attrs.is_empty())
            .collect();

        if filtered_files.is_empty() {
            let parent = self.initial_file.parent().ok_or_else(|| {
                GitAiError::Generic(format!(
                    "INITIAL file has no parent directory: {}",
                    self.initial_file.display()
                ))
            })?;
            match fs::remove_file(&self.initial_file) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            // Also sync when the file is already absent: that may be a retry
            // after remove_file succeeded and the previous directory sync did
            // not. Absence alone is not a durability acknowledgement.
            sync_working_log_parent_directory(parent)?;
            return Ok(());
        }

        let mut file_blobs = initial.file_blobs;
        file_blobs.retain(|file_path, _| filtered_files.contains_key(file_path));

        let initial_data = InitialAttributions {
            files: filtered_files,
            prompts: initial.prompts,
            file_blobs,
            humans: initial.humans,
            sessions: initial.sessions,
        };

        self.validate_initial_blob_references(&initial_data)?;

        let parent = self.initial_file.parent().ok_or_else(|| {
            GitAiError::Generic(format!(
                "INITIAL file has no parent directory: {}",
                self.initial_file.display()
            ))
        })?;
        fs::create_dir_all(parent)?;
        let mut temp = tempfile::Builder::new()
            .prefix(".initial-")
            .suffix(".tmp")
            .tempfile_in(parent)?;
        {
            let mut output = BufWriter::new(temp.as_file_mut());
            serde_json::to_writer_pretty(&mut output, &initial_data)?;
            output.flush()?;
        }
        temp.as_file().sync_all()?;
        let persisted = temp.persist(&self.initial_file).map_err(|error| {
            GitAiError::Generic(format!(
                "failed to atomically replace INITIAL {}: {}",
                self.initial_file.display(),
                error.error
            ))
        })?;
        persisted.sync_all()?;
        sync_working_log_parent_directory(parent)?;

        Ok(())
    }

    pub fn initial_file_content_from(
        &self,
        initial: &InitialAttributions,
        file_path: &str,
    ) -> Result<Option<String>, GitAiError> {
        if let Some(content) = self.stored_initial_file_content_from(initial, file_path)? {
            return Ok(Some(content));
        }
        if initial.files.contains_key(file_path) {
            return Err(GitAiError::Generic(format!(
                "INITIAL missing persisted file snapshot for {}",
                file_path
            )));
        }
        Ok(None)
    }

    pub fn stored_initial_file_content_from(
        &self,
        initial: &InitialAttributions,
        file_path: &str,
    ) -> Result<Option<String>, GitAiError> {
        if let Some(blob_sha) = initial.file_blobs.get(file_path) {
            return self.get_file_version(blob_sha).map(Some);
        }
        Ok(None)
    }

    pub fn latest_checkpoint_file_content(
        &self,
        file_path: &str,
    ) -> Result<Option<String>, GitAiError> {
        let checkpoints = self.read_all_checkpoints()?;
        let entry = checkpoints.iter().rev().find_map(|checkpoint| {
            checkpoint
                .entries
                .iter()
                .find(|entry| entry.file == file_path)
        });
        entry
            .map(|entry| self.get_file_version(&entry.blob_sha))
            .transpose()
    }

    pub fn effective_tracked_file_content(
        &self,
        initial: &InitialAttributions,
        file_path: &str,
    ) -> Result<Option<String>, GitAiError> {
        if let Some(content) = self.latest_checkpoint_file_content(file_path)? {
            return Ok(Some(content));
        }
        self.initial_file_content_from(initial, file_path)
    }

    /// Read initial attributions from the INITIAL file.
    /// Returns empty attributions only if the file doesn't exist. Every other
    /// I/O, JSON, missing-blob, or corrupt-blob failure is returned to the
    /// caller so a broken baseline cannot be mistaken for an empty one.
    pub fn read_initial_attributions(&self) -> Result<InitialAttributions, GitAiError> {
        let content = match fs::read_to_string(&self.initial_file) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::symlink_metadata(&self.initial_file) {
                    Err(metadata_error)
                        if metadata_error.kind() == std::io::ErrorKind::NotFound =>
                    {
                        return Ok(InitialAttributions::default());
                    }
                    Ok(_) | Err(_) => {
                        return Err(GitAiError::EvidenceError(format!(
                            "cannot read preserved INITIAL {}: {}",
                            self.initial_file.display(),
                            error
                        )));
                    }
                }
            }
            Err(error) => {
                return Err(GitAiError::EvidenceError(format!(
                    "cannot read preserved INITIAL {}: {}",
                    self.initial_file.display(),
                    error
                )));
            }
        };
        let initial: InitialAttributions = serde_json::from_str(&content).map_err(|error| {
            GitAiError::EvidenceError(format!(
                "preserved INITIAL {} is not valid JSON: {}",
                self.initial_file.display(),
                error
            ))
        })?;
        self.validate_initial_blob_references(&initial)?;
        Ok(initial)
    }

    fn validate_initial_blob_references(
        &self,
        initial: &InitialAttributions,
    ) -> Result<(), GitAiError> {
        for file_path in initial.files.keys() {
            if !initial.file_blobs.contains_key(file_path) {
                return Err(GitAiError::EvidenceError(format!(
                    "INITIAL missing persisted file snapshot for {}",
                    file_path
                )));
            }
        }

        for (file_path, blob_sha) in &initial.file_blobs {
            self.get_file_version(blob_sha).map_err(|error| {
                GitAiError::EvidenceError(format!(
                    "INITIAL blob for {} is missing or corrupt ({}): {}",
                    file_path, blob_sha, error
                ))
            })?;
        }
        Ok(())
    }
}

#[cfg(unix)]
fn sync_working_log_parent_directory(parent: &Path) -> Result<(), GitAiError> {
    // The file bytes and final-name handle were already synced. Syncing the
    // directory makes the rename itself durable across a machine crash.
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_working_log_parent_directory(_parent: &Path) -> Result<(), GitAiError> {
    // Rust does not expose a portable directory handle on Windows. tempfile's
    // persist uses replacement semantics and the final file handle is synced.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn attr(author: &str) -> Vec<LineAttribution> {
        vec![LineAttribution::new(1, 1, author.to_string(), None)]
    }

    fn checkpoint(trace_id: &str) -> Checkpoint {
        let mut checkpoint = Checkpoint::new(
            CheckpointKind::AiAgent,
            "diff".to_string(),
            "agent".to_string(),
            Vec::new(),
        );
        checkpoint.trace_id = Some(trace_id.to_string());
        checkpoint
    }

    #[test]
    fn stable_trace_makes_working_log_append_idempotent() {
        let tmp = TempDir::new().unwrap();
        let workdir = tmp.path().join("workdir");
        fs::create_dir_all(&workdir).unwrap();
        let storage = RepoStorage::for_repo_path(&tmp.path().join("repo"), &workdir).unwrap();
        let log = storage.working_log_for_base_commit("base").unwrap();
        let checkpoint = checkpoint("checkpoint-job:stable");

        assert!(log.append_checkpoint_idempotent(&checkpoint).unwrap());
        assert!(!log.append_checkpoint_idempotent(&checkpoint).unwrap());
        assert_eq!(log.read_all_checkpoints().unwrap().len(), 1);
    }

    #[test]
    fn corrupt_working_log_is_not_truncated_during_failed_append() {
        let tmp = TempDir::new().unwrap();
        let workdir = tmp.path().join("workdir");
        fs::create_dir_all(&workdir).unwrap();
        let storage = RepoStorage::for_repo_path(&tmp.path().join("repo"), &workdir).unwrap();
        let log = storage.working_log_for_base_commit("base").unwrap();
        let corrupt = b"{not-json}\n";
        fs::write(log.checkpoints_file(), corrupt).unwrap();

        assert!(
            log.append_checkpoint_idempotent(&checkpoint("checkpoint-job:new"))
                .is_err()
        );
        assert_eq!(fs::read(log.checkpoints_file()).unwrap(), corrupt);
    }

    #[test]
    fn stash_quarantine_preserves_oversized_bytes_and_resets_live_log() {
        let tmp = TempDir::new().unwrap();
        let workdir = tmp.path().join("workdir");
        fs::create_dir_all(&workdir).unwrap();
        let storage = RepoStorage::for_repo_path(&tmp.path().join("repo"), &workdir).unwrap();
        let log = storage.working_log_for_base_commit("base").unwrap();
        let oversized = b"forensic checkpoint bytes\n";
        fs::write(log.checkpoints_file(), oversized).unwrap();

        let quarantine = log
            .quarantine_oversized_checkpoints_for_stash_with_limit(8)
            .unwrap()
            .expect("oversized file should be quarantined");

        assert_eq!(fs::read(quarantine).unwrap(), oversized);
        assert_eq!(fs::read(log.checkpoints_file()).unwrap(), b"");
        assert!(
            log.quarantine_oversized_checkpoints_for_stash_with_limit(8)
                .unwrap()
                .is_none(),
            "empty live log should make quarantine idempotent"
        );
    }

    #[test]
    fn atomic_replacement_publishes_only_complete_json_lines() {
        let tmp = TempDir::new().unwrap();
        let workdir = tmp.path().join("workdir");
        fs::create_dir_all(&workdir).unwrap();
        let storage = RepoStorage::for_repo_path(&tmp.path().join("repo"), &workdir).unwrap();
        let log = storage.working_log_for_base_commit("base").unwrap();
        log.write_all_checkpoints(&[checkpoint("one"), checkpoint("two")])
            .unwrap();

        let contents = fs::read_to_string(log.checkpoints_file()).unwrap();
        let parsed = contents
            .lines()
            .map(serde_json::from_str::<Checkpoint>)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(parsed.len(), 2);
        assert!(contents.ends_with('\n'));
    }

    #[test]
    fn missing_initial_is_the_only_empty_read_case() {
        let tmp = TempDir::new().unwrap();
        let workdir = tmp.path().join("workdir");
        fs::create_dir_all(&workdir).unwrap();
        let storage = RepoStorage::for_repo_path(&tmp.path().join("repo"), &workdir).unwrap();
        let log = storage.working_log_for_base_commit("base").unwrap();

        assert!(log.read_initial_attributions().unwrap().files.is_empty());

        fs::write(&log.initial_file, b"{not-json}").unwrap();
        assert!(matches!(
            log.read_initial_attributions(),
            Err(GitAiError::EvidenceError(_))
        ));

        fs::remove_file(&log.initial_file).unwrap();
        fs::create_dir(&log.initial_file).unwrap();
        assert!(matches!(
            log.read_initial_attributions(),
            Err(GitAiError::EvidenceError(_))
        ));

        #[cfg(unix)]
        {
            fs::remove_dir(&log.initial_file).unwrap();
            std::os::unix::fs::symlink(log.dir.join("missing-initial-target"), &log.initial_file)
                .unwrap();
            assert!(matches!(
                log.read_initial_attributions(),
                Err(GitAiError::EvidenceError(_))
            ));
        }
    }

    #[test]
    fn initial_read_fails_closed_for_missing_or_corrupt_blob() {
        let tmp = TempDir::new().unwrap();
        let workdir = tmp.path().join("workdir");
        fs::create_dir_all(&workdir).unwrap();
        let storage = RepoStorage::for_repo_path(&tmp.path().join("repo"), &workdir).unwrap();
        let log = storage.working_log_for_base_commit("base").unwrap();

        let mut missing_mapping = InitialAttributions::default();
        missing_mapping
            .files
            .insert("missing.txt".into(), attr("ai"));
        fs::write(
            &log.initial_file,
            serde_json::to_vec(&missing_mapping).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            log.read_initial_attributions(),
            Err(GitAiError::EvidenceError(_))
        ));

        let missing_sha = "0".repeat(64);
        missing_mapping
            .file_blobs
            .insert("missing.txt".into(), missing_sha.clone());
        fs::write(
            &log.initial_file,
            serde_json::to_vec(&missing_mapping).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            log.read_initial_attributions(),
            Err(GitAiError::EvidenceError(_))
        ));

        let blobs_dir = log.dir.join("blobs");
        fs::create_dir_all(&blobs_dir).unwrap();
        fs::write(blobs_dir.join(missing_sha), b"corrupt").unwrap();
        assert!(matches!(
            log.read_initial_attributions(),
            Err(GitAiError::EvidenceError(_))
        ));
    }

    #[test]
    fn existing_blob_is_verified_and_never_overwritten() {
        let tmp = TempDir::new().unwrap();
        let workdir = tmp.path().join("workdir");
        fs::create_dir_all(&workdir).unwrap();
        let storage = RepoStorage::for_repo_path(&tmp.path().join("repo"), &workdir).unwrap();
        let log = storage.working_log_for_base_commit("base").unwrap();

        let expected = "durable content";
        let sha = log.persist_file_version(expected).unwrap();
        assert_eq!(log.persist_file_version(expected).unwrap(), sha);
        assert_eq!(log.get_file_version(&sha).unwrap(), expected);

        fs::write(log.dir.join("blobs").join(&sha), b"corrupt").unwrap();
        assert!(log.persist_file_version(expected).is_err());
        assert_eq!(
            fs::read(log.dir.join("blobs").join(&sha)).unwrap(),
            b"corrupt"
        );
        assert!(log.get_file_version("../INITIAL").is_err());
    }

    #[test]
    fn invalid_initial_update_preserves_last_valid_baseline() {
        let tmp = TempDir::new().unwrap();
        let workdir = tmp.path().join("workdir");
        fs::create_dir_all(&workdir).unwrap();
        let storage = RepoStorage::for_repo_path(&tmp.path().join("repo"), &workdir).unwrap();
        let log = storage.working_log_for_base_commit("base").unwrap();

        let old_sha = log.persist_file_version("old content").unwrap();
        let mut old_initial = InitialAttributions::default();
        old_initial.files.insert("old.txt".into(), attr("ai-old"));
        old_initial.file_blobs.insert("old.txt".into(), old_sha);
        log.write_initial(old_initial).unwrap();
        let old_bytes = fs::read(&log.initial_file).unwrap();

        let mut broken = InitialAttributions::default();
        broken.files.insert("new.txt".into(), attr("ai-new"));
        broken.file_blobs.insert("new.txt".into(), "f".repeat(64));
        assert!(log.write_initial(broken).is_err());
        assert_eq!(fs::read(&log.initial_file).unwrap(), old_bytes);
        assert!(
            log.read_initial_attributions()
                .unwrap()
                .files
                .contains_key("old.txt")
        );

        log.write_initial(InitialAttributions::default()).unwrap();
        assert!(!log.initial_file.exists());
        assert!(log.read_initial_attributions().unwrap().files.is_empty());
    }

    #[test]
    fn direct_working_log_rename_preserves_valid_initial_and_blob() {
        let tmp = TempDir::new().unwrap();
        let workdir = tmp.path().join("workdir");
        fs::create_dir_all(&workdir).unwrap();
        let storage = RepoStorage::for_repo_path(&tmp.path().join("repo"), &workdir).unwrap();
        let old_log = storage.working_log_for_base_commit("old").unwrap();
        let blob_sha = old_log.persist_file_version("carried content").unwrap();
        let mut initial = InitialAttributions::default();
        initial.files.insert("carried.txt".into(), attr("ai"));
        initial
            .file_blobs
            .insert("carried.txt".into(), blob_sha.clone());
        old_log.write_initial(initial).unwrap();

        storage.rename_working_log("old", "new").unwrap();

        assert!(!storage.has_working_log("old"));
        let new_log = storage.working_log_for_base_commit("new").unwrap();
        let carried = new_log.read_initial_attributions().unwrap();
        assert_eq!(carried.file_blobs.get("carried.txt"), Some(&blob_sha));
        assert_eq!(
            new_log.get_file_version(&blob_sha).unwrap(),
            "carried content"
        );
    }

    /// Regression (#9): merge_working_log_dirs (via rename_working_log when the
    /// destination already exists) must preserve OLD-base INITIAL entries on a
    /// shared key, per the documented "preserve the old-base entries first".
    /// The old code used HashMap::extend(new), so `new` clobbered `old` for any
    /// shared path. Each side's unique entries must also survive.
    #[test]
    fn test_merge_working_log_dirs_old_base_wins_on_conflict() {
        let tmp = TempDir::new().unwrap();
        let workdir = tmp.path().join("workdir");
        fs::create_dir_all(&workdir).unwrap();
        let ai_dir = tmp.path().join("ai");
        let storage = RepoStorage::for_repo_path(&ai_dir, &workdir).unwrap();

        let old_sha = "1111111111111111111111111111111111111111";
        let new_sha = "2222222222222222222222222222222222222222";

        // OLD base: shared.txt -> old author, plus a unique old-only file.
        let old_log = storage.working_log_for_base_commit(old_sha).unwrap();
        let mut old_initial = InitialAttributions::default();
        old_initial.files.insert("shared.txt".into(), attr("h_OLD"));
        old_initial
            .files
            .insert("old_only.txt".into(), attr("h_OLD"));
        let old_shared_sha = old_log.persist_file_version("OLD CONTENT").unwrap();
        let old_only_sha = old_log.persist_file_version("OLD ONLY").unwrap();
        old_initial
            .file_blobs
            .insert("shared.txt".into(), old_shared_sha.clone());
        old_initial
            .file_blobs
            .insert("old_only.txt".into(), old_only_sha);
        old_log.write_initial(old_initial).unwrap();

        // NEW base: shared.txt -> new author (conflict), plus a unique new-only file.
        let new_log = storage.working_log_for_base_commit(new_sha).unwrap();
        let mut new_initial = InitialAttributions::default();
        new_initial
            .files
            .insert("shared.txt".into(), attr("ai_NEW"));
        new_initial
            .files
            .insert("new_only.txt".into(), attr("ai_NEW"));
        let new_shared_sha = new_log.persist_file_version("NEW CONTENT").unwrap();
        let new_only_sha = new_log.persist_file_version("NEW ONLY").unwrap();
        new_initial
            .file_blobs
            .insert("shared.txt".into(), new_shared_sha);
        new_initial
            .file_blobs
            .insert("new_only.txt".into(), new_only_sha);
        new_log.write_initial(new_initial).unwrap();

        // Merge old into new (destination already exists).
        storage.rename_working_log(old_sha, new_sha).unwrap();

        let merged = storage
            .working_log_for_base_commit(new_sha)
            .unwrap()
            .read_initial_attributions()
            .unwrap();

        // Shared key: OLD base wins.
        assert_eq!(
            merged
                .files
                .get("shared.txt")
                .map(|a| a[0].author_id.as_str()),
            Some("h_OLD"),
            "old-base attribution must win on a shared path"
        );
        assert_eq!(
            merged.file_blobs.get("shared.txt").map(|s| s.as_str()),
            Some(old_shared_sha.as_str()),
            "old-base blob must win on a shared path (kept consistent with files)"
        );
        // Both sides' unique entries survive.
        assert!(merged.files.contains_key("old_only.txt"));
        assert!(merged.files.contains_key("new_only.txt"));
    }

    #[test]
    fn test_rename_working_log_to_same_sha_preserves_log() {
        let tmp = TempDir::new().unwrap();
        let workdir = tmp.path().join("workdir");
        fs::create_dir_all(&workdir).unwrap();
        let ai_dir = tmp.path().join("ai");
        let storage = RepoStorage::for_repo_path(&ai_dir, &workdir).unwrap();
        let sha = "1111111111111111111111111111111111111111";

        let log = storage.working_log_for_base_commit(sha).unwrap();
        let mut initial = InitialAttributions::default();
        initial
            .files
            .insert("pending.txt".into(), attr("ai_PENDING"));
        log.write_initial(initial).unwrap();

        storage.rename_working_log(sha, sha).unwrap();

        let preserved = storage
            .working_log_for_base_commit(sha)
            .unwrap()
            .read_initial_attributions()
            .unwrap();
        assert_eq!(
            preserved
                .files
                .get("pending.txt")
                .map(|attrs| attrs[0].author_id.as_str()),
            Some("ai_PENDING")
        );
    }
}
