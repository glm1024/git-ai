use crate::error::GitAiError;
use crate::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
use crate::mdm::utils::{binary_exists, generate_diff, home_dir};
use crate::utils::LockFile;
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const EVENTS: [&str; 6] = [
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "SessionStart",
    "UserPromptSubmit",
    "Stop",
];
const CHECKPOINT_ARGS: [&str; 5] = [
    "checkpoint",
    "zcode",
    "--strict-errors",
    "--hook-input",
    "stdin",
];

pub struct ZCodeInstaller;

fn matcher(event: &str) -> &'static str {
    match event {
        "PreToolUse" | "PostToolUse" | "PostToolUseFailure" => "Write|Edit|ApplyPatch|Bash",
        _ => "*",
    }
}

// This is an installer override. ZCode itself must be launched with the matching
// user config path; its ZCODE_DATA_BASE_DIR does not relocate CLI hook config.
fn config_path_from(home: &Path, explicit: Option<&str>) -> Result<PathBuf, GitAiError> {
    let path = explicit
        .filter(|path| !path.trim().is_empty())
        .map(|path| expand_path(home, path))
        .unwrap_or_else(|| home.join(".zcode").join("cli").join("config.json"));
    if !path.is_absolute() {
        return Err(GitAiError::Generic(
            "ZCode config path must be absolute; configuration was preserved".into(),
        ));
    }
    Ok(path)
}

fn expand_path(home: &Path, path: &str) -> PathBuf {
    if let Some(relative) = path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        home.join(relative)
    } else {
        PathBuf::from(path)
    }
}

fn validate_config(config: &Value) -> Result<(), GitAiError> {
    let invalid =
        || GitAiError::Generic("Invalid ZCode hook config; existing config was preserved".into());
    if !config.is_object() {
        return Err(invalid());
    }
    if let Some(hooks) = config.get("hooks") {
        if !hooks.is_object()
            || hooks
                .get("enabled")
                .is_some_and(|value| !value.is_boolean())
        {
            return Err(invalid());
        }
        if let Some(events) = hooks.get("events") {
            let events = events.as_object().ok_or_else(invalid)?;
            for blocks in events.values() {
                let blocks = blocks.as_array().ok_or_else(invalid)?;
                for block in blocks {
                    if !block.is_object()
                        || block.get("matcher").is_some_and(|value| !value.is_string())
                        || !block.get("hooks").is_some_and(Value::is_array)
                    {
                        return Err(invalid());
                    }
                }
            }
        }
    }
    Ok(())
}

fn is_owned_hook(hook: &Value) -> bool {
    let Some(command) = hook.get("command").and_then(Value::as_str) else {
        return false;
    };
    let filename = command.rsplit(['/', '\\']).next().unwrap_or(command);
    matches!(filename, "git-ai" | "git-ai.exe")
        && hook.get("type").and_then(Value::as_str) == Some("process")
        && hook
            .get("args")
            .and_then(Value::as_array)
            .is_some_and(|args| {
                args.first().and_then(Value::as_str) == Some("checkpoint")
                    && args.get(1).and_then(Value::as_str) == Some("zcode")
            })
}

fn desired_hook(params: &HookInstallerParams) -> Result<Value, GitAiError> {
    if !params.binary_path.is_absolute() {
        return Err(GitAiError::Generic(
            "ZCode hook requires an absolute git-ai binary path".into(),
        ));
    }
    let command = params
        .binary_path
        .to_str()
        .ok_or_else(|| GitAiError::Generic("ZCode hook binary path is not valid UTF-8".into()))?;
    Ok(json!({
        "type": "process",
        "command": command,
        "args": CHECKPOINT_ARGS,
        "timeoutMs": 30000
    }))
}

