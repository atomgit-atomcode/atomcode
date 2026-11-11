# 纯思考截断恢复预算

基线 release/v5.2.7，685ef57512bb9d7dca00ef83ef830c01f55f6259，工作区干净。
真实 lumen-alpha/high/32768 请求耗时 581377ms，131664 字符思考、无正文，以 length 结束。
既有四次恢复预算会允许同一无进展模式反复消耗。

恢复策略继续由 harness truncation-recovery 行唯一持有。纯思考连续截断默认只自动恢复一次，
第二次进入已有检查点或 headless warning；正文续写仍用原四次预算。正常请求打断连续计数。
检查点允许继续当前设置、按 provider 声明的更低档位重试一次、停止。未知默认档位、无支持档位
不提供降档。用户选择只影响下一请求；失败、取消、新 turn 不遗留到后续请求或会话配置。
选择继续不重置自动预算，每次明确选择只授权额外一请求。取消、缺 driver 和坏回答停止。

复用日志 Notice.retry 的 RetryAttempt 保存真实次数，不新增持久化事件。
AgentEvent.OutputTruncationRecovery 增加可选 reason（反序列化默认 None），同步 CLI/daemon 消费者，
旧日志缺次数时投影为 Warning，避免假 0/0。JSONL retry 沿用现有 reason 字段。
不调整默认 32K，不改用户思考档位，不把 provider 业务语义加入 kernel 命令。

验证：真实 harness 请求回归预算、继续/降档一次性与恢复默认、未知档位拒绝降档，
投影和 TUI 测试；受影响 crate nextest、compile gate、fmt；隔离认证构建实测 lumen-alpha。

## 验证结果

- 新增真实 harness loop 回归：两次纯思考截断停止、明确继续只增加一请求、降档只影响一请求且新 turn 保持原档位、无可用档位与坏回答停止、历史通知无次数时保留 warning、协议缺 reason 可读。
- 最终 harness 全量：324 passed。kernel/harness/TUI/i18n 联合测试（最后追加坏回答测试前）：2742 passed。
- CLI/daemon/coding 完整测试：2390 passed，8 failed，11 skipped。以下 8 项均在独立未修改的 685ef5751 检出中复现：
  - host::a_turn_stopped_before_any_answer_can_be_undone_by_its_number（取消回合撤销点不存在）。
  - tui_bg::shared_navigation_to_a_missing_project_keeps_the_old_foreground（已删除目录被接受）。
  - host_rows::goal_rate_limit_classification_tests::coding_rate_limits_share_the_attempt_and_wait_allowance（等待时间断言）。
  - loop_policy::the_guard_tells_a_repeating_todo_update_that_the_same_status_will_not_change_again（空 todo 列表中的 id 4）。
  - recovery::mixed_failures_share_six_attempts_and_keep_partial_progress（等待时间断言）。
  - recovery::mixed_failures_share_the_wait_allowance_without_shortening_retry_after（等待时间断言）。
  - recovery::ordinary_request_recovery_is_finite（等待时间断言）。
  - recovery::ordinary_requests_survive_a_short_gateway_outage（等待时间断言）。
- `bash gates/compile.sh`（最终坏回答保护后）、格式门、diff 检查通过。Cargo.lock 无修改，三项私有 overlay 依赖名在主检出均为 0。

## 真实服务验证

在隔离检出装入私有认证实现并构建 dev 二进制；仅用于本地验证，不修改主检出 crypto stub 或安装的 release 二进制。
模型 lumen-alpha/high，临时 home / 项目，测试结束移除复制的认证文件。

