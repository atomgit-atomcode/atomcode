# TUI Unified In-App Scroll Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 retained 和 alt-screen 两个 renderer 的 body 滚动统一到 in-app 缓冲，消除 retained 模式下偶现的"无法滚动"问题，并加可视滚动条 + 跳消息键 + `/keys` 文档。

**Architecture:** retained 加 `view_mode` 状态机，sticky 跟底时走原 DECSTBM 流式，view_mode 时切到 alt-screen 风格的 CUP+EL 重绘；retained 同步接管鼠标（`?1002h ?1006h`）。selection 逻辑抽到 `render/selection.rs` 共享模块，alt-screen 和 retained 共用。两个 renderer 共享 `MessageMark` + 滚动条 + 跳转算法。

**Tech Stack:** Rust, crossterm, alt_screen.rs 现有 alt-screen 渲染器，retained.rs 现有 DECSTBM 渲染器。详细设计见 `docs/superpowers/specs/2026-05-25-tuix-unified-in-app-scroll-design.md`。

---

## File Structure

**New files:**
- `crates/atomcode-tuix/src/render/selection.rs` — 共享选择模块（trait + 状态 + 高亮 + OSC 52 / arboard 复制）
- `crates/atomcode-tuix/src/render/scrollbar.rs` — 滚动条绘制 helper
- `crates/atomcode-tuix/src/render/ui_state.rs` — `$ATOMCODE_HOME/ui-state.toml` 读写

**Modified files:**
- `crates/atomcode-tuix/src/render/mod.rs` — `Renderer` trait 加方法
- `crates/atomcode-tuix/src/render/worker.rs` — 新方法通过 worker 转发
- `crates/atomcode-tuix/src/render/alt_screen.rs` — 切到 shared selection 模块，加 scrollbar 接入，加 MessageMark + 跳转
- `crates/atomcode-tuix/src/render/retained.rs` — 大改：view_mode、body 缓冲扩容、scroll_body、mouse 接管、selection 接入、MessageMark + 跳转、scrollbar 接入
- `crates/atomcode-tuix/src/event_loop/mod.rs` — `handle_scroll_key` 加 Alt+↑↓ / Ctrl+↑↓
- `crates/atomcode-tuix/src/event_loop/commands.rs` — `/scrollbar` 命令处理
- `crates/atomcode-tuix/src/commands.rs` — 注册 `scrollbar` 命令
- `crates/atomcode-core/src/i18n/messages.rs` — 新 `Msg` 变体
- `crates/atomcode-core/src/i18n/zh_cn.rs` — i18n 文案 + `KeybindingsHelp` 更新
- `crates/atomcode-core/src/i18n/en.rs` — 同上

---

## Phase 0: i18n message variants

### Task 0.1: Add new Msg variants

**Files:**
- Modify: `crates/atomcode-core/src/i18n/messages.rs`
- Modify: `crates/atomcode-core/src/i18n/zh_cn.rs`
- Modify: `crates/atomcode-core/src/i18n/en.rs`

后续 phase 引用 `Msg::ScrollbarOn` / `Msg::ScrollbarOff` / `CmdDescScrollbar`。先添加，避免后面分散加。

- [ ] **Step 1: Add Msg enum variants**

Edit `crates/atomcode-core/src/i18n/messages.rs`, add to the `Msg` enum:

```rust
ScrollbarOn,
ScrollbarOff,
CmdDescScrollbar,
```

- [ ] **Step 2: Add zh_cn translations**

Edit `crates/atomcode-core/src/i18n/zh_cn.rs`, add new arms in the `t()` match:

```rust
Msg::ScrollbarOn => "Scrollbar: ON".into(),
Msg::ScrollbarOff => "Scrollbar: OFF".into(),
Msg::CmdDescScrollbar => "切换右侧滚动条显示".into(),
```

- [ ] **Step 3: Add en translations**

Edit `crates/atomcode-core/src/i18n/en.rs`:

```rust
Msg::ScrollbarOn => "Scrollbar: ON".into(),
Msg::ScrollbarOff => "Scrollbar: OFF".into(),
Msg::CmdDescScrollbar => "Toggle the right-side scrollbar".into(),
```

- [ ] **Step 4: Build to verify**

Run: `cargo check -p atomcode-core`
Expected: clean build, no warnings about non-exhaustive match.

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-core/src/i18n/{messages.rs,zh_cn.rs,en.rs}
git commit -m "i18n: add ScrollbarOn/Off + CmdDescScrollbar messages"
```

---

## Phase 1: Selection 共享模块

### Task 1.1: Create selection.rs skeleton

**Files:**
- Create: `crates/atomcode-tuix/src/render/selection.rs`
- Modify: `crates/atomcode-tuix/src/render/mod.rs` (declare module)

抽 alt-screen 的 selection 代码到独立模块，先建骨架与类型。

- [ ] **Step 1: Create selection.rs with types**

Create `crates/atomcode-tuix/src/render/selection.rs`:

```rust
//! Shared text-selection module used by both AltScreenRenderer and
//! RetainedRenderer. Owns: anchor/head pos, drag tracking, range
//! computation, line rendering with reverse-video highlight, OSC 52
//! emission and arboard fallback for Ctrl+C copy.
//!
//! Each renderer holds a `SelectionState` and implements `BodyLineView`
//! over its native body buffer type (`Vec<String>` for alt-screen,
//! `Vec<Vec<Cell>>` for retained).

use std::borrow::Cow;

/// A single (row, col) cursor position in body_lines coordinates.
/// `row` is the index into body_lines; `col` is display-column.
pub type BodyPos = (usize, u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub anchor: BodyPos,
    pub head: BodyPos,
}

#[derive(Debug, Default)]
pub struct SelectionState {
    pub selection: Option<Selection>,
    pub active: bool,  // true while mouse button held down
}

/// Trait adapter so the selection module can read body content without
/// caring whether the renderer stores `Vec<String>` or `Vec<Vec<Cell>>`.
pub trait BodyLineView {
    fn line_count(&self) -> usize;
    fn line_text(&self, idx: usize) -> Cow<'_, str>;
}

// Impl for the alt-screen body_lines type.
impl BodyLineView for Vec<String> {
    fn line_count(&self) -> usize { self.len() }
    fn line_text(&self, idx: usize) -> Cow<'_, str> {
        Cow::Borrowed(self.get(idx).map(|s| s.as_str()).unwrap_or(""))
    }
}
```

- [ ] **Step 2: Wire module into render/mod.rs**

Edit `crates/atomcode-tuix/src/render/mod.rs`. Find existing `pub mod alt_screen;` block and add nearby:

```rust
pub mod selection;
```

- [ ] **Step 3: Build to verify wiring**

Run: `cargo check -p atomcode-tuix`
Expected: clean build.

- [ ] **Step 4: Commit**

```bash
git add crates/atomcode-tuix/src/render/{mod.rs,selection.rs}
git commit -m "tuix(render): add selection module skeleton + BodyLineView trait"
```

### Task 1.2: Move SGR-aware text helpers + OSC 52 emitter

**Files:**
- Modify: `crates/atomcode-tuix/src/render/selection.rs`
- Modify: `crates/atomcode-tuix/src/render/alt_screen.rs` (delete moved functions)

把 alt_screen.rs 现有的 `line_display_width_sgr_aware`、`extract_line_selection_text`、`render_line_with_selection`、`selection_col_range_for_line` 搬到 selection.rs。**同时移走** `base64_encode`（alt_screen.rs:269）和 `write_osc52_clipboard`（line 953），后者改名为 `pub fn emit_osc52(out: &mut dyn Write, text: &str)` 以便两个 renderer 共用。

- [ ] **Step 1: Find existing functions**

Run:
```bash
grep -nE "^fn line_display_width_sgr_aware|^fn extract_line_selection_text|^fn render_line_with_selection|^fn selection_col_range_for_line|^fn base64_encode|fn write_osc52_clipboard" crates/atomcode-tuix/src/render/alt_screen.rs
```
Expected: 6 line numbers (4 text helpers + base64_encode + write_osc52_clipboard).

- [ ] **Step 2: Copy functions verbatim to selection.rs**

Open `crates/atomcode-tuix/src/render/alt_screen.rs`, copy the body of all 6 functions. Paste into `crates/atomcode-tuix/src/render/selection.rs` after the trait impls. Change `fn` to `pub fn` and adjust any internal `use` paths to point at `crate::width::display_width` etc. For `write_osc52_clipboard`, rename to `emit_osc52` and change signature so `out` is `&mut dyn std::io::Write`:

```rust
pub fn emit_osc52(out: &mut dyn std::io::Write, text: &str) {
    if text.is_empty() { return; }
    let encoded = base64_encode(text.as_bytes());
    let _ = write!(out, "\x1b]52;c;{}\x07", encoded);
    let _ = out.flush();
}
```

Verify imports compile:

Run: `cargo check -p atomcode-tuix`
Expected: probably duplicated symbols error — that's correct, fix in next step.

- [ ] **Step 3: Delete the originals from alt_screen.rs**

Remove the original 6 functions from alt_screen.rs (the 4 text helpers, `base64_encode`, and `write_osc52_clipboard`).

- [ ] **Step 4: Update call sites in alt_screen.rs**

Add `use crate::render::selection::{self, selection_col_range_for_line, render_line_with_selection, extract_line_selection_text, line_display_width_sgr_aware, emit_osc52};` at the top of alt_screen.rs. Replace bare calls with imported names. The previous `self.write_osc52_clipboard(text)` call site (in `end_selection`) becomes `selection::emit_osc52(&mut self.out, text);`.

- [ ] **Step 5: Run alt_screen selection tests**

Run: `cargo test -p atomcode-tuix --lib render::alt_screen::tests:: -- selection 2>&1 | tail -30`
Expected: all selection-related tests pass (`line_display_width_skips_sgr`, `extract_line_selection_strips_sgr_and_clips_to_range`, `render_line_with_selection_emits_reverse_video`, `render_line_with_selection_drops_inline_csi_inside_range`, `render_line_with_empty_selection_is_plain_truncate`, `selection_range_clamps_to_line_width`, `selection_range_multi_line_shape`).

- [ ] **Step 6: Move test bodies to selection.rs**

Move the tests from alt_screen.rs's `tests` module into a new `#[cfg(test)] mod tests` block at the bottom of selection.rs. Adjust import paths.

- [ ] **Step 7: Run shared module tests**

Run: `cargo test -p atomcode-tuix --lib render::selection::tests 2>&1 | tail -30`
Expected: all 7 tests pass.

- [ ] **Step 8: Commit**

```bash
git add crates/atomcode-tuix/src/render/{alt_screen.rs,selection.rs}
git commit -m "tuix(selection): move SGR-aware text helpers + tests to shared module"
```

### Task 1.3: Move SelectionState mouse-handling logic

