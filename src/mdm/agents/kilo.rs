//! Kilo v7 plugin installer.
//!
//! Kilo v7 intentionally keeps the OpenCode plugin hook contract. The managed
//! Kilo plugin is therefore generated from the canonical OpenCode adapter at
//! install time with a small, fail-closed set of Kilo-specific substitutions.
//! This keeps upstream OpenCode fixes flowing into Kilo without maintaining a
//! second large TypeScript copy in this fork.

use crate::error::GitAiError;
use crate::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
use crate::mdm::utils::{
    binary_exists, generate_diff, home_dir, is_vsc_editor_extension_installed, resolve_editor_clis,
    write_atomic,
};
use std::fs;
use std::path::{Path, PathBuf};

const OPENCODE_PLUGIN_CONTENT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/agent-support/opencode/git-ai.ts"
));

const TOOL_INPUT_ANCHOR: &str = "          tool_input: toolInput,\n";
const KILO_RUNTIME_FIELDS: &str = concat!(
    "          tool_input: toolInput,\n",
    "          platform: process.env.KILO_PLATFORM || process.env.KILO_CLIENT || \"cli\",\n",
    "          client: process.env.KILO_CLIENT || process.env.KILO_PLATFORM || \"cli\",\n",
    "          editor_name: process.env.KILO_EDITOR_NAME,\n",
    "          database_path: process.env.KILO_DB,\n",
);
const KILO_VSCODE_EXTENSION_ID: &str = "kilocode.kilo-code";

pub struct KiloInstaller;

impl KiloInstaller {
    fn has_explicit_config_root() -> bool {
        ["GIT_AI_KILO_CONFIG_HOME", "KILO_CONFIG_DIR"]
            .iter()
            .any(|name| {
                std::env::var_os(name)
                    .map(|value| !value.is_empty())
                    .unwrap_or(false)
            })
    }

    fn has_vscode_extension() -> Result<bool, GitAiError> {
        let mut editor_clis = Vec::new();
        for cli_name in ["code", "code-insiders"] {
            for cli in resolve_editor_clis(cli_name) {
                if !editor_clis.contains(&cli) {
                    editor_clis.push(cli);
                }
            }
        }

        Self::combine_vscode_extension_checks(
            editor_clis
                .into_iter()
                .map(|cli| is_vsc_editor_extension_installed(&cli, KILO_VSCODE_EXTENSION_ID)),
        )
    }

    fn combine_vscode_extension_checks(
        checks: impl IntoIterator<Item = Result<bool, GitAiError>>,
    ) -> Result<bool, GitAiError> {
        let mut errors = Vec::new();
        for check in checks {
            match check {
                Ok(true) => return Ok(true),
                Ok(false) => {}
                Err(error) => errors.push(error.to_string()),
            }
        }

        if !errors.is_empty() {
            return Err(GitAiError::Generic(format!(
                "Unable to inspect every VS Code installation for the Kilo extension: {}",
                errors.join("; ")
            )));
        }

        Ok(false)
    }

