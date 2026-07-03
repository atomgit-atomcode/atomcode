# 终端标题状态圆点 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在终端标签栏/窗口标题前缀一个按状态变化的彩色圆点（🟢 空闲 / 🟡 忙 / 🔴 待确认），让用户不切换到 atomcode 窗口就能看出任务状态。

**Architecture:** 全部判断逻辑落在 `title.rs` 的三个纯函数里（映射、组装、决策），返回 `Option<String>` 表达"Suspended 时不动标题"。事件循环里的 `sync_terminal_title` 只做 plumbing：读 `ctx.config.ui.terminal_status_glyph` + `app.state.phase`，调纯函数，变了才 `set_title`。config 加一个默认 `true` 的开关。

**Tech Stack:** Rust；`crossterm::terminal::SetTitle`（unix OSC / windows `SetConsoleTitleW`）；serde config。

## Global Constraints

- 状态源是现有 `crate::state::UiPhase`（`crates/atomcode-tuix/src/state.rs:33`）四态：`Idle` / `Streaming` / `Approval` / `Suspended`。不新增事件、不新增 phase。
- 圆点映射：`Idle → 🟢`、`Streaming → 🟡`、`Approval → 🔴`、`Suspended → None`（不改标题）。
- 名字截断逻辑（现有 `session_terminal_title`，`MAX_TITLE_CHARS = 40`）**一字不改**；圆点是独立前缀，不占名字预算。
- 默认开启（`default_terminal_status_glyph() -> true`），config 键 `ui.terminal_status_glyph` 可关。
- 开关关闭时行为与今天**完全一致**（纯名字标题，零变化）。
- Commit message 结尾加：`Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`
- 当前分支 `release/v4.25.9`，直接在此分支提交（延续该 release 线的既有工作流）。
- 构建约束：`CARGO_INCREMENTAL=0`，按 package 编译（`-p atomcode-tuix` / `-p atomcode-core`），别全工作区。

---

### Task 1: `title.rs` 纯函数（映射 + 组装 + 决策）

**Files:**
- Modify: `crates/atomcode-tuix/src/title.rs`（顶部加 import；新增三个函数；在 `#[cfg(test)] mod tests` 追加测试）

**Interfaces:**
- Consumes: `crate::state::UiPhase`（现有枚举）；现有 `session_terminal_title(name: &str, fallback: &str) -> String`。
- Produces:
  - `fn phase_status_glyph(phase: UiPhase) -> Option<&'static str>`
  - `pub fn session_terminal_title_with_status(name: &str, fallback: &str, glyph: Option<&str>) -> String`
  - `pub fn status_title(name: &str, fallback: &str, phase: UiPhase, glyph_enabled: bool) -> Option<String>`（`None` = 不动标题）

- [ ] **Step 1: Write the failing tests**

在 `crates/atomcode-tuix/src/title.rs` 的 `mod tests` 里（`FB` 常量已存在 = `"atomcode v9.9.9"`），追加：

```rust
    use crate::state::UiPhase;

    #[test]
    fn glyph_maps_each_phase() {
        assert_eq!(phase_status_glyph(UiPhase::Idle), Some("🟢"));
        assert_eq!(phase_status_glyph(UiPhase::Streaming), Some("🟡"));
        assert_eq!(phase_status_glyph(UiPhase::Approval), Some("🔴"));
        assert_eq!(phase_status_glyph(UiPhase::Suspended), None);
    }

    #[test]
    fn with_status_prefixes_glyph_and_space() {
        let t = session_terminal_title_with_status("fix login bug", FB, Some("🟡"));
        assert_eq!(t, "🟡 fix login bug");
    }

    #[test]
    fn with_status_none_is_identical_to_plain_title() {
        // Toggle-off / no-glyph path must equal today's behaviour exactly.
        assert_eq!(
            session_terminal_title_with_status("fix login bug", FB, None),
            session_terminal_title("fix login bug", FB),
        );
        assert_eq!(
            session_terminal_title_with_status("default", FB, None),
            FB,
        );
    }

    #[test]
    fn placeholder_name_still_gets_glyph() {
        // A brand-new idle window shows 🟢 atomcode v9.9.9 (alive + idle).
        assert_eq!(
            session_terminal_title_with_status("default", FB, Some("🟢")),
            format!("🟢 {FB}"),
        );
    }

    #[test]
    fn long_name_budget_survives_glyph_prefix() {
        // The name portion is still truncated to MAX_TITLE_CHARS; the glyph
        // is extra, so total is MAX + "🟢 " (2 chars) and the name part is intact.
        let name = "a".repeat(50);
        let plain = session_terminal_title(&name, FB); // MAX_TITLE_CHARS chars, ends with …
        let with = session_terminal_title_with_status(&name, FB, Some("🟢"));
        assert_eq!(with, format!("🟢 {plain}"));
        assert!(plain.chars().count() == MAX_TITLE_CHARS);
    }

    #[test]
    fn status_title_suspended_returns_none() {
        assert_eq!(status_title("fix login bug", FB, UiPhase::Suspended, true), None);
    }

    #[test]
    fn status_title_disabled_drops_glyph() {
        assert_eq!(
            status_title("fix login bug", FB, UiPhase::Streaming, false),
            Some("fix login bug".to_string()),
        );
    }

    #[test]
    fn status_title_enabled_prefixes_phase_glyph() {
        assert_eq!(
            status_title("fix login bug", FB, UiPhase::Approval, true),
            Some("🔴 fix login bug".to_string()),
        );
    }
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cd /Users/theo/Documents/workspace/atomcode
CARGO_INCREMENTAL=0 cargo test -p atomcode-tuix --lib title::tests 2>&1 | tail -20
```
Expected: FAIL — `cannot find function phase_status_glyph` / `session_terminal_title_with_status` / `status_title`.

