# ZCode 扩大验收（2026-10-28）

本轮已完成真实桌面、逐行 oracle、自然上报及隔离目标确认；发现的失败终态/误归因恢复、模型跨项目过滤和审计空态问题均已修复。CLI 已本机安装生效，后端修复只在隔离服务加载；不据此宣称所有环境全面适配。

## 预先范围与判定

本轮将正常路径通过与全面适配区分。真实桌面操作、隔离 TestRepo 自动化、后端隔离确认与 UI 展示分别记录，测试数量或总行数一致不能替代逐行来源证明。

- 基础仓库：Babel；基线 0c89941cc8b3f28ac521f20f2fd1bde36e41a287。
- 独立工作树：`/Users/mark/workspace/babel-zcode-acceptance-20261028`，分支 `test/zcode-deep-20261028`。保留原工作树及并行 DSH 变更，不向 GitHub 推送。
- ZCode 项目：babel-zcode-acceptance；模型 deepseek-v4.1-flash，provider new-provider。
- oracle 在操作前定义各阶段每行文本与预期来源，来源不能从实际 Notes/blame 反推。主 agent 在 Hook 外执行受控外部编辑模拟人工；没有编辑器 KnownHuman 凭据，预期 Unknown，不能把未知冒充已确认人工。
- 主 agent 独占 UI 与工作树文件；独立 agent 维护 oracle/验证器，Git AI 自动化与后端确认分别由另外两条工作流负责。

| 场景 | 预期 | 证据状态 |
| --- | --- | --- |
| 自然工具选择 | 不指令使用某种写入工具，实际选择及捕获边界被记录 | 实机 Edit / Write / Bash 均通过 |
| 外部先改→AI / AI→外部覆盖 | 未经观察外部行不归 AI，覆盖不留旧 AI | 实机逐行通过；KnownHuman 另有受控 E2E |
| 多轮修改、删除、重写 | 已删除行不进入最终提交，重复相同内容不膨胀 | 多轮修改/删除实机通过；撤销/重复写入受控 E2E 通过 |
| 部分暂存、分批提交、amend | 每个提交逐行匹配独立 oracle，未暂存不提前上报 | 实机 10/11/13 行逐阶段全部匹配 |
| 会话切换/不同模型 | session/model逐行不串；明确真实或模拟 | 实机不同会话同模型通过；模型切换及并发受控 E2E，未做桌面真并发 |
| 取消/重启/恢复 | 失败部分写入未知；持久事实不重复、不猜测 | 缺陷修复后实机取消/重启/拒绝再成功通过；网络失败恢复隔离真实 HTTP 通过 |
| 候选→确认/汇总/compare/幂等 | 以本地隔离目标证明，不修改生产目标或历史 | 原始自然上报+隔离真实 Git 确认/汇总/API/浏览器审计通过 |

## 实际结果

### 第一阶段：外部混改与第一笔部分提交

预冻 `oracle.json` SHA256 `49829488e35bb1c5c050c814352d28fda174b8ebb4fc163797570eb759289740`。R1 实际会话 `session_0d3bdb1e9ffb7f5d08838f86d5fe042f`；模型自然选择 Edit 追加六行，Bash 用于只读检查。随后外部覆盖 A02、插入 H07；仅将前十行放入 index，另三行留在 worktree。

提交 `68bba57f2f9d44446dea853bcf14813edcf87b69`：独立校验完整文本及每行来源/工具/会话/trace 模型，10/10 一致，AI 2（A01/A03）、Unknown 8。原上报在约一秒自然送达 8082，后端仅两条对应候选，未将外部覆盖或未暂存行算 AI。逐行证据 `evidence/commit1-oracle.json`，实际阶段文件位于 `evidence/`。

### 重启与第二会话

第一会话结束后完整退出并重开 ZCode，未暂存的 A04/A05/A06 保留。R2 实际会话 `session_7825076eb99173157eecb17e414d3a56`，自然选择 Write 重写整文件，真实 checkpoint 为 +4/-4，而不是整文件 13 行归为新 AI。完整实际文本已与预冻 round2 目标逐行一致；分批提交和 amend 已逐行核验通过。

