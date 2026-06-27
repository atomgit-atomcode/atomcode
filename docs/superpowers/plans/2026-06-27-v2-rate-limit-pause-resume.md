# v2 5 小时窗口限流"暂停-自愈"体验 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** v2 引擎收到 5 小时滚动窗口限流(429)时，按 reset 剩余时间分流——≤2 分钟可取消挂起+自动续跑，>2 分钟优雅暂停并显示恢复时间，不再粗暴红字报错、保住已产出内容。

**Architecture:** 路线 A（Hook 注入决策）。kernel 检测 `http_status==429` → 调新 `LifecycleHooks::on_rate_limit` 取决策；决策逻辑（含 usage 数据访问）放宿主侧 `atomcode-coding` 的 `RateLimitHook`，kernel 只执行（可取消等待+续跑 / emit 新 `AgentEvent::RateLimited` 非红字事件）。事件经 bridge → core `TurnEvent` → TUI/daemon-webui 两路渲染暂停态。kernel 不依赖 core。

**Tech Stack:** Rust（atomcode-kernel / atomcode-coding / atomcode-core / atomcode-bridge / atomcode-daemon / atomcode-tuix），Preact+TS（webui），async-trait，tokio。

## Global Constraints

- 仅 v2（kernel）。v1（`--engine v1`）保持现状，不改。
- 仅 5 小时滚动窗口；月度限流已下线。
- 自动等待阈值 `RATE_LIMIT_AUTO_WAIT_SECS = 120`（秒）。
- kernel **不得**依赖 atomcode-core；usage 数据访问只能在宿主 hook（atomcode-coding）里。
- 构建/磁盘约束：所有 cargo 命令加 `CARGO_INCREMENTAL=0`，并 `-p <package>` 按包编，禁止全工作区编译。
- webui 新文案必须 zh + en 两种都加（`webui/src/i18n.ts`）。
- 频繁提交：每个 task 末尾 commit。月度死代码清理为**独立 commit**。
- 当前分支 `release/v4.25.7`（非 main），直接在此分支提交（沿用仓库节奏）。

---

### Task 1: kernel 新类型 + on_rate_limit hook + 事件/StopReason 变体

**Files:**
- Modify: `crates/atomcode-kernel/src/hook.rs`（新增类型 + trait 方法 + HookChain 转发）
- Modify: `crates/atomcode-kernel/src/event.rs`（`StopReason::RateLimited` + `AgentEvent::RateLimited`，约 `event.rs:18` 与 `event.rs:79` 两个枚举）
- Test: `crates/atomcode-kernel/src/hook.rs`（`#[cfg(test)]` 模块内）

**Interfaces:**
- Produces:
  - `pub struct RateLimitHint { pub http_status: Option<u16>, pub retry_after_secs: Option<u64> }`
  - `pub enum RateLimitDecision { WaitAndRetry { secs: u64 }, Pause { reset_at_display: String, reset_label: String, secs_until_reset: Option<u64> } }`
  - `pub const RATE_LIMIT_AUTO_WAIT_SECS: u64 = 120;`
  - `RateLimitDecision::from_hint(hint: &RateLimitHint) -> RateLimitDecision`
  - `LifecycleHooks::on_rate_limit(&self, hint: &RateLimitHint) -> Option<RateLimitDecision>`（默认 `None`）
  - `AgentEvent::RateLimited { reset_at_display: String, reset_label: String, secs_until_reset: Option<u64> }`
  - `StopReason::RateLimited`

- [ ] **Step 1: Write the failing test**

在 `crates/atomcode-kernel/src/hook.rs` 末尾的 `#[cfg(test)] mod tests` 里（若无则新建）加：

```rust
#[test]
fn from_hint_waits_when_reset_imminent() {
    let d = RateLimitDecision::from_hint(&RateLimitHint {
        http_status: Some(429),
        retry_after_secs: Some(45),
    });
    assert_eq!(d, RateLimitDecision::WaitAndRetry { secs: 45 });
}

#[test]
fn from_hint_pauses_when_reset_far_or_unknown() {
    let far = RateLimitDecision::from_hint(&RateLimitHint {
        http_status: Some(429),
        retry_after_secs: Some(600),
    });
    assert!(matches!(far, RateLimitDecision::Pause { .. }));
    let unknown = RateLimitDecision::from_hint(&RateLimitHint {
        http_status: Some(429),
        retry_after_secs: None,
    });
    assert!(matches!(unknown, RateLimitDecision::Pause { .. }));
}

#[tokio::test]
async fn default_hook_on_rate_limit_returns_none() {
    struct Bare;
    #[async_trait::async_trait]
    impl LifecycleHooks for Bare {}
    let hint = RateLimitHint { http_status: Some(429), retry_after_secs: Some(10) };
    assert!(Bare.on_rate_limit(&hint).await.is_none());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_INCREMENTAL=0 cargo test -p atomcode-kernel from_hint_ 2>&1 | tail -20`
Expected: 编译失败 —— `cannot find type RateLimitHint` / `RateLimitDecision`。

- [ ] **Step 3: Write minimal implementation**

在 `crates/atomcode-kernel/src/hook.rs`（trait 定义上方）加类型：

