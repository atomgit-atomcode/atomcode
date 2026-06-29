# v2 5 小时窗口限流的"暂停-自愈"体验

- 日期：2026-06-27
- 引擎范围：仅 v2（kernel）
- 界面范围：TUI + webui

## 背景与问题

用户反馈：限流（429）错误会在任务执行中途**粗暴打断**并弹红字报错，希望"等任务做完再提示限流"。用户提到"5 小时窗口"和"用了 70% 额度"两种都会触发。

经排查澄清：

- "5 小时"和"70% 额度"是**同一限流事件的两个面**——CodingPlan 的 5 小时滚动窗口被打满时，网关对所有后续请求返回 429，即使月度额度只用了 70%。**月度限流现已下线，本设计只考虑 5 小时滚动窗口。**
- "硬限流没法等任务做完再提示"在物理上成立：一旦网关 429，后续 LLM 请求全被拒，任务无法继续，自然没有"执行完"这一刻。
- 但**体验**可以改：不粗暴打断、保住已产出内容、明确告知恢复时间、近期窗口自动等待续跑。参考 opencode 的"暂停-自愈"思路（`session/retry.ts` 读 retry-after 挂起、`message-v2.ts` 保留已产出内容、status 事件驱动 UI 而非红字 error）。

### v2 当前行为（已核对）

- OPEN 失败（`crates/atomcode-kernel/src/agent.rs:967`）：429 被 provider 标成 `retryable` → 走 `MAX_PROVIDER_RETRIES=3` 的 3/6/9s 盲重试（`agent.rs:978` 可取消退避）→ 仍失败则 `agent.rs:989` emit `AgentEvent::Error` 红字终止。对 5h 窗口这 ~18s 重试纯属浪费后硬报错。
- mid-stream 429（`agent.rs:1135`）：另一个 `Error` 终止点。
- 退避已是**可取消**的（esc 能断，`agent.rs:978` 的 `select! { cancel vs sleep }`），可复用。
- 约束：**kernel 不能依赖 core**。reset 时间数据（`rate_limit_windows`）在 `atomcode-core/coding_plan` + usage 轮询里。

## 目标行为（已确认）

429 触发时，按 reset 剩余时间分流：

- **reset ≤ 2 分钟**：可取消地挂起倒计时，窗口恢复后**自动续跑当前 turn**（esc 仍可中断）。
- **reset > 2 分钟**：保住已产出内容，**优雅暂停**（非红字 error），显示"约 HH:MM 恢复"，交给用户（重试 / 换模型）。

阈值 `RATE_LIMIT_AUTO_WAIT_SECS = 120`。

## 架构（路线 A：Hook 注入决策）

kernel 检测 `http_status==429` → 调新 hook 取决策；决策逻辑（含 usage 数据访问）放**宿主侧**，kernel 只负责执行。严守 kernel/core 边界。

### ② 新增 kernel 接口

`LifecycleHooks`（`crates/atomcode-kernel/src/hook.rs`）新增方法：

```rust
async fn on_rate_limit(&self, _hint: RateLimitHint) -> RateLimitDecision {
    // 默认实现：无 usage 数据 → 退回保守行为
    // 有 retry_after_secs 且 <= 阈值则 WaitAndRetry，否则 Pause（无 reset 文案）
    RateLimitDecision::default_from_hint(_hint)
}
```

新类型（kernel 内，不依赖 core）：

```rust
pub struct RateLimitHint {
    pub http_status: Option<u16>,
    pub retry_after_secs: Option<u64>, // kernel best-effort（provider 解析的 Retry-After / "Try again in Ns"）
}

pub enum RateLimitDecision {
    WaitAndRetry { secs: u64 },
    Pause {
        reset_at_display: String,   // "18:09"，可为空
        reset_label: String,        // "当前窗口结束即重置额度（每 5 小时一个窗口）"，可为空
        secs_until_reset: Option<u64>,
    },
}
```

`AgentEvent`（`crates/atomcode-kernel/src/event.rs`）新增变体：

```rust
RateLimited {
    reset_at_display: String,
    reset_label: String,
    secs_until_reset: Option<u64>,
},
```

`StopReason` 新增变体 `RateLimited`，使 `TurnComplete.reason` 能表达"因限流暂停"，区别于 `ProviderError` 红字。

**默认实现保证无回归**：非 CodingPlan、无 hook、或 hook 未实现 `on_rate_limit` 时，退回当前等价行为（按 `retry_after_secs` 或保守重试/暂停）。

### ③ kernel 循环改动（`agent.rs`）

OPEN 失败分支（`:967`）前插 429 专属分支：

```rust
Err(e) if e.http_status == Some(429) => {
    let hint = RateLimitHint { http_status: e.http_status, retry_after_secs: parse_retry_after(&e) };
    match self.hooks.on_rate_limit(hint).await {
        RateLimitDecision::WaitAndRetry { secs } => {
            // 复用 :978 的可取消退避；倒计时期间 emit RateLimited / Warning 让 UI 显示 "Xs 后自动继续"
            // 睡 secs 后 round -= 1 续跑；esc 仍可断
        }
        RateLimitDecision::Pause { reset_at_display, reset_label, secs_until_reset } => {
            self.rt.emit(AgentEvent::RateLimited { reset_at_display, reset_label, secs_until_reset });
            self.finish_turn(convo, StopReason::RateLimited, &turn_ctx).await;
            return; // 不走 :989 的 Error，已产出内容（前轮 assistant 消息）留在 convo
        }
    }
}
```