### 隔离自动化

新增四组复杂 TestRepo 用例，连同原六组最终 10/10 通过。覆盖外部/已知人工与 AI 混改、多轮删/复原重写、同一 SQLite 两会话交替及多模型共存、部分 index 两次提交/amend、取消部分写入、pre 后 daemon 重启、重复 pre/post 幂等。每次提交/amend同时核对完整内容、ExpectedLine 与逐行 tool/session/model。使用真实 CLI/daemon、受控 SQLite/Hook fixture，不冒充所有桌面场景已实测。这四组初始复杂场景未触发生产改动；后续实机发现失败终态与恢复缺陷，另增两组回归并修复生产实现，最终共 12 项。

### 第二笔部分提交与 amend

C2 `de4f607958cc491fefd8029cf4ac215dd1021b7f` 仅暂存前 11 行：完整文本、来源、工具、会话和模型 11/11 一致，AI 4、Unknown 7；A06/B01 仍未暂存。随后只暂存剩余两行执行本地 amend，产生 `e86423064779fa69bd1113900c30b4c238b50086`，13/13 一致：R2 为第 2、7、10、13 行，R1 为第 11–12 行，Unknown 7。证据为 `evidence/commit2-oracle.json` 和 `evidence/amend-oracle.json`。

原始上报自然到达共享 8082：C2 事件 5394；amend lifecycle 事件 5408 将旧 C2 四条候选标为 abandoned/amend，新事件 5409 留六行候选。未手动重放客户端历史上报。

### 不指定工具的批量生成

预冻 `catalog-oracle.json` SHA256 `c81536f5aa0b1c367ff4b74e54fffd75ec101c0b6d9ae0f93ce4467fbbed7721`。普通业务提示要求 64 条固定规则 JSONL，未指定工具；真实模型自然选择 Bash 生成，调用 `call_69576e1cb84e4a92aa7987e5`，会话 `session_20692de393f51830de746c7a8a28bc30`。checkpoint +64 自然上报，实际文件每条字段、类型、顺序及最终换行独立核验。

本地提交 `20c440ecafa8bc6ebe882616fffeecaef59ce4ab`，`evidence/catalog-oracle.json` 核验 64/64 行归属 ZCode / 该会话 / deepseek-v4.1-flash。原始事件 5707 在共享 8082 自然接收，64 行候选；该次成功证明当前 macOS 前台 Bash 写入可捕获，不等于任意后台子进程、跨仓库或无 Hook 写入均支持。

### 统计口径必须区分

| 范围 | checkpoint 累计接受 | 有效历史提交 AI 新增 | 最终文件 AI 存量 |
| --- | ---: | ---: | ---: |
| lineage 多轮 | 10（6+4） | 8（C1 2 + amend 6） | 6 |
| catalog | 64 | 64 | 64 |
| 合计 | 74 | 72 | 70 |

已被 amend 替代的 C2 四行不再参与有效历史提交累计；最终存量与累计历史提交不同，不用同一个数字替代三种业务口径。

### 真实取消及拒绝暴露的新问题

R2 的 Bash 调用 `call_11055bc2f54c4be0831a95f1` 被明确拒绝，源记录 permission_denied，但没有 PostToolUseFailure；Git AI Bash 记录无 end_time。另预冻 `cancellation-plan.json`，真实会话 `session_e229fe7d8dab35c3e24f5a7c97b28453` 调用 `call_4c368d7b7362464d8a26145e` 写入一行后 sleep 60，在 48.846 秒时点击停止。源记录 tool_cancelled / cancelled_by_user=1，桌面显示已停止，仍只收到 pre，未落失败终态。`evidence/zcode-cancelled.png` 为真实界面证据。

因此原有“基础链路正常”不能推广为完整生命周期支持。拒绝未执行与取消前部分写入必须分开；没有冻结前快照时也不能假造已恢复逐行差异。修复后实机结果见下节。

