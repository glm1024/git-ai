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

以上为初次源码交付阶段的验证。该阶段未安装本机新 CLI/Hook，也未执行真实模型请求。随后用户明确授权在本机 ZCode/babel 做完整测试及必要修复，本机实机结果另见下节；没有部署生产或迁移生产数据库。

未被后续实机记录覆盖的 ApplyPatch 开放情况、子会话和 Windows 路径仍需分别验收。macOS 验证不代表 Windows 或生产验收。

未暴露范围的 MCP 写入、后台任务后续写入、跨仓库未观察路径仍为 Unknown；不能把会话期间全部工作区差异归给 ZCode。ZCode普通Hook错误/超时/退出码2有不同语义，不能仅凭CLI非零声称宿主一定阻止操作；缺pre的post会拒绝AI归因。

本机静态证据见 [zcode-installed-evidence-2026-10-28.json](zcode-installed-evidence-2026-10-28.json)。

## 本机实机验收（2026-10-28）

用户授权使用已配置模型的 ZCode，在 `/Users/mark/workspace/babel` 新增、修改文件并本地提交，发现问题直接修复。测试限定到指定测试文件，保留既有 `.DS_Store`、`AGENTS.md` 及其他会话的提交；不推送远端。

- 本机原 CLI 未包含本轮实现，ZCode Hook 配置不存在。通过仓库 `task dev` 安装测试版，保留 Git AI 原配置；备份位于 `/Users/mark/.git-ai-backups/zcode-acceptance-20261028-110649/`。本地后端已具有序号迁移，后续为加载筛选与权限修复更新并重启 8082；8081 保持原进程。
- 实测 ZCode 3.16.3 仅新建任务不会重载此前安装的 Hook；完整退出并重开后，真实 SessionStart、UserPromptSubmit、Stop 均执行。未接入的第一条对照会话不手动补采或重放。安装说明已明确完整重启要求。
- R2 会话 `session_33d89a2e5677a45c7e9ffc9a9958f900` 的纯聊天主请求及稍后标题请求均采集。工具调用实际使用 `Write`、`Bash`，模型为 `deepseek-v4.1-flash`、提供商为 `new-provider`；按 call/message/usage 精确关联，没有借用默认 GLM 模型。
- 第一笔提交 `df84f5403375ec11af48413ca7f457533377c2c6` 仅新增 `tests/test_unicode_text_helpers.py`：4 项 Python 单测通过，Git note 的 1—26 行及逐行 blame 均为 ZCode；前后检查点持久任务均成功应用。该笔原生 outbox 已于 11:41:59 自然送达，后端投影成功，1 个候选共 26 行。此时最终测试版尚未安装，因此不单凭该回执证明后续调度修复生效。
- R2 结束时源库共 11 次请求：含缓存输入 342709、输出 1985、缓存读取 234880，总量 344694。客户端内部普通输入 107829 与缓存分量相加精确一致；首次出站少算缓存的问题已通过 `td5` 导出换算修复，保持后端统一“含缓存输入 + 输出”口径。源库和历史事实不手动修改。
- 实测发现源文件存在时规范路径被追加 `/`，造成重复会话游标；已修复并补源文件创建前后路径稳定性回归。R2 两条游标没有造成重复请求计数，R3 新会话已验证只有一条规范路径游标，处理错误为零。
- 管理端已实际验证工具、提供商、模型组合筛选、无匹配空状态、用户/日期下钻和返回保留筛选；按日弹窗独立显示 ZCode、实际模型及 babel。前端 207 项检查通过，后端筛选与权限回归通过；共享 IDE 品牌映射漏项也已修正，Git AI 记录现显示 `ZCode / ZCode`。一次 HMR 后详情表暂未出现，整页刷新即恢复，控制台无错误；正式页面加载与日明细弹窗已实测。
- 第一笔提交的上传被另一个大型历史任务阻塞，事件已经生成但未尝试上传。已优化分块 JSON 字节预算、前移锁内大包校验，并隔离历史计算与 outbox 上传；await 已使用代次屏障保留多调用者等待语义；deferred commit 单测 18 项、telemetry 单测 41 项、TestRepo 集成 5 项全部通过。大型历史报告仍受原 64 MiB 上限约束，不声称已清空历史积压。没有删除队列或主动重放历史任务。

