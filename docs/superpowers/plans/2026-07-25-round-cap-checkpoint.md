# 回合上限检查点问询 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把主对话回合数上限从「红色错误硬中断」改为「继续 / 停止」的可选问询卡片，每次「继续」重新武装计数器；顺带把上限做成 `[coding] max_rounds` TOML 可配。

**Architecture:** kernel 熔断点从 `emit(Error)+finish_turn(MaxRounds)` 改为（当新开关打开时）走 `RequestCtx::request` 通用往返问驱动，`{continue:true}` → 抬高 `cap` 继续、否则 → `MaxRounds`。开关默认 `false`（所有非 TUI 路径行为零变化、绝不 park），仅 TUI 打开并实现渲染臂（复用 `request_user_input` 的 Single picker 渲染器）。

**Tech Stack:** Rust（atomcode-kernel / atomcode-coding / atomcode-config / atomcode-tuix 四 crate），tokio，serde_json。

## Global Constraints

- 默认开关 `round_cap_checkpoint = false`；只有 TUI 置 `true`。非 TUI 路径必须逐字保持今天的 `emit(Error)+finish_turn(MaxRounds)` 行为。
- 任何降级 `Null` 响应（无 requester / 超时 / Cancel）一律视为「停止」（fail-closed）→ `finish_turn(StopReason::MaxRounds)`。
- kernel 只认最小响应 `{"continue": bool}`；中文标签/统计只存在于 TUI，绝不进 kernel/wire。
- kind 常量单一来源：kernel 导出 `pub const ROUND_CAP_CHECKPOINT_KIND: &str = "round_cap_checkpoint"`，TUI import 它。
- 签名熔断（3 nudge / 6 停 `RepeatLoop`）不改。`StopReason::MaxRounds`、`finish_turn`、Cancel 语义不改。
- 配置优先级：env `ATOMCODE_TURN_MAX_ROUNDS` > `[coding] max_rounds`（TOML）> 默认 200。`0` = 关闭上限（回到无限，复用现有 `if cfg.max_rounds != 0` 门控）。
- webui/daemon 镜像 = 本计划范围外（defer）。

---

## File Structure

- `crates/atomcode-kernel/src/event.rs` — 新增 `ROUND_CAP_CHECKPOINT_KIND` 常量（挨着 `AgentEvent::Request`）。
- `crates/atomcode-kernel/src/agent.rs` — `AgentBuilder` + `RunningAgent` 加 `round_cap_checkpoint: bool`；`build()` 透传；`max_rounds` setter 旁加 setter；`run_turn` 熔断分支改造 + 可变 `round_cap` 再武装。
- `crates/atomcode-config/src/config/mod.rs` — 新增 `CodingConfig { max_rounds }` 段 + `Config.coding` 字段 + `save()` 注释。
- `crates/atomcode-coding/src/config.rs` — `CodingAgentConfig` 加 `round_cap_checkpoint: bool`；新 `resolve_turn_max_rounds`；`CodingRuntimeConfig` 读 `[coding] max_rounds` 并透传 turn 上限。
- `crates/atomcode-coding/src/parts.rs:1333` 与 `crates/atomcode-coding/src/assemble.rs:113` — `builder.max_rounds` 旁加 `builder.round_cap_checkpoint(cfg.round_cap_checkpoint)`。
- `crates/atomcode-tuix/src/state.rs` — 新 `RoundCapPanel` 结构 + `UiState.round_cap_panel` 字段 + `UiPhase::RoundCap` + reset 处清理。
- `crates/atomcode-tuix/src/event_loop/mod.rs` — TUI flip 开关；`Request` 分派新臂；`deliver_round_cap` helper；key 路由 `handle_round_cap_key`。
- `crates/atomcode-tuix/src/render/retained.rs` / `render/mod.rs` — RoundCap 面板渲染（复用 `build_user_input_rows`）。

---

## Task 1: Kernel — 检查点熔断分支 + 再武装

**Files:**
- Modify: `crates/atomcode-kernel/src/event.rs`（`AgentEvent::Request` 定义附近）
- Modify: `crates/atomcode-kernel/src/agent.rs:3191`（builder 字段）、`:3227`（default）、`:3310` 后（setter）、`:851`(build 透传)、`:952`(running 字段)、`:1642`（run_turn 循环前）、`:1662-1674`（熔断分支）
- Test: `crates/atomcode-kernel/tests/failure_perception.rs`

**Interfaces:**
- Produces: `atomcode_kernel::ROUND_CAP_CHECKPOINT_KIND: &str`（Task 3 import）；`AgentBuilder::round_cap_checkpoint(bool) -> Self`（Task 2 调用）。
- Consumes: 现有 `RequestCtx::request(&str, Value) -> Value`（`request.rs:155`）、`StopReason::MaxRounds`、`finish_turn`。

