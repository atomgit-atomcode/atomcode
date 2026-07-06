# 双击 Esc 撤销加冷却 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给双击 Esc 触发的 `/undo` 加一个撤后冷却，使快速连按 Esc 最多撤一轮（防误撤连撤多轮）。

**Architecture:** 纯逻辑集中在 `event_loop/mod.rs` 的 `intercept_empty_bare_esc`：给它加一个 `last_undo_at` 入参，冷却期内 bare Esc 既不武装也不触发。`App` 加一个 `esc_undo_last_at` 时间戳字段，调用点在 `TriggerUndo` 时记录它。单次撤销手感不变，只杀"多轮"。

**Tech Stack:** Rust；crossterm 键事件；`std::time::Instant`。

## Global Constraints

- 冷却常量 `DOUBLE_ESC_UNDO_COOLDOWN = Duration::from_millis(1500)`。
- 作用域仅"空闲 + 输入框为空"的 bare Esc（`intercept_empty_bare_esc` 那条路径）；不触碰流式中 Esc 取消 turn。
- 单次双击 Esc 撤一轮的行为**必须完全不变**（`last_undo_at = None` 时逻辑与现在一致）。
- 不做 redo、不加确认卡、不加冷却提示行、不加 min-gap。
- 构建约束：`CARGO_INCREMENTAL=0`，按 package 编（`-p atomcode-tuix`）。
- Commit message 结尾加：`Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`
- 当前分支 `release/v4.26.0`，直接在此分支提交。

---

### Task 1: 撤后冷却（常量 + 字段 + 纯函数 + 接线 + 测试）

**Files:**
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs`
  - 常量：`:3497` `DOUBLE_ESC_UNDO_WINDOW` 之后加冷却常量
  - `App` 字段：`:3470` `esc_undo_pending` 之后加 `esc_undo_last_at`
  - `App` 构造：`:3540` `esc_undo_pending: None,` 之后加 init
  - 纯函数：`:3509-3520` `intercept_empty_bare_esc` 加参 + 冷却判断
  - 调用点：`:6133` 捕获 `now`、传 `last_undo_at`、`TriggerUndo` 时记 `esc_undo_last_at`
  - 现有测试调用点：`:1507`、`:1520` 补中间实参
  - 新测试：`mod tests` 里 `second_empty_bare_esc_triggers_undo_and_clears_pending`（`:1524`）之后追加

**Interfaces:**
- Produces:
  - `const DOUBLE_ESC_UNDO_COOLDOWN: std::time::Duration`
  - `App.esc_undo_last_at: Option<std::time::Instant>`
  - `fn intercept_empty_bare_esc(pending: &mut Option<Instant>, last_undo_at: Option<Instant>, now: Instant) -> EmptyEscIntercept`（签名新增第二参）

- [ ] **Step 1: 写失败测试 + 改现有两个测试调用点**

先把现有两个调用点补上新的中间实参 `None`（`crates/atomcode-tuix/src/event_loop/mod.rs`）：

`:1507` 处：
```rust
            intercept_empty_bare_esc(&mut pending, None, now),
```
`:1520` 处：
```rust
            intercept_empty_bare_esc(&mut pending, None, second),
