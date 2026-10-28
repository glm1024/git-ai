# Git AI hooks for ZCode

The ZCode installer registers synchronous native `process` hooks for ZCode 3.16.3's user configuration. It does not start or modify the ZCode application package. If ZCode is running when hooks are installed or updated, fully quit and reopen ZCode, then start a new session. Creating another task inside the already-running desktop runtime does not reliably reload hook configuration; this was reproduced with ZCode 3.16.3 during live acceptance.

The default configuration path on Windows, macOS, and Linux is `~/.zcode/cli/config.json`. If ZCode is launched with a different user config, set `GIT_AI_ZCODE_CONFIG_PATH` to that complete path when running Git AI's hook installer. `~/` is expanded against the current user's home. This variable configures Git AI only; the ZCode launch configuration must select the same file. ZCode's `ZCODE_DATA_BASE_DIR` is not a replacement for the CLI user config path.

For a custom path, the same override must also be inherited by ZCode's hook subprocesses; the installer's environment is not persisted into the process-hook command. `GIT_AI_ZCODE_STORAGE_PATH` can explicitly select the database in that environment. Relative config and database paths are rejected because the installer, tool and daemon can have different working directories.

`git-ai install-hooks --dry-run` previews changes for detected integrations. `git-ai install-hooks` installs them. The installer detects ZCode's CLI/configuration and the macOS application bundle. A custom config without an existing directory is not automatically detected; create/select the config used by ZCode first.

The registered events are:

| Event | Matcher |
| --- | --- |
| `PreToolUse`, `PostToolUse`, `PostToolUseFailure` | `Write|Edit|ApplyPatch|Bash` |
| `SessionStart`, `UserPromptSubmit`, `Stop` | `*` |

Each hook executes the absolute installed Git AI binary with separate arguments:

```text
checkpoint zcode --strict-errors --hook-input stdin
```

Hooks use `type: process`, `timeoutMs: 30000`, and no asynchronous setting. The process arguments preserve paths containing spaces without shell quoting. Strict errors report capture failures; actual ZCode blocking behavior, timeouts, cancellation, and tool path rewriting require validation in a real new session.

The CLI's checkpoint receipt acknowledges transport acceptance. The daemon subsequently admits and applies the durable job asynchronously; a receipt is not proof that attribution succeeded. Use the existing `git-ai await` diagnostics to confirm processing. Invalid pairing, scope changes and attempts to revive failed calls are rejected during admission and cannot create AI attribution.

Installation preserves unrelated user settings and hooks, and deduplicates only `process` commands whose executable is `git-ai`/`git-ai.exe` and whose first arguments are `checkpoint zcode`. Uninstallation removes only those hooks and leaves the global enabled state intact. If existing hooks are disabled, or the global enabled flag is absent while hooks already exist, installation stops with a diagnostic and preserves the configuration. Enable the intended hook set explicitly before retrying; installation must not enable unrelated disabled hooks as a side effect.

Writes take an exclusive lock beside the resolved configuration target, reread the configuration after acquiring it, and persist a synced temporary file by atomic replacement. Existing symlinks are preserved; links and direct access to the same target share a lock. Cooperating Git AI installers are serialized. Editors or ZCode processes that bypass that lock remain outside the concurrency guarantee. Invalid JSON or hook structure is reported without replacing the configuration. Dry runs do not create configuration directories or lock files.

Validation in this workspace does not install Git AI, change real user configuration, or launch ZCode. Targeted installer checks:

```sh
task test TEST_FILTER=mdm::agents::zcode CARGO_TEST_ARGS=--lib
```

Native Windows/Linux execution and real ZCode hook delivery remain separate acceptance steps.