- [ ] **Step 1: Write the failing test（继续→再武装；停止/Null→MaxRounds；关→旧行为）**

在 `crates/atomcode-kernel/tests/failure_perception.rs` 末尾追加（沿用文件里现有的 testkit builder 与 `max_rounds_stop_reason` 同款脚手架，见该文件 `max_rounds_stop_reason` 约 119 行）：

```rust
// ── round-cap checkpoint（round_cap_checkpoint = true）─────────────────────
#[tokio::test]
async fn round_cap_checkpoint_continue_rearms_then_stop() {
    // 上限 2；脚本 5 轮工具。第 3 轮触发检查点：先答 continue（cap→4），
    // 第 5 轮再触发：答 stop → MaxRounds。
    let provider = scripted_tool_rounds(5); // 复用文件内已有的脚本 provider 助手
    let mut handle = AgentBuilder::default()
        .provider(provider)
        .tools(noop_tools())
        .max_rounds(2)
        .round_cap_checkpoint(true)
        .build()
        .spawn();
    handle.commands.send(AgentCommand::SendMessage { text: "go".into(), images: vec![] }).unwrap();

    let mut checkpoints = 0;
    let mut stop = None;
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::Request { id, kind, .. } if kind == atomcode_kernel::ROUND_CAP_CHECKPOINT_KIND => {
                checkpoints += 1;
                let cont = checkpoints == 1; // 第一次继续，第二次停止
                handle.commands.send(AgentCommand::Respond {
                    id,
                    value: serde_json::json!({ "continue": cont }),
                }).unwrap();
            }
            AgentEvent::TurnComplete { reason } => { stop = Some(reason); break; }
            _ => {}
        }
    }
    assert_eq!(checkpoints, 2, "continue 必须再武装以触发第二次检查点");
    assert!(matches!(stop, Some(StopReason::MaxRounds)), "停止应终结为 MaxRounds");
}

#[tokio::test]
async fn round_cap_checkpoint_null_response_stops_fail_closed() {
    let provider = scripted_tool_rounds(5);
    let mut handle = AgentBuilder::default()
        .provider(provider)
        .tools(noop_tools())
        .max_rounds(2)
        .round_cap_checkpoint(true)
        .build()
        .spawn();
    handle.commands.send(AgentCommand::SendMessage { text: "go".into(), images: vec![] }).unwrap();
    let mut stop = None;
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::Request { id, kind, .. } if kind == atomcode_kernel::ROUND_CAP_CHECKPOINT_KIND => {
                // 驱动答 Null（模拟无值守/超时）
                handle.commands.send(AgentCommand::Respond { id, value: serde_json::Value::Null }).unwrap();
            }
            AgentEvent::TurnComplete { reason } => { stop = Some(reason); break; }
            _ => {}
        }
    }
    assert!(matches!(stop, Some(StopReason::MaxRounds)), "Null → fail-closed 停止");
}

#[tokio::test]
async fn round_cap_checkpoint_off_keeps_hard_error() {
    // 开关默认关：撞上限仍发 Error + MaxRounds，永不发 Request。
    let provider = scripted_tool_rounds(5);
    let mut handle = AgentBuilder::default()
        .provider(provider)
        .tools(noop_tools())
        .max_rounds(2) // round_cap_checkpoint 默认 false
        .build()
        .spawn();
    handle.commands.send(AgentCommand::SendMessage { text: "go".into(), images: vec![] }).unwrap();
    let mut saw_error = false;
    let mut requests = 0;
    let mut stop = None;
    while let Some(ev) = handle.events.recv().await {
        match ev {
            AgentEvent::Request { kind, .. } if kind == atomcode_kernel::ROUND_CAP_CHECKPOINT_KIND => requests += 1,
            AgentEvent::Error { message, .. } if message.contains("max rounds") => saw_error = true,
            AgentEvent::TurnComplete { reason } => { stop = Some(reason); break; }
            _ => {}
        }
    }
    assert_eq!(requests, 0, "关闭时绝不发检查点 Request");
    assert!(saw_error, "关闭时仍发红色 Error");
    assert!(matches!(stop, Some(StopReason::MaxRounds)));
}
```