```

再在 `second_empty_bare_esc_triggers_undo_and_clears_pending` 测试（其结尾 `}` 在 `:1524`）之后追加四个新测试：

```rust
    #[test]
    fn cooldown_blocks_second_undo_from_mash() {
        // 冷却期内,一个本会触发 undo(pending 在窗口内)的第二次 Esc 被压制。
        let undo_at = std::time::Instant::now();
        let mashed = undo_at + Duration::from_millis(300); // < 冷却
        let armed = mashed - Duration::from_millis(50); // 距 mashed 50ms → 在 2s 窗口内
        let mut pending = Some(armed);
        assert_eq!(
            intercept_empty_bare_esc(&mut pending, Some(undo_at), mashed),
            EmptyEscIntercept::Consumed
        );
        assert_eq!(pending, Some(armed), "冷却压制时不得改动 pending");
    }

    #[test]
    fn cooldown_blocks_arming_too() {
        // 冷却期内,第一次 Esc 连武装都不做。
        let undo_at = std::time::Instant::now();
        let during = undo_at + Duration::from_millis(300);
        let mut pending = None;
        assert_eq!(
            intercept_empty_bare_esc(&mut pending, Some(undo_at), during),
            EmptyEscIntercept::Consumed
        );
        assert_eq!(pending, None, "冷却期内不得武装");
    }

    #[test]
    fn after_cooldown_double_esc_undoes_again() {
        let undo_at = std::time::Instant::now();
        let later = undo_at + DOUBLE_ESC_UNDO_COOLDOWN + Duration::from_millis(1);
        let mut pending = Some(later - Duration::from_millis(50)); // 在窗口内
        assert_eq!(
            intercept_empty_bare_esc(&mut pending, Some(undo_at), later),
            EmptyEscIntercept::TriggerUndo
        );
    }

    #[test]
    fn no_prior_undo_keeps_original_behaviour() {
        // last_undo_at = None → 与改动前完全一致。
        let now = std::time::Instant::now();
        let mut pending = Some(now - Duration::from_millis(50));
        assert_eq!(
            intercept_empty_bare_esc(&mut pending, None, now),
            EmptyEscIntercept::TriggerUndo
        );
        let mut pending2 = None;
        assert_eq!(
            intercept_empty_bare_esc(&mut pending2, None, now),
            EmptyEscIntercept::Consumed
        );
        assert_eq!(pending2, Some(now));
    }
```

- [ ] **Step 2: 跑测试确认失败（编译红）**

```bash
cd /Users/theo/Documents/workspace/atomcode
CARGO_INCREMENTAL=0 cargo test -p atomcode-tuix --lib cooldown 2>&1 | tail -20
```
Expected: 编译失败 —— `intercept_empty_bare_esc` 目前是 2 参，新测试和改过的调用点传了 3 参 / `DOUBLE_ESC_UNDO_COOLDOWN` 未定义 / `App.esc_undo_last_at` 未定义。（这就是本步的 red。）

- [ ] **Step 3: 实现（常量 + 字段 + init + 函数 + 调用点）**

**(a) 冷却常量** —— 在 `crates/atomcode-tuix/src/event_loop/mod.rs:3497`（`const DOUBLE_ESC_UNDO_WINDOW: Duration = Duration::from_secs(2);`）之后加：
```rust
/// After a double-Esc undo fires, ignore bare-Esc undo arming for this long so a
/// rapid Esc mash can't chain multiple undos (the "撤回多轮" complaint). A
/// deliberate second undo just needs a pause longer than this.
const DOUBLE_ESC_UNDO_COOLDOWN: Duration = Duration::from_millis(1500);
```

**(b) App 字段** —— 在 `App` 的 `pub esc_undo_pending: Option<std::time::Instant>,`（`:3470`）之后加：
```rust
    /// When the last double-Esc undo fired. Within `DOUBLE_ESC_UNDO_COOLDOWN`
    /// of this, a bare Esc neither arms nor triggers undo — so a rapid Esc mash
    /// undoes at most once per burst.
    pub esc_undo_last_at: Option<std::time::Instant>,
```

**(c) App 构造** —— 在构造 `App` 的 `esc_undo_pending: None,`（`:3540`）之后加：
```rust
            esc_undo_last_at: None,