- mid-stream 429（`:1135`）：先 finalize 已产出内容，再走同一决策分支。
- 429 不再无条件吃 `MAX_PROVIDER_RETRIES` 的 3/6/9s 盲重试；决策完全交给 hook。其它 retryable（5xx/transport）路径**不变**。

### ④ 宿主 hook 实现（reset 时间关联 + 阈值策略）

宿主侧新增 hook 实现（TUI 与 daemon/webui 各一 thin impl，或共用一份放两边可引用处）。逻辑：

1. 取 usage 窗口：
   - TUI：优先读 `usage_monitor` 已轮询的共享 slot（30s 内新鲜）。
   - daemon/webui：直接 `atomcode_core::coding_plan::client::Client::from_stored_auth().status_v2()` 拉一次。
   - 两者无缓存时 fallback 一次 `status_v2()`。
2. 从 `rate_limit_windows` 找 5h 窗口（月度已无，基本是唯一 / `window_size_seconds <= 18000` 那个），取 `reset_at_display` / `reset_label` / `seconds_until_reset`。
3. fallback 链：`seconds_until_reset` 拿不到 → 用 `hint.retry_after_secs` → 再 fallback 保守默认（120s，触发 Pause）。
4. 套策略：`secs_until_reset <= RATE_LIMIT_AUTO_WAIT_SECS(120)` → `WaitAndRetry { secs }`；否则 `Pause { .. }`。

**kernel 不依赖 core**：usage fetch 全在宿主 hook impl，kernel 只见 trait。

### ⑤ bridge + UI 渲染

bridge 把 `AgentEvent::RateLimited` 映射成两边"暂停态"（非 error 样式）：

- **TUI**：footer 专用提示行，复用 `usage_monitor` 已有 "5小时滚动窗口 重置于 HH:MM" 文案风格，暗色非红；`WaitAndRetry` 显示倒计时"Xs 后自动继续"。
- **webui**：走 status/事件，渲染暂停卡片（非 `chat.error` 红字），带 reset 时间 + "可换模型/稍后重试"；`WaitAndRetry` 显示倒计时。
- 文案进 i18n（`webui/src/i18n.ts` zh+en 都加）。

### ⑥ 顺手清理（独立 commit）

月度下线后 `blocking_exhausted_window`（`crates/atomcode-core/src/coding_plan/setup.rs:1050`，过滤 `window_size_seconds/3600 > 5`）成死代码 → 删除/简化，连带相关测试（`setup.rs` 的 `blocking_exhausted_window_detects_hidden_monthly` 等）。**作为独立小 commit，不混进主改动。**

## 测试（TDD）

- **kernel**：用 `testkit` 可编程 hook 模拟 `WaitAndRetry` / `Pause` 决策：
  - 429 后 `WaitAndRetry` → 等待并续跑当前 turn；
  - 429 后 `Pause` → emit `AgentEvent::RateLimited` + `StopReason::RateLimited`，**不**发 `Error`；
  - esc 能中断 `WaitAndRetry` 等待（走 `finish_cancelled`）；
  - 非 429 retryable 错误路径不变（仍 3/6/9s）。
- **宿主 hook**：喂构造的 `rate_limit_windows`（5h 窗口 reset 近/远 + 无 reset），断言策略选择与 fallback 链。
- **集成**：确定性 agent-loop 集成测试覆盖端到端事件序列。
- MCP/feature 门控：按现有约定跑（kernel 测试不需 `--features mcp`）。

## 不做（YAGNI）

- 不改 v1（`--engine v1` 保持现状）。
- 不动 5 小时窗口以外的限额概念。
- 不做"自动换模型/降级"（只提示用户可换）。
- 不做 webui 的 PAYG 出口（网关无此能力）。

## 受影响文件清单

- `crates/atomcode-kernel/src/hook.rs` — 新 hook 方法 + `RateLimitHint`/`RateLimitDecision`
- `crates/atomcode-kernel/src/event.rs` — `AgentEvent::RateLimited` + `StopReason::RateLimited`
- `crates/atomcode-kernel/src/agent.rs` — OPEN(`:967`)/mid-stream(`:1135`) 429 分支
- `crates/atomcode-kernel/src/testkit.rs` — 可编程限流 hook
- 宿主 hook impl（TUI 侧 + daemon 侧；位置实施时定）
- bridge 事件映射（`atomcode-bridge`）
- `crates/atomcode-tuix/...` — footer 暂停态渲染
- `webui/src/...` — 暂停卡片 + 倒计时；`webui/src/i18n.ts` zh+en 文案
- `crates/atomcode-core/src/coding_plan/setup.rs` — 删 `blocking_exhausted_window`（独立 commit）
