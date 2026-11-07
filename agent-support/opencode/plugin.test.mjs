import assert from "node:assert/strict"
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises"
import { tmpdir } from "node:os"
import { basename, join } from "node:path"
import { test } from "node:test"
import { pathToFileURL } from "node:url"
import ts from "typescript"
import childProcess from "node:child_process"
import { EventEmitter } from "node:events"
import { syncBuiltinESMExports } from "node:module"

test("installed plugin loads without node_modules and registers both APIs", async () => {
  const source = await readFile(new URL("./git-ai.ts", import.meta.url), "utf8")
  const compiled = ts.transpileModule(
    source.replace("__GIT_AI_BINARY_PATH__", "/usr/bin/git-ai"),
    { compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 } },
  ).outputText
  const directory = await mkdtemp(join(tmpdir(), "git-ai-opencode-plugin-"))

  try {
    const pluginPath = join(directory, "git-ai.mjs")
    await writeFile(pluginPath, compiled)
    const { default: plugin } = await import(pathToFileURL(pluginPath).href)

    assert.equal(plugin.id, "git-ai")
    assert.equal(typeof plugin.setup, "function")
    assert.equal(typeof plugin.server, "function")

    const hooks = []
    await plugin.setup({
      location: { directory },
      tool: { hook: async (name) => hooks.push(name) },
    })
    assert.deepEqual(hooks, ["execute.before", "execute.after"])

    const legacyHooks = await plugin.server({ directory })
    assert.equal(typeof legacyHooks["tool.execute.before"], "function")
    assert.equal(typeof legacyHooks["tool.execute.after"], "function")
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
})


// Use actual installed hooks and deterministic child lifecycles; no OpenCode SDK
// or git-ai installation is needed for subprocess/resource boundary tests.
async function fixture(t, childBehavior = (child) => child.emit("close", 0)) {
  const directory = await mkdtemp(join(tmpdir(), "git-ai-opencode-lifecycle-"))
  await mkdir(join(directory, ".git"))
  const source = await readFile(new URL("./git-ai.ts", import.meta.url), "utf8")
  const compiled = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 },
  }).outputText
  const pluginPath = join(directory, "git-ai.mjs")
  await writeFile(pluginPath, compiled)
  const calls = []
  const children = []
  const warnings = []
  t.mock.method(console, "error", (...args) => warnings.push(args.join(" ")))
  t.mock.method(childProcess, "spawn", (_command, args, options) => {
    assert.ok(args.includes("--strict-errors"))
    assert.equal(options.windowsHide, true)
    const child = new EventEmitter()
    child.stderr = new EventEmitter()
    child.stdin = new EventEmitter()
    child.stdin.destroy = () => { child.stdin.destroyed = true }
    child.stderr.destroy = () => { child.stderr.destroyed = true }
    child.stdin.end = (input) => {
      calls.push(JSON.parse(input))
      queueMicrotask(() => childBehavior(child))
    }
    child.kill = (signal) => { child.signals.push(signal); return true }
    child.signals = []
    children.push(child)
    return child
  })
  syncBuiltinESMExports()
  t.after(async () => {
    t.mock.restoreAll()
    syncBuiltinESMExports()
    await rm(directory, { recursive: true, force: true })
  })
  const { default: plugin } = await import(pathToFileURL(pluginPath).href)
  const hooks = new Map()
  let deliver
  let pending = []
  let subscriptions = 0
  let closed = 0
  const cleanup = await plugin.setup({
    location: { directory },
    tool: { hook: async (name, callback) => hooks.set(name, callback) },
    event: { subscribe: ({ signal }) => ({
      [Symbol.asyncIterator]() {
        subscriptions++
        const close = () => {
          closed++
          deliver?.({ done: true })
          deliver = undefined
          return Promise.resolve({ done: true })
        }
        signal.addEventListener("abort", close, { once: true })
        return {
          next: () => pending.length ? Promise.resolve({ value: pending.shift(), done: false }) : new Promise((resolve) => { deliver = resolve }),
          return: close,
        }
      },
    }) },
  })
  t.after(async () => { await cleanup?.() })
  const emit = async (event) => {
    if (deliver) { const resolve = deliver; deliver = undefined; resolve({ value: event, done: false }) }
    else pending.push(event)
    await new Promise(setImmediate)
  }
  return {
    before: hooks.get("execute.before"), after: (event) => hooks.get("execute.after")({ status: "completed", ...event }),
    plugin, directory, calls, children, warnings, emit, cleanup,
    subscriptions: () => subscriptions, closed: () => closed,
    event: (id = "call", input = { path: "a.ts" }, sessionID = "session") => ({ id, sessionID, messageID: "message", tool: "edit", input }),
  }
}

