# 项目全局开发约束

## core/bridge 相关改动的强制前置

当任务涉及以下任一范围时，开始设计或修改代码前必须先阅读：

- `.plan_docs/2026-07-13-core-bridge-retirement-inventory.md`
- `crates/atomcode-core/src/agent/`
- `crates/atomcode-bridge/`
- `crates/atomcode-kernel/`
- CLI、TUI、daemon 的 runtime、session、command/event 协议或 engine v1/v2 切换

阅读规划文档后仍必须以当前代码为准复核。该文档是原始盘点和目标背景，不保证所有“现状”“前置任务”和行号仍然有效。

## 已知基线校正点

以下结论来自对 `main@7bca9b84` 的核对。后续改动前必须重新验证；若代码已经变化，以当前代码为准，并在交付说明中指出差异。

1. kernel 已存在 `LifecycleHooks::turn_complete`，实际 turn 的终止路径经 `Agent::finish_turn` 调用。不得重复立项或新增功能重叠的“普通回合末 hook”。goal/loop 若仍有缺口，应先分析缺的是控制流、持有回合、evaluation 还是 wakeup 能力。
2. `atomcode-capabilities/src/compaction.rs` 已实现锚定压缩，包括 anchor 更新、固定结构、focus、近期上下文保留以及输入/输出/超时限制。不得把“实现锚定压缩”当作未完成任务重新实现。
3. CLI 和 daemon 的 v2 已默认开启，但当前 v2 仍通过 `atomcode_bridge::spawn_bridged_runtime` 或 bridge `AgentClient` 适配旧 channel。不得把“默认使用 v2”等同于“已经脱离 bridge”或“已经使用 kernel 原生 driver 协议”。
4. memory 不能标记为“接口面已退役”：core 仍有 `Remember/Forget/ShowMemory`，TUI 仍发送这些命令，bridge 仍有对应 handler。存储逻辑可以本地执行，不代表 legacy 协议已经删除。
5. 原规划中的 session 命令用户数是各命令独立用户数，不能直接相加后称为去重覆盖人数。没有用户并集数据时，只能称为“未去重人数之和”。
6. 原规划声明面向 `release/v5.0.0`，而上述核对基于 `main@7bca9b84`。在其他分支执行前必须在目标分支重新核对符号、调用路径和 Git 历史。

## 迁移与退役判定

涉及 `core/bridge` 时必须明确区分四种状态：

1. **逻辑已实现**：新栈已有能力；
2. **driver 已切换**：CLI、TUI 或 daemon 已使用新路径；
3. **legacy fallback 仍保留**：旧协议、handler 或 v1 回退仍可达；
4. **legacy 接口面已退役**：旧调用点、命令/事件 variant、bridge handler、依赖和回退已经删除，并通过相关测试。

只有第 4 种状态可以称为“已从 core/bridge 退役”。不得用“v2-native”“local”“默认走 v2”或“逻辑已迁移”代替退役结论。

一个功能的退役改动必须同时检查并报告：

- 所有 driver 发送点和事件消费点；
- `atomcode_core::agent::AgentCommand/AgentEvent` 相关 variant；
- `atomcode-bridge::runtime::on_command` 和事件转换分支；
- v1 engine 中仍可达的处理路径；
- CLI、TUI、daemon 对 bridge/core legacy 类型的依赖；
- 被删除或仍保留的测试与 fallback。

## 架构防偏约束

- 不需要运行中引擎的副作用命令可以优先迁到 driver/local service，但迁移完成后必须实际删除旧 variant 和 bridge handler，不能只新增一条旁路。
- 需要操作运行中 conversation、snapshot、provider 或 respawn 的命令，不得直接改成本地文件操作来绕开协议；必须先定义新的 runtime 所有权和命令/事件边界。
- 在当前 `AgentClient` API 不变的情况下，bridge 需要处理该 driver 的完整命令集合；但这只是当前 API 约束。不得无证据宣称架构上“绝对不能逐命令迁移”。若引入 runtime facade 或双协议路由，必须说明中间态和最终删除目标。
- goal/loop 当前共享 bridge 中的 Snapshot/continuation 生命周期。迁移前必须同时检查互斥、cancel、evaluation、delay/wakeup、turn completion 和 driver 状态事件，不能只搬一个 handler 或复制状态机。
- provider/model/session/cd/resume 等会触发 runtime 重建的能力，必须保持 session id、working directory、snapshot、provider 选择、审批状态和 gateway affinity 语义；不得为了减少 bridge 行数而丢失这些行为。
- retirement 工作以删除 legacy surface 为度量。redo、展示优化、footer、diff/copy/save 增强等产品功能不得计入 bridge 退役进度，除非它们是切换 parity 的硬前置。
- 不得仅凭文件行数评估完成度。每个里程碑必须列出预计删除和实际删除的类型、variant、handler、依赖、feature flag 与 fallback。

## 修改前检查

开始相关改动前必须完成：

1. 记录当前 branch、commit SHA 和 worktree 状态；
2. 使用代码搜索确认目标符号的发送方、处理方和事件消费者；
3. 查看相关文件近期 Git 历史，确认规划中的任务没有已经实现；
4. 写明本次达到上述四种状态中的哪一种；
5. 写明本次预计删除的 legacy surface，而不只是要新增的代码。

如果发现规划文档与当前代码冲突，不得按旧规划盲目实现；应保留规划文档原文作为背景，在交付说明中报告冲突，并依据当前代码选择实现方向。

## 验证与交付

- 先运行受影响 crate 的针对性测试，再运行实际可行的最广 workspace 检查。
- 协议迁移必须覆盖 CLI、TUI、daemon、headless、session resume、approval、cancel 和 provider reload 中受影响的路径。
- 最终说明必须包含：验证基线、达到的迁移状态、实际删除项、仍可达的 legacy surface、测试结果和唯一下一步。
- 未删除旧 variant、handler 或依赖时，必须明确写“尚未退役”，不得以功能可用代替完成声明。