> 注：`scripted_tool_rounds(n)` / `noop_tools()` 若文件内命名不同，用 `max_rounds_stop_reason`（failure_perception.rs:119）里同款脚手架照抄——它已构造「脚本足够多轮以超过 cap」的 provider。

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p atomcode-kernel --test failure_perception round_cap_checkpoint`
Expected: 编译失败 `no method named round_cap_checkpoint` / `no ROUND_CAP_CHECKPOINT_KIND`。

- [ ] **Step 3: 加 kind 常量**

在 `crates/atomcode-kernel/src/event.rs` 顶部（`AgentEvent` 定义上方或同模块公开处）加：

```rust
/// Driver round-trip `kind` for the round-cap checkpoint (kernel-initiated:
/// the fuse pauses the turn and asks the driver "continue past the cap?").
/// The driver answers `{"continue": bool}`; any non-object / missing / Null
/// response degrades to `false` (stop). Distinct from `request_user_input`
/// (model-initiated, in atomcode-capabilities).
pub const ROUND_CAP_CHECKPOINT_KIND: &str = "round_cap_checkpoint";
```

在 `crates/atomcode-kernel/src/lib.rs` 确认 `pub use` 暴露它（若 event 模块已 `pub use event::*` 则无需改；否则加 `pub use event::ROUND_CAP_CHECKPOINT_KIND;`）。

- [ ] **Step 4: builder 字段 + default + setter + build 透传 + running 字段**

`agent.rs:3216` `keep_interrupted_context: bool,` 后加字段：
```rust
    /// When true, the `max_rounds` fuse becomes an interactive CHECKPOINT: instead
    /// of `emit(Error)+MaxRounds`, it round-trips the driver (kind
    /// `ROUND_CAP_CHECKPOINT_KIND`) and only stops on a non-continue answer. Default
    /// `false` → today's hard-stop (so a driver that doesn't implement the kind can
    /// never park). Only a driver that renders the checkpoint sets this true.
    round_cap_checkpoint: bool,
```
`agent.rs:3269` `keep_interrupted_context: false,` 后加 `round_cap_checkpoint: false,`。
setter（`agent.rs:3313` `max_rounds` setter 后）：
```rust
    /// See `AgentBuilder.round_cap_checkpoint`. Default false.
    pub fn round_cap_checkpoint(mut self, on: bool) -> Self {
        self.round_cap_checkpoint = on;
        self
    }
```
build 透传（`agent.rs:875` `keep_interrupted_context: self.keep_interrupted_context,` 后）：`round_cap_checkpoint: self.round_cap_checkpoint,`。
RunningAgent 字段（`agent.rs` struct，`keep_interrupted_context` 字段旁）：`round_cap_checkpoint: bool,`。

- [ ] **Step 5: run_turn 熔断分支改造 + 可变 cap**

`agent.rs:1641`（`let mut repeat_nudged = false;` 后、`loop {` 前）加可变上限：
```rust
        // Re-armable round cap: on each checkpoint "continue" this grows by the base
        // `max_rounds`, giving a CONSTANT interval between confirmations.
        let mut round_cap = self.max_rounds;
```

将 `agent.rs:1662-1674` 的熔断块整体替换为：
```rust
            // Hard cap (safety fuse). With `round_cap_checkpoint`, this becomes an
            // interactive checkpoint instead of a hard error.
            if let Some(cap) = round_cap {
                if round > cap {
                    if self.round_cap_checkpoint {
                        let resp = self
                            .rt
                            .request(
                                crate::event::ROUND_CAP_CHECKPOINT_KIND,
                                serde_json::json!({ "round": round - 1, "cap": cap }),
                            )
                            .await;
                        let cont = resp
                            .get("continue")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false);
                        if cont {
                            // Re-arm by the configured base (guaranteed Some here).
                            let base = self.max_rounds.unwrap_or(cap);
                            round_cap = Some(cap.saturating_add(base));
                            // fall through: this round (== cap+1 <= new cap) proceeds.
                        } else {
                            self.finish_turn(convo, StopReason::MaxRounds, &turn_ctx).await;
                            return;
                        }
                    } else {
                        self.rt.emit(AgentEvent::Error {
                            message: format!("max rounds ({cap}) reached"),
                            http_status: None,
                            code: None,
                        });
                        self.finish_turn(convo, StopReason::MaxRounds, &turn_ctx).await;
                        return;
                    }
                }
            }
```

> `turn_ctx.max_rounds`（agent.rs:1657）保持读 `self.max_rounds`（展示用原始配置值，不动）。

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p atomcode-kernel --test failure_perception round_cap_checkpoint`
Expected: 3 个新测试 PASS。

- [ ] **Step 7: 全量 kernel 回归 + commit**

Run: `cargo test -p atomcode-kernel`
Expected: 绿（含既有 `max_rounds_stop_reason` 等）。
```bash
git add crates/atomcode-kernel/src/event.rs crates/atomcode-kernel/src/agent.rs crates/atomcode-kernel/src/lib.rs crates/atomcode-kernel/tests/failure_perception.rs
git commit -m "feat(kernel): round-cap fuse becomes opt-in interactive checkpoint

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Config — `[coding] max_rounds` TOML + checkpoint 标志透传

**Files:**
- Modify: `crates/atomcode-config/src/config/mod.rs`（新 `CodingConfig`；`Config` 加 `coding` 字段；`save()` 注释）
- Modify: `crates/atomcode-coding/src/config.rs`（`CodingAgentConfig` 加 `round_cap_checkpoint`；`resolve_turn_max_rounds`；`CodingRuntimeConfig` 透传 turn 上限）
- Modify: `crates/atomcode-coding/src/parts.rs:1333`、`crates/atomcode-coding/src/assemble.rs:113`
- Test: `crates/atomcode-coding/src/config.rs`（`#[cfg(test)]` 内，同文件已有 `resolve_loop_max_rounds` 测试 ~525）

