# atomcode-kernel (L0)

中立的 **agent 内核** —— 由一条双向、可序列化的 Command/Event 句柄驱动。

它**不**知道任何关于审批、persona、代码智能的东西，只负责 agent 的运行循环与协议边界。
`atomcode-capabilities`(L1) 与 `atomcode-coding` / `atomcode-review`(L2) 都构建在它之上。

> 这不是「待办」边界，而是**当前已满足**的依赖边界：L0 不依赖 `atomcode-core`
> 或任何 L2/L3 crate（`cargo tree -p atomcode-kernel` 不含 `atomcode-core`）。

---

## 提供什么

- `Agent` —— 运行循环，由 `AgentCommand` / `AgentEvent` 双向驱动
- `provider` —— provider 抽象（`LlmProvider`），不含具体适配器
- `tool` —— 工具 trait 与注册
- `hook` —— `LifecycleHooks`（`offer_continuation` / `turn_complete` 等生命周期钩子，seam 已存在）
- `message` / `stream` / `event` / `request` —— 对话、流、事件、请求原语
- `checkpoint` / `conformance` / `testkit` —— 检查点、一致性测试、测试套件

```rust
use atomcode_kernel::agent::Agent;
// Agent 由 kernel 的运行循环驱动；具体 provider / 工具由上层 L1/L2 注入。
```

> 想要实时轨迹？消费 `AgentEvent`（`atomcode-clix` 就是这样打印逐工具进度的）。

---

## 生命周期 seam 现状

`LifecycleHooks`（`hook.rs`）里**已有**回合边界钩子——`offer_continuation`
（回合欲停止时被调用，返回 `Some` 即续跑）与 `turn_complete`（回合结束回调）。
coding 的 verify loop 接入的是已有的 **`offer_continuation`** seam（edit 后自动
verify、未通过则续跑），无需在内核新增任何回合末 hook。

## Cargo features

- `test-support`（dev）：gates `test_support::isolate_home()`，供各 crate 测试隔离 `~/.atomcode`。
