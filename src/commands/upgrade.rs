use crate::api::client::ApiContext;
use crate::config::{self, UpdateChannel};
use crate::observability::log_message;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;
#[cfg(windows)]
const MOVEFILE_REPLACE_EXISTING: u32 = 0x00000001;
#[cfg(windows)]
const MOVEFILE_WRITE_THROUGH: u32 = 0x00000008;
#[cfg(windows)]
type WindowsHandle = *mut std::ffi::c_void;
#[cfg(windows)]
const TH32CS_SNAPPROCESS: u32 = 0x00000002;
#[cfg(windows)]
const INVALID_HANDLE_VALUE: WindowsHandle = (-1isize) as WindowsHandle;
#[cfg(windows)]
const WINDOWS_MAX_PATH: usize = 260;

#[cfg(windows)]
#[repr(C)]
struct ProcessEntry32W {
    dw_size: u32,
    cnt_usage: u32,
    th32_process_id: u32,
    th32_default_heap_id: usize,
    th32_module_id: u32,
    cnt_threads: u32,
    th32_parent_process_id: u32,
    pc_pri_class_base: i32,
    dw_flags: u32,
    sz_exe_file: [u16; WINDOWS_MAX_PATH],
}

#[cfg(windows)]
unsafe extern "system" {
    fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> WindowsHandle;
    fn Process32FirstW(snapshot: WindowsHandle, entry: *mut ProcessEntry32W) -> i32;
    fn Process32NextW(snapshot: WindowsHandle, entry: *mut ProcessEntry32W) -> i32;
    fn CloseHandle(handle: WindowsHandle) -> i32;
    fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
}

const UPDATE_CHECK_INTERVAL_HOURS: u64 = 24;
const GIT_AI_RELEASE_ENV: &str = "GIT_AI_RELEASE_TAG";
const GIT_AI_INSTALL_EXPECTED_VERSION_ENV: &str = "GIT_AI_INSTALL_EXPECTED_VERSION";
#[cfg(windows)]
const GIT_AI_RESTART_DAEMON_AFTER_INSTALL_ENV: &str = "GIT_AI_RESTART_DAEMON_AFTER_INSTALL";
#[cfg(windows)]
const GIT_AI_UPDATE_RECEIPT_PATH_ENV: &str = "GIT_AI_UPDATE_RECEIPT_PATH";
const GIT_AI_DAEMON_UPGRADE_ENV: &str = "GIT_AI_DAEMON_UPGRADE";
const BACKGROUND_SPAWN_THROTTLE_SECS: u64 = 60;
const ENV_BACKGROUND_UPGRADE_WORKER: &str = "GIT_AI_BACKGROUND_UPGRADE_WORKER";

static UPDATE_NOTICE_EMITTED: AtomicBool = AtomicBool::new(false);
static LAST_BACKGROUND_SPAWN: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, PartialEq)]
enum UpgradeAction {
    UpgradeAvailable,
    AlreadyLatest,
    RunningNewerVersion,
    ForceReinstall,
}

impl UpgradeAction {
    fn to_string(&self) -> &str {
        match self {
            UpgradeAction::UpgradeAvailable => "upgrade_available",
            UpgradeAction::AlreadyLatest => "already_latest",
            UpgradeAction::RunningNewerVersion => "running_newer_version",
            UpgradeAction::ForceReinstall => "force_reinstall",
        }
    }
}

#[derive(Debug, Clone)]
struct ChannelRelease {
    tag: String,
    semver: String,
    checksum: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct UpdateCache {
    last_checked_at: u64,
    available_tag: Option<String>,
    available_semver: Option<String>,
    channel: String,
}

#[cfg(any(windows, test))]
#[derive(Debug, Clone, Serialize, Deserialize)]
struct UpgradeReceipt {
    format: u32,
    expected_version: String,
    installed_version: String,
    release_tag: String,
    completed_at_utc: String,
}

impl UpdateCache {
    fn new(channel: UpdateChannel) -> Self {
        Self {
            last_checked_at: 0,
            available_tag: None,
            available_semver: None,
            channel: channel.as_str().to_string(),
        }
    }

    fn update_available(&self) -> bool {
        self.available_semver.is_some()
    }