**Interfaces:**
- Consumes: `AgentBuilder::round_cap_checkpoint(bool)`（Task 1）。
- Produces: `CodingAgentConfig.round_cap_checkpoint: bool`（Task 3 flip）；`Config.coding.max_rounds: u32`。

- [ ] **Step 1: 加 `[coding]` config 段（失败测试）**

`crates/atomcode-config/src/config/mod.rs`，在 `LoopConfig`（:54）附近加：
```rust
/// `[coding]` table. Turn-level knobs for the main coding agent. `max_rounds` is
/// the per-turn round cap (the interactive checkpoint threshold); `0` = unbounded.
/// Env `ATOMCODE_TURN_MAX_ROUNDS` overrides this.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CodingConfig {
    pub max_rounds: u32,
}
impl Default for CodingConfig {
    fn default() -> Self { Self { max_rounds: 200 } }
}
```
`Config` 结构（:110）加字段（放在 `loop_config` 旁 :163 后）：
```rust
    /// `[coding]` turn-level policy. Missing from older configs → max_rounds=200.
    #[serde(default)]
    pub coding: CodingConfig,
```
若 `Config` 有手写 `Default`/构造器，同步加 `coding: CodingConfig::default()`。

在 `crates/atomcode-coding/src/config.rs` 的 `#[cfg(test)]`（~525 `resolve_loop_max_rounds` 测试旁）加：
```rust
    #[test]
    fn turn_max_rounds_env_overrides_toml() {
        assert_eq!(resolve_turn_max_rounds(200, Some("500")), 500);
        assert_eq!(resolve_turn_max_rounds(200, Some("0")), 0);      // 0 关闭保留
        assert_eq!(resolve_turn_max_rounds(300, Some("bad")), 300);  // 非法回退 TOML
        assert_eq!(resolve_turn_max_rounds(300, None), 300);
    }
```

- [ ] **Step 2: Run to verify fail**

Run: `cargo test -p atomcode-coding --lib turn_max_rounds`
Expected: 编译失败 `cannot find function resolve_turn_max_rounds`。

- [ ] **Step 3: 加 `resolve_turn_max_rounds` + `CodingAgentConfig` 字段**

`crates/atomcode-coding/src/config.rs`，仿 `resolve_loop_max_rounds`（:442）加：
```rust
/// env（若为合法 u32）优先，否则用 TOML 配置值。与 resolve_loop_max_rounds 同形。
pub fn resolve_turn_max_rounds(configured: u32, env: Option<&str>) -> u32 {
    env.and_then(|s| s.trim().parse::<u32>().ok()).unwrap_or(configured)
}
```
`CodingAgentConfig`（:51 `max_rounds` 旁）加字段：
```rust
    /// When true, the kernel turns the `max_rounds` cap into an interactive
    /// checkpoint (see AgentBuilder). Default false; only the TUI driver sets it.
    pub round_cap_checkpoint: bool,
```
在 `CodingAgentConfig::new`（:467 附近，`max_rounds: default_turn_max_rounds(),` 旁）加 `round_cap_checkpoint: false,`。

- [ ] **Step 4: CodingRuntimeConfig 透传 turn 上限**

`crates/atomcode-coding/src/config.rs` 的 `CodingRuntimeConfig`：加字段 `pub turn_max_rounds: u32,`（放 `loop_max_rounds` 旁，:158 附近）。
`from_config`（:219 `loop_max_rounds:` 旁）加：
```rust
            turn_max_rounds: resolve_turn_max_rounds(
                config.coding.max_rounds,
                std::env::var("ATOMCODE_TURN_MAX_ROUNDS").ok().as_deref(),
            ),
```
`agent_config()`（:250 `config.loop_max_rounds = self.loop_max_rounds;` 旁）加 `config.max_rounds = self.turn_max_rounds;`。

- [ ] **Step 5: 两处 builder 透传 checkpoint 标志**

`crates/atomcode-coding/src/parts.rs:1332-1334`：
```rust
    if cfg.max_rounds != 0 {
        builder = builder.max_rounds(cfg.max_rounds);
    }
    builder = builder.round_cap_checkpoint(cfg.round_cap_checkpoint);
```
`crates/atomcode-coding/src/assemble.rs:112-114` 同样在 `builder.max_rounds` 后加 `builder = builder.round_cap_checkpoint(cfg.round_cap_checkpoint);`。

- [ ] **Step 6: save() 写 `[coding]` 注释段**