```rust
/// Threshold: a 429 whose window resets within this many seconds is worth
/// waiting out in-place (auto-resume); beyond it the turn pauses and hands back
/// to the user. Mirrors the spec's `RATE_LIMIT_AUTO_WAIT_SECS`.
pub const RATE_LIMIT_AUTO_WAIT_SECS: u64 = 120;

/// What the kernel knows about a 429 at the moment it fires. The kernel cannot
/// see CodingPlan usage windows (that lives in `atomcode-core`, off-limits here)
/// so this carries only its own best-effort signal: the status and any
/// `Retry-After`-style seconds parsed from the error text.
#[derive(Debug, Clone)]
pub struct RateLimitHint {
    pub http_status: Option<u16>,
    pub retry_after_secs: Option<u64>,
}

/// The host's verdict on a 429. `WaitAndRetry` => kernel sleeps (cancellably)
/// then re-issues the round; `Pause` => kernel emits `RateLimited` and ends the
/// turn cleanly (no red error), preserving already-produced content.
#[derive(Debug, Clone, PartialEq)]
pub enum RateLimitDecision {
    WaitAndRetry { secs: u64 },
    Pause {
        reset_at_display: String,
        reset_label: String,
        secs_until_reset: Option<u64>,
    },
}

impl RateLimitDecision {
    /// Conservative fallback when NO host hook supplies a verdict (non-CodingPlan,
    /// or usage data unavailable): wait only if the kernel's own hint says the
    /// reset is imminent, otherwise pause with whatever little we know.
    pub fn from_hint(hint: &RateLimitHint) -> Self {
        match hint.retry_after_secs {
            Some(s) if s <= RATE_LIMIT_AUTO_WAIT_SECS => RateLimitDecision::WaitAndRetry { secs: s },
            _ => RateLimitDecision::Pause {
                reset_at_display: String::new(),
                reset_label: String::new(),
                secs_until_reset: hint.retry_after_secs,
            },
        }
    }
}
```

在 `LifecycleHooks` trait 内（其它 `async fn` 旁）加方法：

```rust
/// Called when the provider returns a 429. The host (which CAN see usage
/// windows) returns `Some(decision)`; `None` means "no opinion" and the kernel
/// falls back to `RateLimitDecision::from_hint`. Default: `None`.
async fn on_rate_limit(&self, _hint: &RateLimitHint) -> Option<RateLimitDecision> {
    None
}
```

在 `HookChain` 的 `impl LifecycleHooks for HookChain` 内加转发（返回第一个有意见的 hook 的决策）：

```rust
async fn on_rate_limit(&self, hint: &RateLimitHint) -> Option<RateLimitDecision> {
    for h in &self.hooks {
        if let Some(d) = h.on_rate_limit(hint).await {
            return Some(d);
        }
    }
    None
}
```

在 `crates/atomcode-kernel/src/event.rs` 的 `enum StopReason`（`:18`）加变体（放 `PromptRejected` 后、`}` 前）：

```rust
    /// The provider returned 429 and the host chose to PAUSE (reset too far to
    /// wait out). Not a failure — already-produced content is preserved.
    RateLimited,
```

在 `enum AgentEvent`（`:79`）加变体（放 `Warning(String)` 旁）：

```rust
    /// A 429 rate-limit PAUSE (host decided the reset is too far to auto-wait).
    /// A driver renders this as a non-error pause line with the reset time, NOT
    /// as a red error. `secs_until_reset`/`reset_at_display` may be empty when the
    /// host had no usage data.
    RateLimited {
        reset_at_display: String,
        reset_label: String,
        #[serde(default)]
        secs_until_reset: Option<u64>,
    },
```

- [ ] **Step 4: Run test to verify it passes**

Run: `CARGO_INCREMENTAL=0 cargo test -p atomcode-kernel from_hint_ default_hook_on_rate_limit 2>&1 | tail -20`
Expected: 3 个测试 PASS。

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-kernel/src/hook.rs crates/atomcode-kernel/src/event.rs
git commit -m "feat(kernel): on_rate_limit hook + RateLimited event/StopReason

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: testkit 可编程 RateLimitHook（测试基建）

**Files:**
- Modify: `crates/atomcode-kernel/src/testkit.rs`（新增 `ScriptedRateLimitHook`）
- Test: `crates/atomcode-kernel/src/testkit.rs`（同文件 `#[cfg(test)]`，仅验证 hook 自身行为）

**Interfaces:**
- Consumes: `RateLimitHint`, `RateLimitDecision`, `LifecycleHooks`（Task 1）
- Produces: `pub struct ScriptedRateLimitHook { decision: RateLimitDecision }`，`ScriptedRateLimitHook::new(decision: RateLimitDecision) -> Self`，实现 `on_rate_limit` 恒返回 `Some(self.decision.clone())`

- [ ] **Step 1: Write the failing test**

在 `crates/atomcode-kernel/src/testkit.rs` 的测试模块加：

```rust
#[tokio::test]
async fn scripted_rate_limit_hook_returns_programmed_decision() {
    let hook = ScriptedRateLimitHook::new(RateLimitDecision::WaitAndRetry { secs: 7 });
    let got = hook
        .on_rate_limit(&RateLimitHint { http_status: Some(429), retry_after_secs: None })
        .await;
    assert_eq!(got, Some(RateLimitDecision::WaitAndRetry { secs: 7 }));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_INCREMENTAL=0 cargo test -p atomcode-kernel scripted_rate_limit 2>&1 | tail -20`
Expected: 编译失败 `cannot find ScriptedRateLimitHook`。

- [ ] **Step 3: Write minimal implementation**

在 `crates/atomcode-kernel/src/testkit.rs`（其它 hook 定义旁）加（注意 import `RateLimitHint`/`RateLimitDecision`）：

```rust
/// A `LifecycleHooks` that returns a FIXED `on_rate_limit` verdict — lets tests
/// drive the kernel's 429 branch (wait-and-retry vs pause) without any network
/// or usage data.
pub struct ScriptedRateLimitHook {
    decision: RateLimitDecision,
}

impl ScriptedRateLimitHook {
    pub fn new(decision: RateLimitDecision) -> Self {
        Self { decision }
    }
}

#[async_trait]
impl LifecycleHooks for ScriptedRateLimitHook {
    async fn on_rate_limit(&self, _hint: &RateLimitHint) -> Option<RateLimitDecision> {
        Some(self.decision.clone())
    }
}
```