**Files:**
- Modify: `crates/atomcode-tuix/src/render/selection.rs`
- Modify: `crates/atomcode-tuix/src/render/alt_screen.rs`

把 alt-screen 的 `begin_selection`/`update_selection`/`end_selection`/`copy_selection` 逻辑搬到 `SelectionState` methods，参数化 `BodyLineView`。

- [ ] **Step 1: Locate existing implementations**

Run:
```bash
grep -nE "fn begin_selection|fn update_selection|fn end_selection|fn copy_selection|fn screen_to_body" crates/atomcode-tuix/src/render/alt_screen.rs
```
Expected: 5 line numbers (4 trait impls + 1 helper `screen_to_body`).

- [ ] **Step 2: Add SelectionState methods to selection.rs**

In `selection.rs`, add (assuming you've extracted the original logic; preserve OSC 52 + arboard behavior):

```rust
impl SelectionState {
    /// Start a new selection at body coordinates `pos`.
    pub fn begin(&mut self, pos: BodyPos) {
        self.selection = Some(Selection { anchor: pos, head: pos });
        self.active = true;
    }

    /// Extend selection head to `pos` while button held.
    pub fn update(&mut self, pos: BodyPos) {
        if !self.active { return; }
        if let Some(sel) = self.selection.as_mut() {
            sel.head = pos;
        }
    }

    /// Finalise selection. Returns the selected text if non-empty, so the
    /// caller can emit OSC 52 to the host terminal. Selection state is
    /// preserved so the highlight stays drawn until the next click.
    pub fn end<B: BodyLineView>(&mut self, body: &B) -> Option<String> {
        self.active = false;
        let sel = self.selection.as_ref()?;
        let text = extract_text(body, sel);
        if text.is_empty() { None } else { Some(text) }
    }

    /// Copy current selection to system clipboard via arboard. Returns
    /// true iff a non-empty selection was copied. Clears highlight.
    pub fn copy<B: BodyLineView>(&mut self, body: &B) -> bool {
        let Some(sel) = self.selection else { return false };
        let text = extract_text(body, &sel);
        if text.is_empty() { return false; }
        let copied = match arboard::Clipboard::new() {
            Ok(mut cb) => cb.set_text(text).is_ok(),
            Err(_) => false,
        };
        if copied {
            self.selection = None;
            self.active = false;
        }
        copied
    }

    pub fn clear(&mut self) {
        self.selection = None;
        self.active = false;
    }
}

/// Concatenate the selected text across (possibly multiple) body lines,
/// using the existing per-line range helpers.
fn extract_text<B: BodyLineView>(body: &B, sel: &Selection) -> String {
    let (lo, hi) = ord(sel.anchor, sel.head);
    let mut out = String::new();
    for row in lo.0..=hi.0 {
        let line = body.line_text(row);
        let Some((start, end)) = selection_col_range_for_line(row, lo, hi, &line) else {
            continue;
        };
        if row > lo.0 { out.push('\n'); }
        out.push_str(&extract_line_selection_text(&line, start, end));
    }
    out
}

fn ord(a: BodyPos, b: BodyPos) -> (BodyPos, BodyPos) {
    if a < b { (a, b) } else { (b, a) }
}
```

- [ ] **Step 3: Add unit tests**

In selection.rs `#[cfg(test)] mod tests`:

```rust
#[test]
fn selection_state_begin_sets_anchor_and_active() {
    let mut s = SelectionState::default();
    s.begin((2, 5));
    assert_eq!(s.selection, Some(Selection { anchor: (2, 5), head: (2, 5) }));
    assert!(s.active);
}

#[test]
fn selection_state_update_only_while_active() {
    let mut s = SelectionState::default();
    s.begin((0, 0));
    s.update((1, 4));
    assert_eq!(s.selection.unwrap().head, (1, 4));
    s.active = false;
    s.update((2, 9));
    // head shouldn't change after active = false
    assert_eq!(s.selection.unwrap().head, (1, 4));
}

#[test]
fn selection_state_end_returns_concatenated_text() {
    let body: Vec<String> = vec!["first".into(), "second".into(), "third".into()];
    let mut s = SelectionState::default();
    s.begin((0, 3));
    s.update((2, 2));
    let text = s.end(&body).expect("non-empty");
    // Selection spans (0,3) → (2,2)
    assert_eq!(text, "st\nsecond\nthi");
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p atomcode-tuix --lib render::selection::tests -- 2>&1 | tail -20`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-tuix/src/render/selection.rs
git commit -m "tuix(selection): add SelectionState begin/update/end/copy with tests"
```

### Task 1.4: alt-screen uses shared SelectionState

**Files:**
- Modify: `crates/atomcode-tuix/src/render/alt_screen.rs`

替换 alt-screen 的 `selection: Option<Selection>` + `selection_active: bool` 字段为 `SelectionState`，trait 方法委托。

- [ ] **Step 1: Replace fields**

In `AltScreenRenderer` struct definition, remove:
```rust
selection: Option<Selection>,
selection_active: bool,
```

Add:
```rust
selection: crate::render::selection::SelectionState,
```

Update the `Self { ... }` constructor accordingly (initialize to `SelectionState::default()`). Delete the old `Selection` struct definition in alt_screen.rs (now defined in selection.rs).

- [ ] **Step 2: Update trait method bodies**

Replace `begin_selection` body:
```rust
fn begin_selection(&mut self, col: u16, row: u16) {
    if let Some(pos) = self.screen_to_body(col, row) {
        self.selection.begin(pos);
    } else {
        self.selection.clear();
    }
    self.body_dirty = true;
    self.paint_frame();
}
```

`update_selection`:
```rust
fn update_selection(&mut self, col: u16, row: u16) {
    if let Some(pos) = self.screen_to_body(col, row) {
        self.selection.update(pos);
        self.body_dirty = true;
        self.paint_frame();
    }
}
```

`end_selection`:
```rust
fn end_selection(&mut self) {
    if let Some(text) = self.selection.end(&self.body_lines) {
        crate::render::selection::emit_osc52(&mut self.out, &text);
    }
}
```

`copy_selection`:
```rust
fn copy_selection(&mut self) -> bool {
    let copied = self.selection.copy(&self.body_lines);
    if copied {
        self.body_dirty = true;
        self.paint_frame();
    }
    copied
}
```

Note: preserve the exact OSC 52 wire format from the original implementation. If the original uses a different base64 lib path, mirror it.

- [ ] **Step 3: Update paint_body to use shared range helpers**

In `paint_body`, the existing selection-highlight code probably calls `selection_col_range_for_line` / `render_line_with_selection`. Update the imports to point at `crate::render::selection::*`. Replace `self.selection` reads with `self.selection.selection`.

- [ ] **Step 4: Run full alt-screen tests**

Run: `cargo test -p atomcode-tuix --lib render::alt_screen::tests 2>&1 | tail -30`
Expected: all tests pass (including `multi_line_drag_extracts_across_rows` etc.).

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-tuix/src/render/alt_screen.rs
git commit -m "tuix(alt-screen): delegate selection to shared SelectionState"
```

---

## Phase 2: retained body buffer + MessageMark

### Task 2.1: Extend body_lines cap to 5000

**Files:**
- Modify: `crates/atomcode-tuix/src/render/retained.rs`

把 `height * 4` 的 cap 改为 `MAX_SCROLLBACK_ROWS = 5000` 常量，与 alt-screen 对齐。

- [ ] **Step 1: Write the failing test**

Add to retained.rs `#[cfg(test)] mod tests`:

```rust
#[test]
fn retained_body_lines_cap_is_5000_not_height_times_4() {
    let (mut r, _buf) = new_capturing(80, 24);
    // Push 5050 user lines (use a method that goes through push_body_row).
    for i in 0..5050 {
        r.render(UiLine::User(format!("line {}", i)));
    }
    assert_eq!(r.body_lines.len(), 5000, "body_lines should cap at 5000, got {}", r.body_lines.len());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p atomcode-tuix --lib retained_body_lines_cap_is_5000 2>&1 | tail -10`
Expected: FAIL — current cap is `height * 4 = 96`.

- [ ] **Step 3: Implement constant + replace inline expressions**

Near the top of `retained.rs` (after imports), add:

```rust
/// Max body_lines kept in the in-app scrollback buffer (matches alt-screen).
/// Bounded so memory doesn't grow without limit on long sessions.
pub const MAX_SCROLLBACK_ROWS: usize = 5000;
```

Replace every `(self.screen.height() as usize).saturating_mul(4).max(128)` with `MAX_SCROLLBACK_ROWS`. Confirm with grep:

Run: `grep -nE "saturating_mul\(4\)" crates/atomcode-tuix/src/render/retained.rs`
Expected: no results.

- [ ] **Step 4: Run test**

Run: `cargo test -p atomcode-tuix --lib retained_body_lines_cap_is_5000 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 5: Run full retained test suite**

Run: `cargo test -p atomcode-tuix --lib render::retained::tests 2>&1 | tail -10`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add crates/atomcode-tuix/src/render/retained.rs
git commit -m "tuix(retained): cap body_lines at MAX_SCROLLBACK_ROWS=5000"
```

### Task 2.2: Add MessageMark struct + field

**Files:**
- Modify: `crates/atomcode-tuix/src/render/retained.rs`
- Modify: `crates/atomcode-tuix/src/render/alt_screen.rs`

两个 renderer 都加 `message_marks: Vec<MessageMark>` 字段。共享 `MessageMark` 类型放 `render/mod.rs`。

- [ ] **Step 1: Add types in render/mod.rs**

Add near the top of `crates/atomcode-tuix/src/render/mod.rs` (after the module decls):

```rust
/// Boundary marker for an originated message in the body buffer. Drives
/// "jump to prev/next message" navigation keys. Marked at push time;
/// kept in sync when body_lines drains from the front.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkKind {
    User,
    Assistant,
    ToolCall,
    ToolResult,
}

#[derive(Debug, Clone, Copy)]
pub struct MessageMark {
    pub line_idx: usize,
    pub kind: MarkKind,
}
```

- [ ] **Step 2: Add field to RetainedRenderer**

In `RetainedRenderer<W>` struct definition, add (place near `body_lines`):

```rust
message_marks: Vec<crate::render::MessageMark>,
```

Add to constructor:
```rust
message_marks: Vec::new(),
```

- [ ] **Step 3: Add field to AltScreenRenderer**

Same change in `alt_screen.rs`.

- [ ] **Step 4: Build to verify**

Run: `cargo check -p atomcode-tuix`
Expected: clean build (no usages yet, just field).

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-tuix/src/render/{mod.rs,retained.rs,alt_screen.rs}
git commit -m "tuix(render): add MessageMark type + message_marks field on both renderers"
```

### Task 2.3: Mark messages on push + drain sync (retained)

**Files:**
- Modify: `crates/atomcode-tuix/src/render/retained.rs`

在 retained 的 `render(UiLine)` 的 User/Assistant/ToolCall/ToolResult 分支入口处打标记；body_lines drain front 时同步更新 marks。

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn retained_message_marks_tracked_on_user_push() {
    let (mut r, _buf) = new_capturing(80, 24);
    r.render(UiLine::User("hi".into()));
    assert_eq!(r.message_marks.len(), 1);
    assert_eq!(r.message_marks[0].kind, crate::render::MarkKind::User);
}

#[test]
fn retained_message_marks_decremented_on_drain() {
    let (mut r, _buf) = new_capturing(80, 24);
    // Push 5005 user lines so body_lines drains 5 from front.
    for i in 0..5005 {
        r.render(UiLine::User(format!("line {}", i)));
    }
    // First mark's line_idx should reflect the drain: original idx=0 dropped,
    // remaining marks shifted by 5.
    assert_eq!(r.message_marks.len(), 5000);
    assert_eq!(r.message_marks[0].line_idx, 0, "first surviving mark should point at body_lines[0] after drain");
}
```

- [ ] **Step 2: Run to verify fail**

Run: `cargo test -p atomcode-tuix --lib retained_message_marks 2>&1 | tail -15`
Expected: FAIL — no marks pushed.

- [ ] **Step 3: Add mark_message helper**

In retained.rs impl block:

```rust
fn mark_message(&mut self, kind: crate::render::MarkKind) {
    self.message_marks.push(crate::render::MessageMark {
        line_idx: self.body_lines.len(),
        kind,
    });
}
```

- [ ] **Step 4: Wire mark_message into render(UiLine) branches**

In retained.rs `render(line: UiLine)` (look for the big `match line { UiLine::User(...) => ..., UiLine::AssistantText(...) => ..., UiLine::ToolCall(...) => ..., UiLine::ToolResult(...) => ..., ... }`).

For each branch that starts a new logical message (the existing code likely already has a helper like `push_user_row` / `push_tool_row`), insert a call to `self.mark_message(MarkKind::...)` **before** the first body_lines push of that message.

Specifically:
- `UiLine::User(...)` arm → `self.mark_message(MarkKind::User);` before pushing.
- `UiLine::AssistantText(...)` arm: mark only on the FIRST chunk of a turn. The simplest heuristic: if `message_marks.last()` is not `MarkKind::Assistant` OR if a `UiLine::TurnSeparator` / new turn boundary fired since, push a new mark. Concretely add a `last_mark_was_assistant: bool` (cleared on `UiLine::User` / `UiLine::ToolCall` / `UiLine::TurnSeparator`) and gate mark insertion on it.
- `UiLine::ToolCall(...)` arm → `MarkKind::ToolCall`.
- `UiLine::ToolResult(...)` arm → `MarkKind::ToolResult`.

Mirror the new `last_mark_was_assistant` field in the struct and constructor.

- [ ] **Step 5: Sync drain in push_body_row**

Find the existing `body_lines.drain(0..drain)` in `push_body_row` (around line 1426). Replace with:

```rust
let drain = self.body_lines.len() - MAX_SCROLLBACK_ROWS;
self.body_lines.drain(0..drain);
self.message_marks.retain(|m| m.line_idx >= drain);
for m in self.message_marks.iter_mut() {
    m.line_idx -= drain;
}
```

Do the same in any other place that drains body_lines (search: `grep -nE "body_lines\.drain|body_lines\.remove" retained.rs`).

- [ ] **Step 6: Run tests**

Run: `cargo test -p atomcode-tuix --lib retained_message_marks 2>&1 | tail -15`
Expected: PASS.

- [ ] **Step 7: Run full retained suite**

Run: `cargo test -p atomcode-tuix --lib render::retained::tests 2>&1 | tail -10`
Expected: all pass.

- [ ] **Step 8: Commit**

```bash
git add crates/atomcode-tuix/src/render/retained.rs
git commit -m "tuix(retained): mark message boundaries + sync marks on drain"
```

### Task 2.4: Mark messages on push (alt-screen)

**Files:**
- Modify: `crates/atomcode-tuix/src/render/alt_screen.rs`

镜像 retained 的逻辑到 alt-screen。

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn alt_message_marks_tracked_on_user_push() {
    let mut buf = Vec::new();
    let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
    r.render(UiLine::User("hi".into()));
    assert_eq!(r.message_marks.len(), 1);
    assert_eq!(r.message_marks[0].kind, crate::render::MarkKind::User);
}

#[test]
fn alt_message_marks_decremented_on_drain() {
    let mut buf = Vec::new();
    let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 24);
    for i in 0..5005 {
        r.render(UiLine::User(format!("line {}", i)));
    }
    assert_eq!(r.message_marks.len(), 5000);
    assert_eq!(r.message_marks[0].line_idx, 0);
}
```

- [ ] **Step 2: Verify fail**

Run: `cargo test -p atomcode-tuix --lib alt_message_marks 2>&1 | tail -15`
Expected: FAIL.

- [ ] **Step 3: Add mark_message + wiring**

Mirror Phase 2.3 in alt_screen.rs. Same `mark_message` helper, same branch insertions. The drain location in alt_screen.rs is in `push_body_row` (line ~740 `body_lines.remove(0)`); convert to:

```rust
while self.body_lines.len() > self.max_scrollback_rows {
    self.body_lines.remove(0);
    self.message_marks.retain(|m| m.line_idx > 0);
    for m in self.message_marks.iter_mut() {
        m.line_idx -= 1;
    }
}
```

(`remove(0)` per row keeps the existing logic; drain semantics identical.)

Also adjust `reflow_body_lines` drain block at the bottom of that function with the same retain+shift.

- [ ] **Step 4: Verify tests pass**

Run: `cargo test -p atomcode-tuix --lib message_marks 2>&1 | tail -15`
Expected: both retained and alt tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-tuix/src/render/alt_screen.rs
git commit -m "tuix(alt-screen): mirror MessageMark push + drain sync from retained"
```