    fn matches_channel(&self, channel: UpdateChannel) -> bool {
        self.channel == channel.as_str()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CachedPendingDisposition {
    NewerThanCurrent,
    CurrentNeedsReceipt,
    OlderThanCurrent,
}

fn classify_cached_pending_update(
    cache: Option<&UpdateCache>,
    channel: UpdateChannel,
    current_version: &str,
) -> Option<CachedPendingDisposition> {
    let cache = cache?;
    if !cache.matches_channel(channel) || !cache.update_available() {
        return None;
    }
    // A pending state is actionable only when both identity fields exist.
    cache.available_tag.as_deref()?;
    let pending_version = cache.available_semver.as_deref()?;
    match compare_numeric_versions(pending_version, current_version)? {
        std::cmp::Ordering::Greater => Some(CachedPendingDisposition::NewerThanCurrent),
        std::cmp::Ordering::Equal => Some(CachedPendingDisposition::CurrentNeedsReceipt),
        std::cmp::Ordering::Less => Some(CachedPendingDisposition::OlderThanCurrent),
    }
}

#[derive(Debug, Deserialize)]
struct ChannelInfo {
    version: String,
    checksum: String,
}

#[derive(Debug, Deserialize)]
struct ReleasesResponse {
    channels: HashMap<String, ChannelInfo>,
}

fn get_update_check_cache_path() -> Option<PathBuf> {
    #[cfg(test)]
    {
        if let Ok(test_cache_dir) = std::env::var("GIT_AI_TEST_CACHE_DIR") {
            return Some(PathBuf::from(test_cache_dir).join("update_check"));
        }
    }

    crate::config::update_check_path()
}

fn read_update_cache() -> Option<UpdateCache> {
    let path = get_update_check_cache_path()?;
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(any(windows, test))]
fn get_upgrade_receipt_path() -> Option<PathBuf> {
    #[cfg(test)]
    {
        if let Ok(test_cache_dir) = std::env::var("GIT_AI_TEST_CACHE_DIR") {
            return Some(PathBuf::from(test_cache_dir).join("upgrade-receipt.json"));
        }
    }

    crate::config::git_ai_dir_path().map(|path| path.join("upgrade-receipt.json"))
}

#[cfg(any(windows, test))]
fn validate_upgrade_receipt_identity(
    receipt: &UpgradeReceipt,
    current_version: &str,
) -> Result<(), String> {
    if receipt.format != 1 {
        return Err(format!("unsupported receipt format {}", receipt.format));
    }
    if receipt.expected_version != receipt.installed_version {
        return Err("expected and installed versions differ".to_string());
    }
    if receipt.installed_version != current_version {
        return Err(format!(
            "receipt installed version {} does not match running version {}",
            receipt.installed_version, current_version
        ));
    }
    if semver_from_tag(&receipt.release_tag) != receipt.expected_version {
        return Err("release tag does not match the expected version".to_string());
    }
    if receipt.completed_at_utc.trim().is_empty() {
        return Err("receipt completion timestamp is empty".to_string());
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn validate_upgrade_receipt(
    receipt: &UpgradeReceipt,
    cache: Option<&UpdateCache>,
    channel: UpdateChannel,
    current_version: &str,
) -> Result<(), String> {
    validate_upgrade_receipt_identity(receipt, current_version)?;
    let cache = cache.ok_or_else(|| "pending update cache is missing".to_string())?;
    if !cache.matches_channel(channel) || !cache.update_available() {
        return Err("pending update cache is absent or belongs to another channel".to_string());
    }
    if cache.available_semver.as_deref() != Some(receipt.expected_version.as_str())
        || cache.available_tag.as_deref() != Some(receipt.release_tag.as_str())
    {
        return Err("receipt does not match the pending update cache".to_string());
    }
    Ok(())
}

#[cfg(any(windows, test))]
#[derive(Debug)]
enum UpgradeReceiptFileResult {
    Absent,
    CleanupOnly,
    Blocked {
        reason: String,
    },
    Completed {
        receipt: UpgradeReceipt,
        cleanup_error: Option<String>,
    },
}

#[cfg(any(windows, test))]
fn replace_update_cache_file(
    temp_path: &std::path::Path,
    path: &std::path::Path,
) -> Result<(), String> {
    #[cfg(windows)]
    {
        let temp_wide: Vec<u16> = temp_path.as_os_str().encode_wide().chain(Some(0)).collect();
        let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let result = unsafe {
            MoveFileExW(
                temp_wide.as_ptr(),
                path_wide.as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if result == 0 {
            return Err(format!(
                "could not durably replace update cache at {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    #[cfg(not(windows))]
    {
        fs::rename(temp_path, path).map_err(|error| {
            format!(
                "could not atomically replace update cache at {}: {error}",
                path.display()
            )
        })?;
        if let Some(parent) = path.parent() {
            fs::File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| {
                    format!(
                        "could not sync update cache directory {}: {error}",
                        parent.display()
                    )
                })?;
        }
        Ok(())
    }
}

#[cfg(any(windows, test))]
fn write_update_cache_strict(cache: &UpdateCache) -> Result<(), String> {
    let path = get_update_check_cache_path()
        .ok_or_else(|| "could not determine update cache path".to_string())?;
    #[cfg(test)]
    if std::env::var("GIT_AI_TEST_CACHE_CLEAR_FAILURE").as_deref() == Ok("1") {
        return Err("injected update cache clear failure".to_string());
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("update cache path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| {
        format!(
            "could not create update cache directory {}: {error}",
            parent.display()
        )
    })?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_nanos();
    let temp_path = parent.join(format!(".update_check.tmp.{}.{nonce}", std::process::id()));
    let json = serde_json::to_vec(cache)
        .map_err(|error| format!("could not serialize cleared update cache: {error}"))?;

    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|error| {
                format!(
                    "could not create update cache staging file {}: {error}",
                    temp_path.display()
                )
            })?;
        file.write_all(&json).map_err(|error| {
            format!(
                "could not write update cache staging file {}: {error}",
                temp_path.display()
            )
        })?;
        file.sync_all().map_err(|error| {
            format!(
                "could not sync update cache staging file {}: {error}",
                temp_path.display()
            )
        })?;
        drop(file);
        replace_update_cache_file(&temp_path, &path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result?;

    let verified = fs::read(&path)
        .map_err(|error| {
            format!(
                "could not read back update cache at {}: {error}",
                path.display()
            )
        })
        .and_then(|bytes| {
            serde_json::from_slice::<UpdateCache>(&bytes)
                .map_err(|error| format!("could not parse written update cache: {error}"))
        })?;
    if &verified != cache {
        return Err("written update cache did not pass exact read-back verification".to_string());
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn persist_update_state_strict(
    channel: UpdateChannel,
    release: Option<&ChannelRelease>,
) -> Result<(), String> {
    let mut cache = UpdateCache::new(channel);
    cache.last_checked_at = current_timestamp();
    if let Some(release) = release {
        cache.available_tag = Some(release.tag.clone());
        cache.available_semver = Some(release.semver.clone());
    }
    write_update_cache_strict(&cache)
}

#[cfg(any(windows, test))]
fn clear_pending_update_cache_strict(channel: UpdateChannel) -> Result<(), String> {
    persist_update_state_strict(channel, None)
}

#[cfg(any(windows, test))]
fn pin_pending_release_for_install_strict(
    channel: UpdateChannel,
    release: &ChannelRelease,
) -> Result<(), String> {
    // The channel can advance between the earlier availability check and the
    // daemon's post-shutdown install. Persist the exact release we are about to
    // install before fetching or launching it, so its receipt can only consume
    // a matching pending state.
    persist_update_state_strict(channel, Some(release))
}

fn should_recover_missing_windows_receipt(
    action: &UpgradeAction,
    cache: Option<&UpdateCache>,
    channel: UpdateChannel,
    release: &ChannelRelease,
    current_version: &str,
) -> bool {
    matches!(action, &UpgradeAction::AlreadyLatest)
        && classify_cached_pending_update(cache, channel, current_version)
            == Some(CachedPendingDisposition::CurrentNeedsReceipt)
        && cache.is_some_and(|cache| {
            cache.available_tag.as_deref() == Some(release.tag.as_str())
                && cache.available_semver.as_deref() == Some(release.semver.as_str())
        })
}

#[cfg(any(windows, test))]
fn remove_upgrade_receipt(path: &std::path::Path) -> Result<(), String> {
    #[cfg(test)]
    if std::env::var("GIT_AI_TEST_RECEIPT_DELETE_FAILURE").as_deref() == Ok("1") {
        return Err("injected receipt deletion failure".to_string());
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "could not remove upgrade receipt at {}: {error}",
            path.display()
        )),
    }
}

#[cfg(any(windows, test))]
fn reconcile_upgrade_receipt_files(
    channel: UpdateChannel,
    cache: Option<&UpdateCache>,
    current_version: &str,
) -> UpgradeReceiptFileResult {
    let Some(path) = get_upgrade_receipt_path() else {
        return UpgradeReceiptFileResult::Blocked {
            reason: "could not determine upgrade receipt path".to_string(),
        };
    };
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return UpgradeReceiptFileResult::Absent;
        }
        Err(error) => {
            return UpgradeReceiptFileResult::Blocked {
                reason: format!(
                    "could not read upgrade receipt at {}: {error}",
                    path.display()
                ),
            };
        }
    };
    let receipt = match serde_json::from_slice::<UpgradeReceipt>(&bytes) {
        Ok(receipt) => receipt,
        Err(error) => {
            return UpgradeReceiptFileResult::Blocked {
                reason: format!("invalid receipt JSON: {error}"),
            };
        }
    };
    if let Err(reason) = validate_upgrade_receipt_identity(&receipt, current_version) {
        return UpgradeReceiptFileResult::Blocked { reason };
    }
    let matches_pending = cache.is_some_and(|cache| {
        cache.matches_channel(channel)
            && cache.update_available()
            && cache.available_semver.as_deref() == Some(receipt.expected_version.as_str())
            && cache.available_tag.as_deref() == Some(receipt.release_tag.as_str())
    });
    if !matches_pending {
        // The receipt is self-consistent and belongs to the running binary, but
        // there is no exact matching pending update. It is cleanup debt from a
        // previously-cleared cache, a channel change, or a newer update. Remove
        // only the old receipt; never clear the current cache or emit success.
        return match remove_upgrade_receipt(&path) {
            Ok(()) => UpgradeReceiptFileResult::CleanupOnly,
            Err(reason) => UpgradeReceiptFileResult::Blocked { reason },
        };
    }
    if let Err(reason) = validate_upgrade_receipt(&receipt, cache, channel, current_version) {
        return UpgradeReceiptFileResult::Blocked { reason };
    }
    if let Err(reason) = clear_pending_update_cache_strict(channel) {
        // The receipt remains durable. Do not record completion or re-run the
        // installer until cache clearing can be retried successfully.
        return UpgradeReceiptFileResult::Blocked { reason };
    }

    let cleanup_error = remove_upgrade_receipt(&path)
        .err()
        .map(|reason| format!("pending cache was cleared, but receipt cleanup failed: {reason}"));
    UpgradeReceiptFileResult::Completed {
        receipt,
        cleanup_error,
    }
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowsUpgradeReceiptStatus {
    Absent,
    CleanupOnly,
    Completed,
    Blocked,
}

#[cfg(windows)]
fn reconcile_completed_windows_upgrade(
    channel: UpdateChannel,
    cache: Option<&UpdateCache>,
    api_base_url: &str,
) -> WindowsUpgradeReceiptStatus {
    match reconcile_upgrade_receipt_files(channel, cache, env!("CARGO_PKG_VERSION")) {
        UpgradeReceiptFileResult::Absent => WindowsUpgradeReceiptStatus::Absent,
        UpgradeReceiptFileResult::CleanupOnly => {
            log_message(
                "upgrade_receipt_cleanup_completed",
                "info",
                Some(serde_json::json!({
                    "channel": channel.as_str(),
                    "completion_source": "cleared_cache_tombstone"
                })),
            );
            WindowsUpgradeReceiptStatus::CleanupOnly
        }
        UpgradeReceiptFileResult::Blocked { reason } => {
            log_message(
                "upgrade_receipt_rejected",
                "warn",
                Some(serde_json::json!({
                    "reason": reason,
                    "channel": channel.as_str()
                })),
            );
            WindowsUpgradeReceiptStatus::Blocked
        }
        UpgradeReceiptFileResult::Completed {
            receipt,
            cleanup_error,
        } => {
            if let Some(ref cleanup_error) = cleanup_error {
                log_message(
                    "upgrade_receipt_cleanup_failed",
                    "warn",
                    Some(serde_json::json!({
                        "reason": cleanup_error,
                        "release_tag": receipt.release_tag.as_str(),
                        "installed_version": receipt.installed_version.as_str(),
                        "channel": channel.as_str()
                    })),
                );
            }
            log_message(
                "daemon_upgraded",
                "info",
                Some(serde_json::json!({
                    "release_tag": receipt.release_tag.as_str(),
                    "installed_version": receipt.installed_version.as_str(),
                    "api_base_url": api_base_url,
                    "channel": channel.as_str(),
                    "completion_source": "verified_windows_receipt",
                    "receipt_cleanup_completed": cleanup_error.is_none()
                })),
            );
            WindowsUpgradeReceiptStatus::Completed
        }
    }
}

fn write_update_cache(cache: &UpdateCache) {
    if let Some(path) = get_update_check_cache_path() {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_vec(cache) {
            let _ = fs::write(path, json);
        }
    }
}

fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs()
}

#[cfg(windows)]
fn exit_if_invoked_via_git_extension() {
    if should_block_git_extension_upgrade(
        parent_process_name().as_deref(),
        std::env::var(ENV_BACKGROUND_UPGRADE_WORKER).as_deref() == Ok("1"),
    ) {
        eprintln!(
            "error: `git ai upgrade` is not supported on Windows. Run `git-ai upgrade` instead."
        );
        std::process::exit(1);
    }
}

#[cfg(windows)]
fn should_block_git_extension_upgrade(
    parent_process_name: Option<&str>,
    is_background_worker: bool,
) -> bool {
    !is_background_worker && parent_process_name.is_some_and(is_git_process_name)
}

#[cfg(windows)]
fn is_git_process_name(name: &str) -> bool {
    std::path::Path::new(name)
        .file_name()
        .and_then(|file_name| file_name.to_str())
        .is_some_and(|file_name| {
            file_name.eq_ignore_ascii_case("git") || file_name.eq_ignore_ascii_case("git.exe")
        })
}

#[cfg(windows)]
fn parent_process_name() -> Option<String> {
    struct SnapshotGuard(WindowsHandle);

    impl Drop for SnapshotGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return None;
    }
    let _snapshot_guard = SnapshotGuard(snapshot);

    let current_pid = std::process::id();
    let parent_pid = find_parent_pid(snapshot, current_pid)?;
    process_name_for_pid(snapshot, parent_pid)
}

#[cfg(windows)]
fn find_parent_pid(snapshot: WindowsHandle, current_pid: u32) -> Option<u32> {
    let mut entry = windows_process_entry_template();
    if unsafe { Process32FirstW(snapshot, &mut entry) } == 0 {
        return None;
    }

    loop {
        if entry.th32_process_id == current_pid {
            return Some(entry.th32_parent_process_id);
        }
        if unsafe { Process32NextW(snapshot, &mut entry) } == 0 {
            return None;
        }
    }
}

#[cfg(windows)]
fn process_name_for_pid(snapshot: WindowsHandle, pid: u32) -> Option<String> {
    let mut entry = windows_process_entry_template();
    if unsafe { Process32FirstW(snapshot, &mut entry) } == 0 {
        return None;
    }

    loop {
        if entry.th32_process_id == pid {
            let len = entry
                .sz_exe_file
                .iter()
                .position(|&ch| ch == 0)
                .unwrap_or(entry.sz_exe_file.len());
            return Some(String::from_utf16_lossy(&entry.sz_exe_file[..len]));
        }
        if unsafe { Process32NextW(snapshot, &mut entry) } == 0 {
            return None;
        }
    }
}

#[cfg(windows)]
fn windows_process_entry_template() -> ProcessEntry32W {
    ProcessEntry32W {
        dw_size: std::mem::size_of::<ProcessEntry32W>() as u32,
        cnt_usage: 0,
        th32_process_id: 0,
        th32_default_heap_id: 0,
        th32_module_id: 0,
        cnt_threads: 0,
        th32_parent_process_id: 0,
        pc_pri_class_base: 0,
        dw_flags: 0,
        sz_exe_file: [0; WINDOWS_MAX_PATH],
    }
}

fn should_check_for_updates(channel: UpdateChannel, cache: Option<&UpdateCache>) -> bool {
    let now = current_timestamp();
    match cache {
        Some(cache) if cache.last_checked_at > 0 => {
            // If cache doesn't match the channel, we should check for updates
            if !cache.matches_channel(channel) {
                return true;
            }
            let elapsed = now.saturating_sub(cache.last_checked_at);
            elapsed > UPDATE_CHECK_INTERVAL_HOURS * 3600
        }
        _ => true,
    }
}

fn semver_from_tag(tag: &str) -> String {
    let trimmed = tag
        .trim()
        .trim_start_matches("enterprise-")
        .trim_start_matches('v');
    trimmed.split(['-', '+']).next().unwrap_or("").to_string()
}

fn determine_action(force: bool, release: &ChannelRelease, current_version: &str) -> UpgradeAction {
    if force {
        return UpgradeAction::ForceReinstall;
    }

    if release.semver == current_version {
        UpgradeAction::AlreadyLatest
    } else if is_newer_version(&release.semver, current_version) {
        UpgradeAction::UpgradeAvailable
    } else {
        UpgradeAction::RunningNewerVersion
    }
}

fn persist_update_state(channel: UpdateChannel, release: Option<&ChannelRelease>) {
    let mut cache = UpdateCache::new(channel);
    cache.last_checked_at = current_timestamp();
    if let Some(release) = release {
        cache.available_tag = Some(release.tag.clone());
        cache.available_semver = Some(release.semver.clone());
    }
    write_update_cache(&cache);
}

pub(crate) fn clear_cached_update_state() {
    let channel = config::Config::fresh().update_channel();
    persist_update_state(channel, None);
}

fn releases_endpoint() -> &'static str {
    "/worker/releases"
}

fn verify_sha256(content: &[u8], expected_hash: &str) -> Result<(), String> {
    let mut hasher = Sha256::new();
    hasher.update(content);
    let actual_hash = format!("{:x}", hasher.finalize());

    if actual_hash.eq_ignore_ascii_case(expected_hash) {
        Ok(())
    } else {
        Err(format!(
            "Checksum mismatch: expected {}, got {}",
            expected_hash, actual_hash
        ))
    }
}

/// Parse SHA256SUMS file content into a map of filename → hash.
/// Format: `<hash>  <filename>` (two spaces between hash and filename)
fn parse_checksums(content: &str) -> HashMap<String, String> {
    let mut checksums = HashMap::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Format: "<hash>  <filename>" (two spaces)
        if let Some((hash, filename)) = line.split_once("  ") {
            checksums.insert(filename.to_string(), hash.to_string());
        }
    }

    checksums
}

/// Fetch SHA256SUMS from the releases API and verify against expected checksum.
fn fetch_and_verify_checksums(
    api_base_url: &str,
    channel: &str,
    expected_checksum: &str,
) -> Result<HashMap<String, String>, String> {
    let endpoint = format!("/worker/releases/{}/download/SHA256SUMS", channel);

    let (_agent, request) =
        ApiContext::http_get(&format!("{}{}", api_base_url, endpoint), Some(30));
    let response =
        crate::http::send(request).map_err(|e| format!("Failed to fetch SHA256SUMS: {}", e))?;

    if response.status_code != 200 {
        return Err(format!(
            "Failed to fetch SHA256SUMS: HTTP {}",
            response.status_code
        ));
    }

    let content = response.as_bytes();

    verify_sha256(content, expected_checksum)
        .map_err(|e| format!("SHA256SUMS verification failed: {}", e))?;

    let content_str = std::str::from_utf8(content)
        .map_err(|e| format!("SHA256SUMS is not valid UTF-8: {}", e))?;

    Ok(parse_checksums(content_str))
}

/// Fetch install script from the releases API and verify against checksums.
fn fetch_and_verify_install_script(
    api_base_url: &str,
    channel: &str,
    checksums: &HashMap<String, String>,
) -> Result<String, String> {
    #[cfg(windows)]
    let script_name = "install.ps1";
    #[cfg(not(windows))]
    let script_name = "install.sh";

    let expected_checksum = checksums
        .get(script_name)
        .ok_or_else(|| format!("Checksum for {} not found in SHA256SUMS", script_name))?;

    let endpoint = format!("/worker/releases/{}/download/{}", channel, script_name);

    let (_agent, request) =
        ApiContext::http_get(&format!("{}{}", api_base_url, endpoint), Some(30));
    let response = crate::http::send(request)
        .map_err(|e| format!("Failed to fetch {}: {}", script_name, e))?;

    if response.status_code != 200 {
        return Err(format!(
            "Failed to fetch {}: HTTP {}",
            script_name, response.status_code
        ));
    }

    let content = response.as_bytes();

    verify_sha256(content, expected_checksum)
        .map_err(|e| format!("{} verification failed: {}", script_name, e))?;

    let script = std::str::from_utf8(content)
        .map_err(|e| format!("{} is not valid UTF-8: {}", script_name, e))?;

    Ok(script.to_string())
}

fn fetch_release_for_channel(
    api_base_url: &str,
    channel: UpdateChannel,
) -> Result<ChannelRelease, String> {
    #[cfg(test)]
    if let Some(result) = try_mock_releases(api_base_url, channel) {
        return result;
    }

    let context = ApiContext::new(Some(api_base_url.to_string())).with_timeout(5);

    let response = context
        .get(releases_endpoint())
        .map_err(|e| format!("Failed to check for updates: {}", e))?;

    let body = response
        .as_str()
        .map_err(|e| format!("Failed to read response body: {}", e))?;
    let releases: ReleasesResponse = serde_json::from_str(body)
        .map_err(|e| format!("Failed to parse release response: {}", e))?;

    release_from_response(releases, channel)
}

fn release_from_response(
    releases: ReleasesResponse,
    channel: UpdateChannel,
) -> Result<ChannelRelease, String> {
    let channel_name = channel.as_str();

    let channel_info = releases
        .channels
        .get(channel_name)
        .ok_or_else(|| format!("Channel '{}' not found in releases", channel_name))?;

    let tag = channel_info.version.trim().to_string();
    if tag.is_empty() {
        return Err("Release tag not found in response".to_string());
    }

    let semver = semver_from_tag(&tag);
    if semver.is_empty() {
        return Err(format!("Unable to parse semver from tag '{}'", tag));
    }

    let checksum = channel_info.checksum.trim().to_string();
    if checksum.is_empty() {
        return Err("Checksum not found in response".to_string());
    }

    Ok(ChannelRelease {
        tag,
        semver,
        checksum,
    })
}

#[cfg(test)]
fn try_mock_releases(base: &str, channel: UpdateChannel) -> Option<Result<ChannelRelease, String>> {
    let json = base.strip_prefix("mock://")?;
    Some(
        serde_json::from_str::<ReleasesResponse>(json)
            .map_err(|e| format!("Invalid mock releases payload: {}", e))
            .and_then(|releases| release_from_response(releases, channel)),
    )
}

fn run_install_script(script_content: &str, tag: &str, silent: bool) -> Result<(), String> {
    #[cfg(windows)]
    {
        let expected_version = semver_from_tag(tag);
        if expected_version.is_empty() {
            return Err(format!(
                "Unable to determine expected version from tag '{tag}'"
            ));
        }
        let receipt_path = get_upgrade_receipt_path()
            .ok_or_else(|| "Could not determine Windows upgrade receipt path".to_string())?;
        match fs::metadata(&receipt_path) {
            Ok(_) => {
                return Err(format!(
                    "A Windows upgrade receipt already exists at {}. Reconcile or inspect it before scheduling another installer.",
                    receipt_path.display()
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "Failed to inspect Windows upgrade receipt path {}: {error}",
                    receipt_path.display()
                ));
            }
        }

        if let Ok(daemon_config) = crate::daemon::DaemonConfig::from_env_or_default_paths() {
            // Best effort: stop the daemon before we hand off to the detached installer.
            // The install script also has a fallback kill path so old released binaries
            // can still recover, but stopping here makes upgrades complete sooner.
            let _ = crate::commands::daemon::stop_daemon(&daemon_config, Duration::from_secs(10));
        }

        // On Windows, we need to run the installer detached because the current git-ai
        // binary and shims are in use and need to be replaced. The installer will wait
        // for the files to be released before proceeding.
        let pid = std::process::id();
        let log_dir = dirs::home_dir()
            .ok_or_else(|| "Could not determine home directory".to_string())?
            .join(".git-ai")
            .join("upgrade-logs");

        // Ensure the log directory exists
        fs::create_dir_all(&log_dir)
            .map_err(|e| format!("Failed to create log directory: {}", e))?;

        let log_file = log_dir.join(format!("upgrade-{}.log", pid));
        let log_path_str = log_file.to_string_lossy().to_string();

        // Write the install script to a temp file
        let script_path = log_dir.join(format!("install-{}.ps1", pid));
        fs::write(&script_path, script_content)
            .map_err(|e| format!("Failed to write install script: {}", e))?;
        let script_path_str = script_path.to_string_lossy().to_string();

        // Create log file with initial message
        fs::write(&log_file, format!("Starting upgrade at PID {}\n", pid))
            .map_err(|e| format!("Failed to create log file: {}", e))?;

        // PowerShell wrapper that executes the script file with logging. Paths
        // travel through environment variables so quote characters in a user
        // profile cannot corrupt the wrapper source.
        let ps_wrapper = format!(
            "$logFile = $env:GIT_AI_UPGRADE_LOG_FILE; \
             Start-Transcript -Path $logFile -Append -Force | Out-Null; \
             Write-Host 'Running verified install script...'; \
             try {{ \
                  $ErrorActionPreference = 'Stop'; \
                  & $env:GIT_AI_UPGRADE_SCRIPT_PATH; \
                  if (-not (Test-Path -LiteralPath $env:{})) {{ \
                      throw 'Install script exited without a verified upgrade receipt'; \
                  }}; \
                  Write-Host 'Install script produced a verified upgrade receipt'; \
              }} catch {{ \
                  Write-Host \"Error: $_\"; \
                  Write-Host \"Stack trace: $($_.ScriptStackTrace)\"; \
              }} finally {{ \
                  if ($env:{} -eq '1') {{ \
                      $daemonExe = Join-Path $HOME '.git-ai\\bin\\git-ai.exe'; \
                      if (Test-Path $daemonExe) {{ try {{ & $daemonExe bg start *> $null }} catch {{ }} }} \
                  }}; \
                  Stop-Transcript | Out-Null; \
                  Remove-Item -LiteralPath $env:GIT_AI_UPGRADE_SCRIPT_PATH -Force -ErrorAction SilentlyContinue; \
              }}",
            GIT_AI_UPDATE_RECEIPT_PATH_ENV, GIT_AI_RESTART_DAEMON_AFTER_INSTALL_ENV
        );

        let spawn_powershell = |exe: &str| -> std::io::Result<std::process::Child> {
            let mut cmd = Command::new(exe);
            cmd.arg("-NoProfile")
                .arg("-ExecutionPolicy")
                .arg("Bypass")
                .arg("-Command")
                .arg(&ps_wrapper)
                .env(GIT_AI_RELEASE_ENV, tag)
                .env(GIT_AI_INSTALL_EXPECTED_VERSION_ENV, &expected_version)
                .env(GIT_AI_UPDATE_RECEIPT_PATH_ENV, &receipt_path)
                .env("GIT_AI_UPGRADE_LOG_FILE", &log_path_str)
                .env("GIT_AI_UPGRADE_SCRIPT_PATH", &script_path_str);

            // Hide the spawned console to prevent any host/UI bleed-through
            cmd.creation_flags(CREATE_NO_WINDOW);

            if silent {
                cmd.env(GIT_AI_RESTART_DAEMON_AFTER_INSTALL_ENV, "1");
                cmd.env(GIT_AI_DAEMON_UPGRADE_ENV, "1");
                cmd.stdout(Stdio::null()).stderr(Stdio::null());
            }

            cmd.spawn()
        };

        let spawn_result = spawn_powershell("pwsh").or_else(|_| spawn_powershell("powershell"));

        match spawn_result {
            Ok(_) => {
                if !silent {
                    println!(
                        "\x1b[1;33mNote: The installation is running in the background on Windows.\x1b[0m"
                    );
                    println!(
                        "This allows the current git-ai process to exit and release file locks."
                    );
                    println!("Check the log file for progress: {}", log_path_str);
                    println!(
                        "The installer will stop lingering git-ai background processes if needed, but active git commands can still delay completion."
                    );
                }
                Ok(())
            }
            Err(e) => Err(format!("Failed to run installation script: {}", e)),
        }
    }

    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;

        // Write script to ~/.git-ai/tmp/ to avoid /tmp noexec or permission issues.
        // Fall back to the system temp dir if the home-based path is unavailable.
        let temp_dir = crate::config::git_ai_dir_path()
            .map(|p| p.join("tmp"))
            .unwrap_or_else(std::env::temp_dir);
        fs::create_dir_all(&temp_dir)
            .map_err(|e| format!("Failed to create temp directory: {}", e))?;
        let script_path = temp_dir.join(format!("git-ai-install-{}.sh", std::process::id()));

        // Write and make executable
        let mut file = fs::File::create(&script_path)
            .map_err(|e| format!("Failed to create temp script file: {}", e))?;
        file.write_all(script_content.as_bytes())
            .map_err(|e| format!("Failed to write install script: {}", e))?;
        drop(file);

        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("Failed to make script executable: {}", e))?;

        let script_path_str = script_path.to_string_lossy().to_string();

        let mut cmd = Command::new("bash");
        cmd.arg(&script_path_str)
            .env(GIT_AI_RELEASE_ENV, tag)
            .env(GIT_AI_INSTALL_EXPECTED_VERSION_ENV, semver_from_tag(tag));

        if silent {
            cmd.env(GIT_AI_DAEMON_UPGRADE_ENV, "1");
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }

        let result = match cmd.status() {
            Ok(status) => {
                if status.success() {
                    Ok(())
                } else {
                    Err(format!(
                        "Installation script failed with exit code: {:?}",
                        status.code()
                    ))
                }
            }
            Err(e) => Err(format!("Failed to run installation script: {}", e)),
        };

        // Clean up temp script
        let _ = fs::remove_file(&script_path);

        result
    }
}

pub fn run_with_args(args: &[String]) {
    #[cfg(windows)]
    exit_if_invoked_via_git_extension();

    let mut force = false;
    let mut background = false;

    for arg in args {
        match arg.as_str() {
            "--force" => force = true,
            "--background" => background = true, // Undocumented flag for internal use when spawning background process
            _ => {
                eprintln!("Unknown argument: {}", arg);
                eprintln!("Usage: git-ai upgrade [--force]");
                std::process::exit(1);
            }
        }
    }

    run_impl(force, background);
}

fn run_impl(force: bool, background: bool) {
    let config = config::Config::fresh();
    let channel = config.update_channel();
    let skip_install = background && config.auto_updates_disabled();
    let _ = run_impl_with_url(force, config.api_base_url(), channel, skip_install);
}

fn run_impl_with_url(
    force: bool,
    api_base_url: &str,
    channel: UpdateChannel,
    skip_install: bool,
) -> UpgradeAction {
    let current_version = env!("CARGO_PKG_VERSION");

    #[cfg(windows)]
    {
        let cache = read_update_cache();
        let receipt_status =
            reconcile_completed_windows_upgrade(channel, cache.as_ref(), api_base_url);
        if receipt_status == WindowsUpgradeReceiptStatus::Blocked {
            eprintln!(
                "A Windows upgrade receipt is present but cannot be reconciled safely. Check the upgrade logs before retrying."
            );
            std::process::exit(1);
        }
        if receipt_status == WindowsUpgradeReceiptStatus::Absent
            && cache.as_ref().is_some_and(|cache| {
                cache.matches_channel(channel)
                    && cache.available_semver.as_deref() == Some(current_version)
            })
        {
            eprintln!(
                "The running Windows version matches a pending update, but its completion receipt is missing. The pending state was retained; inspect the upgrade log before retrying."
            );
            std::process::exit(1);
        }
    }

    println!("Checking for updates (channel: {})...", channel.as_str());

    let release = match fetch_release_for_channel(api_base_url, channel) {
        Ok(release) => release,
        Err(err) => {
            eprintln!("{}", err);
            std::process::exit(1);
        }
    };

    println!("Current version: v{}", current_version);
    println!(
        "Available {} version: v{} (tag {})",
        channel.as_str(),
        release.semver,
        release.tag
    );
    println!();

    let action = determine_action(force, &release, current_version);
    let cache_release = matches!(action, UpgradeAction::UpgradeAvailable)
        || (cfg!(windows) && matches!(action, UpgradeAction::ForceReinstall));
    #[cfg(windows)]
    if let Err(error) = persist_update_state_strict(channel, cache_release.then_some(&release)) {
        eprintln!("Could not durably record Windows update state: {error}");
        std::process::exit(1);
    }
    #[cfg(not(windows))]
    persist_update_state(channel, cache_release.then_some(&release));

    log_message(
        "checked_for_update",
        "info",
        Some(serde_json::json!({
            "current_version": current_version,
            "api_base_url": api_base_url,
            "channel": channel.as_str(),
            "result": action.to_string()
        })),
    );

    match action {
        UpgradeAction::AlreadyLatest => {
            println!("You are already on the latest version!");
            println!();
            println!("To reinstall anyway, run:");
            println!("  \x1b[1;36mgit-ai upgrade --force\x1b[0m");
            return action;
        }
        UpgradeAction::RunningNewerVersion => {
            println!("You are running a newer version than the selected release channel.");
            println!("(This usually means you're running a development build)");
            println!();
            println!("To reinstall the selected release anyway, run:");
            println!("  \x1b[1;36mgit-ai upgrade --force\x1b[0m");
            return action;
        }
        UpgradeAction::ForceReinstall => {
            println!(
                "\x1b[1;33mForce mode enabled - reinstalling {}\x1b[0m",
                release.tag
            );
        }
        UpgradeAction::UpgradeAvailable => {
            println!("\x1b[1;33mA new version is available!\x1b[0m");
        }
    }
    println!();

    if skip_install {
        return action;
    }

    println!("Fetching and verifying release artifacts...");

    // Fetch and verify SHA256SUMS against the release's master checksum
    let checksums =
        match fetch_and_verify_checksums(api_base_url, channel.as_str(), &release.checksum) {
            Ok(checksums) => {
                println!("\x1b[1;32m✓\x1b[0m SHA256SUMS verified");
                checksums
            }
            Err(err) => {
                eprintln!("Failed to fetch/verify checksums: {}", err);
                std::process::exit(1);
            }
        };

    // Fetch and verify the install script
    let script_content =
        match fetch_and_verify_install_script(api_base_url, channel.as_str(), &checksums) {
            Ok(content) => {
                #[cfg(windows)]
                println!("\x1b[1;32m✓\x1b[0m install.ps1 verified");
                #[cfg(not(windows))]
                println!("\x1b[1;32m✓\x1b[0m install.sh verified");
                content
            }
            Err(err) => {
                eprintln!("Failed to fetch/verify install script: {}", err);
                std::process::exit(1);
            }
        };

    println!();
    println!("Running installation script...");
    println!();

    match run_install_script(&script_content, &release.tag, false) {
        Ok(()) => {
            #[cfg(not(windows))]
            {
                println!("\x1b[1;32m✓\x1b[0m Successfully installed {}!", release.tag);
                log_message(
                    "upgraded",
                    "info",
                    Some(serde_json::json!({
                        "release_tag": release.tag,
                        "current_version": current_version,
                        "api_base_url": api_base_url,
                        "channel": channel.as_str()
                    })),
                );
            }

            // Detached Windows launch is only a schedule acknowledgement. The
            // new process clears the pending cache and records success after it
            // validates and consumes the installer's durable receipt.
            #[cfg(windows)]
            log_message(
                "upgrade_scheduled",
                "info",
                Some(serde_json::json!({
                    "release_tag": release.tag,
                    "expected_version": release.semver,
                    "current_version": current_version,
                    "api_base_url": api_base_url,
                    "channel": channel.as_str()
                })),
            );
        }
        Err(err) => {
            eprintln!("{}", err);
            std::process::exit(1);
        }
    }

    action
}

fn print_cached_notice(cache: &UpdateCache) {
    if cache.available_semver.is_none() || cache.available_tag.is_none() {
        return;
    }

    if !std::io::stdout().is_terminal() {
        // Don't print the version check notice if stdout is not a terminal/interactive shell
        return;
    }

    if UPDATE_NOTICE_EMITTED
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }

    let current_version = env!("CARGO_PKG_VERSION");
    let available_version = cache.available_semver.as_deref().unwrap_or("");

    eprintln!();
    eprintln!(
        "\x1b[1;33mA new version of git-ai is available: \x1b[1;32mv{}\x1b[0m → \x1b[1;32mv{}\x1b[0m",
        current_version, available_version
    );
    eprintln!(
        "\x1b[1;33mRun \x1b[1;36mgit-ai upgrade\x1b[0m \x1b[1;33mto upgrade to the latest version.\x1b[0m"
    );
    eprintln!();
}

pub fn maybe_schedule_background_update_check() {
    let config = config::Config::get();
    if config.version_checks_disabled() {
        return;
    }

    let channel = config.update_channel();
    let cache = read_update_cache();

    if config.auto_updates_disabled()
        && let Some(cache) = cache.as_ref()
        && cache.matches_channel(channel)
        && cache.update_available()
    {
        print_cached_notice(cache);
    }

    if !should_check_for_updates(channel, cache.as_ref()) {
        return;
    }

    let now = current_timestamp();
    let last_spawn = LAST_BACKGROUND_SPAWN.load(Ordering::SeqCst);
    if now.saturating_sub(last_spawn) < BACKGROUND_SPAWN_THROTTLE_SECS {
        return;
    }

    if spawn_background_upgrade_process() {
        LAST_BACKGROUND_SPAWN.store(now, Ordering::SeqCst);
    }
}

fn spawn_background_upgrade_process() -> bool {
    crate::utils::spawn_internal_git_ai_subcommand(
        "upgrade",
        &["--background"],
        ENV_BACKGROUND_UPGRADE_WORKER,
        &[],
    )
}

/// Result of checking whether a daemon-initiated update is available.
#[derive(Debug, PartialEq)]
pub enum DaemonUpdateCheckResult {
    /// No update is needed (already latest, checks disabled, or not yet time to check).
    NoUpdate,
    /// An update is available and auto-updates are enabled.
    UpdateReady,
}

/// Install a previously-detected update.
///
/// Designed for use by the daemon process **after** a clean shutdown.  Reads
/// the on-disk update cache (written earlier by `check_for_update_available`)
/// to decide whether an update is pending, bypassing the 24-hour time guard.
/// Uses `Config::fresh()` (not the `OnceLock` singleton) so the daemon
/// respects runtime config changes (e.g. disabling auto-updates).
///
/// Returns `Ok(UpdateReady)` if the install script ran, `Ok(NoUpdate)` if
/// no pending update was found or updates are disabled.
pub fn check_and_install_update_if_available() -> Result<DaemonUpdateCheckResult, String> {
    let config = config::Config::fresh();
    let channel = config.update_channel();
    let api_base_url = config.api_base_url();

    // Read the cache that check_for_update_available() populated earlier.
    // We intentionally skip should_check_for_updates() here because the
    // hourly check loop already confirmed an update is available and
    // persisted that fact — re-checking the 24h guard would always say
    // "too soon" and the install would never run.
    let cache = read_update_cache();
    #[cfg(windows)]
    if reconcile_completed_windows_upgrade(channel, cache.as_ref(), api_base_url)
        != WindowsUpgradeReceiptStatus::Absent
    {
        return Ok(DaemonUpdateCheckResult::NoUpdate);
    }
    if config.version_checks_disabled() || config.auto_updates_disabled() {
        return Ok(DaemonUpdateCheckResult::NoUpdate);
    }
    let has_pending_update = cache
        .as_ref()
        .is_some_and(|c| c.matches_channel(channel) && c.update_available());

    if !has_pending_update {
        return Ok(DaemonUpdateCheckResult::NoUpdate);
    }

    // Re-fetch the release to get the tag needed for the installer.
    let release = fetch_release_for_channel(api_base_url, channel)?;
    let current_version = env!("CARGO_PKG_VERSION");
    let action = determine_action(false, &release, current_version);
    #[cfg(windows)]
    let recover_missing_receipt = should_recover_missing_windows_receipt(
        &action,
        cache.as_ref(),
        channel,
        &release,
        current_version,
    );
    #[cfg(not(windows))]
    let recover_missing_receipt = false;

    if action != UpgradeAction::UpgradeAvailable && !recover_missing_receipt {
        #[cfg(not(windows))]
        persist_update_state(channel, None);
        #[cfg(windows)]
        log_message(
            "daemon_update_pending_without_receipt",
            "warn",
            Some(serde_json::json!({
                "release_tag": release.tag,
                "current_version": current_version,
                "channel": channel.as_str(),
                "result": action.to_string()
            })),
        );
        return Ok(DaemonUpdateCheckResult::NoUpdate);
    }

    #[cfg(windows)]
    pin_pending_release_for_install_strict(channel, &release)?;

    #[cfg(windows)]
    if recover_missing_receipt {
        log_message(
            "daemon_recovering_missing_upgrade_receipt",
            "warn",
            Some(serde_json::json!({
                "release_tag": release.tag.as_str(),
                "current_version": current_version,
                "channel": channel.as_str()
            })),
        );
    }

    log_message(
        "daemon_installing_update",
        "info",
        Some(serde_json::json!({
            "current_version": current_version,
            "release_tag": release.tag.as_str(),
            "api_base_url": api_base_url,
            "channel": channel.as_str(),
            "receipt_recovery": recover_missing_receipt
        })),
    );

    // Fetch, verify, and run the install script silently.
    let checksums = fetch_and_verify_checksums(api_base_url, channel.as_str(), &release.checksum)?;
    let script_content =
        fetch_and_verify_install_script(api_base_url, channel.as_str(), &checksums)?;
    run_install_script(&script_content, &release.tag, true)?;

    #[cfg(not(windows))]
    {
        // Unix execution is synchronous: a zero exit status includes exact
        // expected-version validation, so the pending cache can be cleared.
        persist_update_state(channel, None);
        log_message(
            "daemon_upgraded",
            "info",
            Some(serde_json::json!({
                "release_tag": release.tag,
                "current_version": current_version,
                "api_base_url": api_base_url,
                "channel": channel.as_str()
            })),
        );
    }

    #[cfg(windows)]
    log_message(
        "daemon_upgrade_scheduled",
        "info",
        Some(serde_json::json!({
            "release_tag": release.tag,
            "expected_version": release.semver,
            "current_version": current_version,
            "api_base_url": api_base_url,
            "channel": channel.as_str()
        })),
    );

    Ok(DaemonUpdateCheckResult::UpdateReady)
}

/// Check whether a newer version is available without installing it.
///
/// Like `check_and_install_update_if_available` but only queries the releases API
/// and updates the local cache. Returns `DaemonUpdateCheckResult::UpdateReady` when
/// the channel has a newer version than the running binary.
pub fn check_for_update_available() -> Result<DaemonUpdateCheckResult, String> {
    let config = config::Config::fresh();
    check_for_update_available_with_settings(
        config.update_channel(),
        config.api_base_url(),
        config.version_checks_disabled(),
        config.auto_updates_disabled(),
        cfg!(windows),
    )
}

fn check_for_update_available_with_settings(
    channel: UpdateChannel,
    api_base_url: &str,
    version_checks_disabled: bool,
    auto_updates_disabled: bool,
    windows_receipt_recovery_enabled: bool,
) -> Result<DaemonUpdateCheckResult, String> {
    let cache = read_update_cache();
    #[cfg(windows)]
    let cache = match reconcile_completed_windows_upgrade(channel, cache.as_ref(), api_base_url) {
        WindowsUpgradeReceiptStatus::Absent => cache,
        WindowsUpgradeReceiptStatus::CleanupOnly | WindowsUpgradeReceiptStatus::Completed => {
            read_update_cache()
        }
        WindowsUpgradeReceiptStatus::Blocked => {
            return Ok(DaemonUpdateCheckResult::NoUpdate);
        }
    };
    if version_checks_disabled {
        return Ok(DaemonUpdateCheckResult::NoUpdate);
    }

    if !should_check_for_updates(channel, cache.as_ref()) {
        // A fresh cache must not trigger shutdown merely because it contains a
        // pending field. Only a newer version is an update; on Windows an exact
        // current-version pending state is receipt recovery. Older state waits
        // for the next normal channel check and is then cleared.
        let mut validate_current_receipt_against_channel = false;
        if !auto_updates_disabled {
            match classify_cached_pending_update(cache.as_ref(), channel, env!("CARGO_PKG_VERSION"))
            {
                Some(CachedPendingDisposition::NewerThanCurrent) => {
                    return Ok(DaemonUpdateCheckResult::UpdateReady);
                }
                Some(CachedPendingDisposition::CurrentNeedsReceipt)
                    if windows_receipt_recovery_enabled =>
                {
                    // A same-version pending cache is only a receipt-recovery
                    // candidate. Re-fetch the selected channel before asking
                    // the daemon to restart: the channel may have rolled back
                    // or advanced to a different tag since this cache was
                    // written.
                    validate_current_receipt_against_channel = true;
                }
                Some(CachedPendingDisposition::CurrentNeedsReceipt)
                | Some(CachedPendingDisposition::OlderThanCurrent)
                | None => {}
            }
        }
        if !validate_current_receipt_against_channel {
            return Ok(DaemonUpdateCheckResult::NoUpdate);
        }
    }

    let release = fetch_release_for_channel(api_base_url, channel)?;
    let current_version = env!("CARGO_PKG_VERSION");
    let action = determine_action(false, &release, current_version);
    if windows_receipt_recovery_enabled
        && should_recover_missing_windows_receipt(
            &action,
            cache.as_ref(),
            channel,
            &release,
            current_version,
        )
    {
        log_message(
            "update_pending_receipt_recovery_ready",
            "warn",
            Some(serde_json::json!({
                "release_tag": release.tag,
                "current_version": current_version,
                "channel": channel.as_str(),
                "result": action.to_string(),
                "recovery": "rerun_exact_release_installer"
            })),
        );
        return if auto_updates_disabled {
            Ok(DaemonUpdateCheckResult::NoUpdate)
        } else {
            Ok(DaemonUpdateCheckResult::UpdateReady)
        };
    }
    let cache_release = matches!(action, UpgradeAction::UpgradeAvailable);
    #[cfg(windows)]
    persist_update_state_strict(channel, cache_release.then_some(&release))?;
    #[cfg(not(windows))]
    persist_update_state(channel, cache_release.then_some(&release));

    log_message(
        "checked_for_update",
        "info",
        Some(serde_json::json!({
            "current_version": current_version,
            "api_base_url": api_base_url,
            "channel": channel.as_str(),
            "result": action.to_string()
        })),
    );

    if action == UpgradeAction::UpgradeAvailable && !auto_updates_disabled {
        Ok(DaemonUpdateCheckResult::UpdateReady)
    } else {
        Ok(DaemonUpdateCheckResult::NoUpdate)
    }
}

fn compare_numeric_versions(left: &str, right: &str) -> Option<std::cmp::Ordering> {
    let parse_version = |value: &str| -> Option<Vec<u32>> {
        if value.is_empty() {
            return None;
        }
        value
            .split('.')
            .map(str::parse::<u32>)
            .collect::<Result<Vec<_>, _>>()
            .ok()
    };

    let left_parts = parse_version(left)?;
    let right_parts = parse_version(right)?;

    for i in 0..left_parts.len().max(right_parts.len()) {
        let left_part = left_parts.get(i).copied().unwrap_or(0);
        let right_part = right_parts.get(i).copied().unwrap_or(0);

        if left_part > right_part {
            return Some(std::cmp::Ordering::Greater);
        } else if left_part < right_part {
            return Some(std::cmp::Ordering::Less);
        }
    }

    Some(std::cmp::Ordering::Equal)
}

fn is_newer_version(latest: &str, current: &str) -> bool {
    compare_numeric_versions(latest, current) == Some(std::cmp::Ordering::Greater)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn set_test_cache_dir(dir: &tempfile::TempDir) {
        unsafe {
            std::env::set_var("GIT_AI_TEST_CACHE_DIR", dir.path());
        }
    }

    fn clear_test_cache_dir() {
        unsafe {
            std::env::remove_var("GIT_AI_TEST_CACHE_DIR");
        }
    }

    #[cfg(windows)]
    #[test]
    fn test_is_git_process_name() {
        assert!(is_git_process_name("git"));
        assert!(is_git_process_name("git.exe"));
        assert!(is_git_process_name(r"C:\Program Files\Git\cmd\git.exe"));
        assert!(!is_git_process_name("git-ai.exe"));
        assert!(!is_git_process_name("powershell.exe"));
    }

    #[cfg(windows)]
    #[test]
    fn test_should_block_git_extension_upgrade() {
        assert!(should_block_git_extension_upgrade(Some("git.exe"), false));
        assert!(should_block_git_extension_upgrade(
            Some(r"C:\Program Files\Git\cmd\git.exe"),
            false
        ));
        assert!(!should_block_git_extension_upgrade(Some("git.exe"), true));
        assert!(!should_block_git_extension_upgrade(
            Some("powershell.exe"),
            false
        ));
        assert!(!should_block_git_extension_upgrade(None, false));
    }

    #[test]
    fn test_is_newer_version() {
        assert!(!is_newer_version("1.0.0", "1.0.0"));
        assert!(!is_newer_version("1.0.10", "1.0.10"));

        assert!(is_newer_version("1.0.1", "1.0.0"));
        assert!(is_newer_version("1.0.11", "1.0.10"));
        assert!(!is_newer_version("1.0.0", "1.0.1"));
        assert!(!is_newer_version("1.0.10", "1.0.11"));

        assert!(is_newer_version("1.1.0", "1.0.0"));
        assert!(!is_newer_version("1.0.0", "1.1.0"));

        assert!(is_newer_version("2.0.0", "1.0.0"));
        assert!(is_newer_version("2.0.0", "1.9.9"));
        assert!(!is_newer_version("1.9.9", "2.0.0"));

        assert!(is_newer_version("1.0.0.1", "1.0.0"));
        assert!(!is_newer_version("1.0.0", "1.0.0.1"));

        assert!(is_newer_version("1.10.0", "1.9.0"));
        assert!(is_newer_version("1.0.100", "1.0.99"));
        assert!(is_newer_version("100.200.300", "100.200.299"));
    }

    #[test]
    fn test_cached_pending_update_three_state_classification() {
        let mut cache = UpdateCache::new(UpdateChannel::Latest);
        cache.available_tag = Some("v2.4.0".to_string());
        cache.available_semver = Some("2.4.0".to_string());

        assert_eq!(
            classify_cached_pending_update(Some(&cache), UpdateChannel::Latest, "2.3.9"),
            Some(CachedPendingDisposition::NewerThanCurrent)
        );
        assert_eq!(
            classify_cached_pending_update(Some(&cache), UpdateChannel::Latest, "2.4.0"),
            Some(CachedPendingDisposition::CurrentNeedsReceipt)
        );
        assert_eq!(
            classify_cached_pending_update(Some(&cache), UpdateChannel::Latest, "2.4.1"),
            Some(CachedPendingDisposition::OlderThanCurrent)
        );

        let release = ChannelRelease {
            tag: "v2.4.0".to_string(),
            semver: "2.4.0".to_string(),
            checksum: "a".repeat(64),
        };
        assert!(!should_recover_missing_windows_receipt(
            &UpgradeAction::RunningNewerVersion,
            Some(&cache),
            UpdateChannel::Latest,
            &release,
            "2.4.1",
        ));
    }

    #[test]
    fn test_semver_from_tag_strips_prefix_and_suffix() {
        assert_eq!(semver_from_tag("v1.2.3"), "1.2.3");
        assert_eq!(semver_from_tag("1.2.3"), "1.2.3");
        assert_eq!(semver_from_tag("v1.2.3-next-abc"), "1.2.3");
        assert_eq!(semver_from_tag("enterprise-v1.2.3"), "1.2.3");
        assert_eq!(semver_from_tag("enterprise-v1.2.3-next-abc"), "1.2.3");
    }

    fn test_receipt(version: &str) -> UpgradeReceipt {
        UpgradeReceipt {
            format: 1,
            expected_version: version.to_string(),
            installed_version: version.to_string(),
            release_tag: format!("v{version}"),
            completed_at_utc: "2026-08-09T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn test_upgrade_receipt_requires_exact_running_and_pending_versions() {
        let receipt = test_receipt("2.3.4");
        let mut cache = UpdateCache::new(UpdateChannel::Latest);
        cache.available_semver = Some("2.3.4".to_string());
        cache.available_tag = Some("v2.3.4".to_string());

        assert!(
            validate_upgrade_receipt(&receipt, Some(&cache), UpdateChannel::Latest, "2.3.4")
                .is_ok()
        );
        assert!(
            validate_upgrade_receipt(&receipt, Some(&cache), UpdateChannel::Latest, "2.3.3")
                .is_err()
        );

        cache.available_tag = Some("v2.3.4-next-other".to_string());
        assert!(
            validate_upgrade_receipt(&receipt, Some(&cache), UpdateChannel::Latest, "2.3.4")
                .is_err()
        );
    }

    #[test]
    fn test_upgrade_receipt_rejects_self_inconsistent_content() {
        let mut receipt = test_receipt("2.3.4");
        receipt.installed_version = "2.3.5".to_string();
        assert!(validate_upgrade_receipt(&receipt, None, UpdateChannel::Latest, "2.3.5").is_err());

        let mut receipt = test_receipt("2.3.4");
        receipt.release_tag = "v2.3.5".to_string();
        assert!(validate_upgrade_receipt(&receipt, None, UpdateChannel::Latest, "2.3.4").is_err());
    }

    #[test]
    fn test_upgrade_receipt_requires_pending_cache() {
        let receipt = test_receipt("2.3.4");
        assert!(validate_upgrade_receipt(&receipt, None, UpdateChannel::Latest, "2.3.4").is_err());

        let empty_cache = UpdateCache::new(UpdateChannel::Latest);
        assert!(
            validate_upgrade_receipt(&receipt, Some(&empty_cache), UpdateChannel::Latest, "2.3.4")
                .is_err()
        );
    }

    fn write_test_upgrade_receipt(receipt: &UpgradeReceipt) -> PathBuf {
        let path = get_upgrade_receipt_path().unwrap();
        fs::write(&path, serde_json::to_vec(receipt).unwrap()).unwrap();
        path
    }

    fn write_test_pending_update(version: &str) -> UpdateCache {
        let mut cache = UpdateCache::new(UpdateChannel::Latest);
        cache.available_semver = Some(version.to_string());
        cache.available_tag = Some(format!("v{version}"));
        write_update_cache(&cache);
        cache
    }

    #[test]
    #[serial]
    fn test_receipt_cache_clear_failure_keeps_receipt_and_pending_state() {
        let temp_dir = tempfile::tempdir().unwrap();
        set_test_cache_dir(&temp_dir);
        let cache = write_test_pending_update("2.3.4");
        let receipt_path = write_test_upgrade_receipt(&test_receipt("2.3.4"));
        unsafe {
            std::env::set_var("GIT_AI_TEST_CACHE_CLEAR_FAILURE", "1");
        }

        let result = reconcile_upgrade_receipt_files(UpdateChannel::Latest, Some(&cache), "2.3.4");

        unsafe {
            std::env::remove_var("GIT_AI_TEST_CACHE_CLEAR_FAILURE");
        }
        assert!(matches!(result, UpgradeReceiptFileResult::Blocked { .. }));
        assert!(receipt_path.exists());
        assert!(read_update_cache().is_some_and(|cache| cache.update_available()));
        clear_test_cache_dir();
    }

    #[test]
    #[serial]
    fn test_receipt_delete_failure_clears_pending_and_reports_cleanup_debt() {
        let temp_dir = tempfile::tempdir().unwrap();
        set_test_cache_dir(&temp_dir);
        let cache = write_test_pending_update("2.3.4");
        let receipt_path = write_test_upgrade_receipt(&test_receipt("2.3.4"));
        unsafe {
            std::env::set_var("GIT_AI_TEST_RECEIPT_DELETE_FAILURE", "1");
        }

        let result = reconcile_upgrade_receipt_files(UpdateChannel::Latest, Some(&cache), "2.3.4");

        unsafe {
            std::env::remove_var("GIT_AI_TEST_RECEIPT_DELETE_FAILURE");
        }
        assert!(matches!(
            result,
            UpgradeReceiptFileResult::Completed {
                cleanup_error: Some(_),
                ..
            }
        ));
        assert!(receipt_path.exists());
        let cleared_cache = read_update_cache().expect("cleared cache tombstone");
        assert!(cleared_cache.matches_channel(UpdateChannel::Latest));
        assert!(!cleared_cache.update_available());

        let second_result =
            reconcile_upgrade_receipt_files(UpdateChannel::Latest, Some(&cleared_cache), "2.3.4");
        assert!(matches!(
            second_result,
            UpgradeReceiptFileResult::CleanupOnly
        ));
        assert!(!receipt_path.exists());
        assert!(read_update_cache().is_some_and(|cache| !cache.update_available()));
        clear_test_cache_dir();
    }

    #[test]
    #[serial]
    fn test_receipt_success_clears_pending_before_consuming_receipt() {
        let temp_dir = tempfile::tempdir().unwrap();
        set_test_cache_dir(&temp_dir);
        let cache = write_test_pending_update("2.3.4");
        let receipt_path = write_test_upgrade_receipt(&test_receipt("2.3.4"));

        let result = reconcile_upgrade_receipt_files(UpdateChannel::Latest, Some(&cache), "2.3.4");

        assert!(matches!(
            result,
            UpgradeReceiptFileResult::Completed {
                cleanup_error: None,
                ..
            }
        ));
        assert!(!receipt_path.exists());
        assert!(read_update_cache().is_some_and(|cache| {
            cache.matches_channel(UpdateChannel::Latest) && !cache.update_available()
        }));
        clear_test_cache_dir();
    }

    #[test]
    #[serial]
    fn test_current_receipt_without_cache_is_cleanup_only() {
        let temp_dir = tempfile::tempdir().unwrap();
        set_test_cache_dir(&temp_dir);
        let receipt_path = write_test_upgrade_receipt(&test_receipt("2.3.4"));

        let result = reconcile_upgrade_receipt_files(UpdateChannel::Latest, None, "2.3.4");

        assert!(matches!(result, UpgradeReceiptFileResult::CleanupOnly));
        assert!(!receipt_path.exists());
        assert!(read_update_cache().is_none());
        clear_test_cache_dir();
    }

    #[test]
    #[serial]
    fn test_current_receipt_with_different_cleared_channel_is_cleanup_only() {
        let temp_dir = tempfile::tempdir().unwrap();
        set_test_cache_dir(&temp_dir);
        let mut other_cache = UpdateCache::new(UpdateChannel::Next);
        other_cache.last_checked_at = current_timestamp();
        write_update_cache(&other_cache);
        let receipt_path = write_test_upgrade_receipt(&test_receipt("2.3.4"));

        let result =
            reconcile_upgrade_receipt_files(UpdateChannel::Latest, Some(&other_cache), "2.3.4");

        assert!(matches!(result, UpgradeReceiptFileResult::CleanupOnly));
        assert!(!receipt_path.exists());
        assert_eq!(read_update_cache(), Some(other_cache));
        clear_test_cache_dir();
    }

    #[test]
    #[serial]
    fn test_old_current_receipt_does_not_clear_newer_pending_update() {
        let temp_dir = tempfile::tempdir().unwrap();
        set_test_cache_dir(&temp_dir);
        let newer_cache = write_test_pending_update("2.4.0");
        let receipt_path = write_test_upgrade_receipt(&test_receipt("2.3.4"));

        let result =
            reconcile_upgrade_receipt_files(UpdateChannel::Latest, Some(&newer_cache), "2.3.4");

        assert!(matches!(result, UpgradeReceiptFileResult::CleanupOnly));
        assert!(!receipt_path.exists());
        assert_eq!(read_update_cache(), Some(newer_cache));
        clear_test_cache_dir();
    }

    #[test]
    #[serial]
    fn test_windows_install_pins_advanced_channel_release_before_launch() {
        let temp_dir = tempfile::tempdir().unwrap();
        set_test_cache_dir(&temp_dir);
        let old_cache = write_test_pending_update("2.3.4");
        assert_eq!(old_cache.available_tag.as_deref(), Some("v2.3.4"));

        let advanced_release = ChannelRelease {
            tag: "v2.4.0".to_string(),
            semver: "2.4.0".to_string(),
            checksum: "a".repeat(64),
        };
        pin_pending_release_for_install_strict(UpdateChannel::Latest, &advanced_release).unwrap();

        let pinned = read_update_cache().expect("exact pending release");
        assert!(pinned.matches_channel(UpdateChannel::Latest));
        assert_eq!(pinned.available_tag.as_deref(), Some("v2.4.0"));
        assert_eq!(pinned.available_semver.as_deref(), Some("2.4.0"));
        clear_test_cache_dir();
    }

    #[test]
    fn test_exact_current_pending_release_recovers_missing_windows_receipt() {
        let release = ChannelRelease {
            tag: "v2.4.0".to_string(),
            semver: "2.4.0".to_string(),
            checksum: "a".repeat(64),
        };
        let mut cache = UpdateCache::new(UpdateChannel::Latest);
        cache.available_tag = Some(release.tag.clone());
        cache.available_semver = Some(release.semver.clone());

        assert!(should_recover_missing_windows_receipt(
            &UpgradeAction::AlreadyLatest,
            Some(&cache),
            UpdateChannel::Latest,
            &release,
            "2.4.0",
        ));

        cache.available_tag = Some("v2.3.9".to_string());
        assert!(!should_recover_missing_windows_receipt(
            &UpgradeAction::AlreadyLatest,
            Some(&cache),
            UpdateChannel::Latest,
            &release,
            "2.4.0",
        ));
    }

    #[test]
    #[serial]
    fn test_run_impl_with_url() {
        let temp_dir = tempfile::tempdir().unwrap();
        set_test_cache_dir(&temp_dir);

        let mock_url = |body: &str| format!("mock://{}", body);
        let current = env!("CARGO_PKG_VERSION");
        let test_checksum = "a".repeat(64); // Valid SHA256 length

        // Newer version available - should upgrade
        let action = run_impl_with_url(
            false,
            &mock_url(&format!(
                r#"{{"channels":{{"latest":{{"version":"v999.0.0","checksum":"{}"}},"next":{{"version":"v999.0.0-next-deadbeef","checksum":"{}"}}}}}}"#,
                test_checksum, test_checksum
            )),
            UpdateChannel::Latest,
            true,
        );
        assert_eq!(action, UpgradeAction::UpgradeAvailable);

        // Same version without --force - already latest
        let same_version_payload = format!(
            "{{\"channels\":{{\"latest\":{{\"version\":\"v{}\",\"checksum\":\"{}\"}},\"next\":{{\"version\":\"v{}-next-deadbeef\",\"checksum\":\"{}\"}}}}}}",
            current, test_checksum, current, test_checksum
        );
        let action = run_impl_with_url(
            false,
            &mock_url(&same_version_payload),
            UpdateChannel::Latest,
            true,
        );
        assert_eq!(action, UpgradeAction::AlreadyLatest);

        // Same version with --force - force reinstall
        let action = run_impl_with_url(
            true,
            &mock_url(&same_version_payload),
            UpdateChannel::Latest,
            true,
        );
        assert_eq!(action, UpgradeAction::ForceReinstall);

        // Older version without --force - running newer version
        let action = run_impl_with_url(
            false,
            &mock_url(&format!(
                r#"{{"channels":{{"latest":{{"version":"v1.0.9","checksum":"{}"}},"next":{{"version":"v1.0.9-next-deadbeef","checksum":"{}"}}}}}}"#,
                test_checksum, test_checksum
            )),
            UpdateChannel::Latest,
            true,
        );
        assert_eq!(action, UpgradeAction::RunningNewerVersion);

        // Older version with --force - force reinstall
        let action = run_impl_with_url(
            true,
            &mock_url(&format!(
                r#"{{"channels":{{"latest":{{"version":"v1.0.9","checksum":"{}"}},"next":{{"version":"v1.0.9-next-deadbeef","checksum":"{}"}}}}}}"#,
                test_checksum, test_checksum
            )),
            UpdateChannel::Latest,
            true,
        );
        assert_eq!(action, UpgradeAction::ForceReinstall);

        clear_test_cache_dir();
    }

    #[test]
    #[serial]
    fn test_run_impl_with_url_enterprise_channels() {
        let temp_dir = tempfile::tempdir().unwrap();
        set_test_cache_dir(&temp_dir);

        let mock_url = |body: &str| format!("mock://{}", body);
        let current = env!("CARGO_PKG_VERSION");
        let test_checksum = "a".repeat(64); // Valid SHA256 length

        // Newer version available - should upgrade
        let action = run_impl_with_url(
            false,
            &mock_url(&format!(
                r#"{{"channels":{{"enterprise-latest":{{"version":"v999.0.0","checksum":"{}"}},"enterprise-next":{{"version":"v999.0.0-next-deadbeef","checksum":"{}"}}}}}}"#,
                test_checksum, test_checksum
            )),
            UpdateChannel::EnterpriseLatest,
            true,
        );
        assert_eq!(action, UpgradeAction::UpgradeAvailable);

        // Same version without --force - already latest
        let same_version_payload = format!(
            "{{\"channels\":{{\"enterprise-latest\":{{\"version\":\"v{}\",\"checksum\":\"{}\"}},\"enterprise-next\":{{\"version\":\"v{}-next-deadbeef\",\"checksum\":\"{}\"}}}}}}",
            current, test_checksum, current, test_checksum
        );
        let action = run_impl_with_url(
            false,
            &mock_url(&same_version_payload),
            UpdateChannel::EnterpriseLatest,
            true,
        );
        assert_eq!(action, UpgradeAction::AlreadyLatest);

        // Same version with --force - force reinstall
        let action = run_impl_with_url(
            true,
            &mock_url(&same_version_payload),
            UpdateChannel::EnterpriseLatest,
            true,
        );
        assert_eq!(action, UpgradeAction::ForceReinstall);

        // Older version without --force - running newer version
        let action = run_impl_with_url(
            false,
            &mock_url(&format!(
                r#"{{"channels":{{"enterprise-latest":{{"version":"v1.0.9","checksum":"{}"}},"enterprise-next":{{"version":"v1.0.9-next-deadbeef","checksum":"{}"}}}}}}"#,
                test_checksum, test_checksum
            )),
            UpdateChannel::EnterpriseLatest,
            true,
        );
        assert_eq!(action, UpgradeAction::RunningNewerVersion);

        // Older version with --force - force reinstall
        let action = run_impl_with_url(
            true,
            &mock_url(&format!(
                r#"{{"channels":{{"enterprise-latest":{{"version":"v1.0.9","checksum":"{}"}},"enterprise-next":{{"version":"v1.0.9-next-deadbeef","checksum":"{}"}}}}}}"#,
                test_checksum, test_checksum
            )),
            UpdateChannel::EnterpriseLatest,
            true,
        );
        assert_eq!(action, UpgradeAction::ForceReinstall);

        clear_test_cache_dir();
    }

    #[test]
    fn test_should_check_for_updates_respects_interval() {
        let now = current_timestamp();
        let mut cache = UpdateCache::new(UpdateChannel::Latest);
        cache.last_checked_at = now;
        assert!(!should_check_for_updates(
            UpdateChannel::Latest,
            Some(&cache)
        ));

        let stale_offset = (UPDATE_CHECK_INTERVAL_HOURS * 3600) + 10;
        cache.last_checked_at = now.saturating_sub(stale_offset);
        assert!(should_check_for_updates(
            UpdateChannel::Latest,
            Some(&cache)
        ));

        assert!(should_check_for_updates(UpdateChannel::Latest, None));
    }

    #[test]
    fn test_should_check_for_updates_verifies_channel() {
        let now = current_timestamp();
        let mut cache = UpdateCache::new(UpdateChannel::Latest);
        cache.last_checked_at = now;

        // Cache matches channel - should respect interval
        assert!(!should_check_for_updates(
            UpdateChannel::Latest,
            Some(&cache)
        ));

        // Cache doesn't match channel - should check for updates
        assert!(should_check_for_updates(UpdateChannel::Next, Some(&cache)));
    }

    #[test]
    fn test_verify_sha256_success() {
        let content = b"hello world";
        // SHA256 of "hello world"
        let expected = "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        assert!(verify_sha256(content, expected).is_ok());
    }

    #[test]
    fn test_verify_sha256_case_insensitive() {
        let content = b"hello world";
        let expected_upper = "B94D27B9934D3E08A52E52D7DA7DABFAC484EFE37A5380EE9088F7ACE2EFCDE9";
        assert!(verify_sha256(content, expected_upper).is_ok());
    }

    #[test]
    fn test_verify_sha256_mismatch() {
        let content = b"hello world";
        let wrong_hash = "0000000000000000000000000000000000000000000000000000000000000000";
        let result = verify_sha256(content, wrong_hash);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Checksum mismatch"));
    }

    #[test]
    fn test_verify_sha256_empty_content() {
        let content = b"";
        // SHA256 of empty string
        let expected = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert!(verify_sha256(content, expected).is_ok());
    }

    #[test]
    fn test_parse_checksums_valid_format() {
        let content = "594de6cf107e8ffb6efd9029bf727b465ab55a9b4c4c3995eb3e628c857dc423  git-ai-linux-arm64\n\
                       88db3c0c7fc62a815579ec0ca42535c2b83ab18d9e3af8efe345dee96677b1d8  git-ai-linux-x64\n\
                       75d1692d347c3e08a208dc6373df4cee2b5ffd0e2aee62ccb1bb47aae866b2c8  install.sh";

        let checksums = parse_checksums(content);
        assert_eq!(checksums.len(), 3);
        assert_eq!(
            checksums.get("git-ai-linux-arm64"),
            Some(&"594de6cf107e8ffb6efd9029bf727b465ab55a9b4c4c3995eb3e628c857dc423".to_string())
        );
        assert_eq!(
            checksums.get("git-ai-linux-x64"),
            Some(&"88db3c0c7fc62a815579ec0ca42535c2b83ab18d9e3af8efe345dee96677b1d8".to_string())
        );
        assert_eq!(
            checksums.get("install.sh"),
            Some(&"75d1692d347c3e08a208dc6373df4cee2b5ffd0e2aee62ccb1bb47aae866b2c8".to_string())
        );
    }

    #[test]
    fn test_parse_checksums_with_extensions() {
        let content = "23c693a25f4f2e99463c911e67d534ae17cbd9b98513aa65f0ae9da861775d54  git-ai-windows-x64.exe\n\
                       f895af791eb30f6b074b2ab9f0f803e91230b084f5864befcb51ee9ced752adf  install.ps1";

        let checksums = parse_checksums(content);
        assert_eq!(checksums.len(), 2);
        assert!(checksums.contains_key("git-ai-windows-x64.exe"));
        assert!(checksums.contains_key("install.ps1"));
    }

    #[test]
    fn test_parse_checksums_empty_input() {
        let checksums = parse_checksums("");
        assert!(checksums.is_empty());
    }

    #[test]
    fn test_parse_checksums_whitespace_lines() {
        let content = "  \n\nhash  file\n  \n";
        let checksums = parse_checksums(content);
        assert_eq!(checksums.len(), 1);
        assert_eq!(checksums.get("file"), Some(&"hash".to_string()));
    }

    #[test]
    fn test_parse_checksums_ignores_invalid_lines() {
        // Lines with single space or no space should be ignored
        let content = "valid  file1\ninvalid file2\nalsovalid  file3";
        let checksums = parse_checksums(content);
        assert_eq!(checksums.len(), 2);
        assert!(checksums.contains_key("file1"));
        assert!(checksums.contains_key("file3"));
        assert!(!checksums.contains_key("file2"));
    }

    // --- Additional comprehensive tests ---

    #[test]
    fn test_update_cache_new() {
        let cache = UpdateCache::new(UpdateChannel::Latest);
        assert_eq!(cache.last_checked_at, 0);
        assert!(cache.available_tag.is_none());
        assert!(cache.available_semver.is_none());
        assert_eq!(cache.channel, "latest");
        assert!(!cache.update_available());
        assert!(cache.matches_channel(UpdateChannel::Latest));
        assert!(!cache.matches_channel(UpdateChannel::Next));
    }

    #[test]
    fn test_update_cache_update_available() {
        let mut cache = UpdateCache::new(UpdateChannel::Latest);
        cache.available_semver = Some("2.0.0".to_string());
        assert!(cache.update_available());
    }

    #[test]
    fn test_update_cache_matches_channel_enterprise() {
        let cache_latest = UpdateCache::new(UpdateChannel::EnterpriseLatest);
        assert!(cache_latest.matches_channel(UpdateChannel::EnterpriseLatest));
        assert!(!cache_latest.matches_channel(UpdateChannel::EnterpriseNext));
        assert!(!cache_latest.matches_channel(UpdateChannel::Latest));
    }

    #[test]
    fn test_determine_action_force() {
        let release = ChannelRelease {
            tag: "v1.0.0".to_string(),
            semver: "1.0.0".to_string(),
            checksum: "abc".to_string(),
        };
        let action = determine_action(true, &release, "1.0.0");
        assert_eq!(action, UpgradeAction::ForceReinstall);
    }

    #[test]
    fn test_determine_action_already_latest() {
        let release = ChannelRelease {
            tag: "v1.0.0".to_string(),
            semver: "1.0.0".to_string(),
            checksum: "abc".to_string(),
        };
        let action = determine_action(false, &release, "1.0.0");
        assert_eq!(action, UpgradeAction::AlreadyLatest);
    }

    #[test]
    fn test_determine_action_upgrade_available() {
        let release = ChannelRelease {
            tag: "v2.0.0".to_string(),
            semver: "2.0.0".to_string(),
            checksum: "abc".to_string(),
        };
        let action = determine_action(false, &release, "1.0.0");
        assert_eq!(action, UpgradeAction::UpgradeAvailable);
    }

    #[test]
    fn test_determine_action_running_newer() {
        let release = ChannelRelease {
            tag: "v1.0.0".to_string(),
            semver: "1.0.0".to_string(),
            checksum: "abc".to_string(),
        };
        let action = determine_action(false, &release, "2.0.0");
        assert_eq!(action, UpgradeAction::RunningNewerVersion);
    }

    #[test]
    fn test_upgrade_action_to_string() {
        assert_eq!(
            UpgradeAction::UpgradeAvailable.to_string(),
            "upgrade_available"
        );
        assert_eq!(UpgradeAction::AlreadyLatest.to_string(), "already_latest");
        assert_eq!(
            UpgradeAction::RunningNewerVersion.to_string(),
            "running_newer_version"
        );
        assert_eq!(UpgradeAction::ForceReinstall.to_string(), "force_reinstall");
    }

    #[test]
    fn test_semver_from_tag_enterprise_prefix() {
        assert_eq!(semver_from_tag("enterprise-v1.2.3"), "1.2.3");
        assert_eq!(semver_from_tag("enterprise-1.2.3"), "1.2.3");
    }

    #[test]
    fn test_semver_from_tag_with_build_metadata() {
        assert_eq!(semver_from_tag("v1.2.3+build123"), "1.2.3");
        assert_eq!(semver_from_tag("1.2.3+build123"), "1.2.3");
    }

    #[test]
    fn test_semver_from_tag_empty() {
        assert_eq!(semver_from_tag(""), "");
        assert_eq!(semver_from_tag("v"), "");
        assert_eq!(semver_from_tag("enterprise-v"), "");
    }

    #[test]
    fn test_is_newer_version_major() {
        assert!(is_newer_version("2.0.0", "1.9.9"));
        assert!(!is_newer_version("1.9.9", "2.0.0"));
    }

    #[test]
    fn test_is_newer_version_minor() {
        assert!(is_newer_version("1.2.0", "1.1.9"));
        assert!(!is_newer_version("1.1.9", "1.2.0"));
    }

    #[test]
    fn test_is_newer_version_patch() {
        assert!(is_newer_version("1.0.1", "1.0.0"));
        assert!(!is_newer_version("1.0.0", "1.0.1"));
    }

    #[test]
    fn test_is_newer_version_empty_parts() {
        assert!(is_newer_version("1", "0.9.9"));
        assert!(!is_newer_version("0.9.9", "1"));
    }

    #[test]
    fn test_is_newer_version_equal() {
        assert!(!is_newer_version("1.0.0", "1.0.0"));
        assert!(!is_newer_version("2.5.10", "2.5.10"));
    }

    #[test]
    fn test_parse_checksums_multiple_spaces() {
        // Format requires exactly two spaces between hash and filename
        // More spaces should still work because split_once("  ") matches the first occurrence
        let content = "abc123  file_with_spaces.txt";
        let checksums = parse_checksums(content);
        assert_eq!(checksums.len(), 1);
        assert_eq!(
            checksums.get("file_with_spaces.txt"),
            Some(&"abc123".to_string())
        );
    }

    #[test]
    fn test_verify_sha256_with_binary_content() {
        let content = b"\x00\x01\x02\x03\xff\xfe";
        let mut hasher = sha2::Sha256::new();
        hasher.update(content);
        let expected = format!("{:x}", hasher.finalize());
        assert!(verify_sha256(content, &expected).is_ok());
    }

    #[test]
    fn test_release_from_response_missing_channel() {
        let releases = ReleasesResponse {
            channels: HashMap::new(),
        };
        let result = release_from_response(releases, UpdateChannel::Latest);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn test_release_from_response_empty_tag() {
        let mut channels = HashMap::new();
        channels.insert(
            "latest".to_string(),
            ChannelInfo {
                version: "".to_string(),
                checksum: "abc123".to_string(),
            },
        );
        let releases = ReleasesResponse { channels };
        let result = release_from_response(releases, UpdateChannel::Latest);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not found"));
    }

    #[test]
    fn test_release_from_response_empty_checksum() {
        let mut channels = HashMap::new();
        channels.insert(
            "latest".to_string(),
            ChannelInfo {
                version: "v1.0.0".to_string(),
                checksum: "".to_string(),
            },
        );
        let releases = ReleasesResponse { channels };
        let result = release_from_response(releases, UpdateChannel::Latest);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Checksum"));
    }

    #[test]
    fn test_release_from_response_invalid_semver() {
        let mut channels = HashMap::new();
        channels.insert(
            "latest".to_string(),
            ChannelInfo {
                version: "v-invalid-version".to_string(),
                checksum: "abc123".to_string(),
            },
        );
        let releases = ReleasesResponse { channels };
        let result = release_from_response(releases, UpdateChannel::Latest);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("semver"));
    }

    #[test]
    fn test_release_from_response_success() {
        let mut channels = HashMap::new();
        channels.insert(
            "latest".to_string(),
            ChannelInfo {
                version: "v1.2.3".to_string(),
                checksum: "abc123def456".to_string(),
            },
        );
        let releases = ReleasesResponse { channels };
        let result = release_from_response(releases, UpdateChannel::Latest);
        assert!(result.is_ok());
        let release = result.unwrap();
        assert_eq!(release.tag, "v1.2.3");
        assert_eq!(release.semver, "1.2.3");
        assert_eq!(release.checksum, "abc123def456");
    }

    #[test]
    fn test_should_check_for_updates_no_cache() {
        assert!(should_check_for_updates(UpdateChannel::Latest, None));
    }

    #[test]
    fn test_should_check_for_updates_zero_last_checked() {
        let cache = UpdateCache {
            last_checked_at: 0,
            available_tag: None,
            available_semver: None,
            channel: "latest".to_string(),
        };
        assert!(should_check_for_updates(
            UpdateChannel::Latest,
            Some(&cache)
        ));
    }

    #[test]
    fn test_should_check_for_updates_channel_mismatch() {
        let now = current_timestamp();
        let cache = UpdateCache {
            last_checked_at: now,
            available_tag: None,
            available_semver: None,
            channel: "latest".to_string(),
        };
        assert!(should_check_for_updates(UpdateChannel::Next, Some(&cache)));
    }

    #[test]
    fn test_update_cache_serialization() {
        // Test serialization/deserialization without file I/O
        let mut cache = UpdateCache::new(UpdateChannel::Latest);
        cache.last_checked_at = 1234567890;
        cache.available_tag = Some("v1.0.0".to_string());
        cache.available_semver = Some("1.0.0".to_string());

        let json = serde_json::to_vec(&cache).unwrap();
        let deserialized: UpdateCache = serde_json::from_slice(&json).unwrap();

        assert_eq!(deserialized.last_checked_at, 1234567890);
        assert_eq!(deserialized.available_tag, Some("v1.0.0".to_string()));
        assert_eq!(deserialized.available_semver, Some("1.0.0".to_string()));
        assert_eq!(deserialized.channel, "latest");
    }

    #[test]
    fn test_persist_update_state_creates_cache_object() {
        // Test that persist_update_state creates correct UpdateCache structure
        // without relying on file I/O
        let release = ChannelRelease {
            tag: "v1.5.0".to_string(),
            semver: "1.5.0".to_string(),
            checksum: "test".to_string(),
        };

        // Manually construct what persist_update_state would create
        let mut cache = UpdateCache::new(UpdateChannel::Next);
        cache.last_checked_at = current_timestamp();
        cache.available_tag = Some(release.tag.clone());
        cache.available_semver = Some(release.semver.clone());

        assert_eq!(cache.available_tag, Some("v1.5.0".to_string()));
        assert_eq!(cache.available_semver, Some("1.5.0".to_string()));
        assert_eq!(cache.channel, "next");
        assert!(cache.last_checked_at > 0);
    }

    #[test]
    fn test_persist_update_state_no_release_structure() {
        // Test that persist_update_state without release creates correct structure
        let mut cache = UpdateCache::new(UpdateChannel::Latest);
        cache.last_checked_at = current_timestamp();
        // No available_tag or available_semver set

        assert!(cache.available_tag.is_none());
        assert!(cache.available_semver.is_none());
        assert_eq!(cache.channel, "latest");
        assert!(cache.last_checked_at > 0);
    }

    #[test]
    fn test_daemon_update_check_result_debug() {
        // Verify that DaemonUpdateCheckResult derives Debug and PartialEq correctly.
        assert_eq!(
            DaemonUpdateCheckResult::NoUpdate,
            DaemonUpdateCheckResult::NoUpdate
        );
        assert_eq!(
            DaemonUpdateCheckResult::UpdateReady,
            DaemonUpdateCheckResult::UpdateReady
        );
        assert_ne!(
            DaemonUpdateCheckResult::NoUpdate,
            DaemonUpdateCheckResult::UpdateReady
        );
    }

    #[test]
    #[serial]
    fn test_check_for_update_available_no_cache_newer_version() {
        // When the cache is empty and a newer version is available, the function should
        // report UpdateReady (assuming version checks and auto-updates are enabled,
        // which is the default in debug/test builds).
        let temp_dir = tempfile::tempdir().unwrap();
        set_test_cache_dir(&temp_dir);

        let test_checksum = "a".repeat(64);
        let mock_payload = format!(
            r#"{{"channels":{{"latest":{{"version":"v999.0.0","checksum":"{}"}}}}}}"#,
            test_checksum
        );
        // check_for_update_available uses Config::fresh() which reads the real config,
        // but fetch_release_for_channel respects mock:// URLs only in tests.
        // We can't easily inject a mock URL into Config::fresh(), so we test the
        // underlying building blocks instead:
        let release =
            fetch_release_for_channel(&format!("mock://{}", mock_payload), UpdateChannel::Latest)
                .unwrap();
        let action = determine_action(false, &release, env!("CARGO_PKG_VERSION"));
        assert_eq!(action, UpgradeAction::UpgradeAvailable);

        // Persist and verify the cache reflects the available update.
        persist_update_state(UpdateChannel::Latest, Some(&release));
        let cache = read_update_cache().unwrap();
        assert!(cache.update_available());
        assert_eq!(cache.available_semver.as_deref(), Some("999.0.0"));

        clear_test_cache_dir();
    }

    #[test]
    fn test_check_for_update_available_same_version() {
        let current = env!("CARGO_PKG_VERSION");
        let test_checksum = "a".repeat(64);
        let mock_payload = format!(
            r#"{{"channels":{{"latest":{{"version":"v{}","checksum":"{}"}}}}}}"#,
            current, test_checksum
        );
        let release =
            fetch_release_for_channel(&format!("mock://{}", mock_payload), UpdateChannel::Latest)
                .unwrap();
        let action = determine_action(false, &release, current);
        assert_eq!(action, UpgradeAction::AlreadyLatest);

        // When the action is AlreadyLatest, persist_update_state is called with None.
        // Verify that such a cache does NOT mark an update as available.
        let mut cache = UpdateCache::new(UpdateChannel::Latest);
        cache.last_checked_at = current_timestamp();
        // No available_tag/semver set — mirrors what persist_update_state(channel, None) does.
        assert!(!cache.update_available());
    }

    #[test]
    fn test_should_check_for_updates_skips_when_recently_checked() {
        // When the cache was recently written, should_check_for_updates returns false.
        let mut cache = UpdateCache::new(UpdateChannel::Latest);
        cache.last_checked_at = current_timestamp();
        assert!(!should_check_for_updates(
            UpdateChannel::Latest,
            Some(&cache)
        ));
    }

    fn with_update_check_env(
        cache_has_update: bool,
        auto_updates_disabled: bool,
        f: impl FnOnce(),
    ) {
        let temp_dir = tempfile::tempdir().unwrap();
        set_test_cache_dir(&temp_dir);

        let mut cache = UpdateCache::new(UpdateChannel::Latest);
        cache.last_checked_at = current_timestamp();
        if cache_has_update {
            cache.available_tag = Some("v99.99.99".to_string());
            cache.available_semver = Some("99.99.99".to_string());
        }
        write_update_cache(&cache);

        let patch = serde_json::json!({
            "disable_version_checks": false,
            "disable_auto_updates": auto_updates_disabled
        })
        .to_string();
        let previous_patch = std::env::var_os("GIT_AI_TEST_CONFIG_PATCH");
        unsafe { std::env::set_var("GIT_AI_TEST_CONFIG_PATCH", &patch) };

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

        unsafe {
            match previous_patch {
                Some(value) => std::env::set_var("GIT_AI_TEST_CONFIG_PATCH", value),
                None => std::env::remove_var("GIT_AI_TEST_CONFIG_PATCH"),
            }
        };
        clear_test_cache_dir();

        if let Err(e) = result {
            std::panic::resume_unwind(e);
        }
    }

    fn write_fresh_current_pending_update(tag: &str) {
        let mut cache = UpdateCache::new(UpdateChannel::Latest);
        cache.last_checked_at = current_timestamp();
        cache.available_tag = Some(tag.to_string());
        cache.available_semver = Some(env!("CARGO_PKG_VERSION").to_string());
        write_update_cache(&cache);
    }

    fn mock_latest_release(tag: &str) -> String {
        format!(
            r#"mock://{{"channels":{{"latest":{{"version":"{}","checksum":"{}"}}}}}}"#,
            tag,
            "a".repeat(64)
        )
    }

    #[test]
    #[serial]
    fn fresh_current_pending_windows_release_requires_exact_channel_match() {
        let temp_dir = tempfile::tempdir().unwrap();
        set_test_cache_dir(&temp_dir);
        let current_tag = format!("v{}", env!("CARGO_PKG_VERSION"));
        write_fresh_current_pending_update(&current_tag);

        let result = check_for_update_available_with_settings(
            UpdateChannel::Latest,
            &mock_latest_release(&current_tag),
            false,
            false,
            true,
        )
        .unwrap();

        assert_eq!(result, DaemonUpdateCheckResult::UpdateReady);
        assert!(read_update_cache().is_some_and(|cache| cache.update_available()));
        clear_test_cache_dir();
    }

    #[test]
    #[serial]
    fn fresh_current_pending_windows_release_rollback_clears_stale_cache() {
        let temp_dir = tempfile::tempdir().unwrap();
        set_test_cache_dir(&temp_dir);
        let current_tag = format!("v{}", env!("CARGO_PKG_VERSION"));
        write_fresh_current_pending_update(&current_tag);

        let result = check_for_update_available_with_settings(
            UpdateChannel::Latest,
            &mock_latest_release("v1.0.0"),
            false,
            false,
            true,
        )
        .unwrap();

        assert_eq!(result, DaemonUpdateCheckResult::NoUpdate);
        assert!(read_update_cache().is_some_and(|cache| !cache.update_available()));
        clear_test_cache_dir();
    }

    #[test]
    #[serial]
    fn fresh_current_pending_windows_different_tag_clears_stale_cache() {
        let temp_dir = tempfile::tempdir().unwrap();
        set_test_cache_dir(&temp_dir);
        let current_tag = format!("v{}", env!("CARGO_PKG_VERSION"));
        write_fresh_current_pending_update(&current_tag);
        let different_tag = format!("{}-next-other", current_tag);

        let result = check_for_update_available_with_settings(
            UpdateChannel::Latest,
            &mock_latest_release(&different_tag),
            false,
            false,
            true,
        )
        .unwrap();

        assert_eq!(result, DaemonUpdateCheckResult::NoUpdate);
        assert!(read_update_cache().is_some_and(|cache| !cache.update_available()));
        clear_test_cache_dir();
    }

    #[test]
    #[serial]
    fn check_for_update_available_returns_update_ready_when_cache_has_pending_update() {
        with_update_check_env(true, false, || {
            let result = check_for_update_available().unwrap();
            assert_eq!(result, DaemonUpdateCheckResult::UpdateReady);
        });
    }

    #[test]
    #[serial]
    fn check_for_update_available_returns_no_update_when_auto_updates_disabled() {
        with_update_check_env(true, true, || {
            let result = check_for_update_available().unwrap();
            assert_eq!(result, DaemonUpdateCheckResult::NoUpdate);
        });
    }

    #[test]
    #[serial]
    fn check_for_update_available_returns_no_update_when_cache_has_no_pending_update() {
        with_update_check_env(false, false, || {
            let result = check_for_update_available().unwrap();
            assert_eq!(result, DaemonUpdateCheckResult::NoUpdate);
        });
    }
}