文件顶部 use 补 `RateLimitHint, RateLimitDecision`（与现有 `use crate::hook::...` 合并）。

- [ ] **Step 4: Run test to verify it passes**

Run: `CARGO_INCREMENTAL=0 cargo test -p atomcode-kernel scripted_rate_limit 2>&1 | tail -20`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-kernel/src/testkit.rs
git commit -m "test(kernel): ScriptedRateLimitHook for driving 429 branch

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: kernel 循环 429 分支（open + mid-stream）

**Files:**
- Modify: `crates/atomcode-kernel/src/agent.rs`（OPEN 失败分支 `:967` 区，mid-stream `:1135` 区，新增 `parse_retry_after_secs` 辅助函数）
- Test: `crates/atomcode-kernel/tests/`（新增集成测试文件 `rate_limit.rs`，或追加现有 agent-loop 集成测试文件）

**Interfaces:**
- Consumes: `ScriptedRateLimitHook`（Task 2），`AgentEvent::RateLimited` / `StopReason::RateLimited`（Task 1），现有 `MockProvider`/测试夹具（参照同目录现有集成测试的 provider mock 模式）
- Produces: kernel 行为——429 + `WaitAndRetry` ⇒ 等待后续跑；429 + `Pause` ⇒ emit `RateLimited` + `TurnComplete{RateLimited}`，不 emit `Error`

**说明：** 先按现有集成测试约定（参照 `crates/atomcode-kernel/tests/` 下已有文件如 agent-loop / empty-response 测试）确认 mock provider 如何返回一个 `ProviderError { http_status: Some(429), retryable: true, .. }`。下方测试以该夹具为前提；若现有夹具命名不同，按现有命名套用（不要新造一套）。

- [ ] **Step 1: Write the failing test**

新建 `crates/atomcode-kernel/tests/rate_limit.rs`（import 路径参照同目录现有测试文件头部）：

```rust
// 夹具：参照现有集成测试的 build_agent / MockProvider 用法。
// 关键点：provider 第一次 open 返回 429 ProviderError(retryable=true)，
// 之后返回正常完成；hooks 注入 ScriptedRateLimitHook。

#[tokio::test]
async fn rate_limit_pause_emits_ratelimited_not_error() {
    // provider: 首轮 open -> Err(ProviderError{http_status:Some(429),retryable:true})
    // hook: ScriptedRateLimitHook(Pause{reset_at_display:"18:09", reset_label:"5h", secs_until_reset:Some(7200)})
    let events = run_turn_collecting_events(/* …夹具… */).await;
    assert!(events.iter().any(|e| matches!(e, AgentEvent::RateLimited { .. })),
        "must emit RateLimited: {events:?}");
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error { .. })),
        "must NOT emit Error on pause: {events:?}");
    assert!(events.iter().any(|e| matches!(e,
        AgentEvent::TurnComplete { reason: StopReason::RateLimited })));
}

#[tokio::test]
async fn rate_limit_wait_then_resumes_turn() {
    // provider: 首轮 open -> 429; 次轮 open -> 正常流(产出文本 "ok") -> Done
    // hook: ScriptedRateLimitHook(WaitAndRetry{secs:0})  // secs:0 => 测试不真睡
    let events = run_turn_collecting_events(/* …夹具… */).await;
    assert!(events.iter().any(|e| matches!(e, AgentEvent::TextDelta(t) if t.contains("ok"))),
        "turn must resume and produce content: {events:?}");
    assert!(events.iter().any(|e| matches!(e,
        AgentEvent::TurnComplete { reason: StopReason::Stopped })));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_INCREMENTAL=0 cargo test -p atomcode-kernel --test rate_limit 2>&1 | tail -30`
Expected: FAIL —— 当前 429 走 3/6/9s 重试后 emit `Error`，断言"无 Error / 有 RateLimited"失败。

- [ ] **Step 3: Write minimal implementation**

在 `crates/atomcode-kernel/src/agent.rs` 顶部辅助函数区加：

```rust
/// Best-effort parse of a "try again in N seconds" hint out of a provider error
/// message (some OpenAI-compatible gateways embed it on a 429). Returns None if
/// no such hint — the host hook is the authoritative reset source; this is only a
/// fallback for the default (no-host) path.
fn parse_retry_after_secs(msg: &str) -> Option<u64> {
    let lower = msg.to_ascii_lowercase();
    let idx = lower.find("try again in ")? + "try again in ".len();
    let rest = &lower[idx..];
    let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    num.parse::<u64>().ok()
}
```

在 OPEN 失败 match 里，**在现有 `Err(e) if e.retryable && provider_retry < MAX_PROVIDER_RETRIES =>` 分支之前**插入 429 专属分支（`agent.rs:967` 前）：

```rust
// 429 RATE LIMIT: defer to the host's usage-aware verdict instead of the blind
// 3/6/9s transient retry (useless for a 5h window). WaitAndRetry => cancellable
// sleep then re-issue this round; Pause => clean RateLimited stop preserving
// already-produced content (NOT a red Error).
Err(e) if e.http_status == Some(429) => {
    let hint = crate::hook::RateLimitHint {
        http_status: e.http_status,
        retry_after_secs: parse_retry_after_secs(&e.message),
    };
    let decision = self
        .hooks
        .on_rate_limit(&hint)
        .await
        .unwrap_or_else(|| crate::hook::RateLimitDecision::from_hint(&hint));
    match decision {
        crate::hook::RateLimitDecision::WaitAndRetry { secs } => {
            self.rt.emit(AgentEvent::RateLimited {
                reset_at_display: String::new(),
                reset_label: String::new(),
                secs_until_reset: Some(secs),
            });
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    self.finish_cancelled(convo, rollback_len, &turn_ctx).await;
                    return;
                }
                _ = tokio::time::sleep(std::time::Duration::from_secs(secs)) => {}
            }
            round -= 1; // re-issue this round
            continue;
        }
        crate::hook::RateLimitDecision::Pause { reset_at_display, reset_label, secs_until_reset } => {
            self.rt.emit(AgentEvent::RateLimited { reset_at_display, reset_label, secs_until_reset });
            self.finish_turn(convo, StopReason::RateLimited, &turn_ctx).await;
            return;
        }
    }
}
```

