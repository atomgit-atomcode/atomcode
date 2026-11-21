# 输出截断恢复修复

基线：release/v5.2.7，1d946853286c98d2e7ec05b3b19b22ef18edb5bb。
开始时工作区有 MCP / computer-use 等未提交修改，保留；实现期间这些修改由其他工作提交。

日志中的五次 length 都消耗 32768 completion tokens，只有 reasoning，没有正文。
现有恢复预算按整个 turn 累积，因此分散数小时的截断也会耗尽四次续接。

## 设计与所有权

恢复策略仍只归 harness 的 truncation-recovery 行持有，agent-loop 只按其结果决定继续。
正常 ModelResponse 重置计数和重复正文检测；连续截断仍最多自动恢复四次。
纯思考截断要求基于已有推理推进任务，不要求追加不存在的正文文件。
检查点追加可选 reasoning_only 字段，旧消费者可忽略；TUI 缺字段时使用原通用提示。
无交互 driver 或拒绝继续时沿用现有结束语义；不改变租约、持久化格式、generation 或审批路径。

模型预算继续走现有模型配置 max_tokens -> ChatOptions -> provider，覆盖默认输出上限。
没有已证明可用的 lumen-alpha 输出能力值，因此不猜测新上限，不自动改变用户思考档位。
OpenAI-compatible 请求日志补实际 max_tokens / max_completion_tokens，终止日志补 finish_reason。
不扩展 StreamEvent 公共协议；其他 provider 的完整结构化原因归一化留作后续独立工作。

参考本地 OpenCode 的连续 length 判断；oh-my-pi 的输出上限与上下文不足分流；
Codex 对 incomplete 原因的保留；deepseek-harness 的模型级输出预算配置。

## 验证

用真实 harness loop + 脚本 provider 回归正常工具轮次打断截断链、纯思考恢复指令、连续截断停止。
检查 TUI 新旧检查点 payload、已有正文恢复与重复正文保护，以及模型显式输出预算覆盖。
运行受影响 crate nextest、compile gate 与格式检查。

## 实际验证结果

- `cargo nextest run -p atomcode-harness`：319 passed（审查补充正文重复检测回归后重跑）。
- `cargo nextest run -p atomcode-tui -p atomcode-i18n -p atomcode-capabilities`：4229 passed，3 skipped。
- `bash gates/compile.sh`：workspace/all-targets、feature 门与两项阴性对照均通过。
- `cargo fmt --all -- --check`、`git diff --check`：通过。
- Cargo.lock 无 diff，hkdf / hmac / zeroize_derive 均为 0；未改变 index 标志。
- 未调用真实 lumen-alpha 服务；没有据此声称增加正文产出率或缩短模型思考时间。

## 提交前审查

复核恢复计数、重复正文检测、检查点可选字段兼容与继续/停止响应、日志字段和预算配置路径，未发现阻塞项。
补充真实 harness loop 回归：连续重复正文会停止，正常工具请求后的相同正文可继续恢复。
新增测试后重跑 harness 全部测试、compile gate 与格式检查，均通过。
默认输出上限与用户思考强度保持；模型输出能力适配、真实 lumen-alpha 效果尚未验证。
