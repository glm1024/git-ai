# ZCode 接入 Git AI

日期：2026-10-28。适用基线：本机 macOS ARM 的 ZCode 3.16.3 / build 3.16.3.8992，以及本工作区两仓源码。本文已从初始研究更新为实施说明；代码验证与实机启用分开记录。

## 实施范围

本次同时修改 `git-ai` 与 `ai-coding-backend`，不修改 ZCode 安装包或 `kilocode-legacy`。

| 能力 | 实现 |
| --- | --- |
| 安装 | 独立 ZCode 安装器，在用户配置幂等合并六类同步 process Hook；保留其他配置、禁用状态和软链；锁后重读并原子替换 |
| 代码来源 | 独立 zcode preset，严格 session/call/别名与文件范围；Write、Edit、ApplyPatch 和前台 Bash 复用持久检查点及 trace2 |
| 失败 | 失败、取消、超时、后台调用终结跟踪；已观察部分修改为 Unknown，持久快照保护避免后续猜成 AI；已失败调用不能被迟到成功重放激活 |
| 模型 | 按唯一工具 callID → part.message_id → assistant message / model_usage 取得实际 provider/model；冲突或尚无证据为 unknown；不取会话最新模型 |
| Token | 独立只读 SQLite 流，按请求尝试 ID 去重，不读取正文；普通输入从含缓存的输入总量中扣除缓存，推理不重复加到输出 |
| 纯会话 | SessionStart、UserPromptSubmit、Stop 独立登记；首次边界持久化后确认，即使源库尚未创建；共享数据库按会话隔离，WAL 和迟到落库由已登记会话轮询覆盖 |
| 修订 | ZCode 单请求用量可修正缓存分类、减少和归零；原日维度冻结，稳定 td5 来源加独立递增序号，后端只接受同来源新序号 |
| 管理端 | ZCode 独立标签、筛选及 SQL 工具维度，覆盖代码、Token、dashboard物化和混合工具提交明细；既有 other/unknown 口径不变 |

首次接入只统计之后的新请求，不扫描未登记历史，不回填或重算既有统计。相同检查点回执重试复用首次持久化的身份/模型，不因稍后模型落库而重写历史归属。关闭会话流的既有用户配置仍被尊重。

## 已核实的本机能力

| 项目 | 证据与含义 |
| --- | --- |
| 应用 | `/Applications/ZCode.app`，bundle ID `dev.zcode.app`，版本 `3.16.3`，build `3.16.3.8992` |
| 运行时 | Electron，内含 `Contents/Resources/glm/zcode.cjs`；桌面侧启动 Agent 的 `app-server --stdio` |
| 配置 | 运行时默认用户配置为 `~/.zcode/cli/config.json`，配置结构为 `hooks.enabled` 与 `hooks.events`；包内默认 `hooks.enabled=false` |
| 事件 | 包含 `SessionStart`、`UserPromptSubmit`、`PreToolUse`、`PermissionRequest`、`PostToolUse`、`PostToolUseFailure`、`Stop` |
| 调用标识 | 工具 Hook 包含 `sessionId`、`toolCallId`、`traceId`、`turnId`、`cwd`，另提供 `session_id`、`tool_use_id` 等别名 |
| 文件工具 | 找到 `Write` / `Edit` 实现及 `tool_input.file_path` schema；可取得执行前精确文件范围 |
| Bash | 找到 `Bash` 实现及 `tool_input.command`，输出可表达失败、超时、取消和后台运行状态 |
| 补丁 | 找到 `ApplyPatch` 相关 schema、分类和 matcher alias；公开源码有对应 handler。本机包的实际开放与执行情况仍需运行验证 |
| 模型 | `SessionStart` 带 `providerId/modelId` 组合值；工具前后 Hook 的构造没有模型字段。不能把启动模型当作整个会话永不变化的模型 |
| transcript | 每次 Hook 创建独立临时 JSONL，完成后删除。工具前后事件的文件内容为空；用户提交与 Stop 只输出当前用户文本或最终助手文本 |
| 本地用量 | 包内存在 `model_usage` / `tool_usage` / `turn_usage` schema；默认会话库路径为 `~/.zcode/cli/db/db.sqlite` |

以上来自安装包静态检查。默认 `~/.zcode` 在检查时未发现，因此没有读取实际会话库或用户凭据。包内能力不等于真实会话已触发和验收。