mid-stream 429（`agent.rs:1135` 的 `StreamEvent::Error` 终止处）：在 emit `Error` 前判 `http_status == Some(429)`，先 finalize 已产出内容（沿用该处已有的 finalize 调用），再走与上面**相同**的决策逻辑（抽成一个 `self.handle_rate_limit(...)` 私有 async 方法复用，避免重复——DRY）。私有方法签名：

```rust
/// Returns true if the turn was terminated (Pause or cancel) — caller must
/// `return`. Returns false after a WaitAndRetry sleep — caller decrements round
/// and continues. (For the mid-stream caller, "continue" means re-open.)
async fn handle_rate_limit(
    &self,
    e_message: &str,
    convo: &mut Conversation,
    rollback_len: usize,
    turn_ctx: &TurnCtx,
    cancel: &CancellationToken,
) -> RateLimitOutcome { /* WaitAndRetry / PausedOrCancelled */ }
```

> 实施提示：若抽方法因借用 `round`/`continue` 不便，可保持 open 分支内联、mid-stream 分支内联，但务必把决策→执行的核心抽成一个返回 `RateLimitDecision` 已解析后的小 helper，二者共享。优先 DRY，但不要为此与借用检查器硬刚到改坏循环结构。

- [ ] **Step 4: Run test to verify it passes**

Run: `CARGO_INCREMENTAL=0 cargo test -p atomcode-kernel --test rate_limit 2>&1 | tail -30`
Expected: 两个测试 PASS。
再跑回归：`CARGO_INCREMENTAL=0 cargo test -p atomcode-kernel 2>&1 | tail -20` —— 既有测试全过（尤其非 429 retryable 仍走 3/6/9s 的测试）。

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-kernel/src/agent.rs crates/atomcode-kernel/tests/rate_limit.rs
git commit -m "feat(kernel): route 429 to host on_rate_limit (wait-and-resume vs pause)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: 宿主 RateLimitHook（usage 关联 + 阈值策略）

**Files:**
- Create: `crates/atomcode-coding/src/rate_limit.rs`
- Modify: `crates/atomcode-coding/src/lib.rs`（`mod rate_limit;`）
- Modify: `crates/atomcode-coding/src/parts.rs`（`hooks.push(Arc::new(RateLimitHook::new()))`）
- Test: `crates/atomcode-coding/src/rate_limit.rs`（`#[cfg(test)]`）

**Interfaces:**
- Consumes: `atomcode_kernel::hook::{LifecycleHooks, RateLimitHint, RateLimitDecision, RATE_LIMIT_AUTO_WAIT_SECS}`，`atomcode_core::coding_plan::types::RateLimitWindow`
- Produces: `pub struct RateLimitHook`，`RateLimitHook::new()`，纯函数 `decide_from_windows(windows: &[RateLimitWindow], hint: &RateLimitHint) -> RateLimitDecision`（可单测，不触网）；`on_rate_limit` 调 `status_v2()` 取 windows 后委托纯函数

**说明：** 把"挑 5h 窗口 + 套 120s 阈值 + fallback 链"做成**纯函数** `decide_from_windows`，单测覆盖；`on_rate_limit` 只负责取数据（`Client::from_stored_auth().status_v2()`，非 CodingPlan / 取数失败时 `None` 让 kernel 回退）。

- [ ] **Step 1: Write the failing test**

新建 `crates/atomcode-coding/src/rate_limit.rs`，先写测试（用 `RateLimitWindow` 构造，参照 `crates/atomcode-core/src/coding_plan/setup.rs` 测试里的字段写法）：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_core::coding_plan::types::RateLimitWindow;
    use atomcode_kernel::hook::{RateLimitDecision, RateLimitHint};

    fn win(secs_until_reset: i64, exhausted: bool) -> RateLimitWindow {
        RateLimitWindow {
            show_enable: 1,
            window_size_seconds: 18000, // 5h
            usage_percent: 100.0,
            quota_exhausted: exhausted,
            reset_at: "2026-06-27T18:09:30".into(),
            reset_at_display: "18:09".into(),
            seconds_until_reset: secs_until_reset,
            reset_label: "当前窗口结束即重置额度（每 5 小时一个窗口）".into(),
            ..Default::default()
        }
    }

    fn hint() -> RateLimitHint { RateLimitHint { http_status: Some(429), retry_after_secs: None } }

    #[test]
    fn near_reset_waits() {
        let d = decide_from_windows(&[win(90, true)], &hint());
        assert_eq!(d, RateLimitDecision::WaitAndRetry { secs: 90 });
    }

    #[test]
    fn far_reset_pauses_with_display() {
        let d = decide_from_windows(&[win(7200, true)], &hint());
        match d {
            RateLimitDecision::Pause { reset_at_display, secs_until_reset, .. } => {
                assert_eq!(reset_at_display, "18:09");
                assert_eq!(secs_until_reset, Some(7200));
            }
            _ => panic!("expected Pause, got {d:?}"),
        }
    }

    #[test]
    fn no_window_falls_back_to_hint() {
        let h = RateLimitHint { http_status: Some(429), retry_after_secs: Some(30) };
        assert_eq!(decide_from_windows(&[], &h), RateLimitDecision::WaitAndRetry { secs: 30 });
    }
}
```

> 若 `RateLimitWindow` 未派生 `Default`，测试里改为显式构造全字段（参照 `setup.rs` 测试中的写法），不要给生产类型加 `Default` 仅为测试。

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_INCREMENTAL=0 cargo test -p atomcode-coding rate_limit::tests 2>&1 | tail -20`
Expected: 编译失败 `cannot find function decide_from_windows`。