在 `config/mod.rs` 的 `save()` 里，仿 `[loop_config]`/`[subagent]` 的写法追加 `[coding]` 段并带注释（说明 `max_rounds`=每回合轮次上限、`0`=无限、env `ATOMCODE_TURN_MAX_ROUNDS` 覆盖）。定位 `save()` 内现有 `loop_config` 写出处，照抄结构改字段名。

- [ ] **Step 7: Run tests to verify pass + 回归**

Run: `cargo test -p atomcode-coding --lib turn_max_rounds && cargo test -p atomcode-config && cargo build -p atomcode-coding`
Expected: 绿。

- [ ] **Step 8: Commit**
```bash
git add crates/atomcode-config/src/config/mod.rs crates/atomcode-coding/src/config.rs crates/atomcode-coding/src/parts.rs crates/atomcode-coding/src/assemble.rs
git commit -m "feat(config): [coding] max_rounds TOML + thread round_cap_checkpoint flag

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: TUI — flip 开关 + 分派臂 + 面板状态 + deliver

**Files:**
- Modify: `crates/atomcode-tuix/src/state.rs`（`RoundCapPanel`；`UiState.round_cap_panel`；`UiPhase::RoundCap`；reset 清理 :1394/:1418/:1633）
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs`（flip :102；分派臂 :14878 前；`deliver_round_cap` 挨 :12428）
- Test: `crates/atomcode-tuix/src/state.rs`（`#[cfg(test)]`）

**Interfaces:**
- Consumes: `atomcode_kernel::ROUND_CAP_CHECKPOINT_KIND`；`CodingAgentConfig.round_cap_checkpoint`。
- Produces: `RoundCapPanel { id, cap, cursor }`；`deliver_round_cap(ctx, id, cont: bool)`；`UiPhase::RoundCap`（Task 4/5 消费）。

- [ ] **Step 1: 面板状态 + phase（失败测试）**

`crates/atomcode-tuix/src/state.rs` 加：
```rust
/// Round-cap checkpoint panel state (kernel-initiated; distinct from UserInputPanel
/// which is model-initiated). Two fixed options: 0 = 继续, 1 = 停止.
#[derive(Debug, Clone)]
pub struct RoundCapPanel {
    pub id: u64,
    pub cap: u32,
    pub cursor: usize, // 0=继续 1=停止
}
impl RoundCapPanel {
    pub fn new(id: u64, cap: u32) -> Self { Self { id, cap, cursor: 0 } }
    /// true = 继续
    pub fn chosen_continue(&self) -> bool { self.cursor == 0 }
    pub fn move_up(&mut self) { self.cursor = 0; }
    pub fn move_down(&mut self) { self.cursor = 1; }
}
```
`UiPhase` 枚举加变体 `RoundCap`。`UiState` 加字段 `pub round_cap_panel: Option<RoundCapPanel>,`（挨 `user_input_panel` :787），构造器（:1072 旁）加 `round_cap_panel: None,`。在 reset/clear 三处（:1394/:1418/:1633 现有 `self.user_input_panel = None;` 旁）各加 `self.round_cap_panel = None;`。

`title.rs:68` `UiPhase::UserInput => Some("🔴"),` 旁加 `UiPhase::RoundCap => Some("🔴"),`（沿用同图标）。检查所有对 `UiPhase` 的 `match` 是否穷尽——编译器会报缺失臂，逐个补（多数可并入 `UserInput` 同臂，如 `commands.rs:53/188` 的 mid-turn 白名单、mod.rs:14998 的 phase 判定）。

测试（state.rs `#[cfg(test)]`）：
```rust
    #[test]
    fn round_cap_panel_toggle_and_choice() {
        let mut p = crate::state::RoundCapPanel::new(7, 200);
        assert!(p.chosen_continue());
        p.move_down();
        assert!(!p.chosen_continue());
        p.move_up();
        assert!(p.chosen_continue());
    }
```

- [ ] **Step 2: Run to verify fail**

Run: `cargo test -p atomcode-tuix --lib round_cap_panel_toggle`
Expected: 编译失败（类型不存在 / match 非穷尽）。

- [ ] **Step 3: flip 开关（仅 TUI）**

`crates/atomcode-tuix/src/event_loop/mod.rs:101-105`，把 `config.agent_config()` 换成本地开启 checkpoint：
```rust
    let mut agent_cfg = config.agent_config();
    agent_cfg.round_cap_checkpoint = true; // TUI 实现了检查点渲染臂
    ctx.runtime.reload_provider(
        agent_cfg,
        ctx.foreground_runtime_id,
        ctx.runtime_event_tx.clone(),
    )
```
> 说明：默认 false，此为唯一 flip 点。若 TUI 另有构造 `CodingAgentConfig` 的 spawn 站点（grep `agent_config()` 于 tuix crate 确认），同样置 true；漏设某站点只是那里退回旧硬停（安全，绝不 park）。