- [ ] **Step 3: Implement the three functions**

在 `crates/atomcode-tuix/src/title.rs` 顶部，`use crate::sanitize::scrub_controls;` 下面加：

```rust
use crate::state::UiPhase;
```

在 `session_terminal_title` 函数之后（`}` 之后、`#[cfg(test)]` 之前）加：

```rust
/// Colored status dot for the terminal-title prefix, keyed off the current
/// UI phase. `None` means "no dot" — used for `Suspended` (external handoff:
/// `/shell`, OAuth) where we leave whatever title was last shown.
fn phase_status_glyph(phase: UiPhase) -> Option<&'static str> {
    match phase {
        UiPhase::Idle => Some("🟢"),
        UiPhase::Streaming => Some("🟡"),
        UiPhase::Approval => Some("🔴"),
        UiPhase::Suspended => None,
    }
}

/// Build the title, optionally prefixed with a status `glyph`. The name
/// portion reuses [`session_terminal_title`] unchanged (so its truncation /
/// scrubbing budget is untouched); the glyph is an extra 1-scalar + space
/// prefix, so a status title is at most 2 chars longer than the plain one.
pub fn session_terminal_title_with_status(name: &str, fallback: &str, glyph: Option<&str>) -> String {
    let title = session_terminal_title(name, fallback);
    match glyph {
        Some(g) => format!("{g} {title}"),
        None => title,
    }
}

/// Decide the full terminal title to emit for `(name, phase, glyph_enabled)`.
/// Returns `None` when the title should be left untouched (the `Suspended`
/// phase, where the terminal is handed to an external child). When
/// `glyph_enabled` is false, no dot is added — behaviour identical to before
/// this feature.
pub fn status_title(name: &str, fallback: &str, phase: UiPhase, glyph_enabled: bool) -> Option<String> {
    let glyph = phase_status_glyph(phase);
    if phase == UiPhase::Suspended {
        return None;
    }
    let glyph = if glyph_enabled { glyph } else { None };
    Some(session_terminal_title_with_status(name, fallback, glyph))
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cd /Users/theo/Documents/workspace/atomcode
CARGO_INCREMENTAL=0 cargo test -p atomcode-tuix --lib title:: 2>&1 | tail -20
```
Expected: PASS — all `title::tests` (existing + new) green.

- [ ] **Step 5: Commit**