- [ ] **Step 3: Write minimal implementation**

在 `crates/atomcode-coding/src/rate_limit.rs`（测试模块上方）写：

```rust
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_core::coding_plan::types::RateLimitWindow;
use atomcode_kernel::hook::{
    LifecycleHooks, RateLimitDecision, RateLimitHint, RATE_LIMIT_AUTO_WAIT_SECS,
};

/// Pure policy: pick the 5-hour rolling window, apply the auto-wait threshold,
/// and fall back to the kernel hint when no window data is available. Monthly
/// windows are gone, so the relevant window is the small (`<= 5h`) one.
pub fn decide_from_windows(windows: &[RateLimitWindow], hint: &RateLimitHint) -> RateLimitDecision {
    // 5h rolling window = the one with the smallest size (<= 18000s). With monthly
    // retired this is typically the only window; min-by keeps it robust if extras appear.
    let w = windows
        .iter()
        .filter(|w| w.window_size_seconds > 0 && w.window_size_seconds <= 18_000)
        .min_by_key(|w| w.window_size_seconds);
    let Some(w) = w else {
        return RateLimitDecision::from_hint(hint);
    };
    let secs = w.seconds_until_reset.max(0) as u64;
    if secs <= RATE_LIMIT_AUTO_WAIT_SECS {
        RateLimitDecision::WaitAndRetry { secs }
    } else {
        RateLimitDecision::Pause {
            reset_at_display: w.reset_at_display.clone(),
            reset_label: w.reset_label.clone(),
            secs_until_reset: Some(secs),
        }
    }
}

/// Host hook: on a 429, fetch the current CodingPlan usage windows and delegate
/// to `decide_from_windows`. Non-CodingPlan users / fetch failures return `None`
/// so the kernel falls back to its hint-based default (no behavior change).
pub struct RateLimitHook;

impl RateLimitHook {
    pub fn new() -> Self { Self }
}

impl Default for RateLimitHook {
    fn default() -> Self { Self::new() }
}

#[async_trait]
impl LifecycleHooks for RateLimitHook {
    async fn on_rate_limit(&self, hint: &RateLimitHint) -> Option<RateLimitDecision> {
        // Blocking client on a blocking thread (mirrors usage_monitor::spawn_check).
        let windows = tokio::task::spawn_blocking(|| {
            let client = atomcode_core::coding_plan::client::Client::from_stored_auth().ok()?;
            let status = client.status_v2().ok()?;
            Some(status.rate_limit_windows)
        })
        .await
        .ok()
        .flatten();
        match windows {
            Some(w) if !w.is_empty() => Some(decide_from_windows(&w, hint)),
            // No CodingPlan / empty windows: defer to kernel default rather than
            // forcing a decision on a non-CodingPlan provider.
            _ => None,
        }
    }
}
```

> 校验：确认 `status_v2()` 返回类型字段名为 `rate_limit_windows`（见 `types.rs:212`）。若 `from_stored_auth`/`status_v2` 签名不同，按实际签名调整（保持"取 windows → 纯函数"结构不变）。

在 `crates/atomcode-coding/src/lib.rs` 加 `mod rate_limit;`（若需对外则 `pub mod`，否则私有 + 在 parts.rs `use crate::rate_limit::RateLimitHook;`）。

在 `crates/atomcode-coding/src/parts.rs` 的 hooks 装配段（`let mut hooks ... = Vec::new();` 之后、其它 `hooks.push(...)` 旁）加：

```rust
hooks.push(Arc::new(crate::rate_limit::RateLimitHook::new()) as Arc<dyn LifecycleHooks>);
```

- [ ] **Step 4: Run test to verify it passes**

Run: `CARGO_INCREMENTAL=0 cargo test -p atomcode-coding rate_limit::tests 2>&1 | tail -20`
Expected: 3 个测试 PASS。

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-coding/src/rate_limit.rs crates/atomcode-coding/src/lib.rs crates/atomcode-coding/src/parts.rs
git commit -m "feat(coding): RateLimitHook — 5h window reset drives wait-vs-pause

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: core TurnEvent::RateLimited + bridge 映射

**Files:**
- Modify: `crates/atomcode-core/src/turn/event.rs`（在 `Error(String)`/`Warning(String)` 旁，约 `:61`/`:65`，加 `RateLimited` 变体）
- Modify: `crates/atomcode-bridge/src/runtime.rs`（`on_kernel_event` 的 `KEv::Warning`/`KEv::Error` 旁，约 `:1517`，加 `KEv::RateLimited`）
- Test: `crates/atomcode-bridge/`（追加单测断言映射，或随 Task 6 的 wire 测试覆盖）

**Interfaces:**
- Consumes: `AgentEvent::RateLimited`（Task 1）
- Produces: `atomcode_core::turn::event::TurnEvent::RateLimited { reset_at_display: String, reset_label: String, secs_until_reset: Option<u64> }`

- [ ] **Step 1: Write the failing test**

在 `crates/atomcode-bridge/src/runtime.rs` 测试模块（若无独立映射测试，加一个最小的）：

```rust
#[test]
fn ratelimited_event_variant_exists() {
    // 编译期保证：core 侧变体可构造
    let _ = atomcode_core::turn::event::TurnEvent::RateLimited {
        reset_at_display: "18:09".into(),
        reset_label: "5h".into(),
        secs_until_reset: Some(7200),
    };
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_INCREMENTAL=0 cargo test -p atomcode-bridge ratelimited_event_variant 2>&1 | tail -20`
Expected: 编译失败 —— core 无 `TurnEvent::RateLimited`。