for (const type of ["session.tool.failed", "session.tool.success", "session.deleted"]) {
  test(`terminal ${type} releases only the affected pending call/session`, async (t) => {
    const f = await fixture(t)
    const abandoned = f.event("abandoned")
    const healthy = f.event("healthy", { path: "b.ts" }, "other-session")
    await f.before(abandoned)
    await f.before(healthy)
    await f.emit({ type, created: Date.now(), data: { sessionID: abandoned.sessionID, id: abandoned.id, assistantMessageID: abandoned.messageID } })
    await f.after(abandoned)
    await f.after(healthy)
    assert.deepEqual(f.calls.filter((x) => x.hook_event_name === "PostToolUse").map((x) => x.tool_use_id), ["healthy"])
    await f.cleanup?.()
  })
}

test("patch headers preserve order and ignore inline or added-line lookalikes", async (t) => {
  const f = await fixture(t)
  const event = { ...f.event(), tool: "patch", input: { patchText: [
    "inline *** Update File: wrong.ts",
    "*** Begin Patch",
    "  *** Update File: 'a.ts'",
    "+*** Add File: ignored.ts",
    "\t*** Add File: b.ts",
    "*** Delete File: c.ts",
    "*** Move to: d.ts",
    "*** End Patch",
  ].join("\r\n") } }
  await f.before(event)
  await f.after(event)
  assert.deepEqual(f.calls.map((call) => call.tool_input.file_paths.map((path) => basename(path))), [
    ["a.ts", "b.ts", "c.ts", "d.ts"],
    ["a.ts", "b.ts", "c.ts", "d.ts"],
  ])
})

test("cancellation during pre checkpoint cannot resurrect a pending call", async (t) => {
  const f = await fixture(t, () => {})
  const event = f.event()
  const pre = f.before(event)
  while (f.children.length === 0) await new Promise(setImmediate)
  await f.emit({ type: "session.tool.failed", created: Date.now(), data: { sessionID: event.sessionID, id: event.id, assistantMessageID: event.messageID } })
  f.children[0].emit("close", 0)
  await pre
  const after = f.after(event)
  await new Promise(setImmediate)
  for (const child of f.children) child.emit("close", 0)
  await after
  assert.equal(f.calls.length, 1, "cancelled reservation must not emit a post checkpoint")
  await f.cleanup?.()
})

test("pending capacity preserves existing long-running calls and resumes after completion", async (t) => {
  const f = await fixture(t)
  for (let i = 0; i < 64; i++) await f.before(f.event(`call-${i}`))
  await assert.rejects(f.before(f.event("call-64")), /capacity/i)
  assert.equal(f.calls.length, 64, "refuse new tracking instead of growing or evicting an active baseline")
  assert.ok(f.warnings.some((line) => /limit|capacity/i.test(line)), "capacity refusal must be visible without debug enabled")
  await f.after(f.event("call-0"))
  await f.before(f.event("replacement"))
  await f.after(f.event("replacement"))
  assert.equal(f.calls.length, 67)
  await f.cleanup?.()
})

test("shell checkpoints retain only bounded canonical command input", async (t) => {
  const f = await fixture(t)
  const event = { ...f.event(), tool: "shell", input: { cmd: "echo hello", unused: "x".repeat(1024 * 1024) } }
  await f.before(event)
  await f.after(event)
  assert.deepEqual(f.calls.map((x) => x.tool_input), [{ command: "echo hello" }, { command: "echo hello" }])
  await assert.rejects(f.before({ ...event, id: "oversized", input: { command: "x".repeat(1024 * 1024 + 1) } }), /capacity/i)
  assert.equal(f.calls.length, 2, "oversized commands must be rejected before pre capture")
  assert.ok(f.warnings.length)
  await f.cleanup?.()
})