---

## Phase 3: retained view_mode state machine + scroll

### Task 3.1: Add view_mode + viewport_top + sticky_bottom fields

**Files:**
- Modify: `crates/atomcode-tuix/src/render/retained.rs`

- [ ] **Step 1: Add fields**

In `RetainedRenderer<W>`:

```rust
/// True iff user has scrolled away from the tail. While true, body
/// emit suppresses terminal writes and paint_body redraws from
/// body_lines[viewport_top..] via CUP+EL instead of DECSTBM \n.
view_mode: bool,
/// Top body_lines index visible at body region top, when view_mode = true.
viewport_top: usize,
/// True iff viewport_top >= max_top (auto-tail). Drives view_mode entry/exit.
sticky_bottom: bool,
```

Constructor: `view_mode: false, viewport_top: 0, sticky_bottom: true,`.

- [ ] **Step 2: Build to verify**

Run: `cargo check -p atomcode-tuix`
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add crates/atomcode-tuix/src/render/retained.rs
git commit -m "tuix(retained): add view_mode/viewport_top/sticky_bottom state fields"
```

### Task 3.2: Implement scroll_body + variants

**Files:**
- Modify: `crates/atomcode-tuix/src/render/retained.rs`

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn retained_scroll_up_enters_view_mode() {
    let (mut r, _buf) = new_capturing(80, 24);
    for i in 0..30 {
        r.render(UiLine::User(format!("L{}", i)));
    }
    assert!(r.sticky_bottom);
    assert!(!r.view_mode);
    r.scroll_body(-3);
    assert!(r.view_mode, "scroll up must enter view_mode");
    assert!(!r.sticky_bottom);
}

#[test]
fn retained_scroll_to_bottom_exits_view_mode() {
    let (mut r, _buf) = new_capturing(80, 24);
    for i in 0..30 {
        r.render(UiLine::User(format!("L{}", i)));
    }
    r.scroll_body(-5);
    assert!(r.view_mode);
    r.scroll_body_to_bottom();
    assert!(!r.view_mode);
    assert!(r.sticky_bottom);
}

#[test]
fn retained_scroll_up_then_to_top_lands_at_zero() {
    let (mut r, _buf) = new_capturing(80, 24);
    for i in 0..30 {
        r.render(UiLine::User(format!("L{}", i)));
    }
    r.scroll_body_to_top();
    assert_eq!(r.viewport_top, 0);
    assert!(r.view_mode);
}
```

- [ ] **Step 2: Verify fail**

Run: `cargo test -p atomcode-tuix --lib retained_scroll 2>&1 | tail -15`
Expected: FAIL.

- [ ] **Step 3: Implement scroll_body**

Inside `impl<W> Renderer for RetainedRenderer<W>` (find the existing trait impl block), add:

```rust
fn scroll_body(&mut self, delta: i32) {
    let body_height = self.body_bottom_row() as usize;
    let total = self.body_lines.len();
    let max_top = total.saturating_sub(body_height);
    if max_top == 0 {
        // nothing to scroll; stay sticky
        self.sticky_bottom = true;
        self.view_mode = false;
        return;
    }
    let current_top = if self.sticky_bottom { max_top } else { self.viewport_top };
    let new_top: usize = if delta < 0 {
        current_top.saturating_sub(delta.unsigned_abs() as usize)
    } else {
        (current_top + delta as usize).min(max_top)
    };
    self.viewport_top = new_top;
    self.sticky_bottom = new_top >= max_top;
    let was_view = self.view_mode;
    self.view_mode = !self.sticky_bottom;
    // Trigger paint. When transitioning out of view_mode (was_view=true,
    // view_mode=false), the next paint_body must repaint the body tail
    // without a `\n` scroll (handled in Task 3.5).
    if was_view != self.view_mode || self.view_mode {
        // mark body dirty; concrete paint happens via existing render path
        // Use the renderer's standard "redraw body region" mechanism. If
        // retained currently invalidates via a screen-level dirty flag,
        // call that. If it lacks one, force a body repaint here.
        self.repaint_body_region();
    }
}

fn scroll_body_to_top(&mut self) {
    let body_height = self.body_bottom_row() as usize;
    let total = self.body_lines.len();
    if total <= body_height {
        return;
    }
    self.viewport_top = 0;
    self.sticky_bottom = false;
    self.view_mode = true;
    self.repaint_body_region();
}

fn scroll_body_to_bottom(&mut self) {
    let was_view = self.view_mode;
    self.viewport_top = self.body_lines.len().saturating_sub(self.body_bottom_row() as usize);
    self.sticky_bottom = true;
    self.view_mode = false;
    if was_view {
        // Exiting view: repaint body tail without LF (Task 3.5).
        self.repaint_body_region();
    }
}
```

Also add the helper:

