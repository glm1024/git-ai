/**
 * git-ai plugin for OpenCode
 *
 * This plugin integrates git-ai with OpenCode to track AI-generated code.
 * It uses the tool execute before/after hooks to create checkpoints that mark
 * code changes as human or AI-authored.
 *
 * Supports both plugin APIs from a single default export:
 *   - OpenCode V2 (`@opencode/plugin`): `setup(ctx)` registers the hooks through
 *     `ctx.tool.hook("execute.before" | "execute.after", ...)`.
 *   - OpenCode V1 (`@opencode-ai/plugin`, >= 1.18.29): `server()` returns the
 *     legacy `"tool.execute.before" | "tool.execute.after"` hook object.
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
 * @see https://opencode.ai/v2/docs/build/plugins
 */

import type { Plugin } from "@opencode/plugin"
import { spawn } from "child_process"
import { createHash } from "crypto"
import { readFile, stat } from "fs/promises"
import { dirname, isAbsolute, join, resolve } from "path"

// Absolute path to git-ai binary, replaced at install time by `git-ai install-hooks`
const GIT_AI_BIN = "__GIT_AI_BINARY_PATH__"
const CHECKPOINT_TIMEOUT_MS = 10_000
const CHECKPOINT_KILL_GRACE_MS = 250
const MAX_STDERR_BYTES = 64 * 1024
const MAX_PENDING_CALLS = 64
const MAX_PENDING_BYTES = 8 * 1024 * 1024
const MAX_CALL_BYTES = 1024 * 1024
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

const normalizeToolName = (toolName: string): string => {
  const lower = toolName.toLowerCase()
  return lower.startsWith("functions.") ? lower.slice("functions.".length) : lower
}

const isEditTool = (toolName: string): boolean => FILE_EDIT_TOOLS.has(normalizeToolName(toolName))