test("plugin disposal releases pending state and the event subscription", async (t) => {
  const f = await fixture(t)
  const event = f.event()
  await f.before(event)
  assert.equal(typeof f.cleanup, "function")
  await f.cleanup()
  await f.after(event)
  assert.equal(f.calls.length, 1)
  assert.ok(f.closed() > 0)
})

test("checkpoint stderr is bounded even for a large single chunk", async (t) => {
  const previous = process.env.GIT_AI_OPENCODE_DEBUG
  process.env.GIT_AI_OPENCODE_DEBUG = "1"
  t.after(() => { if (previous === undefined) delete process.env.GIT_AI_OPENCODE_DEBUG; else process.env.GIT_AI_OPENCODE_DEBUG = previous })
  const f = await fixture(t, (child) => {
    child.stderr.emit("data", Buffer.alloc(4 * 1024 * 1024, "x"))
    child.emit("close", 23)
  })
  await assert.rejects(f.before(f.event()), /exited with 23/)
  assert.ok(f.warnings.join("").length < 66 * 1024, "stderr retained in the error must be bounded")
  assert.match(f.warnings.join(""), /truncat/i)
})

test("checkpoint timeout escalates to SIGKILL and clears timers on close", async (t) => {
  const f = await fixture(t, () => {})
  t.mock.timers.enable({ apis: ["setTimeout"] })
  const pre = assert.rejects(f.before(f.event()), /timed out/)
  while (f.children.length === 0) await new Promise(setImmediate)
  t.mock.timers.tick(10_000)
  assert.deepEqual(f.children[0].signals, ["SIGTERM"])
  t.mock.timers.tick(250)
  assert.deepEqual(f.children[0].signals, ["SIGTERM", "SIGKILL"])
  f.children[0].emit("close", null)
  await pre
  t.mock.timers.tick(10_000)
  assert.equal(f.children[0].signals.length, 2)
  await f.cleanup?.()
})


test("a delayed terminal from another assistant message cannot erase a new call", async (t) => {
  const f = await fixture(t)
  const event = f.event()
  await f.before(event)
  await f.emit({ type: "session.tool.failed", data: { sessionID: event.sessionID, id: event.id, assistantMessageID: "older-message" } })
  await f.after(event)
  assert.equal(f.calls.length, 2)
  await f.cleanup?.()
})

test("byte budget bounds accepted shell payloads without evicting an active command", async (t) => {
  const f = await fixture(t)
  const command = "x".repeat(600 * 1024)
  for (let i = 0; i < 20; i++) {
    try { await f.before({ ...f.event(`large-${i}`), tool: "shell", input: { command } }) }
    catch (error) { assert.match(error.message, /capacity/i) }
  }
  assert.ok(f.calls.length <= 13, "8 MiB total budget must bound accepted command strings")
  await f.after({ ...f.event("large-0"), tool: "shell" })
  assert.equal(f.calls.at(-1).hook_event_name, "PostToolUse")
  assert.equal(f.calls.at(-1).tool_input.command, command)
  await f.cleanup?.()
})

test("cancelled pre calls keep their capacity reservation until their child exits", async (t) => {
  const f = await fixture(t, () => {})
  const pending = []
  for (let i = 0; i < 64; i++) {
    const event = f.event(`slow-${i}`)
    pending.push(f.before(event))
    while (f.children.length <= i) await new Promise(setImmediate)
    await f.emit({ type: "session.tool.failed", data: { sessionID: event.sessionID, id: event.id, assistantMessageID: event.messageID } })
  }
  let extraFinished = false
  const extra = assert.rejects(f.before(f.event("extra")), /capacity/i).then(() => { extraFinished = true })
  while (!extraFinished && f.children.length === 64) await new Promise(setImmediate)
  const childCount = f.children.length
  for (const child of f.children) child.emit("close", 0)
  await Promise.all([...pending, extra])
  assert.equal(childCount, 64, "cancelled but running children must still consume admission capacity")
  await f.cleanup?.()
})