fn remove_owned_hooks(config: &mut Value) {
    let Some(events) = config
        .pointer_mut("/hooks/events")
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    for blocks in events.values_mut() {
        let Some(blocks) = blocks.as_array_mut() else {
            continue;
        };
        blocks.retain_mut(|block| {
            let hooks = block["hooks"].as_array_mut().expect("validated hook array");
            let before = hooks.len();
            hooks.retain(|hook| !is_owned_hook(hook));
            // Keep user metadata and preexisting empty blocks.
            before == hooks.len()
                || !hooks.is_empty()
                || block.as_object().is_some_and(|object| {
                    object.keys().any(|key| key != "matcher" && key != "hooks")
                })
        });
    }
}

fn config_target(path: &Path) -> Result<PathBuf, GitAiError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(fs::canonicalize(path)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .ok_or_else(|| GitAiError::Generic("ZCode config has no parent".into()))?;
            let filename = path
                .file_name()
                .ok_or_else(|| GitAiError::Generic("ZCode config has no filename".into()))?;
            Ok(fs::canonicalize(parent)?.join(filename))
        }
        Err(error) => Err(error.into()),
    }
}

fn acquire_config_lock(target: &Path) -> Result<LockFile, GitAiError> {
    let filename = target
        .file_name()
        .ok_or_else(|| GitAiError::Generic("ZCode config has no filename".into()))?;
    let mut lockname = filename.to_os_string();
    lockname.push(".lock");
    let lock_path = target.with_file_name(lockname);
    let started = Instant::now();
    loop {
        if let Some(lock) = LockFile::try_acquire_result(&lock_path)? {
            return Ok(lock);
        }
        if started.elapsed() >= Duration::from_secs(5) {
            return Err(GitAiError::Generic(format!(
                "Timed out waiting for ZCode config lock {}",
                lock_path.display()
            )));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn persist_config(target: &Path, bytes: &[u8]) -> Result<(), GitAiError> {
    let parent = target
        .parent()
        .ok_or_else(|| GitAiError::Generic("ZCode config has no parent".into()))?;
    // NamedTempFile::persist supplies replacement semantics on Windows as well
    // as Unix, and unique names prevent collisions with unrelated writers.
    let mut temp = tempfile::Builder::new()
        .prefix(".git-ai-zcode-")
        .tempfile_in(parent)?;
    if let Ok(metadata) = fs::metadata(target) {
        temp.as_file().set_permissions(metadata.permissions())?;
    }
    temp.write_all(bytes)?;
    temp.as_file().sync_all()?;
    let persisted = temp
        .persist(target)
        .map_err(|error| GitAiError::IoError(error.error))?;
    persisted.sync_all()?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

impl ZCodeInstaller {
    fn settings_path() -> Result<PathBuf, GitAiError> {
        config_path_from(
            &home_dir(),
            std::env::var("GIT_AI_ZCODE_CONFIG_PATH").ok().as_deref(),
        )
    }

    fn hook_status(
        config: &Value,
        params: &HookInstallerParams,
    ) -> Result<(bool, bool), GitAiError> {
        validate_config(config)?;
        let desired = desired_hook(params)?;
        let Some(events) = config.pointer("/hooks/events").and_then(Value::as_object) else {
            return Ok((false, false));
        };
        let mut installed = false;
        let mut exact_count = 0;
        let mut owned_count = 0;
        for (event, blocks) in events {
            for block in blocks.as_array().expect("validated event array") {
                for hook in block["hooks"].as_array().expect("validated hook array") {
                    if is_owned_hook(hook) {
                        installed = true;
                        owned_count += 1;
                        if EVENTS.contains(&event.as_str())
                            && block["matcher"] == matcher(event)
                            && hook == &desired
                        {
                            exact_count += 1;
                        }
                    }
                }
            }
        }
        let all_events = EVENTS.iter().all(|event| {
            events
                .get(*event)
                .and_then(Value::as_array)
                .is_some_and(|blocks| {
                    blocks.iter().any(|block| {
                        block["matcher"] == matcher(event)
                            && block["hooks"]
                                .as_array()
                                .is_some_and(|hooks| hooks.contains(&desired))
                    })
                })
        });
        Ok((
            installed,
            config.pointer("/hooks/enabled") == Some(&json!(true))
                && all_events
                && exact_count == EVENTS.len()
                && owned_count == EVENTS.len(),
        ))
    }

    fn update_at(
        settings_path: &Path,
        dry_run: bool,
        install: Option<&HookInstallerParams>,
    ) -> Result<Option<String>, GitAiError> {
        if !settings_path.is_absolute() {
            return Err(GitAiError::Generic(
                "ZCode config path must be absolute; configuration was preserved".into(),
            ));
        }
        let desired = install.map(desired_hook).transpose()?;
        if install.is_none() && !settings_path.exists() && !settings_path.is_symlink() {
            return Ok(None);
        }
        let (_lock, target) = if dry_run {
            (None, settings_path.to_path_buf())
        } else {
            if let Some(parent) = settings_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let target = config_target(settings_path)?;
            let lock = acquire_config_lock(&target)?;
            // Resolve again after obtaining the lock; a replaced symlink must
            // not redirect this write to an unlocked managed file.
            if config_target(settings_path)? != target {
                return Err(GitAiError::Generic(
                    "ZCode config target changed while waiting for its lock; retry installation"
                        .into(),
                ));
            }
            (Some(lock), target)
        };
        let content = match fs::read_to_string(&target) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !target.is_symlink() => {
                String::new()
            }
            Err(error) => return Err(error.into()),
        };
        let before: Value = if content.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&content)?
        };
        validate_config(&before)?;
        let mut after = before.clone();
        if let Some(desired) = desired {
            let has_hooks = before
                .pointer("/hooks/events")
                .and_then(Value::as_object)
                .is_some_and(|events| {
                    events
                        .values()
                        .filter_map(Value::as_array)
                        .flatten()
                        .any(|block| {
                            block["hooks"]
                                .as_array()
                                .is_some_and(|hooks| !hooks.is_empty())
                        })
                });
            if has_hooks && before.pointer("/hooks/enabled") != Some(&json!(true)) {
                return Err(GitAiError::Generic(format!(
                    "ZCode hooks are disabled in {}; preserving the disabled state and existing hooks. Enable hooks explicitly before installing Git AI, then start a new ZCode session.",
                    settings_path.display()
                )));
            }
            let root = after.as_object_mut().expect("validated config object");
            let hooks = root
                .entry("hooks")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .expect("validated hooks object");
            hooks.insert("enabled".into(), json!(true));
            hooks.entry("events").or_insert_with(|| json!({}));
            remove_owned_hooks(&mut after);
            let events = after
                .pointer_mut("/hooks/events")
                .and_then(Value::as_object_mut)
                .expect("validated events object");
            for event in EVENTS {
                events
                    .entry(event)
                    .or_insert_with(|| json!([]))
                    .as_array_mut()
                    .expect("validated event array")
                    .push(json!({"matcher":matcher(event),"hooks":[desired.clone()]}));
            }
        } else {
            remove_owned_hooks(&mut after);
        }
        if before == after {
            return Ok(None);
        }
        let new_content = serde_json::to_string_pretty(&after)?;
        let diff = generate_diff(settings_path, &content, &new_content);
        if !dry_run {
            persist_config(&target, new_content.as_bytes())?;
        }
        Ok(Some(diff))
    }

    fn install_hooks_at(
        path: &Path,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        Self::update_at(path, dry_run, Some(params))
    }

    fn uninstall_hooks_at(path: &Path, dry_run: bool) -> Result<Option<String>, GitAiError> {
        Self::update_at(path, dry_run, None)
    }
}

impl HookInstaller for ZCodeInstaller {
    fn name(&self) -> &str {
        "ZCode"
    }
    fn id(&self) -> &str {
        "zcode"
    }
    fn process_names(&self) -> Vec<&str> {
        vec!["ZCode", "zcode"]
    }

    fn check_hooks(&self, params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        let path = Self::settings_path()?;
        let has_config = path.exists() || path.is_symlink();
        let tool_installed =
            has_config || path.parent().is_some_and(Path::exists) || binary_exists("zcode");
        #[cfg(target_os = "macos")]
        let tool_installed = tool_installed
            || Path::new("/Applications/ZCode.app").exists()
            || home_dir().join("Applications/ZCode.app").exists();
        let (hooks_installed, hooks_up_to_date) = if has_config {
            let config: Value = serde_json::from_str(&fs::read_to_string(&path)?)?;
            Self::hook_status(&config, params)?
        } else {
            (false, false)
        };
        Ok(HookCheckResult {
            tool_installed,
            hooks_installed,
            hooks_up_to_date,
        })
    }

    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        Self::install_hooks_at(&Self::settings_path()?, params, dry_run)
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        Self::uninstall_hooks_at(&Self::settings_path()?, dry_run)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn params() -> HookInstallerParams {
        HookInstallerParams {
            binary_path: std::env::temp_dir()
                .join("Git AI with spaces")
                .join(if cfg!(windows) {
                    "git-ai.exe"
                } else {
                    "git-ai"
                }),
        }
    }

    fn setup(value: Value) -> (TempDir, PathBuf) {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("config.json");
        fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        (temp, path)
    }

    fn read(path: &Path) -> Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn fresh_install_uses_synchronous_process_argv_for_all_events() {
        let (_temp, path) = setup(json!({"mcp":{"servers":{"example":{"command":"preserve"}}}}));
        ZCodeInstaller::install_hooks_at(&path, &params(), false).unwrap();
        let config = read(&path);
        assert_eq!(config["mcp"]["servers"]["example"]["command"], "preserve");
        assert_eq!(config["hooks"]["enabled"], true);
        for event in EVENTS {
            let block = &config["hooks"]["events"][event][0];
            assert_eq!(block["matcher"], matcher(event));
            let hook = &block["hooks"][0];
            assert_eq!(hook["type"], "process");
            assert_eq!(hook["command"], params().binary_path.to_str().unwrap());
            assert_eq!(
                hook["args"],
                json!([
                    "checkpoint",
                    "zcode",
                    "--strict-errors",
                    "--hook-input",
                    "stdin"
                ])
            );
            assert_eq!(hook["timeoutMs"], 30000);
            assert!(hook.get("async").is_none());
        }
        assert!(
            ZCodeInstaller::install_hooks_at(&path, &params(), false)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            ZCodeInstaller::hook_status(&config, &params()).unwrap(),
            (true, true)
        );
    }

    #[test]
    fn install_preserves_foreign_hooks_and_uninstall_only_removes_zcode() {
        let foreign = json!({"type":"process","command":"audit","args":["--save"]});
        let other_agent =
            json!({"type":"process","command":"/old/git-ai","args":["checkpoint","claude"]});
        let before = json!({"hooks":{"enabled":true,"custom":"keep","events":{
            "PreToolUse":[{"matcher":"*","custom":"keep","hooks":[foreign,other_agent]}],
            "PermissionRequest":[{"matcher":"*","hooks":[{"type":"command","command":"audit"}]}]
        }}, "provider":"keep"});
        let (_temp, path) = setup(before.clone());
        ZCodeInstaller::install_hooks_at(&path, &params(), false).unwrap();
        let installed = read(&path);
        assert_eq!(
            installed["hooks"]["events"]["PreToolUse"][0],
            before["hooks"]["events"]["PreToolUse"][0]
        );
        ZCodeInstaller::uninstall_hooks_at(&path, false).unwrap();
        let removed = read(&path);
        assert_eq!(
            removed["hooks"]["events"]["PreToolUse"][0],
            before["hooks"]["events"]["PreToolUse"][0]
        );
        assert_eq!(
            removed["hooks"]["events"]["PermissionRequest"],
            before["hooks"]["events"]["PermissionRequest"]
        );
        assert_eq!(removed["hooks"]["custom"], "keep");
        assert_eq!(removed["hooks"]["enabled"], true);
        assert!(
            ZCodeInstaller::uninstall_hooks_at(&path, false)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn ownership_handles_windows_spaces_without_claiming_unrelated_commands() {
        let hook = json!({
            "type":"process",
            "command":r"C:\Program Files\Git AI\git-ai.exe",
            "args":["checkpoint","zcode","--hook-input","stdin"]
        });
        assert!(is_owned_hook(&hook));
        assert!(!is_owned_hook(
            &json!({"type":"process","command":"git-ai","args":["checkpoint","claude"]})
        ));
        assert!(!is_owned_hook(
            &json!({"type":"process","command":"custom-git-ai","args":["checkpoint","zcode"]})
        ));
        assert!(!is_owned_hook(
            &json!({"type":"command","command":"git-ai checkpoint zcode --hook-input stdin"})
        ));
    }

    #[test]
    fn disabled_existing_hooks_are_not_enabled_or_modified() {
        for enabled in [Some(json!(false)), None] {
            let mut before = json!({"hooks":{"events":{"PreToolUse":[{"matcher":"*","hooks":[{"type":"process","command":"audit"}]}]}}});
            if let Some(enabled) = enabled {
                before["hooks"]["enabled"] = enabled;
            }
            let (_temp, path) = setup(before.clone());
            let old = fs::read(&path).unwrap();
            let error = ZCodeInstaller::install_hooks_at(&path, &params(), false).unwrap_err();
            assert!(error.to_string().contains("disabled"));
            assert_eq!(fs::read(path).unwrap(), old);
        }
    }

    #[test]
    fn disabled_empty_config_can_enable_only_our_hooks() {
        let (_temp, path) = setup(json!({"hooks":{"enabled":false,"events":{}}}));
        ZCodeInstaller::install_hooks_at(&path, &params(), false).unwrap();
        assert_eq!(read(&path)["hooks"]["enabled"], true);
    }

    #[test]
    fn stale_duplicate_own_hooks_are_updated_without_touching_other_presets() {
        let old = json!({"type":"process","command":"/old/git-ai","args":["checkpoint","zcode","--hook-input","stdin"]});
        let (_temp, path) = setup(
            json!({"hooks":{"enabled":true,"events":{"PreToolUse":[{"matcher":"Write","hooks":[old.clone(),old,{"type":"process","command":"audit"}]}]}}}),
        );
        ZCodeInstaller::install_hooks_at(&path, &params(), false).unwrap();
        let config = read(&path);
        let blocks = config["hooks"]["events"]["PreToolUse"].as_array().unwrap();
        assert_eq!(
            blocks[0]["hooks"],
            json!([{"type":"process","command":"audit"}])
        );
        assert_eq!(
            blocks
                .iter()
                .flat_map(|b| b["hooks"].as_array().unwrap())
                .filter(|h| is_owned_hook(h))
                .count(),
            1
        );
        assert_eq!(
            ZCodeInstaller::hook_status(&config, &params()).unwrap(),
            (true, true)
        );
    }

    #[test]
    fn malformed_config_is_rejected_without_overwrite() {
        for before in [
            json!([]),
            json!({"hooks":null}),
            json!({"hooks":{"enabled":"true"}}),
            json!({"hooks":{"events":[]}}),
            json!({"hooks":{"events":{"PreToolUse":{}}}}),
            json!({"hooks":{"events":{"PreToolUse":[{"hooks":"wrong"}]}}}),
        ] {
            let (_temp, path) = setup(before);
            let old = fs::read(&path).unwrap();
            assert!(ZCodeInstaller::install_hooks_at(&path, &params(), false).is_err());
            assert_eq!(fs::read(&path).unwrap(), old);
        }
        let (_temp, path) = setup(json!({}));
        fs::write(&path, "{invalid").unwrap();
        assert!(ZCodeInstaller::install_hooks_at(&path, &params(), false).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{invalid");
    }

    #[test]
    fn dry_run_never_creates_config_or_directories() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("not-created/cli/config.json");
        assert!(
            ZCodeInstaller::install_hooks_at(&path, &params(), true)
                .unwrap()
                .is_some()
        );
        assert!(!path.parent().unwrap().exists());
        assert!(
            ZCodeInstaller::uninstall_hooks_at(&path, true)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn install_rejects_relative_binary_path() {
        let (_temp, path) = setup(json!({}));
        let params = HookInstallerParams {
            binary_path: PathBuf::from("git-ai"),
        };
        assert!(ZCodeInstaller::install_hooks_at(&path, &params, false).is_err());
        assert_eq!(read(&path), json!({}));
    }

    #[test]
    fn status_requires_all_current_hooks_and_enabled_config() {
        let (_temp, path) = setup(json!({}));
        ZCodeInstaller::install_hooks_at(&path, &params(), false).unwrap();
        let mut config = read(&path);
        config["hooks"]["enabled"] = json!(false);
        assert_eq!(
            ZCodeInstaller::hook_status(&config, &params()).unwrap(),
            (true, false)
        );
        config["hooks"]["enabled"] = json!(true);
        config["hooks"]["events"]["Stop"] = json!([]);
        assert_eq!(
            ZCodeInstaller::hook_status(&config, &params()).unwrap(),
            (true, false)
        );
    }

    #[test]
    fn concurrent_locked_writer_is_reread_before_install() {
        let (_temp, path) = setup(json!({}));
        let lock = acquire_config_lock(&path).unwrap();
        let other = path.clone();
        let worker =
            std::thread::spawn(move || ZCodeInstaller::install_hooks_at(&other, &params(), false));
        fs::write(
            &path,
            serde_json::to_vec(&json!({"concurrent":"preserved"})).unwrap(),
        )
        .unwrap();
        drop(lock);
        worker.join().unwrap().unwrap();
        assert_eq!(read(&path)["concurrent"], "preserved");
    }

    #[cfg(unix)]
    #[test]
    fn relative_symlink_is_preserved_and_uses_target_lock() {
        let temp = TempDir::new().unwrap();
        let managed = temp.path().join("managed");
        fs::create_dir_all(&managed).unwrap();
        let target = managed.join("config.json");
        fs::write(&target, "{}").unwrap();
        let link = temp.path().join("config.json");
        std::os::unix::fs::symlink("managed/config.json", &link).unwrap();
        let lock = acquire_config_lock(&target).unwrap();
        let other = link.clone();
        let worker =
            std::thread::spawn(move || ZCodeInstaller::install_hooks_at(&other, &params(), false));
        fs::write(&target, "{\"concurrent\":true}").unwrap();
        drop(lock);
        worker.join().unwrap().unwrap();
        assert!(link.is_symlink());
        assert_eq!(read(&target)["concurrent"], true);
        ZCodeInstaller::uninstall_hooks_at(&link, false).unwrap();
        assert!(link.is_symlink());
    }

    #[test]
    fn relative_config_override_is_rejected_without_any_filesystem_write() {
        let home = std::env::temp_dir().join("home");
        assert!(config_path_from(&home, Some("relative/config.json")).is_err());
        let path = Path::new("relative-zcode-config-must-not-be-created/config.json");
        for dry_run in [true, false] {
            assert!(ZCodeInstaller::install_hooks_at(path, &params(), dry_run).is_err());
            assert!(ZCodeInstaller::uninstall_hooks_at(path, dry_run).is_err());
            assert!(!path.parent().unwrap().exists());
        }
    }

    #[test]
    fn path_override_selects_full_config_and_expands_home() {
        let home = std::env::temp_dir().join("home");
        assert_eq!(
            config_path_from(&home, None).unwrap(),
            home.join(".zcode/cli/config.json")
        );
        let explicit = home.join("explicit config.json");
        assert_eq!(
            config_path_from(&home, Some(explicit.to_str().unwrap())).unwrap(),
            explicit
        );
        assert_eq!(
            config_path_from(&home, Some("~/custom config.json")).unwrap(),
            home.join("custom config.json")
        );
        assert_eq!(
            config_path_from(&home, Some("")).unwrap(),
            home.join(".zcode/cli/config.json")
        );
    }
}