```

**(d) 纯函数** —— 把 `:3509-3520` 现有的：
```rust
fn intercept_empty_bare_esc(
    pending: &mut Option<std::time::Instant>,
    now: std::time::Instant,
) -> EmptyEscIntercept {
    if second_esc_triggers_undo(*pending, now) {
        *pending = None;
        EmptyEscIntercept::TriggerUndo
    } else {
        *pending = Some(now);
        EmptyEscIntercept::Consumed
    }
}
```
替换为：
```rust
fn intercept_empty_bare_esc(
    pending: &mut Option<std::time::Instant>,
    last_undo_at: Option<std::time::Instant>,
    now: std::time::Instant,
) -> EmptyEscIntercept {
    // Cooldown: within DOUBLE_ESC_UNDO_COOLDOWN of the last undo, a bare Esc
    // neither arms nor triggers — so a rapid Esc mash undoes at most once.
    if last_undo_at.is_some_and(|t| now.duration_since(t) <= DOUBLE_ESC_UNDO_COOLDOWN) {
        return EmptyEscIntercept::Consumed;
    }
    if second_esc_triggers_undo(*pending, now) {
        *pending = None;
        EmptyEscIntercept::TriggerUndo
    } else {
        *pending = Some(now);
        EmptyEscIntercept::Consumed
    }
}
```

**(e) 调用点** —— 把 `:6133` 起的：
```rust
        match intercept_empty_bare_esc(&mut app.esc_undo_pending, std::time::Instant::now()) {
```
替换为（捕获 `now` 以便复用）：
```rust
        let now = std::time::Instant::now();
        match intercept_empty_bare_esc(&mut app.esc_undo_pending, app.esc_undo_last_at, now) {
```
并在同一 `match` 的 `EmptyEscIntercept::TriggerUndo =>` 臂里（`app.exit_pending = None;` 之后、`dispatch_undo(...)` 之前）加一行记录冷却起点：
```rust
                app.esc_undo_last_at = Some(now);
```

- [ ] **Step 4: 跑测试确认通过**

```bash
cd /Users/theo/Documents/workspace/atomcode
CARGO_INCREMENTAL=0 cargo test -p atomcode-tuix --lib cooldown 2>&1 | tail -8
CARGO_INCREMENTAL=0 cargo test -p atomcode-tuix --lib esc 2>&1 | tail -12
```
Expected: 4 个新 `*cooldown*`/`after_cooldown*`/`no_prior*` 测试通过；现有 `second_esc_within_window_triggers_undo` / `second_esc_after_window_does_not_trigger_undo` / `first_empty_bare_esc_is_consumed_and_arms_undo` / `second_empty_bare_esc_triggers_undo_and_clears_pending` 仍绿。

- [ ] **Step 5: 全 lib 测试 + clippy（确认没打破别处）**

```bash
cd /Users/theo/Documents/workspace/atomcode
CARGO_INCREMENTAL=0 cargo test -p atomcode-tuix --lib 2>&1 | grep -E "^test result" | tail
CARGO_INCREMENTAL=0 cargo clippy -p atomcode-tuix 2>&1 | grep -iE "intercept_empty_bare_esc|esc_undo_last_at|DOUBLE_ESC_UNDO_COOLDOWN" | head
```
Expected: `test result: ok`（可能有 4 个预存的 `render::retained::tests::retained_*` 字节预算红测试，与本改动无关——确认失败列表只有这 4 个且都是 `retained_*`）；clippy 对新增代码无告警（grep 空）。

- [ ] **Step 6: Commit**

```bash
cd /Users/theo/Documents/workspace/atomcode
git add crates/atomcode-tuix/src/event_loop/mod.rs
git commit -m "fix(tui): cooldown after double-Esc undo so a rapid mash can't chain undos

A bare-Esc mash re-armed undo immediately after each fire, so Esc,Esc,Esc,Esc
undid two turns (user 撤回多轮 complaint, no redo to recover). Add a 1.5s
post-undo cooldown: within it a bare Esc neither arms nor triggers, so a mash
undoes at most once. Single deliberate double-Esc is unchanged.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review

**1. Spec coverage：**
- 冷却常量 1500ms → Step 3(a)。✅
- `intercept_empty_bare_esc` 加 `last_undo_at` + 冷却分支 → Step 3(d)。✅
- `App.esc_undo_last_at` 字段 + init → Step 3(b)(c)。✅
- 调用点 `TriggerUndo` 记 `last_undo_at` + 传参 → Step 3(e)。✅
- 单次双击不变（`last_undo_at=None` 一致）→ 由 `no_prior_undo_keeps_original_behaviour` 测试 + 保留原逻辑保证。✅
- 测试：冷却内不触发/不武装、冷却过后可再撤、无前置照常、现有 4 测试仍绿 → Step 1 + Step 4。✅
- 不做 redo/确认卡/提示行/min-gap；不碰流式 Esc → 计划未引入。✅

**2. Placeholder scan：** 无 TBD/TODO；每处 code step 均有完整前后代码与预期输出。✅

**3. Type consistency：** `intercept_empty_bare_esc(&mut Option<Instant>, Option<Instant>, Instant) -> EmptyEscIntercept` 在函数定义、主调用点、两个既有测试点、四个新测试点全部一致；`esc_undo_last_at: Option<Instant>` 字段/init/读写一致；`DOUBLE_ESC_UNDO_COOLDOWN: Duration` 常量定义与使用一致。✅