- 128-token 纯推理：2 个 length 响应，共 256 completion tokens，1 次自动恢复后停止；JSONL 为 attempt=1/max_attempts=1，附带真实原因；正文仍为空，不能说模型问题消失。
- 默认 32768-token 简单计算：正常回答 1591，一次请求结束。
- TUI 实际出现继续 / medium 降档一次 / 停止；选降档、普通继续、停止后，实际发出 high/high/medium/high 四个请求，终态 Stopped，没有额外请求。
- 本次未重跑默认 32K 的困难题长等待；先前 581377ms/32768-token 复现仍是其基线，不能据本次小预算验证宣称缩短首次思考时间。
- 本地证据 `/tmp/atomcode-reasoning-check-20261111/summary.json`、`tui-provider-evidence.log`、`tui-checkpoints.txt`。

## 进一步优化：按会话隔离与隐藏推理

继续开发时仍在 685ef5751，保留上一轮未提交修改。核对 agent::as_agent/scoped、TurnStart、AgentRemoved 及近期历史。
既有 truncation-recovery 行的共享计数/重复正文/一次性 effort 会被子 agent 的 TurnStart 和成功响应重置；
其 pending effort 也可能被另一会话的下一请求消费。改为该行持有按 session id 索引的短期状态，
每个请求仅操作自己的状态；新 turn、成功响应只清自己的条目，AgentRemoved 清理被移除成员。
不持有 live AgentHandle，不引入第二生命周期 owner；锁不跨 await，已移除会话的迟到请求只持有旧状态 Arc。

无正文且没有完整工具参数的 length 响应也属于无可用进展，即使 provider 不暴露 reasoning。
沿用一次自动恢复预算，但提示不声称观察到了隐藏推理。检查点增加可选 no_visible_progress；
旧 payload 保持原解释，只有模型声明支持降档且当前档位已知时才提供显式降档选项。
不改变默认 32K，不改变模型/默认 effort。已存在的 exact reasoning loop detector 继续负责流式复读，
不新增猜测式时间/字符阈值或第二检测器。

补充测试覆盖交错会话额度、成员 turn/reset 不影响 lead、一次性 effort 不被成员消费、
AgentRemoved 清理、跨会话相同正文、隐藏推理/不完整参数的无进展保护，以及新旧 TUI payload。

### 进一步优化的最终验证

- harness 最终全量 333 passed（含真实 AgentLoop 中未完成参数不能绕过停止额度的回归）；harness/TUI/i18n 联合 2365 passed；单独会话隔离 8 passed。
- coding runtime_criteria + mount_wiring：139 passed，覆盖实际装配、取消、会话、模型重载等入口。
- 最终 compile gate 与 fmt/diff 检查通过。主检出 Cargo.lock 未变。
- 发现并修正审计声明：SessionSvc 是 agent realm 内的 core 服务，不作为挂载时由插件提供的 sessions seam 声明；缺少运行时会话时仍明确失败。
- 停止无进展恢复时清除尚未完整的工具调用，防止 loop 因工具回合仍有欠账而再次请求模型；有完整工具调用的响应仍保留正常额度。
- 最终 lumen-alpha/high/128-token 实测：两次 length、合计 256 completion tokens、1/1 自动恢复通知，随后 Stopped；正文仍为空。
- 未重跑默认 32K 困难题；隐藏推理/未完成调用/多会话交错由可重复的回归验证，不能称为真实团队模型已完整压测。
- 本轮未再次跑 CLI/daemon/coding 全量；此前独立基线已复现的 8 项失败保留，不称为整个项目测试全绿。
- 本地证据 `/tmp/atomcode-no-progress-check-20261111/summary.json`；测试认证文件已清理，安装的 release 二进制未覆盖。


## 最终补齐（2026-11-11）

首次思考等待保护、无工具技能目录边界和八项基线失败的处理及最终证据见
[剩余问题实施与验证记录](2026-11-11-remaining-response-and-baseline-fixes.md#final-verification)。
旧段落中的“尚未实现/尚未重跑”是当时验证阶段的状态；本次以新记录为准。
默认 120 秒保护通过缩短为 3 秒的真实模型请求验证，未再等待完整两分钟。
工具正文标记在新实测中未复现，不能据此声称上游模型/网关格式问题已彻底消除。