- [ ] **Step 4: deliver helper**

`event_loop/mod.rs`，挨 `deliver_user_input`（:12428）加：
```rust
/// Answer a round-cap checkpoint with `{continue: bool}` and clear panel state.
fn deliver_round_cap(ctx: &mut LoopCtx, id: u64, cont: bool) {
    ctx.runtime
        .dispatch(atomcode_coding::DriverCommand::Respond {
            id,
            value: serde_json::json!({ "continue": cont }),
        })
        .ok();
}
```

- [ ] **Step 5: 分派臂**

`event_loop/mod.rs`，在 `if request.kind != APPROVAL_KIND {` （:14879）之前插入新臂：
```rust
                    if request.kind == atomcode_kernel::ROUND_CAP_CHECKPOINT_KIND {
                        let cap = request
                            .payload
                            .get("cap")
                            .and_then(serde_json::Value::as_u64)
                            .unwrap_or(0) as u32;
                        // 无值守/免审批模式：fail-closed 停止（等同旧 MaxRounds）。
                        if user_input_should_auto_skip(state.agent_mode) {
                            deliver_round_cap(ctx, request.id, false);
                            return;
                        }
                        state.round_cap_panel =
                            Some(crate::state::RoundCapPanel::new(request.id, cap));
                        state.phase = UiPhase::RoundCap;
                        redraw_idle_plain(buf, state, ctx, renderer);
                        return;
                    }
```

测试（分派 + auto-skip）留给 Task 5 的集成断言；本 Task 编译 + 单元 toggle 测试通过即可。

- [ ] **Step 6: Run + commit**

Run: `cargo build -p atomcode-tuix && cargo test -p atomcode-tuix --lib round_cap_panel_toggle`
Expected: 绿。
```bash
git add crates/atomcode-tuix/src/state.rs crates/atomcode-tuix/src/title.rs crates/atomcode-tuix/src/event_loop/mod.rs crates/atomcode-tuix/src/event_loop/commands.rs
git commit -m "feat(tuix): round-cap checkpoint state, dispatch arm, deliver helper

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: TUI — 渲染（复用 Single picker，样式 B）

**Files:**
- Modify: `crates/atomcode-tuix/src/render/mod.rs`（RoundCap 面板视图接入）
- Modify: `crates/atomcode-tuix/src/render/retained.rs`（复用 `build_user_input_rows`）
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs`（把 state.round_cap_panel 喂给渲染，随 `redraw_idle_plain`）
- Test: `crates/atomcode-tuix/src/render/retained.rs`（`#[cfg(test)]`）

**Interfaces:**
- Consumes: `RoundCapPanel { id, cap, cursor }`；`state.turn_elapsed() -> Option<Duration>`（state.rs:1255）；`state.total_tokens`（state.rs:707）；`build_user_input_rows`（retained.rs:2729）；`UserInputPanelView`（render/mod.rs:643）。
- Produces: 一个把 `RoundCapPanel` + 统计渲染成行的函数 `round_cap_view(panel, elapsed, tokens) -> UserInputPanelView`。

- [ ] **Step 1: 视图构造函数（失败测试）**

在 `render/mod.rs`（`UserInputPanelView` :643 定义旁）加纯函数，把 checkpoint 面板映射成一个 Single 模式 `UserInputPanelView`（`custom:false`，两个带描述的选项），复用现有渲染器：
```rust
use atomcode_capabilities::tools::request_user_input::{UserInputMode, UserInputOption};

/// 把 RoundCapPanel + 实时统计渲成样式 B 的 Single picker 视图。
pub fn round_cap_view(cap: u32, cursor: usize, stats: &str) -> UserInputPanelView {
    let question = if stats.is_empty() {
        format!("已运行 {cap} 轮，继续吗？")
    } else {
        format!("已运行 {cap} 轮（{stats}），继续吗？")
    };
    UserInputPanelView {
        header: "轮次上限".to_string(),
        question,
        mode: UserInputMode::Single,
        options: vec![
            ("继续".to_string(), Some(format!("再跑 {cap} 轮后重新确认"))),
            ("停止".to_string(), Some("结束本回合".to_string())),
        ],
        checked: vec![],
        cursor,
        custom: false,
        custom_text: String::new(),
        batch: None,
    }
}
```
> `UserInputPanelView` 的确切字段以 render/mod.rs:643 为准；若字段名/形状不同（如 `options: Vec<(String, Option<String>)>`），按实际调整。目的是复用 `build_user_input_rows` 而非新写渲染。