    fn config_root() -> PathBuf {
        std::env::var_os("GIT_AI_KILO_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("KILO_CONFIG_DIR")
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
            })
            .or_else(|| {
                std::env::var_os("XDG_CONFIG_HOME")
                    .filter(|value| !value.is_empty())
                    .map(PathBuf::from)
                    .map(|path| path.join("kilo"))
            })
            .unwrap_or_else(|| home_dir().join(".config").join("kilo"))
    }

    fn plugin_path() -> PathBuf {
        Self::config_root().join("plugin").join("git-ai.ts")
    }

    fn legacy_plugin_path() -> PathBuf {
        Self::config_root().join("plugins").join("git-ai.ts")
    }

    fn replace_required(
        content: String,
        from: &str,
        to: &str,
        expected_count: usize,
    ) -> Result<String, GitAiError> {
        let count = content.matches(from).count();
        if count != expected_count {
            return Err(GitAiError::Generic(format!(
                "Kilo adapter source anchor changed: expected {expected_count} occurrence(s) of {from:?}, found {count}"
            )));
        }
        Ok(content.replace(from, to))
    }

    /// Generate a Kilo plugin from the upstream-compatible OpenCode adapter.
    fn generate_plugin_content(binary_path: &Path) -> Result<String, GitAiError> {
        let mut content = OPENCODE_PLUGIN_CONTENT.to_string();
        content = Self::replace_required(
            content,
            "import type { Plugin } from \"@opencode-ai/plugin\"",
            "import type { Plugin } from \"@kilocode/plugin\"",
            1,
        )?;
        content = Self::replace_required(
            content,
            "const CHECKPOINT_ARGS = [\"checkpoint\", \"opencode\", \"--hook-input\", \"stdin\"]",
            "const CHECKPOINT_ARGS = [\"checkpoint\", \"kilo\", \"--hook-input\", \"stdin\"]",
            1,
        )?;
        content = Self::replace_required(
            content,
            "process.env.GIT_AI_OPENCODE_DEBUG ?? process.env.GIT_AI_DEBUG",
            "process.env.GIT_AI_KILO_DEBUG ?? process.env.GIT_AI_DEBUG",
            1,
        )?;
        content = Self::replace_required(content, TOOL_INPUT_ANCHOR, KILO_RUNTIME_FIELDS, 2)?;
        content = content
            .replace("git-ai plugin for OpenCode", "git-ai plugin for Kilo v7")
            .replace(
                "integrates git-ai with OpenCode",
                "integrates git-ai with Kilo v7",
            )
            .replace("~/.config/opencode/plugins", "~/.config/kilo/plugin")
            .replace(".opencode/plugins", ".kilo/plugin")
            .replace(
                "https://opencode.ai/docs/plugins/",
                "https://kilo.ai/docs/automate/extending/plugins",
            )
            .replace("[git-ai opencode]", "[git-ai kilo]")
            .replace("git-ai checkpoint opencode", "git-ai checkpoint kilo");

        let path = binary_path.display().to_string().replace('\\', "\\\\");
        Self::replace_required(content, "__GIT_AI_BINARY_PATH__", &path, 1)
    }

    fn remove_legacy_plugin(dry_run: bool) -> Result<Option<String>, GitAiError> {
        let legacy_path = Self::legacy_plugin_path();
        if !legacy_path.exists() {
            return Ok(None);
        }

        let existing = fs::read_to_string(&legacy_path)?;
        let diff = generate_diff(&legacy_path, &existing, "");
        if !dry_run {
            fs::remove_file(&legacy_path)?;
        }
        Ok(Some(diff))
    }
}

impl HookInstaller for KiloInstaller {
    fn name(&self) -> &str {
        "Kilo v7"
    }

    fn id(&self) -> &str {
        "kilo"
    }

    fn process_names(&self) -> Vec<&str> {
        vec!["kilo", "kilocode"]
    }