```bash
cd /Users/theo/Documents/workspace/atomcode
git add crates/atomcode-tuix/src/title.rs
git commit -m "feat(tui): status-glyph title helpers (phase → 🟢/🟡/🔴 prefix)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: config 开关 `ui.terminal_status_glyph`

**Files:**
- Modify: `crates/atomcode-core/src/config/mod.rs`（加默认函数、`UiConfig` 字段、`Default` impl、测试）

**Interfaces:**
- Produces: `UiConfig.terminal_status_glyph: bool`（TOML `ui.terminal_status_glyph`，缺省 `true`）— Task 3 读取。

- [ ] **Step 1: Write the failing test**

在 `crates/atomcode-core/src/config/mod.rs` 的测试模块里（紧挨现有 `auto_copy_code_blocks_defaults_off` 附近，约 `:836`），加：

```rust
    #[test]
    fn terminal_status_glyph_defaults_on() {
        // Default-on: fresh config and a config missing the key both enable it.
        assert!(UiConfig::default().terminal_status_glyph);
        let ui: UiConfig = toml::from_str("").unwrap();
        assert!(ui.terminal_status_glyph, "missing key → default on");
    }
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cd /Users/theo/Documents/workspace/atomcode
CARGO_INCREMENTAL=0 cargo test -p atomcode-core --lib terminal_status_glyph_defaults_on 2>&1 | tail -20
```
Expected: FAIL — `no field terminal_status_glyph on type UiConfig` (compile error).

- [ ] **Step 3: Add the default fn, struct field, and Default entry**

在 `crates/atomcode-core/src/config/mod.rs`，`default_ai_session_naming` 函数附近加：

```rust
fn default_terminal_status_glyph() -> bool {
    // ON by default: a colored status dot (🟢 idle / 🟡 busy / 🔴 approval)
    // prefixed to the terminal tab title so the user can tell state without
    // switching windows. Off for terminals that render emoji as tofu boxes
    // (tmux, plain VT, some embedded IDE terminals). Read from ctx.config, so
    // /reload picks up a change.
    true
}
```

在 `UiConfig` struct 里，`ai_session_naming` 字段之后加：

```rust
    /// Prefix a colored status dot (🟢 idle / 🟡 busy / 🔴 needs-approval) to
    /// the terminal tab/window title. Default on; turn off if your terminal
    /// shows emoji as monochrome tofu boxes.
    #[serde(default = "default_terminal_status_glyph")]
    pub terminal_status_glyph: bool,
```

在 `impl Default for UiConfig` 的 `Self { ... }` 里，`ai_session_naming: default_ai_session_naming(),` 之后加：

```rust
            terminal_status_glyph: default_terminal_status_glyph(),
```

- [ ] **Step 4: Run test to verify it passes**

```bash
cd /Users/theo/Documents/workspace/atomcode
CARGO_INCREMENTAL=0 cargo test -p atomcode-core --lib terminal_status_glyph_defaults_on 2>&1 | tail -20
```
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cd /Users/theo/Documents/workspace/atomcode
git add crates/atomcode-core/src/config/mod.rs
git commit -m "feat(config): add ui.terminal_status_glyph toggle (default on)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: 接线 `sync_terminal_title`（phase + config → status_title）

**Files:**
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs:6546-6553`（`sync_terminal_title` 函数体 + 签名）
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs:3817`（调用点，传 `app.state.phase`）

**Interfaces:**
- Consumes: `crate::title::status_title`（Task 1）；`ctx.config.ui.terminal_status_glyph`（Task 2）；`crate::state::UiPhase`（现有，`event_loop` 已 import）；`app.state.phase`（现有字段）。
- Produces: 无对外新符号；行为改动 = 标题带状态圆点。

- [ ] **Step 1: 改函数签名与函数体**

把 `crates/atomcode-tuix/src/event_loop/mod.rs` 现有的（`:6546` 起）：

```rust
fn sync_terminal_title(ctx: &LoopCtx, renderer: &mut dyn Renderer, last: &mut Option<String>) {
    const VERSION_FALLBACK: &str = concat!("atomcode v", env!("CARGO_PKG_VERSION"));
    let title = crate::title::session_terminal_title(&ctx.current_session.name, VERSION_FALLBACK);
    if last.as_deref() != Some(title.as_str()) {
        renderer.set_title(title.clone());
        *last = Some(title);
    }
}
```

替换为：

```rust
fn sync_terminal_title(
    ctx: &LoopCtx,
    renderer: &mut dyn Renderer,
    last: &mut Option<String>,
    phase: UiPhase,
) {
    const VERSION_FALLBACK: &str = concat!("atomcode v", env!("CARGO_PKG_VERSION"));
    // `None` = leave the title untouched (Suspended: an external child owns
    // the terminal during /shell, OAuth, etc.).
    let Some(title) = crate::title::status_title(
        &ctx.current_session.name,
        VERSION_FALLBACK,
        phase,
        ctx.config.ui.terminal_status_glyph,
    ) else {
        return;
    };
    if last.as_deref() != Some(title.as_str()) {
        renderer.set_title(title.clone());
        *last = Some(title);
    }
}
```

同时把该函数的文档注释首段补一句 phase 语义（在现有注释块里，`/// Fallback for un-named …` 段之前加一行）：

```rust
/// The title is prefixed with a status dot derived from `phase`
/// (🟢 idle / 🟡 busy / 🔴 approval) when `ctx.config.ui.terminal_status_glyph`
/// is on; a phase change re-emits on the next loop iteration.
```

- [ ] **Step 2: 改调用点传 phase**