统计字符串助手（event_loop/mod.rs 或 render 内）：
```rust
/// "2h0m · 305K tokens"（tool 计数无实时 UiState 累加器，故省略——见 spec 非目标注）。
fn round_cap_stats(state: &UiState) -> String {
    let mut parts = Vec::new();
    if let Some(d) = state.turn_elapsed() {
        parts.push(crate::render::fmt_dur(d));
    }
    if state.total_tokens > 0 {
        parts.push(format!("{} tokens", crate::render::fmt_tokens(state.total_tokens)));
    }
    parts.join(" · ")
}
```
> `fmt_tokens` 若不存在，用现有 token 缩写函数（grep `fn fmt_tokens`/`305` 风格格式化；footer 已有同款缩写，复用它）。

测试（retained.rs）：
```rust
    #[test]
    fn round_cap_view_renders_header_and_two_options() {
        let view = crate::render::round_cap_view(200, 0, "2h0m · 305K tokens");
        assert_eq!(view.header, "轮次上限");
        assert!(view.question.contains("已运行 200 轮"));
        assert_eq!(view.options.len(), 2);
        assert_eq!(view.options[0].0, "继续");
        assert_eq!(view.options[1].0, "停止");
    }
```

- [ ] **Step 2: Run to verify fail**

Run: `cargo test -p atomcode-tuix --lib round_cap_view_renders`
Expected: 编译失败（`round_cap_view` 未定义）。

- [ ] **Step 3: 渲染接入**

在渲染帧里，凡是现在读 `state.user_input_panel` 构造 `UserInputPanelView` 并调 `build_user_input_rows` 的地方（grep `build_user_input_rows` 的调用点 + `user_input_panel` 在 render 路径的消费点），并列加一支：当 `state.round_cap_panel` 为 `Some` 时，用 `round_cap_view(panel.cap, panel.cursor, &round_cap_stats(state))` 得到视图，走同一个 `build_user_input_rows` 渲染。镜像 `user_input_panel` 的渲染分支即可（同一 chokepoint，多一个 Option 判定）。

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p atomcode-tuix --lib round_cap_view_renders && cargo build -p atomcode-tuix`
Expected: 绿。

- [ ] **Step 5: Commit**
```bash
git add crates/atomcode-tuix/src/render/mod.rs crates/atomcode-tuix/src/render/retained.rs crates/atomcode-tuix/src/event_loop/mod.rs
git commit -m "feat(tuix): render round-cap checkpoint via reused Single picker (样式 B)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: TUI — 按键路由 + 端到端

**Files:**
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs`（key 路由 :9383；新 `handle_round_cap_key`）
- Test: `crates/atomcode-tuix/src/event_loop/mod.rs`（`#[cfg(test)]`）

**Interfaces:**
- Consumes: `RoundCapPanel`（Task 3）；`deliver_round_cap`（Task 3）。

- [ ] **Step 1: key handler（失败测试）**

`event_loop/mod.rs`，仿 `handle_user_input_key`（:13255）加最小 handler：
```rust
/// Key handling while `UiPhase::RoundCap`. Two options: ↑/↓ toggle, Enter chooses,
/// Esc = stop (fail-closed). Mirrors handle_approval_key's shape.
fn handle_round_cap_key(
    app: &mut App,
    ctx: &mut LoopCtx,
    renderer: &mut impl Renderer,
    code: KeyCode,
    _modifiers: KeyModifiers,
) -> anyhow::Result<()> {
    let Some(panel) = app.state.round_cap_panel.as_ref() else { return Ok(()); };
    let id = panel.id;
    match code {
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(p) = app.state.round_cap_panel.as_mut() { p.move_up(); }
            redraw_idle_plain(/* buf */ &mut app.buf, &mut app.state, ctx, renderer); // 按实际重绘签名
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(p) = app.state.round_cap_panel.as_mut() { p.move_down(); }
            redraw_idle_plain(&mut app.buf, &mut app.state, ctx, renderer);
        }
        KeyCode::Enter => {
            let cont = app.state.round_cap_panel.as_ref().map(|p| p.chosen_continue()).unwrap_or(false);
            deliver_round_cap(ctx, id, cont);
            app.state.round_cap_panel = None;
            app.state.phase = UiPhase::Streaming; // 继续则回流式；停止时内核随即终结
        }
        KeyCode::Esc => {
            deliver_round_cap(ctx, id, false);
            app.state.round_cap_panel = None;
            app.state.phase = UiPhase::Streaming;
        }
        _ => {}
    }
    Ok(())
}
```
> `App`/`ctx`/重绘签名以 `handle_user_input_key` 的实际参数为准（照抄它的签名与重绘调用；上面的 `app.buf` 等按真实字段名修正）。Enter 后 phase 设回 `Streaming`：继续时内核会接着发轮次事件；停止时内核紧接着发 `TurnComplete{MaxRounds}` 由既有路径收尾。

key 路由（:9383 `UiPhase::UserInput => handle_user_input_key(...)` 旁）加：
```rust
                UiPhase::RoundCap => handle_round_cap_key(app, ctx, renderer, code, modifiers)?,
```