test("timeout settles and closes owned pipes even if an exited child's descendant holds them", async (t) => {
  const f = await fixture(t, (child) => child.emit("exit", 0))
  t.mock.timers.enable({ apis: ["setTimeout"] })
  let settled = false
  const pre = assert.rejects(f.before(f.event()), /timed out/).then(() => { settled = true })
  while (f.children.length === 0) await new Promise(setImmediate)
  await new Promise(setImmediate)
  t.mock.timers.tick(10_250)
  await new Promise(setImmediate)
  const observed = { settled, stdin: f.children[0].stdin.destroyed, stderr: f.children[0].stderr.destroyed }
  f.children[0].emit("close", 0)
  await pre
  assert.deepEqual(observed, { settled: true, stdin: true, stderr: true })
})

test("disposal during pre checkpoints cannot restart lifecycle subscriptions", async (t) => {
  const f = await fixture(t, () => {})
  const first = f.before(f.event("first"))
  const second = f.before(f.event("second"))
  while (f.children.length < 2) await new Promise(setImmediate)
  await f.cleanup()
  f.children[0].emit("close", 0)
  await new Promise(setImmediate)
  const subscriptions = f.subscriptions()
  f.children[1].emit("close", 0)
  await Promise.all([first, second])
  assert.equal(subscriptions, 1)
})

test("sessions can use the same call ID without overwriting each other's baseline", async (t) => {
  const f = await fixture(t)
  const first = f.event("shared", { path: "a.ts" }, "first")
  const second = f.event("shared", { path: "b.ts" }, "second")
  await f.before(first)
  await f.before(second)
  await f.after(first)
  await f.after(second)
  assert.deepEqual(f.calls.filter((x) => x.hook_event_name === "PostToolUse").map((x) => x.session_id), ["first", "second"])
})

test("accepted long-running tools do not expire", async (t) => {
  const f = await fixture(t)
  t.mock.timers.enable({ apis: ["setTimeout"] })
  await f.before(f.event())
  t.mock.timers.tick(24 * 60 * 60 * 1000)
  await f.after(f.event())
  assert.equal(f.calls.length, 2)
})

test("a duplicate pre is rejected without replacing an acknowledged baseline", async (t) => {
  const f = await fixture(t)
  const event = f.event()
  await f.before(event)
  await assert.rejects(f.before(event), /duplicate/)
  await f.after(event)
  assert.deepEqual(f.calls.map((x) => x.hook_event_name), ["PreToolUse", "PostToolUse"])
})

test("strict checkpoint rejection blocks execution and a failed post remains retryable", async (t) => {
  let attempt = 0
  const f = await fixture(t, (child) => child.emit("close", ++attempt === 2 ? 23 : 0))
  const event = f.event()
  await f.before(event)
  await assert.rejects(f.after(event), /exited with 23/)
  await f.after(event)
  assert.deepEqual(f.calls.map((x) => x.hook_event_name), ["PreToolUse", "PostToolUse", "PostToolUse"])
})

test("tracked edits require stable identity and a visible pre-edit file scope", async (t) => {
  const f = await fixture(t)
  await assert.rejects(f.before({ ...f.event(), id: "" }), /missing callID/)
  await assert.rejects(f.before(f.event("no-path", { patchText: "@@ -1 +1 @@\n-old\n+new" })), /did not expose a file path/)
  assert.equal(f.calls.length, 0)
})

test("terminal saturation fails closed for unmatched recovery without forgetting cancelled calls", async (t) => {
  const f = await fixture(t)
  for (let i = 0; i < 66; i++) {
    const event = f.event(`cancel-${i}`)
    await f.before(event)
    await f.emit({ type: "session.tool.failed", data: { sessionID: event.sessionID, id: event.id, assistantMessageID: event.messageID } })
  }
  await f.after(f.event("cancel-0"))
  await assert.rejects(f.after(f.event("cancel-65")), /recovery capacity/)
  const healthy = f.event("healthy")
  await f.before(healthy)
  await f.after(healthy)
  assert.equal(f.calls.filter((x) => x.hook_event_name === "PostToolUse").length, 1)
})

