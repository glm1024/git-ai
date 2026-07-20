# Kilo v7 integration

This fork treats Kilo v7 as a first-class Git AI agent. The integration covers
Kilo Agent edits from VS Code, JetBrains, and the Kilo CLI. It does not collect
IDE Tab/autocomplete activity.

## Architecture

`git-ai install-hooks` installs the managed plugin at Kilo's global plugin
location:

- macOS/Linux: `~/.config/kilo/plugin/git-ai.ts`
- Windows: `%USERPROFILE%\.config\kilo\plugin\git-ai.ts`

The installer generates that plugin from the canonical OpenCode adapter in
`agent-support/opencode/git-ai.ts` with a small, fail-closed set of
substitutions. Kilo keeps the same tool hook contract and SQLite transcript
schema, so this avoids a second large TypeScript fork while keeping Kilo's
identity, paths, runtime dimensions, and checkpoint preset separate.

The generated plugin calls:

```text
git-ai checkpoint kilo --hook-input stdin
```

It tracks Kilo's built-in file edit/write/patch tools and bash/shell changes.
Each checkpoint carries the Kilo runtime channel (`vscode`, `jetbrains`, or
`cli`), editor name when available, the active Kilo database path, and the raw
Kilo session ID. Git AI reads the matching session and transcript from Kilo's
OpenCode-compatible SQLite database and emits Git AI checkpoints, transcript
metrics, and commit attribution with tool identity `kilo`.

## Installation and verification

Build or install this fork's `git-ai` binary, then run:

```bash
git-ai install-hooks --dry-run=false
```

Restart Kilo after installation. Verify Kilo sees the managed plugin:

```bash
kilo debug info
```

The output should list `~/.config/kilo/plugin/git-ai.ts`. After a Kilo Agent
edit and a Git commit, verify local attribution with:

```bash
git log -1 --show-notes=ai
git-ai stats
```

Kilo v7 may install `@kilocode/plugin` into its config directory when it first
loads any external plugin. For offline enterprise clients, pre-provision that
normal Kilo plugin dependency/cache as part of the Kilo installation package;
Git AI checkpointing and metric buffering remain local/offline-first after the
plugin is loaded.

## Runtime and deployment contract

- `KILO_PLATFORM` / `KILO_CLIENT` identify VS Code, JetBrains, or CLI.
- `KILO_EDITOR_NAME` supplies the concrete IDE name when present.
- `KILO_DB` supplies a non-default database path when Kilo uses one.
- `KILO_CONFIG_DIR` and `XDG_CONFIG_HOME` are honored exactly as Kilo honors
  them when resolving the managed plugin directory.
- `GIT_AI_KILO_STORAGE_PATH` is test-only database discovery override.
- `GIT_AI_KILO_CONFIG_HOME` is a scoped installer/managed-deployment override;
  it has highest precedence and must point at the Kilo config root that
  contains `plugin/`.
- `GIT_AI_KILO_DEBUG=1` enables hook diagnostics without changing failure
  isolation: Kilo edits are never blocked by Git AI errors.

The production backend can receive Kilo v7 Git AI events directly in
`official` mode. Shadow ingestion is not a prerequisite. Keep the existing
mode switch only as an operational rollback control; switching modes never
deletes raw events or already-confirmed facts.

## Boundaries

- Tab/autocomplete is intentionally out of scope.
- Agent tools not exposing a file path and arbitrary MCP tools that mutate
  files outside Kilo's edit/write/patch or bash/shell hooks cannot be attributed
  reliably and remain unknown.
- Kilo checkpoints are accepted-edit facts. Formal committed/warehouse facts
  still require Git AI commit attribution and backend target-branch
  confirmation.
- The legacy plural plugin path `~/.config/kilo/plugins/git-ai.ts` is removed by
  installation to prevent duplicate hooks; the official singular `plugin/`
  directory is authoritative.

## Upstream maintenance

When merging a newer upstream Git AI release, preserve the additive Kilo files
and registrations. If upstream changes the OpenCode plugin anchors, Kilo plugin
generation fails closed instead of silently producing a partial adapter. Update
the substitutions and run both Kilo and OpenCode regression tests before
releasing.