把 `crates/atomcode-tuix/src/event_loop/mod.rs:3817` 的：

```rust
        sync_terminal_title(&ctx, renderer, &mut last_terminal_title);
```

替换为：

```rust
        sync_terminal_title(&ctx, renderer, &mut last_terminal_title, app.state.phase);
```

- [ ] **Step 3: 编译 + 跑 tuix 测试**

```bash
cd /Users/theo/Documents/workspace/atomcode
CARGO_INCREMENTAL=0 cargo test -p atomcode-tuix --lib 2>&1 | tail -25
```
Expected: 编译通过；`title::tests` 全绿；无因签名改动导致的编译错误。（`app.state.phase` 现字段、`UiPhase` 已在 `event_loop/mod.rs` import——见 `:7750` 等处的 `use crate::state::…`；若报未 import，在文件顶部 `use` 区补 `UiPhase`。）

> 说明：`sync_terminal_title` 是纯 plumbing，全部判断逻辑已在 Task 1 的 `status_title` 里单测覆盖（含 Suspended → 不 emit）。此处不再为它单独构造 `LoopCtx` 写测试——那需要重量级 fixture 且只会重复 Task 1 已验证的逻辑。

- [ ] **Step 4: Commit**

```bash
cd /Users/theo/Documents/workspace/atomcode
git add crates/atomcode-tuix/src/event_loop/mod.rs
git commit -m "feat(tui): drive terminal title status dot from UI phase + config

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: 全量验证 + 真机自检提示

**Files:** 无改动（验证任务）。

- [ ] **Step 1: 两个 crate 全 lib 测试**

```bash
cd /Users/theo/Documents/workspace/atomcode
CARGO_INCREMENTAL=0 cargo test -p atomcode-tuix --lib 2>&1 | tail -8
CARGO_INCREMENTAL=0 cargo test -p atomcode-core --lib 2>&1 | tail -8
```
Expected: 两者 `test result: ok`。

- [ ] **Step 2: clippy（改动文件不引入新告警）**

```bash
cd /Users/theo/Documents/workspace/atomcode
CARGO_INCREMENTAL=0 cargo clippy -p atomcode-tuix -p atomcode-core 2>&1 | tail -15
```
Expected: 无新增 warning/error（预存告警不算）。

- [ ] **Step 3: 真机自检清单（人工，非自动化）**

在支持彩色 emoji 的终端（iTerm2 / WT / VS Code 内置）跑 `cargo run -p atomcode`（或已编译二进制），确认标签栏标题：
1. 启动后空闲 → `🟢 atomcode v4.25.9`（或会话名）。
2. 发一条消息、模型在跑 → 变 `🟡 …`。
3. 触发一个需审批的工具（如 edit_file）→ 变 `🔴 …`。
4. 审批完/回答完回空闲 → 回 `🟢 …`。
5. `ui.terminal_status_glyph = false` 后重启 → 无圆点，纯名字标题（今天的行为）。

> 真机步骤留给用户/执行者手动确认——TUI 标题无法在 CI/headless 里断言。

---

## Self-Review

**1. Spec coverage：**
- 状态→圆点映射 → Task 1 `phase_status_glyph`。✅
- 组装标题（不动名字预算） → Task 1 `session_terminal_title_with_status`。✅
- 触发（phase 变化 re-emit、Suspended 不动、开关关退化） → Task 1 `status_title` + Task 3 接线。✅
- config 开关默认 on → Task 2。✅
- 测试（纯函数 + 缺省键 + Suspended 不 emit） → Task 1/2 单测覆盖；Suspended 不 emit 由 `status_title(..Suspended..) == None` 保证。✅
- 明确不做（动画/任务栏色/思考回答拆分/✅态/ASCII） → 计划未引入，符合 YAGNI。✅
- 偏离 spec 记录：spec 原文把 Suspended early-return 放在 `sync`、开关"启动读一次"。计划改为：判断全进 `status_title` 纯函数（更好测），开关改为从 `ctx.config` 内联读取（`/reload` 免费生效）。功能等价、更简洁——见本文件顶部 Architecture 段。

**2. Placeholder scan：** 无 TBD/TODO；每个 code step 均有完整代码与预期输出。✅

**3. Type consistency：** `phase_status_glyph(UiPhase) -> Option<&'static str>`、`session_terminal_title_with_status(&str,&str,Option<&str>) -> String`、`status_title(&str,&str,UiPhase,bool) -> Option<String>`、`UiConfig.terminal_status_glyph: bool` —— 三个 Task 引用一致；调用点传 `app.state.phase`（`UiPhase`）匹配签名。✅