test("Kilo derived template retains its callable V1 entrypoint and strict checkpoint arguments", async () => {
  let source = await readFile(new URL("./git-ai.ts", import.meta.url), "utf8")
  const anchors = [
    ['export const GitAiPlugin: Plugin.Plugin = {', 'const GitAiPlugin: Plugin.Plugin = {', 1],
    ['export const GitAiPluginV1 =', 'const GitAiPluginV1 =', 1],
    ['import type { Plugin } from "@opencode/plugin"', 'import type { Plugin } from "@kilocode/plugin"', 1],
    ['const CHECKPOINT_ARGS = ["checkpoint", "opencode", "--strict-errors", "--hook-input", "stdin"]', 'const CHECKPOINT_ARGS = ["checkpoint", "kilo", "--strict-errors", "--hook-input", "stdin"]', 1],
    ['process.env.GIT_AI_OPENCODE_DEBUG ?? process.env.GIT_AI_DEBUG', 'process.env.GIT_AI_KILO_DEBUG ?? process.env.GIT_AI_DEBUG', 1],
    ['          tool_input: toolInput,\n', '          tool_input: toolInput,\n          platform: process.env.KILO_PLATFORM || "cli",\n', 2],
    ['export default {\n  ...GitAiPlugin,\n  async server(ctx?: V1PluginContext) {\n    return GitAiPluginV1(ctx)\n  },\n}', 'export default GitAiPluginV1', 1],
  ]
  for (const [from, to, count] of anchors) {
    assert.equal(source.split(from).length - 1, count, `Kilo source anchor changed: ${from}`)
    source = source.replaceAll(from, to)
  }
  const compiled = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 } }).outputText
  const directory = await mkdtemp(join(tmpdir(), "git-ai-kilo-template-"))
  try {
    const pluginPath = join(directory, "git-ai.mjs")
    await writeFile(pluginPath, compiled)
    const module = await import(pathToFileURL(pluginPath).href)
    assert.deepEqual(Object.keys(module), ["default"])
    const plugin = module.default
    assert.equal(typeof plugin, "function")
    const hooks = await plugin({ directory })
    assert.equal(typeof hooks["tool.execute.before"], "function")
    assert.equal(typeof hooks["tool.execute.after"], "function")
    assert.ok(compiled.includes('"--strict-errors"'))
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
})

test("mixed edits keep every Git repository and exclude explicit non-Git paths", async (t) => {
  const f = await fixture(t)
  const other = await mkdtemp(join(tmpdir(), "git-ai-other-repo-"))
  const outside = await mkdtemp(join(tmpdir(), "git-ai-nongit-"))
  t.after(async () => { await rm(other, { recursive: true, force: true }); await rm(outside, { recursive: true, force: true }) })
  await mkdir(join(other, ".git"))
  const event = f.event("mixed", { file_paths: [join(f.directory, "one.ts"), join(other, "two.ts"), join(outside, "three.ts")] })
  await f.before(event)
  await f.after({ ...event, input: { path: join(outside, "expanded.ts") } })
  for (const call of f.calls) assert.deepEqual(call.git_ai_file_paths, [join(f.directory, "one.ts"), join(other, "two.ts")])
  const shell = { ...f.event("outside-shell"), tool: "shell", input: { command: "echo hello", workdir: outside } }
  await f.before(shell)
  await f.after(shell)
  assert.equal(f.calls.length, 2, "non-Git shell cwd cannot fall back to plugin repository")
})

for (const status of ["error", "interrupted", "cancelled"]) {
  test(`V2 execute.after ${status} never produces a successful AI checkpoint`, async (t) => {
    const f = await fixture(t)
    const event = f.event()
    await f.before(event)
    await f.after({ ...event, status, error: { name: "AbortError", message: "stopped" } })
    await f.after(event)
    assert.deepEqual(f.calls.map((x) => x.hook_event_name), ["PreToolUse"])
  })
}

test("more than 65 successful calls preserve durable cross-process post recovery", async (t) => {
  const f = await fixture(t)
  for (let i = 0; i < 70; i++) {
    const event = f.event(`success-${i}`)
    await f.before(event)
    await f.after(event)
  }
  // Another plugin/window durably acknowledges pre; the original instance
  // receives post without local correlation and must still consult the daemon.
  const external = await f.plugin.server({ directory: f.directory })
  const recovery = f.event("external-pre")
  await external["tool.execute.before"]({ tool: recovery.tool, sessionID: recovery.sessionID, callID: recovery.id, args: recovery.input })
  await f.after(recovery)
  assert.deepEqual(f.calls.slice(-2).map((x) => [x.hook_event_name, x.tool_use_id]), [["PreToolUse", "external-pre"], ["PostToolUse", "external-pre"]])
})
