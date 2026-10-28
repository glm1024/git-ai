# 独立逐行验收 Oracle

`oracle.json` 的预期只来自预定人工/模型操作，已在 R1 后续提交前冻结；不能按实际 notes 或 blame 修改预期。版本为 `zcode-deep-20261028-v1`，原始 SHA256 为 `49829488e35bb1c5c050c814352d28fda174b8ebb4fc163797570eb759289740`。

唯一声明文件为 `tests/fixtures/zcode_deep_20261028_v1/lineage.txt`。每阶段目标不超过 13 行，每个非空行都有唯一稳定 `ZDA-*` ID。人工基线、人工覆盖 A02、人工插入 H07 均预期 Unknown；不因为操作由主 agent 模拟就标为 AI，也不伪装真实编辑器 KnownHuman。

| 阶段 | 操作 | 提交内预期 |
| --- | --- | --- |
| baseline | 人工写入 H01–H06，不暂存 | 6 行 Unknown |
| round1 | ZCode 自然选择工具追加 A01–A06，不暂存 | 工作区 12 行 |
| human | 人工覆盖 A02，A03 后插入 H07 | 工作区 13 行 |
| commit1 | 只暂存前 10 行，A04/A05/A06 留工作区 | AI 2，Unknown 8 |
| round2 | ZCode 改 H02/A01/A04，删 A03，添 B01 | 工作区 13 行 |
| commit2 | 暂存除 A06/B01 外的 11 行 | AI 4，Unknown 7 |
| amend | 补暂存 A06/B01，amend 第二个 commit | AI 6，Unknown 7 |

阶段完整目标由只读命令打印：

```sh
python3 oracle_stage.py human
python3 oracle_stage.py commit1
python3 oracle_stage.py round2
python3 oracle_stage.py commit2
python3 oracle_stage.py amend
```

模型自行选择工具，不指定必须使用 Write/Edit/Bash。部分暂存由测试执行者按 `commit1`/`commit2` 的 index 视图完成，不能为了方便把工作区中剩余 AI 行全部暂存；验证脚本本身不改 index、不提交、不 amend。第二个 commit 的原始 SHA 必须在 amend 前保存，之后仍用该 SHA 验证 `commit2`。

验证示例（从此目录运行）：

```sh
python3 verify_oracle.py \
  --repo /Users/mark/workspace/babel-zcode-acceptance-20261028 \
  --phase commit1 --commit COMMIT1_SHA \
  --session-round1 SESSION_FROM_ZCODE_UI \
  --output /Users/mark/workspace/ai-coding/git-ai/docs/zcode-deep-acceptance-20261028/verification-commit1.json
```

`commit2`/`amend` 同时提供 `--session-round1` 和 `--session-round2`。模型默认均为 `deepseek-v4.1-flash`，可用 `--model-round1`/`--model-round2` 绑定真实选择；不可从 notes 反推本来应该是什么模型/会话。若两轮共用会话和模型，验证仍能检查逐行文本、Unknown/AI、工具、外部会话和每 trace 模型；只有额外提供从真实 Hook/回执观察到的 `--trace-round1`/`--trace-round2` 时，才独立检查轮次的 trace 身份。可重复该选项提供一轮内多个合法 trace。

脚本只读 `git show` 和普通 Git blame；blame 只定位实际行的 origin commit/line，再批量读取对应 authorship notes。普通 Git 历史坐标不会变成预期来源。每次验证最多 5 个固定 Git 进程，没有逐行/逐 commit Git 调用；不调用 `git-ai blame`、网络或上报入口。输出不得放入被测工作区。Notes 尚未异步完成、JSON/范围损坏、文本/来源/模型不符均明确失败，不自动修补证据。

只覆盖声明的 lineage 文件，不把自然生成 Python 模块或其他 Babel 文件算作已通过逐行核验。那些文件需另行功能/来源检查。

验证器自检：

```sh
python3 -m unittest discover -s . -p 'test_verify_oracle.py' -v
```

这些 synthetic 自检只证明验证器能识别模型误绑定、Unknown 被误认领、损坏 batch 和错误坐标，不等于 ZCode 真实流程已验收。真实结果须另存机器可读 JSON，由主 agent 写报告。

## 独立自然批量任务

`catalog-oracle.json` 是另一份独立冻结预期，不修改上述 lineage oracle。初始 SHA256 为 `c81536f5aa0b1c367ff4b74e54fffd75ec101c0b6d9ae0f93ce4467fbbed7721`。目标为 `tests/fixtures/zcode_deep_20261028_v1/catalog.jsonl`：严格 64 个对象行，`id=1..64`，`code=ZDB-%03d`，`enabled=(id % 2 == 0)`。所有行预期为真实批量会话和模型产生的 ZCode AI。

允许 JSON 空白与对象键顺序差异；不允许错序、缺失/重复 ID、重复 JSON 键、额外字段、整数代替布尔或多余空行。验证时目标文件必须在该 commit 中新增，不使用已有历史来源补猜。

```sh
python3 verify_catalog.py \
  --repo /Users/mark/workspace/babel-zcode-acceptance-20261028 \
  --commit CATALOG_COMMIT_SHA --session BATCH_SESSION_FROM_ZCODE_UI \
  --model deepseek-v4.1-flash \
  --output /Users/mark/workspace/ai-coding/git-ai/docs/zcode-deep-acceptance-20261028/evidence/catalog-oracle.json
python3 -m unittest discover -s . -p 'test_verify_*.py' -v
```

批量验证复用同一严格 notes/trace-model 解析器，只使用 4 个固定只读 Git 进程；不调用模型、不生成或写入 Babel fixture、不替模型选择工具。
