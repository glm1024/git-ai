/**
 * git-ai plugin for OpenCode
 *
 * This plugin integrates git-ai with OpenCode to track AI-generated code.
 * It uses the tool.execute.before and tool.execute.after events to create
 * checkpoints that mark code changes as human or AI-authored.
 *
 * Installation:
 *   - Automatically installed by `git-ai install-hooks`
 *   - Or manually copy to ~/.config/opencode/plugins/git-ai.ts (global)
 *   - Or to .opencode/plugins/git-ai.ts (project-local)
 *
 * Requirements:
 *   - git-ai must be installed (path is injected at install time)
 *
 * @see https://github.com/git-ai-project/git-ai
 * @see https://opencode.ai/docs/plugins/
 */

import type { Plugin } from "@opencode-ai/plugin"
import { spawn } from "child_process"
import { readFile, stat } from "fs/promises"
import { dirname, isAbsolute, join, resolve } from "path"

// Absolute path to git-ai binary, replaced at install time by `git-ai install-hooks`
const GIT_AI_BIN = "__GIT_AI_BINARY_PATH__"
const CHECKPOINT_TIMEOUT_MS = 10_000
const CHECKPOINT_ARGS = ["checkpoint", "opencode", "--strict-errors", "--hook-input", "stdin"]

// Tools that modify files and should be tracked
const FILE_EDIT_TOOLS = new Set([
  "edit",
  "write",
  "patch",
  "multiedit",
  "apply_patch",
  "applypatch",
])

const APPLY_PATCH_FILE_PREFIXES = [
  "*** Update File: ",
  "*** Add File: ",
  "*** Delete File: ",
  "*** Move to: ",
]

const isEditTool = (toolName: string): boolean => FILE_EDIT_TOOLS.has(toolName.toLowerCase())

const isBashTool = (toolName: string): boolean => {
  const name = toolName.toLowerCase()
  return name === "bash" || name === "shell"
}