- [ ] **Step 3: Write minimal implementation**

在 `crates/atomcode-core/src/turn/event.rs`（`Warning(String)` 下一行）加：

```rust
    /// A 429 rate-limit PAUSE — driver renders a non-error pause line with the
    /// reset time. Empty strings / None when no usage data was available.
    RateLimited {
        reset_at_display: String,
        reset_label: String,
        secs_until_reset: Option<u64>,
    },
```

在 `crates/atomcode-bridge/src/runtime.rs` `on_kernel_event` 的 `KEv::Warning(w) => ...` 旁加：

```rust
KEv::RateLimited { reset_at_display, reset_label, secs_until_reset } => {
    self.emit(CoreEv::RateLimited { reset_at_display, reset_label, secs_until_reset });
}
```

（`CoreEv` 是 `atomcode_core::turn::event::TurnEvent` 的别名；按文件顶部现有别名用法写。）

- [ ] **Step 4: Run test to verify it passes**

Run: `CARGO_INCREMENTAL=0 cargo test -p atomcode-bridge ratelimited_event_variant 2>&1 | tail -20`
Expected: PASS。
注意：加了 `KEv::RateLimited` 分支后，若 `on_kernel_event` 的 match 是穷尽的，编译器会要求 core/其它消费 `TurnEvent` 的 match 也处理新变体——Task 7/Task 6 会补；本 task 编译可能因下游 match 未尽而报错，属预期，下游 task 补齐。**若下游 match 报 non-exhaustive 阻塞本 task 提交，临时在下游加 `TurnEvent::RateLimited { .. } => {}` 占位，并在对应 task 替换为真实渲染。**

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-core/src/turn/event.rs crates/atomcode-bridge/src/runtime.rs
git commit -m "feat(core,bridge): TurnEvent::RateLimited + KEv mapping

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: daemon LiveWire 事件（webui 传输）

**Files:**
- Modify: `crates/atomcode-daemon/src/live_api.rs`（`enum LiveWireEvent` 约 `:1248`，`to_wire` 的 `TE::Warning` 旁约 `:1378`）
- Test: `crates/atomcode-daemon/src/live_api.rs`（参照现有 `chat_warning_serializes_as_its_own_type` 风格的 `#[test]`）

**Interfaces:**
- Consumes: `TurnEvent::RateLimited`（Task 5）
- Produces: wire JSON `{"type":"rate_limited","reset_at_display":...,"reset_label":...,"secs_until_reset":...}`

- [ ] **Step 1: Write the failing test**

在 `crates/atomcode-daemon/src/live_api.rs` 测试区加：

```rust
#[test]
fn rate_limited_serializes_as_its_own_type() {
    let wire = to_wire(LiveEvent::Turn(TE::RateLimited {
        reset_at_display: "18:09".into(),
        reset_label: "5h".into(),
        secs_until_reset: Some(7200),
    }))
    .expect("should map");
    let json = serde_json::to_string(&wire).unwrap();
    assert!(json.contains(r#""type":"rate_limited""#), "wire type must be rate_limited: {json}");
    assert!(json.contains(r#""reset_at_display":"18:09""#), "{json}");
}
```

（`LiveEvent::Turn(...)` 的确切包裹方式参照同文件 `to_wire` 里 `TE::Warning` 那条 case 的写法。）

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_INCREMENTAL=0 cargo test -p atomcode-daemon rate_limited_serializes 2>&1 | tail -20`
Expected: 编译失败 —— `LiveWireEvent` 无 `RateLimited`。

- [ ] **Step 3: Write minimal implementation**

在 `enum LiveWireEvent`（`:1248`）加（参照 `Warning { message: String }` 的 serde tag 风格，确认其 `#[serde(tag = "type", rename_all = "snake_case")]` 或逐变体 `rename`，与现有一致）：

```rust
    RateLimited {
        reset_at_display: String,
        reset_label: String,
        secs_until_reset: Option<u64>,
    },
```

在 `to_wire`（`TE::Warning(w) => LiveWireEvent::Warning { message: w }` 旁，`:1378`）加：

```rust
            TE::RateLimited { reset_at_display, reset_label, secs_until_reset } =>
                LiveWireEvent::RateLimited { reset_at_display, reset_label, secs_until_reset },
```

> 若枚举用逐变体 `#[serde(rename = "...")]` 而非容器级 `rename_all`，给本变体加 `#[serde(rename = "rate_limited")]` 以匹配测试里的 `"type":"rate_limited"`。

- [ ] **Step 4: Run test to verify it passes**

Run: `CARGO_INCREMENTAL=0 cargo test -p atomcode-daemon rate_limited_serializes 2>&1 | tail -20`
Expected: PASS。

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-daemon/src/live_api.rs
git commit -m "feat(daemon): LiveWireEvent::RateLimited for webui transport

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: TUI 渲染暂停态

**Files:**
- Modify: `crates/atomcode-tuix/src/event_loop/commands.rs`（处理 `CoreEv`/`TurnEvent` 的 match —— 加 `TurnEvent::RateLimited` 分支）
- Modify: `crates/atomcode-tuix/src/render/...`（若需新增 `UiLine` 暂停样式；否则复用现有非红 hint 行）
- Test: 渲染纯函数若有则单测；否则手动验证（见下）

