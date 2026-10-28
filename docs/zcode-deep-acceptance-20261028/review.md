# ZCode 失败终态与归因回退独立审查

审查范围：本轮 `presets/zcode.rs`、`daemon.rs`、`daemon/bash_sessions.rs`、`streams/agents/zcode.rs`、相关失败/取消测试及恢复候选消费者。审查者只读生产代码，不运行 Cargo，不操作 ZCode/Babel；生产修改及测试由 `zcode_usage` 单一负责。

当前结论：下列四项问题已在最终接线后的源码中逐项复核修正，未发现尚存的生产代码阻塞项。修复负责人返回定向测试全部通过；此结论不替代新版本的真实 ZCode 失败路径验收。

## 发现与修复复核

1. **P1：只封 Bash 时间窗仍存在 SessionEvent 时间窗旁路。** `stream_worker.rs` 将 ZCode 用量投影为 `SessionObservation`；`metrics/db.rs::insert_session_observations` 无条件保存有 external session 的 `session_recovery_events`。`attribution_recovery.rs::recover_session_event_mtime` / `select_best_session_event_candidate` 仍可按同仓库和时间窗将 Unknown 补为 AI。无 snapshot 的取消部分写入可能不在 preexisting 保护内；ZCode Token 本身不能证明文件来源。已要求 ZCode 同时退出两条启发式恢复，其他工具保持既有行为，并增加相邻 model_usage 的取消回归。

2. **P1：失败源查询须证明唯一调用区间，不能先只筛失败行。** 初版 `streams/agents/zcode.rs::permission_denied_bash_call` 先筛 `permission_denied` 再 `LIMIT 2`，无法看到同 call 的冲突成功/其他失败行；只检查 completed 不早于 pre，缺少 started 不晚于 pre。已要求先读取同 session/call 全部候选并拒绝歧义，再核验 `started <= frozen pre <= completed <= genuine boundary`，以及 runtime trace/turn。

3. **P1：ZCode 通用结束入口的 DB 失败不应丢失 live snapshot。** 初版 `daemon.rs::BashSessionEnd` 先移除 active pre，之后才落 history；DB 锁/写失败虽返回错误，但取消重试已失去本可用于 Unknown delta 的 snapshot。permission 专用 finisher 已先持久化后删除；取消复用通路也应保持同样顺序，仅对 ZCode 收口。

4. **P2：SQLite 查询后的并发重检需包括 pre trace。** 初版 finisher 已检查 session/call、worktree、started_at 和 invocation key，但没有同时比较捕获的 `start_trace_id`。该字段已在候选中，应在同生命周期锁内一起核对，避免替换后的同 key pre 被旧查询结果关闭。

最终源码证据：

| 项目 | 修复证据 | 结论 |
| --- | --- | --- |
| Bash 与 Token 时间窗 | `src/authorship/attribution_recovery.rs::select_best_bash_candidate`、`select_best_session_event_candidate` 在参与距离/仓库排名前拒绝 `zcode` | 成功调用、同文件、相近 mtime 或真实 Token 请求均不能将 ZCode 的 Unknown 补成 AI；其他工具保留原逻辑 |
| 唯一源终态 | `src/streams/agents/zcode.rs::terminal_bash_call` 对同 session/call 全状态 `LIMIT 2`，只接受唯一 Bash/error，核验 `started <= pre <= completed <= boundary` 及已知 trace/turn | 旧失败行、冲突成功行、其他 session/call、未来终态和不匹配 trace/turn 不关闭 pre |
| DB 写失败保全 | `src/daemon.rs::finish_zcode_terminal_pre_without_snapshot` 与 `BashSessionEnd` 的 ZCode 分支先 `record_end`，成功后才 `end_session` / 清精确 call pending | 写入失败返回错误，live snapshot 仍可重试；非 ZCode 保留原行为 |
| 查询后并发复核 | 专用 finisher 在生命周期锁内核对 worktree/session/call/start time/start trace/invocation；`BashSnapshotQuery` 与通用 End 接受并核验 expected start time/trace | 新 pre 替换相同 key 后，旧查询不能关闭它或借用其 snapshot |
| 无 worker 身份 | `AgentSessionActivity` 在分支入口核验 agent/external/生成 session ID、ZCodeSqlite、绝对源路径，再读取状态 | 关闭 transcript worker 也不会跳过来源身份校验 |

## 已确认的设计边界

- Permission denial 只结束经 SQLite 唯一终态证明的未执行调用，不生成代码归因。
- Cancellation 有可信 live pre 才观察现有 Unknown delta；重启后没有 snapshot 时，仅允许关闭本次已登记的精确持久 pre，不构造文件差量、不恢复 AI。
- 源 DB 缺失、busy、schema 错误、身份/仓库/调用区间不符或多候选时保留证据并明确失败/不处理，不以时间接近代替证明。
- 候选查询在真实生命周期 Hook 控制路径执行，不放入 trace2 接收热路径；禁止按来源不明的宽泛历史扫描回填。
- 重复 Stop/下一次 Prompt 和并发新 pre 必须幂等且不能清除另一个 call 的 pending。
- daemon 重启后的持久 pre 只查询当前 worktree/session、已登记 activation 之后、尚未结束的 ZCode 行，并限制最多 64 个候选。无 live snapshot 时只落失败终态/清本 call，不构造文件差量。
- `stream_worker=None` 时 live pre 仍可精确处理；重启后无法读取 enrollment activation 则保守不扫描/不关闭持久 pre。此时保留旧 pre 是证据边界，不应描述为已恢复或通过猜测补齐。

## 验证边界

已读到以下定向测试覆盖：

- `zcode_terminal_metadata_requires_unique_call_and_exact_pre_interval`：跨 session/call、过早/过晚时间、trace/turn、不明确取消、歧义成功行、缺表错误。
- `zcode_bash_recovery_requires_proven_terminal_success` 与 `zcode_token_metadata_never_proves_unknown_line_authorship`：ZCode 正常成功也不走启发式；旧工具仍允许既有候选。
- `test_zcode_permission_denial_without_failure_hook_clears_exact_pre_and_allows_next_bash`：重复边界、下一真实 pre 清旧拒绝、下一成功调用、人工/Unknown/AI 分行来源。
- `test_zcode_cancelled_without_failure_hook_live_and_restart_never_infers_ai_from_tokens`：live/重启无 snapshot、相邻真实 model_usage 已进入恢复库、DB trigger 拒写后 snapshot 仍在、恢复后重试、重复边界只一条终态、提交保持 Unknown、无成功 AI facts。live 另要求一个 failed/done job 与一行 Human（Unknown）delta；重启则两者均零，证明不伪造差量。

修复负责人返回的最终验证（本审查者未重复执行）：

- `task test TEST_FILTER=zcode CARGO_TEST_ARGS=--lib TEST_THREADS=1`：65/65。
- `CARGO_HOME=/Users/mark/.cargo task test TEST_FILTER=zcode:: CARGO_TEST_ARGS='--test integration' TEST_THREADS=1`：12/12。
- `task test TEST_FILTER=attribution_recovery CARGO_TEST_ARGS=--lib TEST_THREADS=1`：29/29。
- 负责人报告 rustfmt、diff check 通过。最后将 finisher 的仓库 family 解析移到外层一次，已只读确认未改变身份或持久化门禁。

本审查者未运行 Cargo，未将先前实机成功路径等同于本轮失败路径验收。源码审查与负责人测试证据分别记录，不把模拟 SQLite 源终态称为真实 ZCode 运行验收。
