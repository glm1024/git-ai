# DSH 插件与统一统计接入

本接入使用工作区的 `dsh-git-ai` 插件（`0.2.0-dsh.1`），目标 DSH 为 `0.2.1-rc.2`。源码适配、测试验证与安装部署分别记录；本文不代表已升级开发者机器或生产服务。

## 身份和事实

| 数据 | 合同 |
| --- | --- |
| 工具 | 插件默认 `agent_name=deepseek-harness`；后端独立工具键保持同值，管理端显示 DSH |
| 会话 | 原始 `conversation_id` 进入既有稳定 session 身份；模型切换不创建替代会话 |
| 模型 | 每次 checkpoint 冻结当前请求模型；提交按 `session::trace` 与 note 的 `trace_models` 解析 |
| 接受 | Event 4 的 AI 新增行形成接受事实，生成行按后端现有口径等于接受行 |
| 提交 | Event 1 的 note 与 addition hunk 形成候选；正式入库仍须服务端目标分支确认 |
| 使用信号 | Event 2 是 AgentUsage，不是接受行或 Token；本地 usage 可据实际使用/checkpoint 证据计会话 |
| 缺失数据 | 插件不采集会话正文、Token、成本或 IDE/模型提供商；没有事实时不按 DSH/DeepSeek 名称猜测 |

文件工具复用 `checkpoint agent-v1 --strict-errors` 的 human pre / ai_agent post；可选 Bash 复用 pre/post shell。插件负责原生工具识别、路径范围和执行前确认，Git AI 负责来源记录、提交和上报。不会增加另一个 DSH preset、扫描历史会话或自动安装插件。

插件允许自定义 `agentName`。自定义值原样保留，不修改 session hash；后端仅将默认 `deepseek-harness`（忽略外围空白及大小写的既有查询归一）识别为 DSH。`dsh`、`deepseek` 或其他自定义名称不自动成为别名，未知工具继续使用“其他”桶。

## 接入条件

- Git AI CLI 应包含此前的人工相邻行、覆盖、部分暂存/amend 及逐 trace 模型修复；旧插件归档和旧 CLI 不因源码更新而自动升级。
- 企业上报继续使用 Git AI 的 `metrics_api_base_url` 和完整 `reporting_profile`。插件不另存企业人员身份、不直接上传后台；缺少身份或项目证据继续走现有待补全流程。
- 后端和管理端需要本次工具维度适配。接收服务已支持通用工具身份，本次补齐查询/物化分桶、筛选及多工具提交标签。
- 本次 DSH 适配没有数据库迁移，不提升投影或物化版本，不主动回填、重放或重算已有历史。已经落入旧物化桶的数据不会被本次代码变更立即迁移。
- 打包、安装和服务重启由用户按项目流程执行；插件启用后的新会话与实际部署环境仍需验收。

## 验证入口

- Git AI：`tests/integration/dsh.rs` 使用真实 `agent-v1` 调用、Git 提交、Notes 和 metrics 验证 DSH 合同；`src/metrics/local_stats.rs` 覆盖没有正文流的会话计数和去重。
- 后端：`AiGitAiMetricsIngestServiceImplTest` 的 DSH 批次 fixture 核对原始上报、checkpoint、提交候选、模型及缺失维度；mapper 测试核对 DSH 独立分桶与总量守恒。
- 管理端：`npm run test:ai-stats` 覆盖共享标签/筛选及 DSH 与其他工具混合提交。没有浏览器交互验收。

插件自身的捕获边界和上一轮真实工具回归见相邻仓库 `dsh-git-ai/docs/review-20261028.md`。本轮不重新声明未执行的打包、Windows 实机或生产验收通过。

2026-10-28 本轮检查：DSH 文件/Shell checkpoint TestRepo 2 项、本地 usage 12 项、后端接收 125 项、XML mapper 30 项、规定归因/服务/场景 174 项、MySQL 分桶 1 项和管理端 201 项均通过。后端 `dsh-agent-v1-metrics.json` 与引擎导出的 fixture 完全一致，实际包含两个 Event 4 和一个 Event 1；测试构建不发送 Event 2，其 raw-only 合同另由后端协议测试验证。本轮没有重新跑上一轮完整归因矩阵或重新打包插件。

`task lint`（Clippy all-targets，warnings 视为错误）与两仓 `git diff --check` 通过。收尾同时修正当前共享源码中的无用生命周期、复杂字段类型别名、可折叠条件及测试中的多余借用，均不改变业务行为。