```rust
/// Force a fresh paint of body region rows from body_lines.
/// In view_mode: paint body_lines[viewport_top..viewport_top+body_height].
/// Out of view_mode (just exited): paint body_lines tail.
/// Always uses CUP+EL+content per row; never emits LF.
fn repaint_body_region(&mut self) {
    let bottom = self.body_bottom_row();
    if bottom == 0 || self.body_lines.is_empty() { return; }
    let body_height = bottom as usize;
    let total = self.body_lines.len();
    let start = if self.view_mode {
        self.viewport_top.min(total.saturating_sub(1))
    } else {
        total.saturating_sub(body_height)
    };
    let end = (start + body_height).min(total);
    for (i, row) in self.body_lines[start..end].iter().enumerate() {
        let target_row = 1 + i as u16;
        let seq = format!("\x1b[{};1H\x1b[K", target_row);
        let _ = self.out.write_all(seq.as_bytes());
        let bytes = serialize_row(row);
        let _ = self.out.write_all(&bytes);
    }
    // Clear any rows below content (when body_lines is short).
    for i in (end - start)..body_height {
        let target_row = 1 + i as u16;
        let seq = format!("\x1b[{};1H\x1b[K", target_row);
        let _ = self.out.write_all(seq.as_bytes());
    }
    let _ = self.out.flush();
    // Cursor must return to footer's input row — caller / existing paint
    // chain will re-anchor on next footer paint.
    self.screen.invalidate();
}
```

If retained's existing `screen` cell-diff cache complicates this, an alternative is to just set `self.body_dirty = true` (if such a field exists) and call the existing paint path. Either is fine — choose whatever fits the existing patterns in retained.rs.

- [ ] **Step 4: Run tests**

Run: `cargo test -p atomcode-tuix --lib retained_scroll 2>&1 | tail -15`
Expected: PASS.

- [ ] **Step 5: Run full retained suite**

Run: `cargo test -p atomcode-tuix --lib render::retained::tests 2>&1 | tail -10`
Expected: all pass (no regression).

- [ ] **Step 6: Commit**

```bash
git add crates/atomcode-tuix/src/render/retained.rs
git commit -m "tuix(retained): implement scroll_body + scroll_body_to_top/bottom"
```

### Task 3.3: emit_body_line_inner suppresses writes in view_mode

**Files:**
- Modify: `crates/atomcode-tuix/src/render/retained.rs`

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn retained_view_mode_suppresses_terminal_writes() {
    let (mut r, buf) = new_capturing(80, 24);
    // Get into view_mode
    for i in 0..30 {
        r.render(UiLine::User(format!("L{}", i)));
    }
    r.scroll_body(-5);
    assert!(r.view_mode);
    let bytes_before = buf.lock().unwrap().len();
    // Push more content; terminal write count should not grow (view paint
    // is idempotent and we already painted in scroll_body).
    r.render(UiLine::User("after view".into()));
    // Snapshot to drop the lock before further mutation
    let new_bytes = buf.lock().unwrap()[bytes_before..].to_vec();
    let s = String::from_utf8_lossy(&new_bytes);
    assert!(!s.contains('\n'), "view_mode must NOT emit \\n scroll: {:?}", s);
    // body_lines should still grow.
    let non_empty = r.body_lines.iter().filter(|row| !row.is_empty()).count();
    assert!(non_empty >= 31, "expected body_lines to keep growing in view_mode, got {}", non_empty);
}
```

- [ ] **Step 2: Verify fail**

Run: `cargo test -p atomcode-tuix --lib retained_view_mode_suppresses 2>&1 | tail -10`
Expected: FAIL — current emit always writes.

- [ ] **Step 3: Fork emit_body_line_inner**

Find `emit_body_line_inner` (line ~1324). Add an early-return at the top:

```rust
fn emit_body_line_inner(&mut self, row: &[Cell], bottom: u16) {
    if self.view_mode {
        // In view_mode the body_lines buffer is the source of truth and
        // paint_body repaints from buffer. Don't write to terminal here —
        // we'd overwrite scrolled-away content.
        return;
    }
    // ... existing implementation unchanged
}
```

Note: pushing the row to `body_lines` happens in the *caller* (`push_body_row`), so the row will still be buffered. The terminal write is what we skip.

- [ ] **Step 4: Run test**

Run: `cargo test -p atomcode-tuix --lib retained_view_mode_suppresses 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 5: Run full suite**

Run: `cargo test -p atomcode-tuix --lib render::retained::tests 2>&1 | tail -10`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add crates/atomcode-tuix/src/render/retained.rs
git commit -m "tuix(retained): suppress terminal writes in emit_body_line_inner while view_mode"
```

### Task 3.4: Force exit view_mode on reset / clear / resize / approval

**Files:**
- Modify: `crates/atomcode-tuix/src/render/retained.rs`

- [ ] **Step 1: Write tests**

```rust
#[test]
fn retained_reset_clears_view_mode() {
    let (mut r, _buf) = new_capturing(80, 24);
    for i in 0..30 { r.render(UiLine::User(format!("L{}", i))); }
    r.scroll_body(-5);
    assert!(r.view_mode);
    r.reset();
    assert!(!r.view_mode);
    assert!(r.sticky_bottom);
}

#[test]
fn retained_resize_clears_view_mode() {
    let (mut r, _buf) = new_capturing(80, 24);
    for i in 0..30 { r.render(UiLine::User(format!("L{}", i))); }
    r.scroll_body(-5);
    assert!(r.view_mode);
    r.on_resize(100, 30);
    assert!(!r.view_mode);
}
```

- [ ] **Step 2: Verify fail**

Run: `cargo test -p atomcode-tuix --lib "retained_reset_clears_view|retained_resize_clears_view" 2>&1 | tail -10`
Expected: FAIL.

- [ ] **Step 3: Add force-exit helper + invoke from reset/clear/resize/approval**

Add helper:
```rust
fn exit_view_mode(&mut self) {
    if self.view_mode {
        self.view_mode = false;
        self.sticky_bottom = true;
        self.viewport_top = 0;
    }
}
```

Call `self.exit_view_mode();` at the start of:
- `fn reset(&mut self)` 
- `fn clear_screen(&mut self)` (if separate from reset)
- `fn on_resize(&mut self, ...)`
- The approval-prompt arm in `render(UiLine)` for `UiLine::ApprovalPrompt` (or whatever the existing variant is — grep for `ApprovalPrompt` in retained.rs and find the push arm)

- [ ] **Step 4: Add approval test**

```rust
#[test]
fn retained_approval_prompt_forces_view_exit() {
    let (mut r, _buf) = new_capturing(80, 24);
    for i in 0..30 { r.render(UiLine::User(format!("L{}", i))); }
    r.scroll_body(-5);
    assert!(r.view_mode);
    // Push an approval prompt — uses whatever UiLine variant exists.
    r.render(UiLine::ApprovalPrompt {
        // Fill in fields per the actual UiLine::ApprovalPrompt definition;
        // grep `grep -nE "ApprovalPrompt" crates/atomcode-tuix/src/render/mod.rs`
        // to find the exact shape.
        tool: "Bash".into(),
        detail: "ls".into(),
    });
    assert!(!r.view_mode, "approval prompt must force exit from view_mode");
}
```

If the actual `UiLine::ApprovalPrompt` shape differs, adjust the test to the real fields.

- [ ] **Step 5: Run tests**

Run: `cargo test -p atomcode-tuix --lib "retained_reset_clears_view|retained_resize_clears_view|retained_approval_prompt_forces_view" 2>&1 | tail -15`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/atomcode-tuix/src/render/retained.rs
git commit -m "tuix(retained): force exit view_mode on reset/clear/resize/approval"
```

---

## Phase 4: retained mouse capture

### Task 4.1: Emit ?1002h ?1006h at startup + ?1002l ?1006l on shutdown

**Files:**
- Modify: `crates/atomcode-tuix/src/render/retained.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn retained_with_writer_enables_mouse_capture() {
    let mut buf = Vec::new();
    let _r = RetainedRenderer::with_writer(&mut buf, caps_with_color(), 80, 24);
    let s = String::from_utf8_lossy(&buf);
    assert!(s.contains("\x1b[?1002h"), "must enable button-event tracking: {:?}", s);
    assert!(s.contains("\x1b[?1006h"), "must enable SGR coordinates: {:?}", s);
}

#[test]
fn retained_shutdown_disables_mouse_capture() {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let sink = CapturingSink::new(buf.clone());
    let mut r = RetainedRenderer::with_writer(sink, caps_with_color(), 80, 24);
    // clear startup bytes
    buf.lock().unwrap().clear();
    r.shutdown();
    let bytes = buf.lock().unwrap().clone();
    let s = String::from_utf8_lossy(&bytes);
    assert!(s.contains("\x1b[?1002l"), "shutdown must disable button-event: {:?}", s);
    assert!(s.contains("\x1b[?1006l"), "shutdown must disable SGR coords: {:?}", s);
}
```

- [ ] **Step 2: Verify fail**

Run: `cargo test -p atomcode-tuix --lib "retained_with_writer_enables_mouse|retained_shutdown_disables_mouse" 2>&1 | tail -15`
Expected: FAIL.

- [ ] **Step 3: Update with_writer**

Locate the `with_writer` constructor (line ~385). Find the existing `out.write_all(b"\x1b[3J")` line. Change to:

```rust
let _ = out.write_all(b"\x1b[3J\x1b[?1002h\x1b[?1006h");
let _ = out.flush();
```

- [ ] **Step 4: Update shutdown**

Locate `fn shutdown(&mut self)` (search: `grep -nE "fn shutdown" crates/atomcode-tuix/src/render/retained.rs`). At the start (or wherever existing cleanup happens), prepend:

```rust
let _ = self.out.write_all(b"\x1b[?1006l\x1b[?1002l");
let _ = self.out.flush();
```

- [ ] **Step 5: Update Drop impl**

Find `impl<W> Drop for RetainedRenderer<W>` (if exists; otherwise add to shutdown only). Mirror the same disable sequence in Drop as belt-and-suspenders for panic paths.

- [ ] **Step 6: Run tests**

Run: `cargo test -p atomcode-tuix --lib "retained_with_writer_enables_mouse|retained_shutdown_disables_mouse" 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 7: Run full retained suite**

Run: `cargo test -p atomcode-tuix --lib render::retained::tests 2>&1 | tail -10`
Expected: all pass.

- [ ] **Step 8: Commit**

```bash
git add crates/atomcode-tuix/src/render/retained.rs
git commit -m "tuix(retained): enable button-event + SGR mouse capture at startup"
```

### Task 4.2: Suspend/resume mouse capture for external children

**Files:**
- Modify: `crates/atomcode-tuix/src/render/retained.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn retained_suspend_disables_mouse_capture() {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let sink = CapturingSink::new(buf.clone());
    let mut r = RetainedRenderer::with_writer(sink, caps_with_color(), 80, 24);
    buf.lock().unwrap().clear();
    r.suspend_for_external();
    let s = String::from_utf8_lossy(&buf.lock().unwrap());
    assert!(s.contains("\x1b[?1006l"), "suspend must disable SGR: {:?}", s);
    assert!(s.contains("\x1b[?1002l"), "suspend must disable button-event: {:?}", s);
}

