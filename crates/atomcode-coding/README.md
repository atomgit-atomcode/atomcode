# atomcode-coding (L2)

CODING 特化层。它把中性内核（[`atomcode_kernel`]）+ 能力层
（[`atomcode_capabilities`]）组装成一个**可运行、会自我纠正**的 coding agent。

结构对标 [`atomcode-review`](../atomcode-review)，但面向编码。

---

## 依赖现状：L2 直接依赖 atomcode-core

`atomcode-coding` **依赖 `atomcode-core`**，并非「完全不涉及 core」。依赖图
`atomcode-coding → atomcode-core` 成立，且是**单向**的——core 不反向依赖
kernel / L1 / L2（`Cargo.toml` 注明「L2 is allowed to depend on core」）。当前用到
core 的地方：

- **模型视觉判断**：`assemble.rs` / `parts.rs` 使用 core 的模型视觉（vision）判断。
- **CodingPlan 客户端与类型**：`rate_limit.rs` 使用 core 的 `CodingPlan` 类型及 REST 客户端。
- 直接依赖的 workspace crate：`atomcode-core` / `atomcode-config` /
  `atomcode-telemetry` / `atomcode-review`。

> 若未来想让 L2 脱离 core，需把上述用法（vision 判断、CodingPlan 客户端）下沉到
> capabilities / 本地 service——但这是方向性设想、非既定承诺：当前 `Cargo.toml` 明确
> 允许 L2 依赖 core。

---

## L2 核心抽象（已就位的三种装配原语）

均通过现有内核 seam 挂载，不新增内核面：

1. **Assembly** —— [`build_coding_agent`]：把 provider + tools + codeintel +
   approval + persona + verify 纪律装配进一个内核 `Agent`。
2. **Persona** —— [`persona::coding_persona`]：coding 系统提示词。
3. **Discipline** —— [`discipline::VerifyCadenceHook`]：edit-then-verify 的
   `offer_continuation` 钩子（编码自我纠正循环，接入内核已有的
   `LifecycleHooks::offer_continuation` seam，非新增回合末 hook）。

```rust
# async fn demo() -> Result<(), String> {
use atomcode_coding::{build_coding_agent, CodingAgentConfig};
use atomcode_kernel::agent::AutoRespond;

let agent = build_coding_agent(CodingAgentConfig::new(
    "sk-...", "https://api.deepseek.com/v1", "deepseek-chat", ".",
))?;
let outcome = agent.run_to_completion("fix the build", AutoRespond::AllowAll).await;
println!("{}", outcome.text);
# Ok(()) }
```

> [`build_coding_agent`] 是**最小同步装配**（仅 tools + codeintel）。完整 agent
> （web/skills/mcp/session/memory 全部接好）是 [`parts`] 里的两阶段
> [`prepare`] → [`assemble`]。

---

## 模块与职责（公开 API + 内部实现）

除上面三项核心抽象外，L2 还涉及以下职责。**公开 API**（通过 `pub mod` 或 re-export 暴露）：

- `config` —— coding 专属配置。
- `runtime` —— runtime control（运行期控制）。
- `plan_mode` —— plan 模式。
- `parts` / `persona` / `discipline` —— 两阶段装配、persona、verify 纪律（均 `pub mod`）。
- `telemetry` —— 遥测上报。
- `subagent_tiers` —— 子 agent 分层（task 子 agent 等）。
- `TodoHook`（`pub use todo::TodoHook`）—— todo 跟踪钩子。

**内部实现**（私有 `mod`，未直接公开）：

- `mod rate_limit` —— CodingPlan 限速（`RateLimitHook`），依赖 core 的 CodingPlan 客户端。
- `mod todo` —— todo 钩子内部实现（`TodoHook` 已 re-export）。
- `mod assemble` —— `build_coding_agent` 等最小装配（已 re-export）。
- `mod init_prompt` —— 初始化提示词。
- 会话组装 —— 通过 capabilities 的 `session` feature 接入。

---

## Cargo features

以 `["provider", "tools", "web", "codeintel", "skills", "mcp", "session",
"memory", "cc-hooks", "offline"]` 引入 **coding 默认装配所需的能力集**（注意：
未启用 `atomgit` / `lsp` / `notify` 等 L1 feature —— 该集合是有选择的，并非覆盖
L1 全部 feature）。L2 是带观点的一方装配，生产 coding agent 默认把这些接好。