官方公开源码固定到 [3c5b227649371507ddc4eb46ac5133afe6db1cab](https://github.com/zai-org/ZCode/tree/3c5b227649371507ddc4eb46ac5133afe6db1cab)，用于对照，不冒充本机 build 的源码版本。可定位入口：

- [Hook 输入兼容与临时 transcript](https://github.com/zai-org/ZCode/blob/3c5b227649371507ddc4eb46ac5133afe6db1cab/apps/zcode-cli/packages/core/src/hooks/configured-runner-input.ts)。
- [工具前后及失败 Hook](https://github.com/zai-org/ZCode/blob/3c5b227649371507ddc4eb46ac5133afe6db1cab/apps/zcode-cli/packages/core/src/tool/executor/hook-flow.ts)。
- [CLI Hook 配置及信任机制](https://github.com/zai-org/ZCode/blob/3c5b227649371507ddc4eb46ac5133afe6db1cab/apps/zcode-cli/docs/usage/09-hooks.md)。

网站 [Hooks 文档](https://zcode.z.ai/cn/docs/hooks) 称项目 Hook 被忽略；公开 CLI 指南及本机包则出现项目 Hook 待信任机制。本次采用用户级配置，避免依赖这项版本差异。配置变更后按新会话验收。

## 配置和协议

默认用户配置 `~/.zcode/cli/config.json`，默认库 `~/.zcode/cli/db/db.sqlite`。读取配置的 `storage.sessionDbPath`；`GIT_AI_ZCODE_CONFIG_PATH` 用于 Git AI 选择完整配置路径，`GIT_AI_ZCODE_STORAGE_PATH` 可显式选择数据库。ZCode 也必须使用同一配置/数据库；`ZCODE_DATA_BASE_DIR` 不等价于这些路径。

安装器注册 `PreToolUse`、`PostToolUse`、`PostToolUseFailure`，matcher 为 `Write|Edit|ApplyPatch|Bash`；另注册 SessionStart/UserPromptSubmit/Stop。独立 argv 为 `checkpoint zcode --strict-errors --hook-input stdin`，30秒超时，不设置异步运行。安装和配置保留策略详见 [ZCode安装说明](../agent-support/zcode/README.md)。

工具身份和 IDE 都按事件记录为 zcode，模型提供商单独传递；不修改全机全局 IDE。工具 Hook 的临时 transcript 不能用作 ClaudeJsonl，不复制其内容。代码事实仍受既有项目与身份门禁，Token 继续使用 reporting_profile 和日快照 outbox。

企业 Event 9 的新 `td5:<稳定hash>` 不把序号拼进身份；可选位置 `v[14]` 为正整数 snapshot_sequence。序号持久递增，重放/乱序旧包不能覆盖新值，上传中的旧包与后继快照隔离。旧 td1–td4 保持原协议。

**更新顺序：先升级后端并执行新增 Flyway `V20261028_1__zcode_token_snapshot_sequence.sql`，再更新 Git AI CLI/Hook。** 新迁移只新增 nullable 序号列；没有修改既有迁移，也不重算历史。

## 验证与边界

自动化验证使用临时配置、SQLite fixture、临时 MySQL、TestRepo 与隔离测试守护进程。覆盖安装/卸载保留、事件冲突、模型切换、人工既有修改、失败部分写入、请求重复和修订、接入边界、同毫秒迟到、多会话、重启与后端独立维度。

| 已通过检查 | 证据 |
| --- | --- |
| Git AI 受影响模块单测 | 497 项组合通过，另有 3 项 Bash NoChanges 终态/迟到pre/测试历史库关闭回归通过；包括 ZCode、checkpoint、Bash history、stream/sweep、metrics、deferred jobs、归因恢复与失败快照保护。并行首轮发现两个新测试缺 Tokio runtime，已修复；既有环境变量测试互扰后以单线程组合通过 |
| TestRepo 集成 | 最终 6 项通过：文件来源/同会话切模型、失败部分写入/拒绝重放/无pre、纯聊天Token/晚到记录、Bash成功、Bash失败后邻近成功、Bash无改动后KnownHuman。Bash用例启用独立持久历史库，额外核对KnownHuman检查点实际被准入 |
| 后端回归 | 352 项组合的全部用例取得通过证据；其中新增迁移测试的注释分句问题修复后独立复验通过。另验证物化ZCode独立计数、异source高序号不能覆盖、历史迁移不变 |
| 管理端 | `npm run test:ai-stats` 的 201 项通过；没有操作浏览器宣称UI已验收 |
| 静态检查 | 两仓 `git diff --check` 通过；新增/本次拥有Rust文件已格式化，保留其他任务既有修改 |

CLI 的成功回执仅确认传输接收；后台随后异步准入/应用，缺失pre、失败终态重放、范围扩张不会生成成功AI事实。集成测试直接核对持久任务与最终提交，而不把回执当作应用完成。

尚未安装或启用本机新 CLI/Hook，没有启动 ZCode、执行真实模型请求、重启生产服务或迁移生产数据库。用户按既有工作区约定自行构建、安装和发布。

实机仍需在新会话与临时仓库验证实际 Hook 顺序、ApplyPatch 开放情况、子会话、模型落库时机、Windows 路径、ZCode 终端/桌面 Git trace2 继承及后端展示。macOS fixture通过不代表Windows或生产验收。

未暴露范围的 MCP 写入、后台任务后续写入、跨仓库未观察路径仍为 Unknown；不能把会话期间全部工作区差异归给 ZCode。ZCode普通Hook错误/超时/退出码2有不同语义，不能仅凭CLI非零声称宿主一定阻止操作；缺pre的post会拒绝AI归因。

本机静态证据见 [zcode-installed-evidence-2026-10-28.json](zcode-installed-evidence-2026-10-28.json)。