- 最终测试版通过 `task dev` 安装，原 reporting 配置逐字节保持一致。R3 会话 `session_175e42aff92fe0ad348936d3a0829e3e` 使用 Bash 新增 UTF-8 fixture、两次原生 Edit 更新已有测试；模型和提供商均与 R2 一致。6 项 Python 单测由 ZCode 执行并独立复验通过。
- R3 提交 `0c89941cc8b3f28ac521f20f2fd1bde36e41a287` 仅包含两个指定路径，新增 12 行。Git note/逐行 blame/后端四个候选一致：fixture 第 1 行、测试第 2、7、28—34 行共 10 行归 ZCode；第 8—9 两个分隔空行保留 Unknown，原有 26 行未重复归为本次新增。管理端展示 AI 10 / 人工 0 / Unknown 2，与原始事实一致。8/8 个新增非空行均已归因。两空白 Unknown 源于通用 checkpoint diff 与 Git commit diff 对重复空白的不同对齐：前者把空白增量对齐为第 5—6 行，后者为第 8—9 行；提交归因只认双方新增坐标与 AI 证据交集，不扩大猜测。第 28—34 行范围包含另外两条成功归因的空白，所以不是全局忽略空白，也不能声称 12/12 归因。
- R3 原 outbox 事件在 12:02:36 生成、12:02:37 自然送达，后端 Event 1 投影成功；这是最终安装版上传调度的实机证据。没有手动 flush/reingest、重置事件键或修改队列数据库。
- fixture 有意保留首尾空格以测试 normalize_text，`git diff --cached --check` 对该夹具的 trailing whitespace 提示符合测试设计；两个适配源仓库的 diff check 通过。

### Token 三方对账

ZCode SQLite 原始 model_usage、Git AI 已准入 source/day bucket、后端最新日事实一致；UI 筛选 ZCode + new-provider + deepseek-v4.1-flash 显示 23 请求、735.57K 总 Token，日明细项目为 babel。

| 范围 | 请求数 | 含缓存输入 | 输出 | 缓存读取 | 总 Token |
| --- | ---: | ---: | ---: | ---: | ---: |
| R2 | 11 | 342709 | 1985 | 234880 | 344694 |
| R3（含最后标题请求） | 12 | 386501 | 4376 | 241088 | 390877 |
| 合计 | 23 | 729210 | 6361 | 475968 | 735571 |

缓存写入均为 0；输出已包含 reasoning，不再次相加。最新 td5 快照 sequence=23，原生上传队列内该日快照无待发送项，不将其他历史积压计为这两轮失败。

R3 最后标题请求在主回复结束约 18 秒后完成落库；现有后台扫描于 12:07:21 自然读入，较标题完成晚约 4 分 24 秒。这是更新延迟而非永久漏采。已增加已登记 ZCode 会话的有限补采：名义初读后约 5/20/60 秒追加读取，同会话合并，之后停止；补采和常规周期扫描均不自续期开窗，进程首次恢复只为持久登记源恢复一轮，激活边界不变。调度拥塞和有限失败重试可使读取后移，不是硬实时保证。先以 Stop 空批次未安排补采的断言复现，再完成修复；全部 stream_worker 单测 33/33 通过，覆盖 Stop 后 18 秒标题落库、20 秒补采、历史排除、去重、窗口终止、多会话独立及启动恢复。独立审查无阻塞项。该新调度时序由确定性自动化验证；上面真实标题完整计数来自安装此改进前的自然扫描，不混称新策略的真实请求验收。最终已再次通过 `task dev` 安装，daemon 就绪、原配置逐字节不变；安装日志及二进制备份位于上述备份目录的 `late-title-update/`。安装后自然恢复补采的末次读取发生在 12:19:07，12:19:53 再核水位不再推进，证明有限窗口已停止；R3 仍为单游标、原激活边界、12 请求、零错误。两轮客户端及后端仍恰 23 请求 / sequence 23 / 735571 Token，日快照无待发项且后端同来源仅一条事实，没有重复累计。

截图：[ZCode 执行与提交结果](zcode-acceptance-20261028/zcode-test-result.png)、[管理端 ZCode 独立用量](zcode-acceptance-20261028/token-model-detail.png)、[提交归因审计](zcode-acceptance-20261028/commit-attribution.png)。

验收边界：仅本机 macOS ZCode 3.16.3、指定 provider/model、Write/Edit/Bash 实际运行；ApplyPatch、子会话及 Windows 仍只有对应自动化/静态证据。两个测试提交均未推送，所以管理端候选处于“等待目标分支扫描确认”，不冒充正式 AI 确认入库。保留 Babel 原有未暂存修改及并行 DSH 测试内容；没有部署或迁移生产。