### 后端隔离确认与管理端

共享 8082 只核对自然接收和候选；为避免改动已有目标或历史，本轮另外启动独立 8083 后端、私有数据库/Redis、本地 bare 镜像，手动运行隔离 attribution job。真实 SHA、diff、原生会话和 trace 保持，仅将导入夹具的仓库/身份映射为隔离对象；这不冒充真实工作树已推远端或共享环境已正式入库。

真实项目确认 C1=2、amend=6、catalog=64，旧 C2 作废。管理端经临时 8084 代理复用现有 8081 前端，正常登录私有测试账号；真实项目筛选显示四条记录，三条全部入库、一条被新 commit 替换。截图 `evidence/admin-confirmed-events.png`。代码审计读取目标 Git diff，与 oracle 行号一致；发现并修复错误空态；修复后真实浏览器验证“全部新增有文件 → Unknown 0 正确空态 → 切回恢复文件”，截图 `evidence/admin-amend-audit-fixed.png`。连续同模型只在段首展示为既定 UI；代码审计的区块正式入库数未分配时保留 --，目标确认 6 与列表正式物化 6 分别验证，不伪称区块有正式分配数。传统 Kilo commit-compare 不承载 Git AI 审计，本轮使用 Git AI 记录的“查看代码”。


### 新版本失败终态修复及复测

本轮从真实缺陷补红测再实现，独立审查发现并修正了 Token/SessionEvent 时间恢复旁路、源调用歧义、DB 失败丢 snapshot、并发 pre 身份复核四项风险。生产修复只处理当前真实生命周期边界内已观察/已登记的精确同 worktree、session、call 和 pre 时间/trace，源失败身份必须唯一，不能按时间接近猜测代码来源。ZCode 的 Bash 与 Token 两类 mtime 启发式 AI 恢复均关闭，其余工具行为保持。

- 旧取消 `session_e229fe7d8dab35c3e24f5a7c97b28453`：安装重启后发同会话正常只读请求；原记录自然转 failed/no-checkpoint，无重建 delta、无 AI checkpoint。
- 新取消 `session_50b33911eceecc68e0791831688f9797`：命令真实运行 49.230 秒后停止；同会话下一正常只读提示使用仍在内存的 pre snapshot，落一条 failed done 任务与一行 Human/Unknown checkpoint（内部 Human 表示 Unknown，不是 KnownHuman），自然送达。
- 新拒绝 `session_c0ea22aefc4a20124ba4fbd449e3df0c`：源 permission_denied、实际运行 0，约 1.35 秒自然转 failed/no-checkpoint，不生成 AI 或伪造写入。
- 同会话随后成功 `call_af719c7bdc7a4117a52ab214`：独立 pre/post、唯一 AI checkpoint +1，只覆盖 recovery-success.txt；没有把取消文件或被拒绝命令混入。

预冻 `after-fix-plan.json` SHA256 `a80c5071a666ecb4db2aeac0e485fc4f5037ec49c7fd5c30004c24eefce04ce4`。最终提交 `b3d129e0eaa024e559020150c4b189023558f18b` 只新增三个单行 marker：cancelled.txt 和 cancelled-after-fix.txt 均 Unknown，recovery-success.txt 为 ZCode/真实成功会话/deepseek-v4.1-flash；被拒绝文件不存在。独立校验器使用四次批量只读 Git 调用，3/3 通过，拒绝额外变更、KnownHuman 冒充、重叠归因及错误模型，证据 `evidence/failure-recovery-oracle.json`。

### 验证与安装边界