**Interfaces:**
- Consumes: `TurnEvent::RateLimited { reset_at_display, reset_label, secs_until_reset }`（Task 5）
- Produces: TUI body/footer 一条非红暂停行，文案如 `⏸ 5小时窗口已用尽，约 18:09 恢复（还有 2h11m）· 已保留已完成内容 · 可换模型或稍后重试`；`WaitAndRetry` 场景（`reset_at_display` 空、`secs_until_reset` 小）显示 `⏳ 限流，{N}s 后自动继续…`

- [ ] **Step 1: Write the failing test**

若 commands.rs 有可单测的"事件→UiLine"纯函数，加：

```rust
#[test]
fn rate_limited_renders_non_error_pause_line() {
    let line = format_rate_limited_line("18:09", "（每 5 小时一个窗口）", Some(7200));
    assert!(line.contains("18:09"));
    assert!(line.contains("可换模型") || line.contains("稍后重试"));
}

#[test]
fn rate_limited_wait_shows_countdown() {
    let line = format_rate_limited_line("", "", Some(45));
    assert!(line.contains("45") && line.contains("自动继续"));
}
```

> 若现有架构无此纯函数接缝，则**新建** `format_rate_limited_line(reset_at_display: &str, reset_label: &str, secs_until_reset: Option<u64>) -> String` 纯函数承载文案逻辑（便于单测），分支只调它。

- [ ] **Step 2: Run test to verify it fails**

Run: `CARGO_INCREMENTAL=0 cargo test -p atomcode-tuix rate_limited 2>&1 | tail -20`
Expected: 编译失败 `cannot find function format_rate_limited_line`。

- [ ] **Step 3: Write minimal implementation**

新增纯函数（放 commands.rs 或就近 render 模块）：

```rust
/// Build the non-error rate-limit pause line. Empty `reset_at_display` + a small
/// `secs_until_reset` means the kernel is auto-waiting (countdown); otherwise it's
/// a pause handed back to the user.
pub fn format_rate_limited_line(
    reset_at_display: &str,
    _reset_label: &str,
    secs_until_reset: Option<u64>,
) -> String {
    if reset_at_display.is_empty() {
        let n = secs_until_reset.unwrap_or(0);
        return format!("⏳ 限流，{n}s 后自动继续…");
    }
    let tail = match secs_until_reset {
        Some(s) => format!("（还有 {}）", fmt_dur(s)),
        None => String::new(),
    };
    format!("⏸ 5小时窗口已用尽，约 {reset_at_display} 恢复{tail} · 已保留已完成内容 · 可换模型或稍后重试")
}

/// "2h11m" / "45s"
fn fmt_dur(secs: u64) -> String {
    if secs >= 3600 { format!("{}h{}m", secs / 3600, (secs % 3600) / 60) }
    else if secs >= 60 { format!("{}m", secs / 60) }
    else { format!("{secs}s") }
}
```

在 commands.rs 处理事件的 match 里加分支，用非红 hint 样式渲染（参照现有 `UiLine::Error` 用法但换成普通/dim line，例如 `UiLine::Hint`/`UiLine::Plain` —— 按现有可用变体；**不要**用 `UiLine::Error`）：

```rust
TurnEvent::RateLimited { reset_at_display, reset_label, secs_until_reset } => {
    let line = format_rate_limited_line(&reset_at_display, &reset_label, secs_until_reset);
    renderer.render(UiLine::Hint(line)); // 非红；按现有非错误行变体替换
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `CARGO_INCREMENTAL=0 cargo test -p atomcode-tuix rate_limited 2>&1 | tail -20`
Expected: PASS。

- [ ] **Step 5: 手动验证（无法自动测真 TUI）**

构建二进制并人工触发：`CARGO_INCREMENTAL=0 cargo build -p atomcode-tuix 2>&1 | tail -5`。在 CodingPlan 5h 窗口接近耗尽时观察：限流出现为暗色暂停行（非红错误），含 reset 时间；esc 仍可退出。**若无法构造真限流，至少确认编译通过 + 单测通过 + match 非红样式，标注"真机限流待验"。**

- [ ] **Step 6: Commit**

```bash
git add crates/atomcode-tuix/src/event_loop/commands.rs crates/atomcode-tuix/src/render
git commit -m "feat(tuix): render 429 pause as non-error line with reset time

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 8: webui 渲染暂停卡片 + 倒计时 + i18n

**Files:**
- Modify: `webui/src/api.ts`（事件类型加 `rate_limited`）
- Modify: `webui/src/components/Chat.tsx`（事件处理 `case 'rate_limited'` + 暂停卡片渲染，非 `chat.error` 红字）
- Modify: `webui/src/i18n.ts`（zh + en 新文案）
- Test: `webui` 现有 node --test 套路（若 Chat 事件归并有可测纯函数则加；否则手动验证）

**Interfaces:**
- Consumes: wire 事件 `{"type":"rate_limited", reset_at_display, reset_label, secs_until_reset}`（Task 6）
- Produces: 聊天流里一张暂停卡片（暗色非红），含 reset 时间 + "可换模型/稍后重试"；`secs_until_reset` 小且无 reset_at_display 时显示倒计时

- [ ] **Step 1: 加 i18n 文案（zh + en 都加）**

在 `webui/src/i18n.ts` 的 zh 和 en catalog 各加：

```ts
// zh
'chat.rateLimited.paused': '5 小时窗口已用尽，约 {time} 恢复',
'chat.rateLimited.hint': '已保留已完成内容 · 可换模型或稍后重试',
'chat.rateLimited.waiting': '限流，{secs}s 后自动继续…',
// en
'chat.rateLimited.paused': '5-hour window exhausted — resets around {time}',
'chat.rateLimited.hint': 'Completed work is preserved · switch model or retry later',
'chat.rateLimited.waiting': 'Rate limited — auto-continuing in {secs}s…',
```

- [ ] **Step 2: 加事件类型**

在 `webui/src/api.ts` 的 live 事件 union 里加（参照现有 `error`/`warning` 事件类型）：