#[test]
fn retained_resume_reenables_mouse_capture() {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let sink = CapturingSink::new(buf.clone());
    let mut r = RetainedRenderer::with_writer(sink, caps_with_color(), 80, 24);
    r.suspend_for_external();
    buf.lock().unwrap().clear();
    r.resume_from_external();
    let s = String::from_utf8_lossy(&buf.lock().unwrap());
    assert!(s.contains("\x1b[?1002h"), "resume must re-enable button-event: {:?}", s);
    assert!(s.contains("\x1b[?1006h"), "resume must re-enable SGR: {:?}", s);
}
```

- [ ] **Step 2: Verify fail**

Run: `cargo test -p atomcode-tuix --lib "retained_suspend_disables_mouse|retained_resume_reenables_mouse" 2>&1 | tail -10`
Expected: FAIL.

- [ ] **Step 3: Implement**

In `suspend_for_external` (line ~2985), find the existing cleanup block (raw_mode, bracketed paste, Kitty enhancement). Prepend a mouse-disable write:

```rust
let _ = self.out.write_all(b"\x1b[?1006l\x1b[?1002l");
// ... existing code follows
```

In `resume_from_external`, after the existing re-enable block, append:

```rust
let _ = self.out.write_all(b"\x1b[?1002h\x1b[?1006h");
let _ = self.out.flush();
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p atomcode-tuix --lib "retained_suspend_disables_mouse|retained_resume_reenables_mouse" 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-tuix/src/render/retained.rs
git commit -m "tuix(retained): pop/repush mouse capture in suspend/resume_for_external"
```

### Task 4.3: Windows conhost mouse capture parity

**Files:**
- Modify: `crates/atomcode-tuix/src/render/retained.rs`

复用 alt-screen 的 `enable_conhost_mouse_capture()` 和 `restore_conhost_console_in_mode()`。

- [ ] **Step 1: Locate alt-screen Windows helpers**

Run:
```bash
grep -nE "fn enable_conhost_mouse_capture|fn restore_conhost_console_in_mode|prior_console_in_mode" crates/atomcode-tuix/src/render/alt_screen.rs | head -5
```
Expected: definitions on alt_screen.rs.

- [ ] **Step 2: Hoist helpers to a Windows-only module**

Create `crates/atomcode-tuix/src/render/conhost.rs` (windows-only):

```rust
//! Windows conhost mouse capture helpers, used by both AltScreenRenderer
//! and RetainedRenderer to set/clear `ENABLE_MOUSE_INPUT` while
//! preserving the pre-enter console mode.

#![cfg(windows)]

// Move the existing enable_conhost_mouse_capture + restore_conhost_console_in_mode
// + any associated constants from alt_screen.rs verbatim. Mark them `pub`.

pub fn enable_conhost_mouse_capture() -> Option<u32> {
    // ... existing alt_screen.rs body ...
}

pub fn restore_conhost_console_in_mode(prior: u32) {
    // ... existing alt_screen.rs body ...
}
```

Declare module in `crates/atomcode-tuix/src/render/mod.rs`:

```rust
#[cfg(windows)]
pub mod conhost;
```

- [ ] **Step 3: Update alt_screen.rs to use the shared module**

Replace local calls with `crate::render::conhost::enable_conhost_mouse_capture()` etc. Delete the local definitions.

- [ ] **Step 4: Add Windows field + invocations in retained.rs**

In `RetainedRenderer<W>`:

```rust
#[cfg(windows)]
prior_console_in_mode: Option<u32>,
```

Constructor: `#[cfg(windows)] prior_console_in_mode: None,`.

In `with_writer`, after the `\x1b[3J\x1b[?1002h\x1b[?1006h` write:

```rust
#[cfg(windows)]
let prior_console_in_mode = crate::render::conhost::enable_conhost_mouse_capture();
```

Set the field in `Self { ... #[cfg(windows)] prior_console_in_mode, ... }`.

In `suspend_for_external`:

```rust
#[cfg(windows)]
if let Some(prior) = self.prior_console_in_mode.take() {
    crate::render::conhost::restore_conhost_console_in_mode(prior);
}
```

In `resume_from_external`:

```rust
#[cfg(windows)] {
    self.prior_console_in_mode = crate::render::conhost::enable_conhost_mouse_capture();
}
```

In `shutdown` and Drop:

```rust
#[cfg(windows)]
if let Some(prior) = self.prior_console_in_mode.take() {
    crate::render::conhost::restore_conhost_console_in_mode(prior);
}
```

- [ ] **Step 5: Build check (cross-platform)**

Run: `cargo check -p atomcode-tuix`
Expected: clean on macOS/Linux (the `#[cfg(windows)]` blocks compile out).

If you have a Windows environment, also run `cargo check --target x86_64-pc-windows-msvc -p atomcode-tuix` (or equivalent).

- [ ] **Step 6: Run full alt-screen + retained tests**

Run: `cargo test -p atomcode-tuix --lib 2>&1 | tail -10`
Expected: all pass.

- [ ] **Step 7: Commit**

```bash
git add crates/atomcode-tuix/src/render/{mod.rs,conhost.rs,alt_screen.rs,retained.rs}
git commit -m "tuix: hoist conhost mouse-capture helpers into shared module; retained uses them"
```

---

## Phase 5: retained selection wiring

### Task 5.1: impl BodyLineView for Vec<Vec<Cell>>

**Files:**
- Modify: `crates/atomcode-tuix/src/render/selection.rs`

- [ ] **Step 1: Add the impl**

In `selection.rs`:

```rust
use crate::render::cell::Cell;

impl BodyLineView for Vec<Vec<Cell>> {
    fn line_count(&self) -> usize { self.len() }
    fn line_text(&self, idx: usize) -> Cow<'_, str> {
        let Some(row) = self.get(idx) else { return Cow::Borrowed(""); };
        // Build a visible-text string from cells; skip continuation cells
        // (width == 0) which are placeholders for the 2nd column of a wide glyph.
        let s: String = row.iter().filter(|c| c.width > 0).map(|c| c.ch).collect();
        Cow::Owned(s)
    }
}
```

- [ ] **Step 2: Add test**

```rust
#[cfg(test)]
mod cell_view_tests {
    use super::*;
    use crate::render::cell::{Cell, CellStyle};

    #[test]
    fn vec_vec_cell_line_text_extracts_visible_chars() {
        let row = vec![
            Cell { ch: 'h', style: CellStyle::default(), width: 1 },
            Cell { ch: 'i', style: CellStyle::default(), width: 1 },
            Cell { ch: '中', style: CellStyle::default(), width: 2 },
            Cell { ch: ' ', style: CellStyle::default(), width: 0 }, // continuation
        ];
        let body: Vec<Vec<Cell>> = vec![row];
        assert_eq!(body.line_text(0), "hi中");
    }
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p atomcode-tuix --lib render::selection 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/atomcode-tuix/src/render/selection.rs
git commit -m "tuix(selection): impl BodyLineView for Vec<Vec<Cell>> (retained body type)"
```

### Task 5.2: Add SelectionState field + trait methods to retained

**Files:**
- Modify: `crates/atomcode-tuix/src/render/retained.rs`

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn retained_begin_selection_records_anchor() {
    let (mut r, _buf) = new_capturing(80, 24);
    for i in 0..5 { r.render(UiLine::User(format!("L{}", i))); }
    r.begin_selection(3, 1);
    assert!(r.selection.selection.is_some());
}

#[test]
fn retained_copy_selection_writes_clipboard() {
    let (mut r, _buf) = new_capturing(80, 24);
    r.render(UiLine::User("hello world".into()));
    // Anchor at body row 0 col 0, head at col 5.
    r.selection.begin((0, 0));
    r.selection.update((0, 5));
    assert!(r.copy_selection(), "expected non-empty selection to copy");
}
```

- [ ] **Step 2: Verify fail**

Run: `cargo test -p atomcode-tuix --lib "retained_begin_selection_records|retained_copy_selection_writes" 2>&1 | tail -10`
Expected: FAIL.

- [ ] **Step 3: Add field**

In `RetainedRenderer<W>`:

```rust
selection: crate::render::selection::SelectionState,
```

Constructor: `selection: Default::default(),`.

- [ ] **Step 4: Implement trait methods**

In `impl<W> Renderer for RetainedRenderer<W>`:

```rust
fn begin_selection(&mut self, col: u16, row: u16) {
    if let Some(pos) = self.screen_to_body(col, row) {
        self.selection.begin(pos);
        self.repaint_body_region();
    } else {
        self.selection.clear();
    }
}

fn update_selection(&mut self, col: u16, row: u16) {
    if let Some(pos) = self.screen_to_body(col, row) {
        self.selection.update(pos);
        self.repaint_body_region();
    }
}

fn end_selection(&mut self) {
    if let Some(text) = self.selection.end(&self.body_lines) {
        crate::render::selection::emit_osc52(&mut self.out, &text);
    }
}

