//! Kilo v7 plugin installer.
//!
//! Kilo v7 intentionally keeps the OpenCode plugin hook contract. The managed
//! Kilo plugin is therefore generated from the canonical OpenCode adapter at
//! install time with a small, fail-closed set of Kilo-specific substitutions.
//! This keeps upstream OpenCode fixes flowing into Kilo without maintaining a
//! second large TypeScript copy in this fork.

use crate::error::GitAiError;
use crate::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
use crate::mdm::utils::{binary_exists, generate_diff, home_dir, write_atomic};
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

pub struct KiloInstaller;

impl KiloInstaller {
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
        let has_global_config = Self::config_root().exists();
        let has_local_config = Path::new(".kilo").exists() || Path::new(".kilocode").exists();

        if !has_binary && !has_global_config && !has_local_config {
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
        unsafe {
            std::env::set_var("HOME", temp.path());
            std::env::set_var("USERPROFILE", temp.path());
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
        }
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