```ts
| { type: 'rate_limited'; reset_at_display: string; reset_label: string; secs_until_reset: number | null }
```

- [ ] **Step 3: Chat.tsx 处理事件 + 渲染卡片**

在 `Chat.tsx` 事件 switch（参照 `case 'error':` 约 `:876`）加：

```tsx
case 'rate_limited': {
  const time = event.reset_at_display;
  const text = time
    ? `${t('chat.rateLimited.paused', { time })} · ${t('chat.rateLimited.hint')}`
    : t('chat.rateLimited.waiting', { secs: String(event.secs_until_reset ?? 0) });
  appendRateLimitedNotice(text); // 走暂停样式，NOT appendToLastAssistant(error)
  break;
}
```

渲染：新增暗色非红的暂停样式块（参照 `error-message-content` 约 `Chat.tsx:1589` 但用中性/暗色 class，例如 `rate-limited-notice`），不要复用红色错误样式。对应 CSS 加一条暗色样式。

- [ ] **Step 4: 验证**

构建：`cd webui && npm run build 2>&1 | tail -15`（或仓库现用的构建命令）。Expected: 构建通过、无 TS 报错。
若有 node --test 纯函数测试套：`node --test 2>&1 | tail -20`。
手动：webui 触发限流（或临时注入一条 `rate_limited` 事件）确认渲染为暗色暂停卡片含 reset 时间，非红错误。**真限流难构造时标注"待验"。**

- [ ] **Step 5: Commit**

```bash
git add webui/src/api.ts webui/src/components/Chat.tsx webui/src/i18n.ts
git commit -m "feat(webui): render 429 pause card with reset time + countdown (zh/en)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 9: 清理月度死代码（独立 commit）

**Files:**
- Modify: `crates/atomcode-core/src/coding_plan/setup.rs`（删 `blocking_exhausted_window` 约 `:1050` + 其调用处约 `:385` 的月度分支 + 相关测试 `blocking_exhausted_window_detects_hidden_monthly` 等）

**Interfaces:** 无对外接口变化（纯删死代码）。

**说明：** 月度窗口下线后 `blocking_exhausted_window`（过滤 `window_size_seconds/3600 > 5`）永不匹配。删除它及其唯一调用分支与专属测试。**仅删确认无其它引用的部分**——删前先 grep 调用点。

- [ ] **Step 1: 确认引用点**

Run: `rg -n "blocking_exhausted_window" crates/`
Expected: 仅 `setup.rs` 定义 + `setup.rs:385` 调用 + 测试。若有别处引用，停下评估，不删。

- [ ] **Step 2: 删除**

删 `blocking_exhausted_window` 函数（`:1050` 区）、`setup.rs:385` 的 `if let Some(w) = blocking_exhausted_window(...)` 月度分支（保留 `show_enable==1` 的 5h 窗口渲染分支）、以及测试 `blocking_exhausted_window_detects_hidden_monthly` / `render_..._monthly_exhausted` 等只测月度的用例。保留 5h 窗口渲染测试。

- [ ] **Step 3: 编译 + 测试**

Run: `CARGO_INCREMENTAL=0 cargo test -p atomcode-core coding_plan 2>&1 | tail -25`
Expected: 编译通过、剩余 coding_plan 测试全过（5h 窗口渲染、fallback 测试仍在）。

- [ ] **Step 4: Commit**

```bash
git add crates/atomcode-core/src/coding_plan/setup.rs
git commit -m "chore(coding_plan): drop dead monthly blocking_exhausted_window

月度限流下线后该路径永不匹配；只留 5h 滚动窗口。

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review

**Spec coverage：**
- ① 范围与行为 → Task 3（分流逻辑）+ Task 4（阈值/reset 数据）✅
- ② 新 kernel 接口（hook + 事件 + StopReason）→ Task 1 ✅
- ③ kernel 循环改动（open + mid-stream）→ Task 3 ✅
- ④ 宿主 hook（usage 关联 + 阈值 + fallback）→ Task 4 ✅
- ⑤ bridge + UI（TUI + webui）→ Task 5（bridge/core 事件）+ Task 6（daemon wire）+ Task 7（TUI）+ Task 8（webui/i18n）✅
- ⑥ 月度死代码清理（独立 commit）→ Task 9 ✅
- ⑦ 测试（kernel/hook/集成）→ Task 1/2/3/4/6 含 TDD；TUI/webui 含纯函数单测 + 手动验证说明 ✅
- ⑧ YAGNI（不改 v1/不做换模型/无 PAYG）→ 计划内无越界任务 ✅

**Placeholder scan：** 无 TBD/TODO；TUI/webui 渲染因无法自动测真终端/浏览器，明确标注手动验证步骤并提供可单测纯函数，非占位。Task 3/5 标注的"按现有夹具/别名套用"是对既有命名的对齐指示，非内容缺失。

**Type consistency：** `RateLimitHint`/`RateLimitDecision`/`RATE_LIMIT_AUTO_WAIT_SECS`/`on_rate_limit(&hint) -> Option<RateLimitDecision>`/`from_hint`/`decide_from_windows`/`AgentEvent::RateLimited{reset_at_display,reset_label,secs_until_reset}`/`StopReason::RateLimited`/`TurnEvent::RateLimited`/`LiveWireEvent::RateLimited`/`format_rate_limited_line` 跨 Task 1→8 字段名与签名一致。

**已知实施期需对齐项（非阻塞）：** kernel 集成测试夹具命名（Task 3）、`status_v2()` 返回字段确认（Task 4）、`LiveWireEvent` serde tag 风格（Task 6）、TUI `UiLine` 非错误变体名（Task 7）、webui 构建命令与事件 union 形态（Task 8）——均在对应 task 内标注按现有代码对齐。