    fn check_hooks(&self, params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        let has_binary = binary_exists("kilo") || binary_exists("kilocode");
        let has_explicit_config = Self::has_explicit_config_root();
        let has_global_config = Self::config_root().exists();
        let has_local_config = Path::new(".kilo").exists() || Path::new(".kilocode").exists();
        let has_vscode_extension = !has_binary
            && !has_explicit_config
            && !has_global_config
            && !has_local_config
            && Self::has_vscode_extension()?;

        if !has_binary
            && !has_explicit_config
            && !has_global_config
            && !has_local_config
            && !has_vscode_extension
        {
            return Ok(HookCheckResult {
                tool_installed: false,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        let plugin_path = Self::plugin_path();
        if !plugin_path.exists() {
            return Ok(HookCheckResult {
                tool_installed: true,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        let current_content = fs::read_to_string(&plugin_path).unwrap_or_default();
        let expected_content = Self::generate_plugin_content(&params.binary_path)?;
        Ok(HookCheckResult {
            tool_installed: true,
            hooks_installed: true,
            hooks_up_to_date: current_content.trim() == expected_content.trim(),
        })
    }

    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let plugin_path = Self::plugin_path();
        let existing_content = if plugin_path.exists() {
            fs::read_to_string(&plugin_path)?
        } else {
            String::new()
        };
        let new_content = Self::generate_plugin_content(&params.binary_path)?;
        let plugin_diff = (existing_content.trim() != new_content.trim())
            .then(|| generate_diff(&plugin_path, &existing_content, &new_content));
        let legacy_diff = Self::remove_legacy_plugin(dry_run)?;

        if plugin_diff.is_some() && !dry_run {
            if let Some(dir) = plugin_path.parent() {
                fs::create_dir_all(dir)?;
            }
            write_atomic(&plugin_path, new_content.as_bytes())?;
        }

        Ok(match (legacy_diff, plugin_diff) {
            (Some(legacy), Some(plugin)) => Some(format!("{legacy}\n{plugin}")),
            (Some(legacy), None) => Some(legacy),
            (None, Some(plugin)) => Some(plugin),
            (None, None) => None,
        })
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let plugin_path = Self::plugin_path();
        let plugin_diff = if plugin_path.exists() {
            let existing = fs::read_to_string(&plugin_path)?;
            if !dry_run {
                fs::remove_file(&plugin_path)?;
            }
            Some(generate_diff(&plugin_path, &existing, ""))
        } else {
            None
        };
        let legacy_diff = Self::remove_legacy_plugin(dry_run)?;

        Ok(match (legacy_diff, plugin_diff) {
            (Some(legacy), Some(plugin)) => Some(format!("{legacy}\n{plugin}")),
            (Some(legacy), None) => Some(legacy),
            (None, Some(plugin)) => Some(plugin),
            (None, None) => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    fn params() -> HookInstallerParams {
        HookInstallerParams {
            binary_path: PathBuf::from("/usr/local/bin/git-ai"),
        }
    }

    fn with_temp_home(run: impl FnOnce(&Path)) {
        let temp = TempDir::new().unwrap();
        let previous_home = std::env::var_os("HOME");
        let previous_profile = std::env::var_os("USERPROFILE");
        let previous_git_ai_kilo_config = std::env::var_os("GIT_AI_KILO_CONFIG_HOME");
        let previous_kilo_config = std::env::var_os("KILO_CONFIG_DIR");
        let previous_xdg_config = std::env::var_os("XDG_CONFIG_HOME");
        unsafe {
            std::env::set_var("HOME", temp.path());
            std::env::set_var("USERPROFILE", temp.path());
            std::env::remove_var("GIT_AI_KILO_CONFIG_HOME");
            std::env::remove_var("KILO_CONFIG_DIR");
            std::env::remove_var("XDG_CONFIG_HOME");
        }
        run(temp.path());
        unsafe {
            match previous_home {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
            match previous_profile {
                Some(value) => std::env::set_var("USERPROFILE", value),
                None => std::env::remove_var("USERPROFILE"),
            }
            match previous_git_ai_kilo_config {
                Some(value) => std::env::set_var("GIT_AI_KILO_CONFIG_HOME", value),
                None => std::env::remove_var("GIT_AI_KILO_CONFIG_HOME"),
            }
            match previous_kilo_config {
                Some(value) => std::env::set_var("KILO_CONFIG_DIR", value),
                None => std::env::remove_var("KILO_CONFIG_DIR"),
            }
            match previous_xdg_config {
                Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
    }

    #[cfg(unix)]
    fn write_fake_code_cli(path: &Path, extensions: &str) {
        use std::os::unix::fs::PermissionsExt;

        fs::write(
            path,
            format!(
                "#!/bin/sh\nif [ \"${{1:-}}\" = \"--list-extensions\" ]; then\nprintf '%s\\n' '{}'\nexit 0\nfi\nexit 1\n",
                extensions
            ),
        )
        .unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    fn with_fake_code_channel_extensions(
        stable_extensions: &str,
        insiders_extensions: Option<&str>,
        run: impl FnOnce(),
    ) {
        let temp = TempDir::new().unwrap();
        write_fake_code_cli(&temp.path().join("code"), stable_extensions);
        if let Some(extensions) = insiders_extensions {
            write_fake_code_cli(&temp.path().join("code-insiders"), extensions);
        }

        let previous_path = std::env::var_os("PATH");
        unsafe {
            std::env::set_var("PATH", temp.path());
        }
        run();
        unsafe {
            match previous_path {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    #[cfg(unix)]
    fn with_fake_code_extensions(extensions: &str, run: impl FnOnce()) {
        with_fake_code_channel_extensions(extensions, None, run);
    }

    #[test]
    fn test_kilo_plugin_is_generated_from_opencode_with_kilo_contract() {
        let content = KiloInstaller::generate_plugin_content(&params().binary_path).unwrap();
        assert!(content.contains("@kilocode/plugin"));
        assert!(!content.contains("@opencode-ai/plugin"));
        assert!(content.contains("[\"checkpoint\", \"kilo\", \"--hook-input\", \"stdin\"]"));
        assert!(content.contains("process.env.KILO_PLATFORM"));
        assert!(content.contains("process.env.KILO_CLIENT"));
        assert!(content.contains("process.env.KILO_EDITOR_NAME"));
        assert!(content.contains("process.env.KILO_DB"));
        assert!(content.contains("const GIT_AI_BIN = \"/usr/local/bin/git-ai\""));
        assert!(!content.contains("__GIT_AI_BINARY_PATH__"));
    }

    #[test]
    fn test_kilo_plugin_only_tracks_call_after_pre_checkpoint_succeeds() {
        let content = KiloInstaller::generate_plugin_content(&params().binary_path).unwrap();
        let pre_checkpoint = content
            .find("await runCheckpoint(hookInput)")
            .expect("pre checkpoint call");
        let track_pending_call = content
            .find("pendingCalls.set(callID")
            .expect("pending call insertion");

        assert!(
            pre_checkpoint < track_pending_call,
            "a failed Kilo pre checkpoint must not enable a post-only checkpoint"
        );
    }

    #[test]
    fn test_kilo_plugin_windows_path_is_escaped() {
        let content = KiloInstaller::generate_plugin_content(Path::new(
            r"C:\Users\developer\.git-ai\bin\git-ai.exe",
        ))
        .unwrap();
        assert!(
            content
                .contains(r#"const GIT_AI_BIN = "C:\\Users\\developer\\.git-ai\\bin\\git-ai.exe""#)
        );
    }

    #[test]
    #[serial]
    fn test_kilo_install_uses_official_global_plugin_directory() {
        with_temp_home(|home| {
            let installer = KiloInstaller;
            let diff = installer.install_hooks(&params(), false).unwrap();
            assert!(diff.is_some());
            let path = home.join(".config/kilo/plugin/git-ai.ts");
            assert!(path.exists());
            assert!(
                fs::read_to_string(path)
                    .unwrap()
                    .contains("checkpoint\", \"kilo")
            );
        });
    }

    #[test]
    #[serial]
    fn test_kilo_config_is_enough_for_detection_when_cli_is_bundled_in_ide() {
        with_temp_home(|home| {
            fs::create_dir_all(home.join(".config/kilo")).unwrap();
            let result = KiloInstaller.check_hooks(&params()).unwrap();
            assert!(result.tool_installed);
            assert!(!result.hooks_installed);
        });
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn test_kilo_vscode_extension_is_detected_before_first_kilo_launch() {
        with_temp_home(|home| {
            with_fake_code_extensions(KILO_VSCODE_EXTENSION_ID, || {
                let installer = KiloInstaller;
                let result = installer.check_hooks(&params()).unwrap();
                assert!(result.tool_installed);
                assert!(!result.hooks_installed);

                installer.install_hooks(&params(), false).unwrap();
                assert!(home.join(".config/kilo/plugin/git-ai.ts").exists());
            });
        });
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn test_unrelated_vscode_extension_does_not_enable_kilo_installer() {
        with_temp_home(|_| {
            with_fake_code_extensions("evil.kilocode.kilo-code-wrapper", || {
                let result = KiloInstaller.check_hooks(&params()).unwrap();
                assert!(!result.tool_installed);
            });
        });
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn test_kilo_extension_is_detected_in_insiders_when_stable_does_not_have_it() {
        with_temp_home(|_| {
            with_fake_code_channel_extensions(
                "publisher.unrelated",
                Some(KILO_VSCODE_EXTENSION_ID),
                || {
                    let result = KiloInstaller.check_hooks(&params()).unwrap();
                    assert!(result.tool_installed);
                },
            );
        });
    }

    #[test]
    fn test_extension_query_error_is_not_reduced_to_not_installed() {
        let result = KiloInstaller::combine_vscode_extension_checks([
            Ok(false),
            Err(GitAiError::Generic("insiders query failed".to_string())),
        ]);
        assert!(result.is_err());
    }

    #[test]
    fn test_any_successful_extension_match_wins_over_other_query_errors() {
        let result = KiloInstaller::combine_vscode_extension_checks([
            Err(GitAiError::Generic("stable query failed".to_string())),
            Ok(true),
        ]);
        assert!(result.unwrap());
    }

    #[test]
    #[serial]
    fn test_kilo_install_removes_old_plural_plugin_copy() {
        with_temp_home(|home| {
            let legacy = home.join(".config/kilo/plugins/git-ai.ts");
            fs::create_dir_all(legacy.parent().unwrap()).unwrap();
            fs::write(&legacy, "// old duplicate").unwrap();

            KiloInstaller.install_hooks(&params(), false).unwrap();
            assert!(!legacy.exists());
            assert!(home.join(".config/kilo/plugin/git-ai.ts").exists());
        });
    }

    #[test]
    #[serial]
    fn test_kilo_install_honors_scoped_config_home_override() {
        let temp = TempDir::new().unwrap();
        let config_home = temp.path().join("managed-kilo-config");
        let previous = std::env::var_os("GIT_AI_KILO_CONFIG_HOME");
        unsafe {
            std::env::set_var("GIT_AI_KILO_CONFIG_HOME", &config_home);
        }

        KiloInstaller.install_hooks(&params(), false).unwrap();

        unsafe {
            match previous {
                Some(value) => std::env::set_var("GIT_AI_KILO_CONFIG_HOME", value),
                None => std::env::remove_var("GIT_AI_KILO_CONFIG_HOME"),
            }
        }
        assert!(config_home.join("plugin/git-ai.ts").exists());
    }

    #[test]
    #[serial]
    fn test_kilo_install_follows_native_config_directory_overrides() {
        let temp = TempDir::new().unwrap();
        let kilo_config = temp.path().join("explicit-kilo-config");
        let xdg_config = temp.path().join("xdg-config");
        let previous_kilo = std::env::var_os("KILO_CONFIG_DIR");
        let previous_xdg = std::env::var_os("XDG_CONFIG_HOME");
        unsafe {
            std::env::set_var("KILO_CONFIG_DIR", &kilo_config);
            std::env::set_var("XDG_CONFIG_HOME", &xdg_config);
        }

        KiloInstaller.install_hooks(&params(), false).unwrap();

        unsafe {
            match previous_kilo {
                Some(value) => std::env::set_var("KILO_CONFIG_DIR", value),
                None => std::env::remove_var("KILO_CONFIG_DIR"),
            }
            match previous_xdg {
                Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
                None => std::env::remove_var("XDG_CONFIG_HOME"),
            }
        }
        assert!(kilo_config.join("plugin/git-ai.ts").exists());
        assert!(!xdg_config.join("kilo/plugin/git-ai.ts").exists());
    }
}