- Git AI：65 项 ZCode lib、12 项完整 ZCode integration、29 项 attribution_recovery 均通过；17 项独立 oracle 自测通过。真实桌面是当前 macOS ZCode 3.16.3/build3.16.3.8992 和 deepseek-v4.1-flash。
- 本机通过项目指定 `task dev` 安装新 CLI 并重启 daemon；ZCode 正常退出重开。git-ai 与 ZCode 两份配置 SHA256 安装前后一致。备份在 `/Users/mark/.git-ai-backups/zcode-acceptance-20261028-110649/failure-terminal-update/`。
- 后端：411 项受影响检查，前端 208 项检查通过。修复项目模型查询遗漏 projectKey、审计有表格同时显示空态。SQL 是 Mapper 查询改动，不修改既有迁移。
- 模型项目隔离经私有 8083 真实 API/MySQL 验证；现有模型页面没有项目筛选入口，不虚称该项 UI 已通过。共享 8082 未重启加载本轮 SQL；私有 8083 已加载并验证。完整后端证据见 [后端报告](../../../ai-coding-backend/docs/zcode-backend-deep-acceptance-20261028.md)。
- 网络恢复使用独立 HOME、新建验收事件及真实 HTTP：发送前断连、服务端已接受但 ACK 丢失两类都约 60 秒原生重试，同 body/UUID，后端各唯一一条，本地只在收到真实 ACK 后 delivered。未中断共享后端、未重放历史业务事件。

### Token 独立核对

安装前冻结水位：此前 23 请求 + 本轮四会话 6/7/5/2 请求 = 43。ZCode 原始 model_usage、CLI 日快照及共享 8082 均为：输入 1,245,042，输出 14,723，缓存读 659,904，缓存写 0，总 Token 1,259,765。缓存是输入子集，不再重复相加。管理端工具 ZCode / provider new-provider / model deepseek-v4.1-flash 筛选及每日模型明细均匹配，截图 `evidence/live-zcode-token-model.png`；后续修复验证的增量另列，不覆盖该冻结基线。

### 尚未扩大的范围

Windows/macOS 其他版本、真实桌面不同模型切换、两个桌面会话同时写同一文件、后台进程或跨仓库写入、无暴露文件范围的 MCP 写入尚未完成实机验收。失败后没有冻结快照且会话流关闭/缺少已登记边界时，保守保留 pending，不制造历史来源。旧 R2 被拒绝调用未额外人为重放或回填，须其同会话后续真实边界才自然处理。

结论限定为本轮已验收场景通过及已发现缺陷完成修复，不以总行数、覆盖率或隔离用例替代所有平台、所有写入方式全面适配的声明。


## 最终结算与交付

- 最终提交 `b3d129e0eaa024e559020150c4b189023558f18b` 的原始 outbox 105919 在 1 秒内自然送达共享 8082，收件 7108 succeeded，仅一条 AI candidate；共享环境仍为 pending，不能称共享环境已正式入库。
- 同一新提交另在私有 8083 的独立目标运行真实确认：AI 1、Unknown 2，原四提交结果不变；最终范围累计接受 **75**、有效提交 AI 新增 **73**、最终 AI 存量 **71**。取消的两行未参与 AI 指标。管理端最终五条记录及三文件来源分别见 `evidence/admin-final-confirmed-events.png`、`evidence/admin-final-source-audit.png`。
- 最终 Token 在 14:07:07 与 14:09:25 两次读取均为 **56 请求**：输入 **1,574,575**、输出 **17,669**、缓存读 **840,960**、缓存写 **0**、总量 **1,592,244**。源 SQLite、CLI、共享后端唯一日事实完全一致；43 请求冻结基线后新增 13 请求只属于修复复测，原基线独立保留。有限补采后无重复增长、无待发 outbox。
- Babel 测试提交仅在独立分支 `test/zcode-deep-20261028`，没有 push。工作树最后仅原有/界面产生的 `.DS_Store` 未暂存改动，未纳入提交；原 Babel 工作树改动保留。git-ai/backend 本轮修复源码留在当前工作区供审阅，未代为提交或发布。
- 8084 临时代理已停止、临时浏览器页已关闭；私有 8083/6389 已停止并确认端口释放，私有数据库、bare/镜像及临时凭据已删除，保留脱敏报告和可复现脚本；清理结果已在后端报告记录。共享 8081/8082 保持运行，用户当前 ZCode 配置保持。CLI 新版已生效，后端项目模型查询修复尚需用户正常重启共享后端后生效。