fn copy_selection(&mut self) -> bool {
    let copied = self.selection.copy(&self.body_lines);
    if copied { self.repaint_body_region(); }
    copied
}
```

- [ ] **Step 5: Add screen_to_body helper**

Mirror alt-screen's `fn screen_to_body(&self, col: u16, row: u16) -> Option<(usize, u16)>` for retained. In retained's case, the body region is rows `1..=body_bottom_row()`. The function converts screen coordinates → body_lines index. If `view_mode`, the index is `viewport_top + (row - 1)`; otherwise it's the tail-relative index.

```rust
fn screen_to_body(&self, col: u16, row: u16) -> Option<(usize, u16)> {
    let bottom = self.body_bottom_row();
    if row == 0 || row > bottom { return None; }
    let body_height = bottom as usize;
    let total = self.body_lines.len();
    let viewport_start = if self.view_mode {
        self.viewport_top
    } else {
        total.saturating_sub(body_height)
    };
    let body_row = viewport_start + (row - 1) as usize;
    if body_row >= total { return None; }
    Some((body_row, col))
}
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p atomcode-tuix --lib "retained_begin_selection_records|retained_copy_selection_writes" 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/atomcode-tuix/src/render/retained.rs
git commit -m "tuix(retained): wire SelectionState begin/update/end/copy via trait"
```

### Task 5.3: Apply selection highlight in retained paint_body

**Files:**
- Modify: `crates/atomcode-tuix/src/render/retained.rs`

退出 view_mode 后 `repaint_body_region` 当前没有应用 selection 高亮。要让选中范围用反色显示。

- [ ] **Step 1: Update repaint_body_region**

Modify the `repaint_body_region` body to apply selection per row:

```rust
fn repaint_body_region(&mut self) {
    let bottom = self.body_bottom_row();
    if bottom == 0 || self.body_lines.is_empty() { return; }
    let body_height = bottom as usize;
    let total = self.body_lines.len();
    let start = if self.view_mode {
        self.viewport_top.min(total.saturating_sub(1))
    } else {
        total.saturating_sub(body_height)
    };
    let end = (start + body_height).min(total);
    use crate::render::selection::{selection_col_range_for_line};
    let sel = self.selection.selection.map(|s| (
        if s.anchor < s.head { (s.anchor, s.head) } else { (s.head, s.anchor) }
    ));
    for (i, row) in self.body_lines[start..end].iter().enumerate() {
        let target_row = 1 + i as u16;
        let seq = format!("\x1b[{};1H\x1b[K", target_row);
        let _ = self.out.write_all(seq.as_bytes());
        let body_idx = start + i;
        let row_text: String = row.iter().filter(|c| c.width > 0).map(|c| c.ch).collect();
        // If selection covers this row, emit with reverse-video using
        // the shared helper. Otherwise emit normally via serialize_row.
        let sel_range = sel.and_then(|(lo, hi)| selection_col_range_for_line(body_idx, lo, hi, &row_text));
        if let Some((sel_start, sel_end)) = sel_range {
            let highlighted = crate::render::selection::render_line_with_selection(&row_text, self.screen.width(), sel_start, sel_end);
            let _ = self.out.write_all(highlighted.as_bytes());
        } else {
            let bytes = serialize_row(row);
            let _ = self.out.write_all(&bytes);
        }
    }
    for i in (end - start)..body_height {
        let target_row = 1 + i as u16;
        let seq = format!("\x1b[{};1H\x1b[K", target_row);
        let _ = self.out.write_all(seq.as_bytes());
    }
    let _ = self.out.flush();
    self.screen.invalidate();
}
```

- [ ] **Step 2: Add test**

```rust
#[test]
fn retained_selection_highlight_emits_reverse_video() {
    let (mut r, buf) = new_capturing(80, 24);
    r.render(UiLine::User("hello world".into()));
    buf.lock().unwrap().clear();
    // Force view_mode so repaint_body_region path executes
    r.scroll_body(-1);
    r.selection.begin((0, 0));
    r.selection.update((0, 5));
    r.repaint_body_region();
    let s = String::from_utf8_lossy(&buf.lock().unwrap());
    assert!(s.contains("\x1b[7m"), "selection paint must include reverse-video SGR: {:?}", s);
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p atomcode-tuix --lib retained_selection_highlight 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/atomcode-tuix/src/render/retained.rs
git commit -m "tuix(retained): apply selection highlight via shared render_line_with_selection"
```

---

## Phase 6: scrollbar + /scrollbar command + ui-state.toml

### Task 6.1: Create ui_state.rs persistence

**Files:**
- Create: `crates/atomcode-tuix/src/render/ui_state.rs`
- Modify: `crates/atomcode-tuix/src/render/mod.rs`

- [ ] **Step 1: Write failing test**

Create `crates/atomcode-tuix/src/render/ui_state.rs`:

```rust
//! UI state persisted between sessions. Currently: scrollbar visibility.
//! Stored at `$ATOMCODE_HOME/ui-state.toml`. Load/save are best-effort —
//! missing file or parse error returns default (everything false).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UiState {
    #[serde(default)]
    pub ui: UiSection,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UiSection {
    #[serde(default)]
    pub show_scrollbar: bool,
}

fn ui_state_path() -> Option<PathBuf> {
    let home = std::env::var_os("ATOMCODE_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".atomcode")))?;
    Some(home.join("ui-state.toml"))
}

pub fn load() -> UiState {
    let Some(path) = ui_state_path() else { return UiState::default(); };
    let Ok(text) = std::fs::read_to_string(&path) else { return UiState::default(); };
    toml::from_str(&text).unwrap_or_default()
}

pub fn save(state: &UiState) {
    let Some(path) = ui_state_path() else { return; };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(text) = toml::to_string(state) else {
        crate::tuix_trace!("UI", "ui-state serialize failed");
        return;
    };
    if let Err(e) = std::fs::write(&path, text) {
        crate::tuix_trace!("UI", "ui-state write failed: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use tempfile::TempDir;

    #[test]
    fn ui_state_round_trip_via_atomcode_home() {
        let td = TempDir::new().unwrap();
        env::set_var("ATOMCODE_HOME", td.path());
        let mut s = UiState::default();
        s.ui.show_scrollbar = true;
        save(&s);
        let loaded = load();
        assert!(loaded.ui.show_scrollbar);
    }

    #[test]
    fn ui_state_missing_file_returns_default() {
        let td = TempDir::new().unwrap();
        env::set_var("ATOMCODE_HOME", td.path());
        let loaded = load();
        assert!(!loaded.ui.show_scrollbar);
    }
}
```

Declare in `render/mod.rs`:
```rust
pub mod ui_state;
```

Check `Cargo.toml` for `tempfile` dev-dep — likely already present; if not, add `tempfile = "3"` to `[dev-dependencies]`.

- [ ] **Step 2: Verify dependencies**

Run: `grep -nE "^(toml|dirs|serde)" crates/atomcode-tuix/Cargo.toml`
Expected: all present (serde + toml are pervasive; dirs likely present too). Add any missing.

- [ ] **Step 3: Run test**

Run: `cargo test -p atomcode-tuix --lib render::ui_state 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/atomcode-tuix/src/render/{mod.rs,ui_state.rs} crates/atomcode-tuix/Cargo.toml
git commit -m "tuix(ui_state): persist UI prefs to \$ATOMCODE_HOME/ui-state.toml"
```

### Task 6.2: Create scrollbar.rs helper

**Files:**
- Create: `crates/atomcode-tuix/src/render/scrollbar.rs`
- Modify: `crates/atomcode-tuix/src/render/mod.rs`

- [ ] **Step 1: Write failing test + module skeleton**

Create `crates/atomcode-tuix/src/render/scrollbar.rs`:

```rust
//! Pure compute for the right-edge scrollbar. Both renderers call into
//! `compute()` to decide thumb shape, then call `paint_row(...)` to emit
//! a single column's worth of cells per body row.

/// Vertical thumb position + height in body-region coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollbarShape {
    pub thumb_top: usize,     // 0-indexed body row
    pub thumb_height: usize,
}

/// Returns None when no thumb should be drawn (no overflow or disabled).
pub fn compute(
    total: usize,
    visible: usize,
    viewport_top: usize,
    sticky_bottom: bool,
    show: bool,
) -> Option<ScrollbarShape> {
    if !show || total <= visible || visible == 0 {
        return None;
    }
    let max_top = total - visible;
    let effective_top = if sticky_bottom { max_top } else { viewport_top };
    let thumb_h = ((visible * visible) / total).max(1);
    let track_avail = visible.saturating_sub(thumb_h);
    let thumb_top = if max_top == 0 {
        0
    } else {
        effective_top * track_avail / max_top
    };
    Some(ScrollbarShape { thumb_top, thumb_height: thumb_h })
}

/// Whether the given body row index (0..visible) should paint a thumb char.
pub fn is_thumb_row(shape: &ScrollbarShape, body_row: usize) -> bool {
    body_row >= shape.thumb_top && body_row < shape.thumb_top + shape.thumb_height
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_returns_none_when_no_overflow() {
        assert!(compute(10, 20, 0, true, true).is_none());
        assert!(compute(20, 20, 0, true, true).is_none());
    }

    #[test]
    fn compute_returns_none_when_disabled() {
        assert!(compute(50, 10, 5, false, false).is_none());
    }

    #[test]
    fn compute_thumb_height_proportional_to_visible_over_total() {
        let s = compute(30, 10, 0, true, true).unwrap();
        // 10 * 10 / 30 = 3
        assert_eq!(s.thumb_height, 3);
    }

    #[test]
    fn compute_thumb_at_bottom_when_sticky() {
        let s = compute(30, 10, 0, true, true).unwrap();
        // sticky_bottom => effective_top = max_top = 20
        // thumb_top = 20 * (10 - 3) / 20 = 7
        assert_eq!(s.thumb_top, 7);
    }

    #[test]
    fn compute_thumb_at_top_when_viewport_top_zero() {
        let s = compute(30, 10, 0, false, true).unwrap();
        assert_eq!(s.thumb_top, 0);
    }

    #[test]
    fn is_thumb_row_covers_thumb_range() {
        let shape = ScrollbarShape { thumb_top: 3, thumb_height: 4 };
        assert!(!is_thumb_row(&shape, 2));
        assert!(is_thumb_row(&shape, 3));
        assert!(is_thumb_row(&shape, 6));
        assert!(!is_thumb_row(&shape, 7));
    }
}
```

Declare in `render/mod.rs`:
```rust
pub mod scrollbar;
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p atomcode-tuix --lib render::scrollbar 2>&1 | tail -15`
Expected: PASS (6 tests).

- [ ] **Step 3: Commit**

```bash
git add crates/atomcode-tuix/src/render/{mod.rs,scrollbar.rs}
git commit -m "tuix(scrollbar): add pure compute module for thumb shape + placement"
```

### Task 6.3: Add show_scrollbar field + toggle_scrollbar trait method

**Files:**
- Modify: `crates/atomcode-tuix/src/render/mod.rs`
- Modify: `crates/atomcode-tuix/src/render/retained.rs`
- Modify: `crates/atomcode-tuix/src/render/alt_screen.rs`
- Modify: `crates/atomcode-tuix/src/render/plain.rs`
- Modify: `crates/atomcode-tuix/src/render/worker.rs`

- [ ] **Step 1: Add trait method**

In `render/mod.rs` `Renderer` trait:

```rust
/// Toggle the right-side visible scrollbar. Default: no-op for renderers
/// that don't have a body region (Plain).
fn toggle_scrollbar(&mut self) -> bool { false }
```

(Returns the new state — true = now shown.)

- [ ] **Step 2: Add field + impl in alt-screen**

In `AltScreenRenderer`:

```rust
show_scrollbar: bool,
```

Constructor: read from `ui_state::load().ui.show_scrollbar`. Override the trait method:

```rust
fn toggle_scrollbar(&mut self) -> bool {
    self.show_scrollbar = !self.show_scrollbar;
    let mut state = crate::render::ui_state::load();
    state.ui.show_scrollbar = self.show_scrollbar;
    crate::render::ui_state::save(&state);
    // Body width changes — force reflow + repaint
    self.reflow_body_lines();
    self.body_dirty = true;
    self.paint_frame();
    self.show_scrollbar
}
```

- [ ] **Step 3: Add field + impl in retained**

In `RetainedRenderer<W>`:

```rust
show_scrollbar: bool,
```

Constructor same load. Trait impl:

```rust
fn toggle_scrollbar(&mut self) -> bool {
    self.show_scrollbar = !self.show_scrollbar;
    let mut state = crate::render::ui_state::load();
    state.ui.show_scrollbar = self.show_scrollbar;
    crate::render::ui_state::save(&state);
    // Body width changes — force repaint of body region tail
    self.repaint_body_region();
    self.show_scrollbar
}
```

- [ ] **Step 4: Pipe through worker**

In `crates/atomcode-tuix/src/render/worker.rs`, add a `RenderCmd` variant + AckOp (need a return value, so AckOp pattern):

```rust
pub enum AckOp {
    // ... existing variants ...
    ToggleScrollbar,
}
```

Handle in `run_worker`:

```rust
AckOp::ToggleScrollbar => {
    let _ = inner.toggle_scrollbar();
    // No ack value needed; the toggle outcome is rendered visually + persisted
}
```

In `TaskRenderer`:

```rust
fn toggle_scrollbar(&mut self) -> bool {
    self.ack(AckOp::ToggleScrollbar);
    // Return is best-effort; if the caller needs the new state, it can
    // read from ui_state::load() after this returns.
    crate::render::ui_state::load().ui.show_scrollbar
}
```

(If existing `ack()` pattern signals completion via a channel, ensure the new variant is plumbed identically.)

- [ ] **Step 5: Build**

Run: `cargo check -p atomcode-tuix`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add crates/atomcode-tuix/src/render/{mod.rs,retained.rs,alt_screen.rs,plain.rs,worker.rs}
git commit -m "tuix(scrollbar): add show_scrollbar field + toggle_scrollbar trait method"
```

### Task 6.4: Apply scrollbar in alt-screen paint_body

**Files:**
- Modify: `crates/atomcode-tuix/src/render/alt_screen.rs`

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn alt_scrollbar_paints_thumb_when_enabled_and_overflow() {
    let mut buf = Vec::new();
    let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
    r.show_scrollbar = true;
    for i in 0..30 { r.push_body_row(format!("R{:02}", i)); }
    r.paint_body();
    drop(r);
    let s = String::from_utf8_lossy(&buf);
    assert!(s.contains("█"), "thumb char missing: {:?}", s);
}

#[test]
fn alt_scrollbar_not_painted_when_disabled() {
    let mut buf = Vec::new();
    let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
    r.show_scrollbar = false;
    for i in 0..30 { r.push_body_row(format!("R{:02}", i)); }
    r.paint_body();
    drop(r);
    let s = String::from_utf8_lossy(&buf);
    assert!(!s.contains("█"), "thumb should not appear when disabled: {:?}", s);
}
```

- [ ] **Step 2: Verify fail**

Run: `cargo test -p atomcode-tuix --lib "alt_scrollbar_paints_thumb|alt_scrollbar_not_painted" 2>&1 | tail -10`
Expected: FAIL.

- [ ] **Step 3: Update paint_body**

In `paint_body` (line ~800 in alt_screen.rs), after the existing per-row paint, add scrollbar column:

```rust
let scrollbar_shape = crate::render::scrollbar::compute(
    self.body_lines.len(),
    body_height,
    viewport_start,
    self.sticky_bottom,
    self.show_scrollbar,
);
if let Some(shape) = &scrollbar_shape {
    let scrollbar_col = self.width;  // 1-indexed rightmost
    for row_idx in 0..body_height {
        let target_row = 1 + row_idx as u16;
        let glyph = if crate::render::scrollbar::is_thumb_row(shape, row_idx) { "█" } else { "│" };
        let seq = format!("\x1b[{};{}H{}", target_row, scrollbar_col, glyph);
        let _ = self.out.write_all(seq.as_bytes());
    }
}
```

If body content currently writes into column `width`, also need to clamp the body row paint to `width - 1` when `scrollbar_shape.is_some()` to avoid overwrite. The cleanest way: when `show_scrollbar = true` AND overflow exists, the per-row content emit truncates at `width - 1` (use `truncate_to_width` helper if present, else slice). Add that conditional clamp in the existing row paint loop.

- [ ] **Step 4: Run tests**

Run: `cargo test -p atomcode-tuix --lib "alt_scrollbar_paints_thumb|alt_scrollbar_not_painted" 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 5: Run full alt-screen suite**

Run: `cargo test -p atomcode-tuix --lib render::alt_screen::tests 2>&1 | tail -10`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add crates/atomcode-tuix/src/render/alt_screen.rs
git commit -m "tuix(alt-screen): paint right-side scrollbar when overflow + show_scrollbar"
```

### Task 6.5: Apply scrollbar in retained paint

**Files:**
- Modify: `crates/atomcode-tuix/src/render/retained.rs`

- [ ] **Step 1: Write failing test**

```rust
#[test]
fn retained_scrollbar_paints_when_enabled_in_view_mode() {
    let (mut r, buf) = new_capturing(80, 10);
    r.show_scrollbar = true;
    for i in 0..30 { r.render(UiLine::User(format!("R{:02}", i))); }
    r.scroll_body(-3);  // enter view_mode + repaint_body_region
    let s = String::from_utf8_lossy(&buf.lock().unwrap());
    assert!(s.contains("█"), "thumb missing in view paint: {:?}", s);
}
```

- [ ] **Step 2: Verify fail**

Run: `cargo test -p atomcode-tuix --lib retained_scrollbar_paints 2>&1 | tail -10`
Expected: FAIL.

- [ ] **Step 3: Update repaint_body_region**

In `repaint_body_region`, after the per-row paint loop, mirror the alt-screen scrollbar paint:

```rust
let scrollbar_shape = crate::render::scrollbar::compute(
    total,
    body_height,
    if self.view_mode { self.viewport_top } else { total.saturating_sub(body_height) },
    self.sticky_bottom,
    self.show_scrollbar,
);
if let Some(shape) = &scrollbar_shape {
    let scrollbar_col = self.screen.width();
    for row_idx in 0..body_height {
        let target_row = 1 + row_idx as u16;
        let glyph = if crate::render::scrollbar::is_thumb_row(shape, row_idx) { "█" } else { "│" };
        let seq = format!("\x1b[{};{}H{}", target_row, scrollbar_col, glyph);
        let _ = self.out.write_all(seq.as_bytes());
    }
}
```

Also: when scrollbar visible, in the `emit_body_line_inner` path (sticky mode), truncate row content to `width - 1` so it doesn't overlap the scrollbar column. Easiest: in `serialize_row`-time, clamp; or pre-truncate the row before serializing.

- [ ] **Step 4: Run test**

Run: `cargo test -p atomcode-tuix --lib retained_scrollbar_paints 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 5: Run full retained suite**

Run: `cargo test -p atomcode-tuix --lib render::retained::tests 2>&1 | tail -10`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add crates/atomcode-tuix/src/render/retained.rs
git commit -m "tuix(retained): paint right-side scrollbar in repaint_body_region"
```

### Task 6.6: Register /scrollbar slash command

**Files:**
- Modify: `crates/atomcode-tuix/src/commands.rs`
- Modify: `crates/atomcode-tuix/src/event_loop/commands.rs`

- [ ] **Step 1: Register command**

In `commands.rs` BUILT_INS array, add (near `keys` / `help`):

```rust
Command { name: "scrollbar", desc: "Toggle the right-side scrollbar", needs_args: false },
```

In the `cmd_desc_i18n` arm:

```rust
"scrollbar" => Msg::CmdDescScrollbar,
```

- [ ] **Step 2: Handle command**

In `event_loop/commands.rs`, add an arm in the slash dispatch (near `keys`):

```rust
"scrollbar" => {
    let now_on = renderer.toggle_scrollbar();
    renderer.render(UiLine::CommandOutput(
        t(if now_on { Msg::ScrollbarOn } else { Msg::ScrollbarOff }).into_owned(),
    ));
    renderer.flush();
}
```

- [ ] **Step 3: Build**

Run: `cargo build -p atomcode-tuix 2>&1 | tail -10`
Expected: clean.

- [ ] **Step 4: Add command-registration test**

In `commands.rs` `#[cfg(test)] mod tests`:

```rust
#[test]
fn scrollbar_command_registered_with_i18n_description_in_both_locales() {
    use crate::i18n::Locale;
    assert!(BUILT_INS.iter().any(|c| c.name == "scrollbar"));
    for locale in [Locale::EnUs, Locale::ZhCn] {
        crate::i18n::set_locale(locale);
        let desc = cmd_desc_i18n("scrollbar").expect("CmdDescScrollbar translation");
        assert!(!desc.is_empty(), "CmdDescScrollbar ({locale:?}) must not be empty");
    }
}
```

Run: `cargo test -p atomcode-tuix --lib scrollbar_command_registered 2>&1 | tail -10`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/atomcode-tuix/src/{commands.rs,event_loop/commands.rs}
git commit -m "tuix(commands): register /scrollbar + i18n description"
```

---

## Phase 7: extra scroll keys (Alt+↑/↓, Ctrl+↑/↓)

### Task 7.1: Add scroll_to_prev_message / scroll_to_next_message

**Files:**
- Modify: `crates/atomcode-tuix/src/render/mod.rs`
- Modify: `crates/atomcode-tuix/src/render/alt_screen.rs`
- Modify: `crates/atomcode-tuix/src/render/retained.rs`
- Modify: `crates/atomcode-tuix/src/render/worker.rs`

- [ ] **Step 1: Add trait methods**

In `Renderer` trait:

```rust
/// Jump body viewport to the prev/next message boundary. No-op when no
/// such boundary exists in the configured direction.
fn scroll_to_prev_message(&mut self) {}
fn scroll_to_next_message(&mut self) {}
fn scroll_to_prev_user_message(&mut self) {}
fn scroll_to_next_user_message(&mut self) {}
```

- [ ] **Step 2: Write tests (alt-screen)**

```rust
#[test]
fn alt_scroll_to_prev_message_finds_nearest_above() {
    let mut buf = Vec::new();
    let mut r = AltScreenRenderer::with_writer(&mut buf, caps_default(), 80, 10);
    // Populate body with: 5 user lines, 5 assistant lines, 5 tool lines.
    for i in 0..5 { r.render(UiLine::User(format!("u{}", i))); }
    for i in 0..5 { r.render(UiLine::AssistantText(format!("a{}", i))); }
    for i in 0..5 { r.render(UiLine::ToolCall { /* fill required fields */ }); }
    // Scroll to viewport_top = 10 (toolcall area).
    r.scroll_body(-100);  // top
    r.scroll_body(10);     // back down 10
    let before = r.viewport_top;
    r.scroll_to_prev_message();
    assert!(r.viewport_top < before, "viewport should jump up to prev message");
}
```

(Fill in `UiLine::ToolCall { ... }` per the real shape — grep `grep -nE "enum UiLine" crates/atomcode-tuix/src/render/mod.rs` to find it.)

- [ ] **Step 3: Implement on alt-screen**

```rust
fn scroll_to_prev_message(&mut self) {
    let target = self.message_marks.iter().rev().find(|m| m.line_idx < self.viewport_top);
    if let Some(target) = target {
        self.scroll_body_to(target.line_idx);
    }
}

fn scroll_to_next_message(&mut self) {
    let target = self.message_marks.iter().find(|m| m.line_idx > self.viewport_top);
    if let Some(target) = target {
        self.scroll_body_to(target.line_idx);
    }
}

fn scroll_to_prev_user_message(&mut self) {
    let target = self.message_marks.iter().rev().find(|m| {
        m.line_idx < self.viewport_top && m.kind == crate::render::MarkKind::User
    });
    if let Some(target) = target {
        self.scroll_body_to(target.line_idx);
    }
}

fn scroll_to_next_user_message(&mut self) {
    let target = self.message_marks.iter().find(|m| {
        m.line_idx > self.viewport_top && m.kind == crate::render::MarkKind::User
    });
    if let Some(target) = target {
        self.scroll_body_to(target.line_idx);
    }
}
```

Add helper:
```rust
fn scroll_body_to(&mut self, target: usize) {
    let body_height = self.body_height() as usize;
    let max_top = self.body_lines.len().saturating_sub(body_height);
    self.viewport_top = target.min(max_top);
    self.sticky_bottom = self.viewport_top >= max_top;
    self.body_dirty = true;
    self.footer_dirty = true;
    self.paint_frame();
}
```

- [ ] **Step 4: Implement on retained**

Mirror, using `repaint_body_region()` instead of `paint_frame`:

```rust
fn scroll_to_prev_message(&mut self) {
    let target = self.message_marks.iter().rev().find(|m| m.line_idx < self.viewport_top);
    if let Some(target) = target {
        self.scroll_body_to(target.line_idx);
    }
}
// ... (same shape for other 3)

fn scroll_body_to(&mut self, target: usize) {
    let body_height = self.body_bottom_row() as usize;
    let max_top = self.body_lines.len().saturating_sub(body_height);
    self.viewport_top = target.min(max_top);
    self.sticky_bottom = self.viewport_top >= max_top;
    self.view_mode = !self.sticky_bottom;
    self.repaint_body_region();
}
```

- [ ] **Step 5: Wire through worker**

Add 4 new `RenderCmd` variants + handlers in `worker.rs`:

```rust
RenderCmd::ScrollToPrevMessage,
RenderCmd::ScrollToNextMessage,
RenderCmd::ScrollToPrevUserMessage,
RenderCmd::ScrollToNextUserMessage,
```

And 4 forwarding methods on `TaskRenderer`.

- [ ] **Step 6: Run tests**

Run: `cargo test -p atomcode-tuix --lib "scroll_to_prev_message|scroll_to_next_message|scroll_to_prev_user|scroll_to_next_user" 2>&1 | tail -15`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/atomcode-tuix/src/render/{mod.rs,alt_screen.rs,retained.rs,worker.rs}
git commit -m "tuix(scroll): add scroll_to_prev/next_message and _user_message variants"
```

### Task 7.2: Bind Alt+↑/↓ + Ctrl+↑/↓ in handle_scroll_key

**Files:**
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs`

- [ ] **Step 1: Locate handle_scroll_key**

Run: `grep -nE "fn handle_scroll_key" crates/atomcode-tuix/src/event_loop/mod.rs`
Expected: line ~4160.

- [ ] **Step 2: Add key arms**

In `handle_scroll_key`, add (inside the existing `match code { ... }`):

```rust
KeyCode::Up if modifiers.contains(KeyModifiers::ALT) && !modifiers.contains(KeyModifiers::SHIFT) => {
    renderer.scroll_to_prev_message();
    Some(true)
}
KeyCode::Down if modifiers.contains(KeyModifiers::ALT) && !modifiers.contains(KeyModifiers::SHIFT) => {
    renderer.scroll_to_next_message();
    Some(true)
}
KeyCode::Up if modifiers.contains(KeyModifiers::CONTROL) && !modifiers.contains(KeyModifiers::SHIFT) => {
    renderer.scroll_to_prev_user_message();
    Some(true)
}
KeyCode::Down if modifiers.contains(KeyModifiers::CONTROL) && !modifiers.contains(KeyModifiers::SHIFT) => {
    renderer.scroll_to_next_user_message();
    Some(true)
}
```

(Place these BEFORE the existing `KeyCode::Up if has_shift =>` arms so the modifier check ordering is unambiguous.)

- [ ] **Step 3: Build**

Run: `cargo build -p atomcode-tuix 2>&1 | tail -10`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add crates/atomcode-tuix/src/event_loop/mod.rs
git commit -m "tuix(event_loop): bind Alt+↑/↓ + Ctrl+↑/↓ to message-jump scrolls"
```

---

## Phase 8: /keys docs update

### Task 8.1: Update Chinese KeybindingsHelp

**Files:**
- Modify: `crates/atomcode-core/src/i18n/zh_cn.rs`

- [ ] **Step 1: Edit KeybindingsHelp text**

Find `Msg::KeybindingsHelp => r#"..."#.into(),` (line ~161). After the existing `── 历史 ──` block (before `── 会话 ──`), insert:

```
  ── 翻看输出 ──
    PageUp / PageDown                上下翻一页（10 行）
    Shift+↑ / Shift+↓                上下翻一行
    Alt+↑ / Alt+↓                    跳到上/下一条消息 ***
    Ctrl+↑ / Ctrl+↓                  跳到上/下一条自己发的消息
    Home / End                       跳到最顶 / 跳回最新
    鼠标滚轮                          上下滚（atomcode 接管）
    Shift+拖鼠标                      用宿主终端选择文本（绕过 atomcode）

  ── 显示 ──
    /scrollbar                       切换右侧滚动条显示
```

And append to the existing footnotes block:

```
  *** Alt+↑/↓ macOS Apple Terminal 需在
      Settings → Profiles → Keyboard 启用 "Use Option as Meta key"
      才会发送修饰键。其他终端默认即可。
```

- [ ] **Step 2: Run i18n consistency test**

Run: `cargo test -p atomcode-tuix --lib keys_command_is_registered_with_i18n_description_in_both_locales 2>&1 | tail -10`
Expected: PASS (now or unchanged).

- [ ] **Step 3: Commit**

```bash
git add crates/atomcode-core/src/i18n/zh_cn.rs
git commit -m "i18n(zh-cn): add scroll keys + /scrollbar to /keys help"
```

### Task 8.2: Update English KeybindingsHelp

**Files:**
- Modify: `crates/atomcode-core/src/i18n/en.rs`

- [ ] **Step 1: Mirror the change**

Add to `Msg::KeybindingsHelp` in en.rs:

```
  ── Scrollback ──
    PageUp / PageDown                Page up / down (10 lines)
    Shift+↑ / Shift+↓                Line up / down
    Alt+↑ / Alt+↓                    Jump to prev / next message ***
    Ctrl+↑ / Ctrl+↓                  Jump to prev / next user message
    Home / End                       Jump to top / back to latest
    Mouse wheel                      Scroll body (atomcode captures)
    Shift+drag mouse                 Use host terminal selection (bypass)

  ── Display ──
    /scrollbar                       Toggle right-side scrollbar
```

Add footnote:

```
  *** Alt+↑/↓ on macOS Apple Terminal requires enabling "Use Option as
      Meta key" under Settings → Profiles → Keyboard. Other terminals
      send the modifier by default.
```

- [ ] **Step 2: Build**

Run: `cargo build -p atomcode-core 2>&1 | tail -10`
Expected: clean.

- [ ] **Step 3: Commit**

```bash
git add crates/atomcode-core/src/i18n/en.rs
git commit -m "i18n(en): add scroll keys + /scrollbar to /keys help"
```

---

## Phase 9: Final integration QA

### Task 9.1: Run full test suite

**Files:** none

- [ ] **Step 1: Full workspace test**

Run: `cargo test --workspace 2>&1 | tail -30`
Expected: all green. If failures, fix and re-run; do not proceed.

- [ ] **Step 2: Lint**

Run: `cargo clippy --workspace --all-targets 2>&1 | tail -30`
Expected: no new warnings introduced by this branch (compare against `main`).

### Task 9.2: Manual integration checklist

**Files:** none — this task produces a checklist for the user, not code.

Output to the user (do not auto-execute):

```
请手动验证以下场景（在你日常使用的终端上）：

1. macOS Terminal.app retained + 滚轮上滚 → 进入翻看，新内容静默累积，按 End 跳回
2. iTerm2 alt-screen + Alt+↑/↓ → 在 user/assistant/tool 消息间跳转
3. retained 上 Shift+拖鼠标 → 终端原生选择高亮（atomcode 让出鼠标）
4. retained 上普通拖鼠标 → atomcode 反色高亮，松手 OSC 52 写剪贴板
5. /scrollbar 切换可视滚动条；重启 atomcode 后状态保留
6. streaming 进行时 PageUp → viewport 不动，spinner 继续转
7. 翻看中 /clear → 立即回 sticky 跟底
8. 翻看中 approval 弹出 → 强制回 sticky，approval 在底部正常审批
9. retained 启动 → 滚轮事件确实由 atomcode 处理（验证：宿主终端滚轮不再滚启动前的历史）
10. retained + /bash ls 等长命令走 suspend_for_external → child 期间宿主终端鼠标恢复；resume 后 atomcode 重新接管

复测后告诉我结果。如果有失败的，反馈是哪条 + 终端/OS 信息。
```

Do not auto-execute; the user must drive these. Mark task complete only after user confirms manual checklist done.

---

## Self-Review

跑完所有 Phase 后，按照 spec 的每段对照检查任务覆盖：

- [x] Phase 0: Msg variants — Task 0.1
- [x] Phase 1: Selection 共享模块 — Tasks 1.1-1.4
- [x] Phase 2: body buffer + MessageMark — Tasks 2.1-2.4
- [x] Phase 3: retained view_mode + scroll — Tasks 3.1-3.4
- [x] Phase 4: retained 鼠标接管 — Tasks 4.1-4.3
- [x] Phase 5: retained selection 接入 — Tasks 5.1-5.3
- [x] Phase 6: scrollbar + /scrollbar + ui-state.toml — Tasks 6.1-6.6
- [x] Phase 7: extra scroll keys — Tasks 7.1-7.2
- [x] Phase 8: /keys docs — Tasks 8.1-8.2
- [x] Phase 9: 集成 QA — Tasks 9.1-9.2