const normalizePath = (rawPath: string, cwd?: string): string | null => {
  const trimmed = rawPath.trim().replace(/^['"]|['"]$/g, "")
  if (!trimmed) {
    return null
  }

  const withoutScheme = trimmed
    .replace(/^file:\/\/localhost/, "")
    .replace(/^file:\/\//, "")

  const isWindowsAbs = /^[a-zA-Z]:[\\/]/.test(withoutScheme)
  if (isAbsolute(withoutScheme) || isWindowsAbs) {
    return withoutScheme
  }

  // Use provided cwd, or fall back to process.cwd() for relative paths
  const resolvedCwd = cwd || process.cwd()
  return join(resolvedCwd, withoutScheme)
}

const collectApplyPatchPaths = (raw: string, out: Set<string>): void => {
  for (const line of raw.split("\n")) {
    const trimmed = line.trim()
    for (const prefix of APPLY_PATCH_FILE_PREFIXES) {
      if (trimmed.startsWith(prefix)) {
        const path = trimmed.slice(prefix.length).trim().replace(/^['"]|['"]$/g, "")
        if (path) {
          out.add(path)
        }
      }
    }
  }
}

const collectToolPaths = (value: unknown, out: Set<string>): void => {
  if (typeof value === "string") {
    if (value.startsWith("file://")) {
      out.add(value)
    }
    collectApplyPatchPaths(value, out)
    return
  }

  if (Array.isArray(value)) {
    for (const item of value) {
      collectToolPaths(item, out)
    }
    return
  }

  if (!value || typeof value !== "object") {
    return
  }

  for (const [key, val] of Object.entries(value)) {
    const keyLower = key.toLowerCase()
    const isSinglePathKey = keyLower === "file_path" || keyLower === "filepath" || keyLower === "path" || keyLower === "fspath"
    const isMultiPathKey = keyLower === "files" || keyLower === "filepaths" || keyLower === "file_paths"

    if (isSinglePathKey && typeof val === "string") {
      out.add(val)
    } else if (isMultiPathKey) {
      if (typeof val === "string") {
        out.add(val)
      } else if (Array.isArray(val)) {
        for (const item of val) {
          if (typeof item === "string") {
            out.add(item)
          }
        }
      }
    }

    collectToolPaths(val, out)
  }
}

const extractFilePaths = (args: unknown, cwd?: string): string[] => {
  const rawPaths = new Set<string>()
  collectToolPaths(args, rawPaths)

  const normalizedPaths = new Set<string>()
  for (const rawPath of rawPaths) {
    const normalized = normalizePath(rawPath, cwd)
    if (normalized) {
      normalizedPaths.add(normalized)
    }
  }

  return [...normalizedPaths]
}

type ToolHookInput = {
  tool?: unknown
  sessionID?: unknown
  callID?: unknown
  args?: unknown
}

const asRecord = (value: unknown): Record<string, unknown> | undefined => {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return undefined
  }

  return value as Record<string, unknown>
}

const hookString = (value: unknown): string => typeof value === "string" ? value : ""

const pendingCallKey = (sessionID: string, callID: string): string => JSON.stringify([sessionID, callID])

type GitFileScope = {
  filePaths: string[]
  repoDirs: string[]
}

const extractToolCwd = (args: Record<string, unknown> | undefined): string | undefined => {
  if (typeof args?.workdir === "string") return args.workdir
  if (typeof args?.cwd === "string") return args.cwd
  return undefined
}

const debugEnabled = (): boolean => {
  const value = process.env.GIT_AI_OPENCODE_DEBUG ?? process.env.GIT_AI_DEBUG
  return value === "1" || value?.toLowerCase() === "true"
}

const debugLog = (message: string, error?: unknown): void => {
  if (!debugEnabled()) {
    return
  }

  try {
    const detail = error instanceof Error
      ? `${error.name}: ${error.message}`
      : error === undefined
        ? ""
        : String(error)
    console.error(`[git-ai opencode] ${message}${detail ? `: ${detail}` : ""}`)
  } catch {
    // Debug logging must never be the reason a hook fails.
  }
}

const errorCode = (error: unknown): string | undefined => {
  if (!error || typeof error !== "object") {
    return undefined
  }
  const code = (error as { code?: unknown }).code
  return typeof code === "string" ? code : undefined
}

const isMissingPathError = (error: unknown): boolean => {
  const code = errorCode(error)
  return code === "ENOENT" || code === "ENOTDIR"
}

const failClosedHook = <Args extends unknown[]>(
  label: string,
  hook: (...args: Args) => Promise<void>,
): ((...args: Args) => Promise<void>) => {
  return async (...args) => {
    try {
      await hook(...args)
    } catch (error) {
      const failure = error instanceof Error ? error : new Error(String(error))
      try {
        console.error(`[git-ai opencode] ${label}: ${failure.message}`)
      } catch {
        // Reporting must not replace the checkpoint failure that blocks the operation.
      }
      throw failure
    }
  }
}

const runCheckpoint = (hookInput: string): Promise<void> => {
  return new Promise((resolve, reject) => {
    let settled = false
    let timeout: ReturnType<typeof setTimeout> | undefined
    const finish = (error: Error | null): void => {
      if (settled) {
        return
      }

      settled = true
      if (timeout) {
        clearTimeout(timeout)
      }
      if (error) {
        reject(error)
      } else {
        resolve()
      }
    }

    const child = spawn(GIT_AI_BIN, CHECKPOINT_ARGS, {
      stdio: ["pipe", "ignore", "pipe"],
    })

    timeout = setTimeout(() => {
      try {
        child.kill("SIGTERM")
      } catch (error) {
        debugLog("failed to kill timed-out checkpoint command", error)
      }
      finish(new Error(`git-ai checkpoint opencode timed out after ${CHECKPOINT_TIMEOUT_MS}ms`))
    }, CHECKPOINT_TIMEOUT_MS)

    const stderr: Buffer[] = []

    child.stderr.on("data", (chunk: Buffer) => stderr.push(chunk))
    child.stderr.on("error", (error) => {
      debugLog("failed to read checkpoint stderr", error)
    })
    child.stdin.on("error", () => {
      // The child may exit before stdin is fully written; close/error handling below reports failures.
    })
    child.on("error", finish)
    child.on("close", (code) => {
      if (code === 0) {
        finish(null)
        return
      }

      const stderrText = Buffer.concat(stderr).toString().trim()
      finish(new Error(`git-ai checkpoint opencode exited with ${code}${stderrText ? `: ${stderrText}` : ""}`))
    })

    child.stdin.end(hookInput)
  })
}

export const GitAiPlugin: Plugin = async (ctx) => {
  try {
    return createGitAiPlugin(ctx)
  } catch (error) {
    const failure = error instanceof Error ? error : new Error(String(error))
    try {
      console.error(`[git-ai opencode] failed to initialize plugin: ${failure.message}`)
    } catch {
      // Preserve the original initialization failure.
    }
    throw failure
  }
}

const createGitAiPlugin = (ctx: Parameters<Plugin>[0]): Awaited<ReturnType<Plugin>> => {
  const { worktree, directory } = ctx
  const defaultCwd = worktree || directory || process.cwd()

  // OpenCode may reuse a callID in concurrent sessions, so both identifiers
  // are required to correlate the acknowledged pre-hook with its after-hook.
  const pendingCalls = new Map<string, {
    repoDirs: string[]
    filePaths: string[]
    sessionID: string
    toolName: string
    toolInput: unknown
  }>()
  const nonGitCalls = new Map<string, string>()

  const nearestExistingDirectory = async (pathHint: string): Promise<string | null> => {
    let candidate = pathHint
    while (candidate) {
      try {
        const fileStat = await stat(candidate)
        return fileStat.isDirectory() ? candidate : dirname(candidate)
      } catch (error) {
        if (!isMissingPathError(error)) {
          throw new Error(`failed to inspect path while resolving Git repository: ${candidate}`, { cause: error })
        }
      }

      const parent = dirname(candidate)
      if (parent === candidate) {
        break
      }
      candidate = parent
    }

    return null
  }

  const isGitDirPointer = async (gitFilePath: string, worktreeDir: string): Promise<boolean> => {
    const firstLine = (await readFile(gitFilePath, "utf8")).split(/\r?\n/, 1)[0]?.trim() ?? ""
    if (!firstLine.toLowerCase().startsWith("gitdir:")) {
      throw new Error(`invalid Git metadata pointer: ${gitFilePath}`)
    }

    const gitDir = firstLine.slice("gitdir:".length).trim()
    if (!gitDir) {
      throw new Error(`empty Git metadata pointer: ${gitFilePath}`)
    }

    const gitDirPath = isAbsolute(gitDir) || /^[a-zA-Z]:[\\/]/.test(gitDir)
      ? gitDir
      : resolve(worktreeDir, gitDir)
    const gitDirStat = await stat(gitDirPath)
    if (!gitDirStat.isDirectory()) {
      throw new Error(`Git metadata pointer is not a directory: ${gitDirPath}`)
    }
    return true
  }

  const hasGitMetadata = async (dir: string): Promise<boolean> => {
    const marker = join(dir, ".git")
    try {
      const fileStat = await stat(marker)
      if (fileStat.isDirectory()) {
        return true
      }

      if (fileStat.isFile()) {
        return await isGitDirPointer(marker, dir)
      }
      throw new Error(`unsupported Git metadata entry: ${marker}`)
    } catch (error) {
      if (isMissingPathError(error)) {
        return false
      }
      throw new Error(`failed to inspect Git metadata at ${marker}`, { cause: error })
    }
  }

  // Helper to find git repo root from a file path or directory
  const findGitRepo = async (pathHint: string): Promise<string | null> => {
    let dir = await nearestExistingDirectory(pathHint)
    while (dir) {
      if (await hasGitMetadata(dir)) {
        return dir
      }

      const parent = dirname(dir)
      if (parent === dir) {
        break
      }
      dir = parent
    }

    return null
  }

  const resolveGitFileScope = async (filePaths: string[]): Promise<GitFileScope> => {
    const scopedFilePaths: string[] = []
    const repoDirs = new Set<string>()
    const reposByPath = new Map<string, string | null>()

    for (const filePath of filePaths) {
      let repoDir = reposByPath.get(filePath)
      if (repoDir === undefined) {
        repoDir = await findGitRepo(filePath)
        reposByPath.set(filePath, repoDir)
      }
      if (!repoDir) {
        continue
      }

      scopedFilePaths.push(filePath)
      repoDirs.add(repoDir)
    }

    return {
      filePaths: scopedFilePaths,
      repoDirs: [...repoDirs],
    }
  }

  const resolveCwd = (cwd?: string): string => {
    if (!cwd) {
      return defaultCwd
    }

    return normalizePath(cwd, defaultCwd) || defaultCwd
  }

  const extractMetadataFilePaths = (metadata: unknown, cwd?: string): string[] => {
    if (!metadata || typeof metadata !== "object") {
      return []
    }

    const files = (metadata as { files?: unknown }).files
    if (!Array.isArray(files)) {
      return []
    }

    const paths = new Set<string>()
    for (const file of files) {
      if (!file || typeof file !== "object") {
        continue
      }

      const filePath = (file as { filePath?: unknown; path?: unknown }).filePath ?? (file as { path?: unknown }).path
      if (typeof filePath === "string") {
        const normalized = normalizePath(filePath, cwd ?? defaultCwd)
        if (normalized) {
          paths.add(normalized)
        }
      }
    }

    return [...paths]
  }

  const withMetadataFilePaths = (toolInput: unknown, filePaths: string[]): unknown => {
    if (filePaths.length === 0) {
      return toolInput
    }

    if (toolInput && typeof toolInput === "object" && !Array.isArray(toolInput)) {
      return {
        ...toolInput,
        file_paths: filePaths,
      }
    }

    return {
      input: toolInput,
      file_paths: filePaths,
    }
  }

  return {
    "tool.execute.before": failClosedHook(
      "pre-tool checkpoint failed",
      async (input: ToolHookInput, output?: { args?: unknown }) => {
        const toolName = hookString(input.tool)
        const isTrackedEdit = isEditTool(toolName)
        const isTrackedBash = isBashTool(toolName)
        if (!isTrackedEdit && !isTrackedBash) {
          return
        }

        const callID = hookString(input.callID)
        const sessionID = hookString(input.sessionID)
        if (!callID || !sessionID) {
          throw new Error("tracked tool call is missing callID or sessionID")
        }
        const toolInput = output?.args ?? input.args
        const toolCwd = resolveCwd(extractToolCwd(asRecord(toolInput)))
        const filePaths = isTrackedEdit ? extractFilePaths(toolInput, toolCwd) : []
        if (isTrackedEdit && filePaths.length === 0) {
          throw new Error(`tracked edit tool ${toolName} did not expose a file path`)
        }
        const gitScope = isTrackedEdit ? await resolveGitFileScope(filePaths) : undefined
        const callKey = pendingCallKey(sessionID, callID)
        if (pendingCalls.has(callKey) || nonGitCalls.has(callKey)) {
          throw new Error(`duplicate tracked tool call identity for session ${sessionID} and call ${callID}`)
        }
        const repoDir = isTrackedEdit
          ? gitScope?.repoDirs[0] ?? null
          // The resolved tool cwd is authoritative for Bash. Falling back to
          // the plugin process/default cwd would snapshot an unrelated repo
          // when an explicit non-Git workdir was requested.
          : await findGitRepo(toolCwd)
        if (!repoDir) {
          // Editing outside Git is intentionally out of scope. Remember the
          // correlation so the matching after-hook is not mistaken for data loss.
          nonGitCalls.set(callKey, toolName)
          return
        }

        const hookInput = JSON.stringify({
          hook_event_name: "PreToolUse",
          session_id: sessionID,
          tool_use_id: callID,
          cwd: repoDir,
          tool_name: toolName,
          tool_input: toolInput,
          ...(gitScope ? { git_ai_file_paths: gitScope.filePaths } : {}),
        })
        await runCheckpoint(hookInput)
        pendingCalls.set(callKey, {
          repoDirs: gitScope?.repoDirs ?? [repoDir],
          filePaths: gitScope?.filePaths ?? [],
          sessionID,
          toolName,
          toolInput,
        })
      },
    ),

    "tool.execute.after": failClosedHook(
      "post-tool checkpoint failed",
      async (input: ToolHookInput, output?: { metadata?: unknown }) => {
        const toolName = hookString(input.tool)
        if (!isEditTool(toolName) && !isBashTool(toolName)) {
          return
        }

        const callID = hookString(input.callID)
        const sessionID = hookString(input.sessionID)
        if (!callID || !sessionID) {
          throw new Error("tracked post-tool call is missing callID or sessionID")
        }
        const callKey = pendingCallKey(sessionID, callID)
        const nonGitToolName = nonGitCalls.get(callKey)
        if (nonGitToolName !== undefined) {
          if (nonGitToolName !== toolName) {
            throw new Error(
              `post-tool checkpoint tool mismatch for ${callID}: expected ${nonGitToolName}, received ${toolName}`,
            )
          }
          nonGitCalls.delete(callKey)
          // The acknowledged before-hook already established that the explicit
          // edit target was outside Git. Do not let after-hook metadata or a
          // different cwd reclassify the same call into an unrelated repository.
          return
        }
        const callInfo = pendingCalls.get(callKey)

        if (callInfo && callInfo.toolName !== toolName) {
          throw new Error(`post-tool checkpoint tool mismatch for ${callID}: expected ${callInfo.toolName}, received ${toolName}`)
        }

        const toolCwd = resolveCwd(extractToolCwd(asRecord(input.args)))
        const metadataFilePaths = extractMetadataFilePaths(output?.metadata, toolCwd)
        const postToolInput = input.args ?? callInfo?.toolInput
        const toolInput = withMetadataFilePaths(postToolInput, metadataFilePaths)
        const extractedFilePaths = isEditTool(toolName) ? extractFilePaths(toolInput, toolCwd) : []
        if (isEditTool(toolName) && !callInfo && extractedFilePaths.length === 0) {
          throw new Error(`tracked post-edit tool ${toolName} did not expose a file path`)
        }
        const candidateFilePaths = callInfo
          // The acknowledged pre-hook scope is authoritative in this process.
          // After-hook metadata may report extra touched files, but it must not
          // expand the AI-attributed scope beyond that durable pre evidence.
          ? callInfo.filePaths
          : extractedFilePaths
        const gitScope = isEditTool(toolName)
          ? await resolveGitFileScope(candidateFilePaths)
          : undefined

        if (callInfo && gitScope) {
          const beforeRepos = new Set(callInfo.repoDirs)
          const afterRepos = new Set(gitScope.repoDirs)
          const missingRepos = callInfo.repoDirs.filter((repo) => !afterRepos.has(repo))
          const unexpectedRepos = gitScope.repoDirs.filter((repo) => !beforeRepos.has(repo))
          if (missingRepos.length > 0 || unexpectedRepos.length > 0) {
            throw new Error(
              `post-tool checkpoint repository scope changed for ${callID}`
              + `${missingRepos.length > 0 ? `; missing: ${missingRepos.join(", ")}` : ""}`
              + `${unexpectedRepos.length > 0 ? `; unexpected: ${unexpectedRepos.join(", ")}` : ""}`,
            )
          }
        }

        const repoDir = isEditTool(toolName)
          ? gitScope?.repoDirs[0] ?? null
          : await findGitRepo(toolCwd)
        if (!repoDir) {
          if (callInfo) {
            throw new Error(`post-tool checkpoint lost the acknowledged repository scope for ${callID}`)
          }
          // A matching pre may have run in another plugin process. Re-resolving
          // from the after-hook is the correctness path; genuinely non-Git
          // edits remain intentionally out of scope.
          return
        }
        if (callInfo && isBashTool(toolName)
          && (callInfo.repoDirs.length !== 1 || callInfo.repoDirs[0] !== repoDir)) {
          throw new Error(
            `post-tool checkpoint repository scope changed for ${callID}`
            + `; expected: ${callInfo.repoDirs.join(", ")}; received: ${repoDir}`,
          )
        }

        const hookInput = JSON.stringify({
          hook_event_name: "PostToolUse",
          session_id: sessionID,
          tool_use_id: callID,
          cwd: repoDir,
          tool_name: toolName,
          tool_input: toolInput,
          ...(gitScope ? { git_ai_file_paths: gitScope.filePaths } : {}),
        })
        await runCheckpoint(hookInput)
        pendingCalls.delete(callKey)
      },
    ),
  }
}

export default GitAiPlugin