const isBashTool = (toolName: string): boolean => {
  const name = normalizeToolName(toolName)
  return name === "bash" || name === "shell" || name === "bash_tool"
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
  // Scan header candidates only. Splitting a large edit/patch body creates a
  // separate array entry and string for every line even when it has no paths.
  let offset = raw.indexOf("*** ")
  while (offset !== -1) {
    const lineStart = raw.lastIndexOf("\n", offset - 1) + 1
    const newline = raw.indexOf("\n", offset)
    const lineEnd = newline === -1 ? raw.length : newline
    if (raw.slice(lineStart, offset).trim() === "") {
      const header = raw.slice(offset, lineEnd).trim()
      for (const prefix of APPLY_PATCH_FILE_PREFIXES) {
        if (header.startsWith(prefix)) {
          const path = header.slice(prefix.length).trim().replace(/^['"]|['"]$/g, "")
          if (path) {
            out.add(path)
          }
        }
      }
    }
    offset = newline === -1 ? -1 : raw.indexOf("*** ", newline + 1)
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
  cwd?: string
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

/**
 * Normalized checkpoint event used by the shared core. Both plugin APIs map
 * their own hook payload into this shape.
 */
type CheckpointEvent = {
  tool?: unknown
  sessionID?: unknown
  callID?: unknown
  messageID?: unknown
  toolInput?: unknown
  metadata?: unknown
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

const reportedErrors = new WeakSet<Error>()

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
        if (!reportedErrors.has(failure)) {
          reportedErrors.add(failure)
          console.error(`[git-ai opencode] ${label}: ${failure.message}`)
        }
      } catch {
        // Reporting must not replace the checkpoint failure that blocks the operation.
      }
      throw failure
    }
  }
}

const runCheckpoint = (hookInput: string): Promise<void> => {
  return new Promise((resolve, reject) => {
    let timeout: ReturnType<typeof setTimeout> | undefined
    let killTimeout: ReturnType<typeof setTimeout> | undefined
    let timeoutError: Error | undefined
    let closed = false
    let exited = false
    let stderr: Buffer | undefined
    let stderrBytes = 0
    let stderrTruncated = false
    const finish = (error: Error | null): void => {
      if (closed) return
      closed = true
      clearTimeout(timeout)
      clearTimeout(killTimeout)
      stderr = undefined
      if (error) reject(error)
      else resolve()
    }

    const child = spawn(GIT_AI_BIN, CHECKPOINT_ARGS, {
      windowsHide: true,
      stdio: ["pipe", "ignore", "pipe"],
    })
    const closeTimedOut = (): void => {
      // A descendant can inherit stderr after the checkpoint itself has exited.
      // Close only our pipes; never signal an unrelated process group or daemon.
      child.stdin.destroy()
      child.stderr.destroy()
      finish(timeoutError ?? null)
    }
    timeout = setTimeout(() => {
      timeoutError = new Error(`git-ai checkpoint opencode timed out after ${CHECKPOINT_TIMEOUT_MS}ms`)
      if (exited) return closeTimedOut()
      child.kill("SIGTERM")
      if (!closed) {
        killTimeout = setTimeout(() => {
          child.kill("SIGKILL")
          closeTimedOut()
        }, CHECKPOINT_KILL_GRACE_MS)
      }
    }, CHECKPOINT_TIMEOUT_MS)

    child.stderr.on("data", (chunk: Buffer) => {
      if (closed) return
      const remaining = MAX_STDERR_BYTES - stderrBytes
      const length = Math.min(chunk.length, remaining)
      if (length > 0) {
        stderr ??= Buffer.allocUnsafe(MAX_STDERR_BYTES)
        chunk.copy(stderr, stderrBytes, 0, length)
        stderrBytes += length
      }
      stderrTruncated ||= chunk.length > remaining
    })
    child.stderr.on("error", (error) => debugLog("failed to read checkpoint stderr", error))
    child.stdin.on("error", () => {
      // Early exit/EPIPE is reported through close/error, without an unhandled event.
    })
    child.on("exit", () => {
      exited = true
      if (timeoutError) closeTimedOut()
    })
    child.on("error", finish)
    child.on("close", (code) => {
      const detail = stderr?.toString("utf8", 0, stderrBytes).trim() ?? ""
      finish(timeoutError ?? (code === 0 ? null : new Error(
        `git-ai checkpoint opencode exited with ${code}${detail ? `: ${detail}` : ""}${stderrTruncated ? " [stderr truncated]" : ""}`,
      )))
    })
    child.stdin.end(hookInput)
  })
}

const createGitAiPlugin = (ctx: V1PluginContext, onPendingChange: (active: boolean) => void = () => {}) => {
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
    toolCwd: string
  }>()
  const nonGitCalls = new Map<string, string>()

  type Reservation = { sessionID: string; messageID: string; bytes: number; phase: "pre" | "ready" | "post"; cancelled: boolean }
  const reservations = new Map<string, Reservation>()
  const retiredCalls = new Set<string>()
  const completedCalls = new Set<string>()
  const callDigest = (key: string): string => createHash("sha256").update(key).digest("hex")
  let reconstructionBlocked = false
  const retireFailed = (key: string): void => {
    const digest = callDigest(key)
    if (retiredCalls.has(digest)) return
    if (retiredCalls.size >= MAX_PENDING_CALLS) {
      // Never forget a cancellation and later mistake its late completion for
      // cross-process recovery. Existing acknowledged calls can still finish.
      reconstructionBlocked = true
      return
    }
    retiredCalls.add(digest)
  }
  const complete = (key: string): void => {
    if (completedCalls.size >= MAX_PENDING_CALLS) completedCalls.delete(completedCalls.values().next().value!)
    completedCalls.add(callDigest(key))
  }
  let pendingBytes = 0
  let disposed = false
  const release = (key: string, call: Reservation): void => {
    if (reservations.get(key) !== call) return
    reservations.delete(key)
    pendingCalls.delete(key)
    nonGitCalls.delete(key)
    pendingBytes -= call.bytes
    onPendingChange(!disposed && reservations.size > 0)
  }
  const terminal = (sessionID: string, callID?: string, messageID?: string): void => {
    for (const [key, call] of reservations) {
      if (call.sessionID !== sessionID || (callID !== undefined && key !== pendingCallKey(sessionID, callID))
        || (messageID !== undefined && call.messageID !== messageID)) continue
      call.cancelled = true
      // Do not permit late post hooks to reconstruct a deliberately retired call.
      retireFailed(key)
      if (call.phase === "ready") release(key, call)
    }
  }
  const dispose = (): void => {
    disposed = true
    for (const call of reservations.values()) terminal(call.sessionID)
  }

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

  const hooks = {
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
        const toolCwd = resolveCwd(input.cwd ?? extractToolCwd(asRecord(toolInput)))
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
          toolCwd,
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

        const toolCwd = resolveCwd(callInfo?.toolCwd ?? extractToolCwd(asRecord(input.args)))
        const metadataFilePaths = extractMetadataFilePaths(output?.metadata, toolCwd)
        const postToolInput = callInfo?.toolInput ?? input.args
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
  const ownString = (value: string): string => Buffer.from(value, "utf16le").toString("utf16le")
  const prepareBefore = (event: CheckpointEvent): Promise<void> => {
    const toolName = hookString(event.tool)
    if (!isEditTool(toolName) && !isBashTool(toolName)) return Promise.resolve()
    if (disposed) throw new Error("git-ai plugin is disposed")
    const sessionID = ownString(hookString(event.sessionID))
    const callID = ownString(hookString(event.callID))
    if (!sessionID || !callID) throw new Error("tracked tool call is missing callID or sessionID")
    const key = pendingCallKey(sessionID, callID)
    if (reservations.has(key)) throw new Error("duplicate tracked tool call identity")
    const toolCwd = ownString(resolveCwd(extractToolCwd(asRecord(event.toolInput))))
    const input = asRecord(event.toolInput)
    const command = hookString(input?.command ?? input?.cmd)
    if (Buffer.byteLength(command) > MAX_CALL_BYTES) throw new Error("Checkpoint capacity limit: command is too large")
    const toolInput = isEditTool(toolName)
      ? { file_paths: extractFilePaths(event.toolInput, toolCwd).map(ownString) }
      : { command: ownString(command) }
    const bytes = Buffer.byteLength(JSON.stringify(toolInput)) + Buffer.byteLength(key + toolCwd + hookString(event.messageID))
    if (reservations.size >= MAX_PENDING_CALLS || bytes > MAX_CALL_BYTES || pendingBytes + bytes > MAX_PENDING_BYTES) {
      throw new Error("Checkpoint capacity limit reached")
    }
    const call: Reservation = { sessionID, messageID: ownString(hookString(event.messageID)), bytes, phase: "pre", cancelled: false }
    retiredCalls.delete(callDigest(key))
    completedCalls.delete(callDigest(key))
    reservations.set(key, call)
    pendingBytes += bytes
    onPendingChange(true)
    return (async () => {
      try {
        await hooks["tool.execute.before"]({ tool: ownString(toolName), sessionID, callID, args: toolInput, cwd: toolCwd })
        if (call.cancelled) release(key, call)
        else call.phase = "ready"
      } catch (error) {
        retireFailed(key)
        release(key, call)
        throw error
      }
    })()
  }
  // Canonicalize before suspending so large patch bodies never remain in
  // an async hook frame while repository inspection/checkpoint IO is pending.
  const reportPreFailure = (error: unknown): never => {
    const failure = error instanceof Error ? error : new Error(String(error))
    if (!reportedErrors.has(failure)) {
      reportedErrors.add(failure)
      try { console.error(`[git-ai opencode] pre-tool checkpoint failed: ${failure.message}`) } catch {}
    }
    throw failure
  }
  const before = (event: CheckpointEvent): Promise<void> => {
    try { return prepareBefore(event).catch(reportPreFailure) }
    catch (error) { return Promise.reject(error).catch(reportPreFailure) }
  }
  const after = failClosedHook("post-tool checkpoint failed", async (event: CheckpointEvent) => {
    if (!isEditTool(hookString(event.tool)) && !isBashTool(hookString(event.tool))) return
    if (disposed) return
    const sessionID = hookString(event.sessionID)
    const callID = hookString(event.callID)
    const key = pendingCallKey(sessionID, callID)
    if (retiredCalls.has(callDigest(key)) || completedCalls.has(callDigest(key))) return
    const call = reservations.get(key)
    if (!call && reconstructionBlocked) throw new Error("checkpoint recovery capacity exceeded; restart plugin before recovering an unmatched completion")
    if (call && (call.cancelled || call.phase !== "ready" || call.messageID !== hookString(event.messageID))) return
    if (call) call.phase = "post"
    try {
      await hooks["tool.execute.after"]({ tool: event.tool, sessionID, callID, args: event.toolInput }, { metadata: event.metadata })
      if (call) release(key, call)
      complete(key)
    } catch (error) {
      // Keep acknowledged scope for a post-checkpoint retry.
      if (call) call.phase = "ready"
      throw error
    }
  })
  return { before, after, terminal, dispose }
}

/**
 * OpenCode V2 plugin definition (`@opencode/plugin`).
 *
 * Hooks are registered on the tool domain; the callback receives a single event
 * (`id`, `input`, `tool`, `sessionID`, and `status`/`result` after execution).
 */
export const GitAiPlugin: Plugin.Plugin = {
  id: "git-ai",
  async setup(ctx) {
    try {
      const defaultCwd =
        ctx.location?.directory ?? ctx.location?.project?.directory ?? process.cwd()
      let controller: AbortController | undefined
      const { before, after, terminal, dispose } = createGitAiPlugin({ directory: defaultCwd }, (active) => {
        if (!active) {
          controller?.abort()
          controller = undefined
          return
        }
        if (controller || !ctx.event?.subscribe) return
        const current = new AbortController()
        controller = current
        // 2.2 filters before encoding; 2.1 supports signal but ignores types.
        // Subscribe only while calls are pending, and do no async work per event.
        void (async () => {
          const options = {
            signal: current.signal,
            types: ["session.tool.success", "session.tool.failed", "session.deleted"] as const,
          }
          for await (const event of ctx.event.subscribe(options)) {
            if (current.signal.aborted) break
            const data = asRecord(event.data)
            const sessionID = hookString(data?.sessionID)
            if (event.type === "session.deleted") terminal(sessionID)
            else if (event.type === "session.tool.success" || event.type === "session.tool.failed") {
              terminal(sessionID, hookString(data?.id), hookString(data?.assistantMessageID))
            }
          }
        })().catch((error) => debugLog("lifecycle subscription failed", error)).finally(() => {
          if (controller === current) controller = undefined
        })
      })

      await ctx.tool.hook("execute.before", (event) => before({
        tool: event.tool, sessionID: event.sessionID, callID: event.id,
        messageID: event.messageID, toolInput: event.input,
      }))
      await ctx.tool.hook("execute.after", (event) => {
        // The SDK invokes after for both completed and error outcomes. A
        // cancelled/failed tool must never turn its pre baseline into AI edits.
        if (event.status !== "completed") {
          terminal(event.sessionID, event.id, event.messageID)
          return
        }
        return after({
          tool: event.tool, sessionID: event.sessionID, callID: event.id,
          messageID: event.messageID, toolInput: event.input,
        })
      })
      return () => {
        controller?.abort()
        controller = undefined
        dispose()
      }
    } catch (error) {
      throw error
    }
  },
}

/**
 * OpenCode V1 plugin implementation (`@opencode-ai/plugin`).
 *
 * V1 calls `server(ctx)` and expects the returned hook object. Kept so the same
 * package keeps working on OpenCode 1.18.x while V2 support is live.
 */
type V1PluginContext = {
  worktree?: string
  directory?: string
}

type V1ToolHookInput = {
  tool?: unknown
  sessionID?: unknown
  callID?: unknown
  args?: unknown
}

export const GitAiPluginV1 = (ctx?: V1PluginContext) => {
  const defaultCwd = ctx?.worktree || ctx?.directory || process.cwd()
  const { before, after, terminal } = createGitAiPlugin({ directory: defaultCwd })

  return {
    event: async ({ event }: { event: { type?: string; properties?: Record<string, unknown> } }) => {
      const properties = event.properties
      if (event.type === "session.deleted") terminal(hookString(asRecord(properties?.info)?.id))
      if (event.type === "message.part.updated") {
        const part = asRecord(properties?.part)
        const state = asRecord(part?.state)
        if (part?.type === "tool" && (state?.status === "completed" || state?.status === "error")) {
          terminal(hookString(part.sessionID), hookString(part.callID))
        }
      }
    },
    "tool.execute.before": (input: V1ToolHookInput, output?: { args?: unknown }) =>
      before({
        tool: input.tool,
        sessionID: input.sessionID,
        callID: input.callID,
        toolInput: output?.args ?? input.args,
      }),
    "tool.execute.after": (input: V1ToolHookInput, output?: { metadata?: unknown }) =>
      after({
        tool: input.tool,
        sessionID: input.sessionID,
        callID: input.callID, toolInput: input.args, metadata: output?.metadata,
      }),
  }
}

export default {
  ...GitAiPlugin,
  async server(ctx?: V1PluginContext) {
    return GitAiPluginV1(ctx)
  },
}
