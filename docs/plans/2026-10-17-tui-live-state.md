# TUI 运行状态空档修复

基线：release/v5.2.4，17586a88e；在独立 worktree 修改，保留主工作区其他修改。

运行时唯一 owner 仍是 CodingRuntime；正常运行期间提交继续走原生 steer/inbox。
不新增重试、运行时或输入队列，不绕过取消收尾和策略确认。

确认的空档：日志 TurnEnd、kernel Cancelled 和 runtime TurnFinished 并非同一个提交点。
TUI 先清掉 live 状态时，runtime 可能仍在收尾；取消期间 submit 会明确返回 Busy。
这解释了一种可能路径，尚无反馈用户的日志，不能断言是其唯一根因。

修复：Cancelled 保留正在停止直到 runtime 终态；日志关闭后仍展示有效的运行时
运行/停止状态；输入队列展示同时参考 agent 状态；Busy 使用现有本地化提示。
runtime 拒绝提交时记录 cancellation_pending 或 policy_intervention_pending，
不记录消息、凭据或策略正文，以便复现时进一步归因。

验证：live 状态单测、取消先于终态的 TUI 集成测试、现有运行中追加/取消回归；
完整受影响测试与 compile 门见交付说明。未在 Windows 真实终端复现用户案例。

已完成：TUI lib 1504/1504、e2e 181/181、CLI host 50/50；fmt 与 diff 检查通过。
既有 Ctrl+P 测试的等待条件原先会命中欢迎页 /compact，已改为等待菜单本身出现，
避免在输入事件尚未处理时断言。

隔离基线下 bash gates/compile.sh 全部通过（含 workspace all-targets、
非默认 feature 与两组闸门自检）。主工作区其他进行中的改动不在此验证结论内。

## 2026-10-19 提交前审计

补充修复：Rejected 按回执移除对应的队列项，退回对应原文，保留其他排队消息
及已有草稿，避免假队列或退回最后一条消息。新增回归覆盖连续消息与已有草稿。

接到最新 release 基线 579b63aa6；补充测试 TUI lib 1513/1513、e2e 182/182
通过；原基线及最新基线编译闸门均通过。未实机验证 Windows 原反馈。