测试（mod.rs `#[cfg(test)]`，仿现有 event_loop 单测构造 `App`/`LoopCtx`）：
```rust
    #[test]
    fn round_cap_enter_on_continue_sends_true() {
        // 构造 App，state.round_cap_panel = Some(RoundCapPanel::new(9, 200))（cursor=0）
        // 调 handle_round_cap_key(Enter) → 断言 dispatch 收到 {continue:true} 且 panel 清空
        // （用现有 test 的 fake runtime 捕获 DriverCommand::Respond；参考 handle_user_input_key 的既有测试）
    }
    #[test]
    fn round_cap_esc_sends_false() {
        // cursor 无关；Esc → {continue:false}，panel 清空。
    }
```
> 若 event_loop 单测缺少捕获 `DriverCommand::Respond` 的 fake，沿用 `deliver_user_input` 既有测试的同款 fake runtime（grep 该函数的测试）。若确无脚手架，则本 Task 以 `handle_round_cap_key` 的分支单测（对 `RoundCapPanel.chosen_continue()` 的选择逻辑）+ `cargo build` 通过为准，端到端留待真机。

- [ ] **Step 2: Run to verify fail** → **Step 3: 实现（上）** → **Step 4: Run to verify pass**

Run: `cargo test -p atomcode-tuix --lib round_cap_ && cargo build -p atomcode-tuix`
Expected: 绿。

- [ ] **Step 5: 全量 tuix 回归 + commit**

Run: `cargo test -p atomcode-tuix`
Expected: 绿。
```bash
git add crates/atomcode-tuix/src/event_loop/mod.rs
git commit -m "feat(tuix): round-cap checkpoint key routing (continue/stop)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: 全仓构建 + code-review

- [ ] **Step 1: 全仓构建 + 相关 crate 测试**

Run: `cargo build --workspace && cargo test -p atomcode-kernel -p atomcode-coding -p atomcode-config -p atomcode-tuix`
Expected: 绿。

- [ ] **Step 2: 交叉编译门（若 CI 要求 Windows/musl，按项目惯例跑）**

Run: 按项目 `.github`/Makefile 的交叉编译命令（此前多次真机前的门）。

- [ ] **Step 3: /code-review 本分支 diff**

对本分支 diff 跑 `/code-review high`，重点核对：①非 TUI 路径行为零变化（默认 false 分支）；②`Null→停止` fail-closed；③再武装 `cap += base` 无 off-by-one（round=cap+1 落进新窗口）；④env>TOML>默认 优先级；⑤`UiPhase::RoundCap` 所有 match 臂穷尽无遗漏。修掉 Important 级发现。

- [ ] **Step 4: 真机验证（仅用户可做）**

TUI 里设 `ATOMCODE_TURN_MAX_ROUNDS=3` 跑一个会多轮调工具的任务，确认第 4 轮弹出「轮次上限」问询卡片、选「继续」后接着跑、再次弹出、选「停止」得干净 `✗ 已中断 … MaxRounds`；非 TUI（headless/webui）撞上限仍是旧红错误。

---

## Self-Review 记录（对照 spec）

- **Spec §1 触发/再武装** → Task 1 Step 5（可变 `round_cap`，`cap += base`）。
- **Spec §2 Builder 开关默认 false、仅 TUI** → Task 1 Step 4（字段/setter，default false）+ Task 3 Step 3（TUI flip）。
- **Spec §3 渲染样式 B、复用 picker、统计由 TUI 填、响应按 continue bool** → Task 4（`round_cap_view` + `build_user_input_rows`）+ Task 3/5（`deliver_round_cap` 发 `{continue}`）。统计的 **tool 计数省略**（无实时 UiState 累加器，只有 turn-completion 的 `PendingSeparator.tool_call_count`）——问题行改为「已运行 N 轮（时长 · tokens）」，已在 Task 4 Step 1 注明（no-silent-cap：显式记录取舍）。
- **Spec §4 配置** → Task 2（`CodingConfig` + `resolve_turn_max_rounds` + env>TOML>默认 + `0` 关闭 + save 注释）。
- **Spec §5 不动项** → 签名熔断/StopReason/finish_turn 全未触碰。
- **Spec 边界（Null/Cancel/无 requester）** → Task 1 Step 1 的 `null_response_stops` 测试 + `unwrap_or(false)`；Cancel 经 `cancel_pending → Null → false`（同路径，Task 6 Step 3 复核）。
- **类型一致性**：`ROUND_CAP_CHECKPOINT_KIND`、`round_cap_checkpoint(bool)`、`RoundCapPanel{id,cap,cursor}`、`deliver_round_cap(ctx,id,bool)`、`round_cap_view(cap,cursor,stats)` 全计划内一致。

## Deferred

- webui/daemon 检查点镜像。
- 递增间隔（200→400→800…）。
- checkpoint 问询里显示实时 tool 计数（需新增 UiState in-turn 累加器）。
