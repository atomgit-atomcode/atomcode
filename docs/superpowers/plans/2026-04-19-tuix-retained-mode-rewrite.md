# atomcode-tuix Retained-mode 渲染重写计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 把 atomcode-tuix 从 immediate-mode（"UiLine 事件直接变 ANSI bytes"）改造成 retained-mode（"UiLine 变化更新内存 screen buffer，独立 render loop 按节奏 diff + emit"），消除 Ink Phase 1 之后仍残留的 5 类固有 bug（footer/body 交界漂移、submit 4ms 空窗、terminal state drift、无 full-repaint 能力、scroll 出屏幕的 body 无法 recover）。对齐 CC / Ink 的渲染架构。

**Architecture:** 新增 `render/screen.rs` 实现 W×H cell buffer + damage tracking + cell-level diff。新增 `render/frame_loop.rs` 独立 render loop 按 16ms 节奏消费更新 + 单次 `stdout.write` emit。现有 `AnsiRenderer` 改造成只"更新 Screen model"，不再直接写 stdout。body 进 buffer（bounded scrollback），footer 按绝对行写入固定区域。DECSTBM 去除 —— body 和 footer 同在一个 buffer 里，不再需要滚动区域分割。

**Tech Stack:** Rust, crossterm (terminal size / input), unicode-width（已用）, tokio（render loop 用），现有 `render/cell.rs` 的 Cell/CellStyle 复用扩展。

---

## 核心设计决策（先审这个，再看 Task）

### 1. Screen buffer 模型

```rust
pub struct Screen {
    /// cols × rows cell grid. Addressed as `cells[row][col]`, both
    /// 0-indexed internally (serialise converts to 1-indexed ANSI).
    cells: Vec<Vec<Cell>>,
    /// Prev frame for diff. Replaced every time we emit.
    prev_cells: Vec<Vec<Cell>>,
    width: u16,
    height: u16,
    /// Optional bounding rect for the damaged area. When Some, diff
    /// only scans inside. `None` = whole screen might have changed.
    damage: Option<Rect>,
    /// Next emitted cursor position (after the diff). None = leave
    /// where diff last left it.
    target_cursor: Option<(u16, u16)>,
    /// True when cursor should be visible after the frame paints
    /// (input box). False during no-input phases (future).
    cursor_visible: bool,
}
```

**不做**的：
- `Option<Style>` per cell — 用 `CellStyle::default()` 已够
- bg color — footer 用不到，body 不用彩色 bg
- underline/italic — footer 用不到

### 2. Body 区域处理

body 进 Screen 的 top 部分（rows `[0, H - footer_rows)`），footer 占 rows `[H - footer_rows, H)`。

**scrollback 溢出**：
- 模型不保存已滚出屏幕的行（不做 ring buffer 模拟 tmux）
- `append_body_line(line)`: 把 top 行向上推，最顶行丢弃，bottom 空行接新内容
- terminal 的真 scrollback 由 terminal 自己管 — 我们退出后用户仍能 scroll up 看历史
- **关键**：append 只改 model，不动 terminal；下一帧 diff emit 会让 terminal 看到变化

**resize**：重建 Screen + force full repaint。历史体验损失一次屏幕（scrollback 里还有）。

### 3. Render loop 节奏

```
┌───────────────┐   cmd     ┌───────────────┐   16ms tick   ┌──────────┐
│ event_loop    │──────────>│ RenderLoop    │ ─────────────>│ stdout   │
│ (model update)│           │ owns Screen   │  single write │ (BufW)   │
└───────────────┘           └───────────────┘               └──────────┘
```

- event_loop 收到 UiLine → 发 `RenderCmd::Update` 到 render loop（非阻塞 mpsc send）
- render loop 持有 Screen，update cell buffer
- render loop 每 16ms / 有 pending update 时：diff prev vs current → serialize patches → single `stdout.write_all`
- coalesce：16ms 窗口内多个 update 只触发一次 emit

### 4. DECSTBM 废弃

现有架构用 DECSTBM 把 body 锁在滚动区域、footer 钉在固定区。retained-mode 不需要：
- body 写入 `cells[row][col]` 是内存操作，不触发 terminal scroll
- footer 写入 `cells[H-N..H][col]` 也是内存操作
- emit 阶段按绝对 `\x1b[row;col H` 定位每个 patch

每次启动发 `\x1b[2J\x1b[H` + `\x1b[r`（清除任何 prior DECSTBM）。之后完全不用 DECSTBM。

### 5. Lifecycle 简化

- `shutdown`: render loop 任务退出 + 发 `\x1b[r\x1b[?7h\r\n`（以防万一）
- `suspend_for_external`: 暂停 render loop，发 `\x1b[r` + disable raw mode + 让外部进程跑，resume 时**强制 full repaint 整个 Screen**
- 不再有 `clear_scroll_region` / `sync_scroll_region` 这类半状态方法

### 6. 和现有 Cell / diff 复用关系

- **Cell / CellStyle / continuation** 直接复用 `render/cell.rs`
- `diff_cells` 保留，但 API 改成 `(prev: &[Vec<Cell>], next: &[Vec<Cell>]) -> Vec<Patch>`（按 row index 而不是 HashMap，更快）
- `serialize_patches` + SGR state machine 保留不变
- `push_str_cells` 保留
- **删掉**：`rows_to_frame`（不再需要 HashMap）、`row_to_bytes`（legacy path 删）

---

## 7 Phase 任务分解

### Phase 0: Cell 结构扩容 + 辅助函数

**Files:**
- Modify: `crates/atomcode-tuix/src/render/cell.rs`

- [ ] **Step 0.1: Cell 加 bg 字段**（为未来可能的 bg color，但**目前 always None**，不影响现有用法）

略过 — 确认不需要 bg，不做。

- [ ] **Step 0.2: 改 diff_cells 签名**为 `&[Vec<Cell>]` 两个入参

```rust
/// Diff prev frame vs next frame, both indexed by row 0..H-1.
/// Generates one Patch per changed (row, col) position with absolute
/// coordinates (1-indexed, matching ANSI).
pub fn diff_cells(
    prev: &[Vec<Cell>],
    next: &[Vec<Cell>],
) -> Vec<Patch> {
    let mut patches = Vec::new();
    let max_rows = prev.len().max(next.len());
    let blank = Cell::blank();
    for r in 0..max_rows {
        let p = prev.get(r).map(|v| v.as_slice()).unwrap_or(&[]);
        let n = next.get(r).map(|v| v.as_slice()).unwrap_or(&[]);
        let max_cols = p.len().max(n.len());
        for c in 0..max_cols {
            let pc = p.get(c).unwrap_or(&blank);
            let nc = n.get(c).unwrap_or(&blank);
            if pc != nc {
                patches.push(Patch {
                    row: (r + 1) as u16,
                    col: (c + 1) as u16,
                    cell: nc.clone(),
                });
            }
        }
    }
    patches
}
```

保留原 `HashMap` 版本作为 legacy `diff_cells_hashmap`（Phase 3 再删），新版本并存。

- [ ] **Step 0.3: 删掉不再需要的 `rows_to_frame` / `row_to_bytes`**

在 Phase 6 清理时做，先不删。

---

### Phase 1: Screen struct

**Files:**
- Create: `crates/atomcode-tuix/src/render/screen.rs`

- [ ] **Step 1.1: Screen struct 定义**

```rust
//! W × H cell grid backing the retained-mode renderer. Owns two
//! frames: `cells` is the current target state (being built up by
//! widget draws); `prev_cells` is what the terminal currently shows
//! (i.e. what we emitted at the last `render_diff`). `render_diff`
//! generates ANSI patches from (prev → current) and swaps the frames.

use super::cell::{diff_cells, serialize_patches, Cell, Patch};

pub struct Screen {
    cells: Vec<Vec<Cell>>,
    prev_cells: Vec<Vec<Cell>>,
    width: u16,
    height: u16,
    cursor: Option<(u16, u16)>,
    cursor_visible: bool,
}

impl Screen {
    pub fn new(width: u16, height: u16) -> Self {
        let blank_row = vec![Cell::blank(); width as usize];
        let frame = vec![blank_row; height as usize];
        Self {
            cells: frame.clone(),
            prev_cells: frame,
            width,
            height,
            cursor: None,
            cursor_visible: true,
        }
    }

    pub fn width(&self) -> u16 { self.width }
    pub fn height(&self) -> u16 { self.height }

    /// Clear the whole frame to blank. O(W*H). Caller typically
    /// follows this with a sequence of `draw_row` / `draw_line`
    /// calls before the next `render_diff`.
    pub fn clear(&mut self) {
        let blank = Cell::blank();
        for row in &mut self.cells {
            for c in row {
                *c = blank.clone();
            }
        }
    }

    /// Write a row of cells starting at (row, col). Panics if out of
    /// bounds — callers are expected to clamp first. Cells with
    /// width > 1 consume one output column per cell; the caller (or
    /// push_str_cells) is responsible for continuation cells.
    pub fn draw_row(&mut self, row: usize, col: usize, cells: &[Cell]) {
        if row >= self.cells.len() {
            return;
        }
        let target = &mut self.cells[row];
        for (i, cell) in cells.iter().enumerate() {
            let dst_col = col + i;
            if dst_col >= target.len() { break; }
            target[dst_col] = cell.clone();
        }
    }

    /// Mark where the terminal cursor should park after emit.
    pub fn set_cursor(&mut self, row: u16, col: u16) {
        self.cursor = Some((row, col));
    }

    /// Scroll the body area up by N rows (drop top, blank bottom).
    /// Body area = rows [0, bottom), where `bottom` is typically
    /// `H - footer_rows` so scrolling doesn't eat the footer.
    pub fn scroll_up(&mut self, bottom: usize, n: usize) {
        if n == 0 || bottom == 0 { return; }
        let n = n.min(bottom);
        self.cells.copy_within(n..bottom, 0);
        let blank = Cell::blank();
        for row_idx in (bottom - n)..bottom {
            for c in &mut self.cells[row_idx] {
                *c = blank.clone();
            }
        }
    }

    /// Build ANSI output for (prev → current) and swap frames so
    /// next call diffs against what we just painted. Returns
    /// `(patches_count, bytes)` for tracing.
    pub fn render_diff(&mut self) -> Vec<u8> {
        let patches = diff_cells(&self.prev_cells, &self.cells);
        let mut out = serialize_patches(&patches);
        if let Some((r, c)) = self.cursor {
            use std::io::Write as _;
            let _ = write!(&mut out, "\x1b[{};{}H", r, c);
        }
        if self.cursor_visible {
            out.extend_from_slice(b"\x1b[?25h");
        } else {
            out.extend_from_slice(b"\x1b[?25l");
        }
        // Swap: current becomes prev, old prev becomes scratch we'll
        // overwrite on next draw.
        std::mem::swap(&mut self.prev_cells, &mut self.cells);
        // Wipe current (now the old prev) so stale cells don't
        // survive into the next frame if the new frame only draws
        // parts of the screen.
        self.clear();
        out
    }

    /// Force the next render_diff to emit every cell (as if prev
    /// were all-blank). Used after resume_from_external / resize /
    /// any time terminal state is unknown.
    pub fn invalidate(&mut self) {
        let blank = Cell::blank();
        let blank_row = vec![blank; self.width as usize];
        for row in &mut self.prev_cells {
            *row = blank_row.clone();
        }
    }

    /// Rebuild for new dimensions. Contents of current + prev frames
    /// are wiped; caller must re-draw everything.
    pub fn resize(&mut self, width: u16, height: u16) {
        *self = Self::new(width, height);
    }
}
```

- [ ] **Step 1.2: 测试 Screen basics**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::cell::{push_str_cells, CellStyle};

    #[test]
    fn new_screen_is_all_blank() {
        let mut s = Screen::new(10, 3);
        let bytes = s.render_diff();
        // First diff: prev is blank, current is blank, no patches.
        assert!(bytes.iter().all(|b| *b == b'\x1b' || b.is_ascii()));
        // No cell-diff output — just cursor hide/show maybe.
    }

    #[test]
    fn draw_row_updates_cells() {
        let mut s = Screen::new(20, 5);
        let mut cells = Vec::new();
        push_str_cells(&mut cells, "hello", &CellStyle::default());
        s.draw_row(2, 3, &cells);
        let bytes = s.render_diff();
        let out = String::from_utf8_lossy(&bytes);
        assert!(out.contains("hello"));
        assert!(out.contains("\x1b[3;4H")); // row 3, col 4 (1-indexed)
    }

    #[test]
    fn second_frame_with_same_content_emits_nothing() {
        let mut s = Screen::new(20, 5);
        let mut cells = Vec::new();
        push_str_cells(&mut cells, "x", &CellStyle::default());
        s.draw_row(0, 0, &cells);
        let _ = s.render_diff();
        // Re-draw same: current has 'x' at (0,0), prev has 'x' at (0,0).
        s.draw_row(0, 0, &cells);
        let bytes = s.render_diff();
        let out = String::from_utf8_lossy(&bytes);
        // No cell patches — just cursor controls.
        assert!(!out.contains("x"), "re-drawing same cell should not re-emit: {:?}", out);
    }

    #[test]
    fn scroll_up_shifts_rows() {
        let mut s = Screen::new(10, 5);
        let mut a = Vec::new();
        push_str_cells(&mut a, "A", &CellStyle::default());
        let mut b = Vec::new();
        push_str_cells(&mut b, "B", &CellStyle::default());
        s.draw_row(0, 0, &a);
        s.draw_row(1, 0, &b);
        // Render so prev gets populated.
        let _ = s.render_diff();
        // Re-draw the "current" exactly and then scroll.
        s.draw_row(0, 0, &a);
        s.draw_row(1, 0, &b);
        s.scroll_up(2, 1);
        // After scroll_up(2, 1): row 0 = B (was at row 1), row 1 = blank
        let bytes = s.render_diff();
        let out = String::from_utf8_lossy(&bytes);
        assert!(out.contains("B"));
    }

    #[test]
    fn invalidate_forces_full_emit() {
        let mut s = Screen::new(10, 3);
        let mut cells = Vec::new();
        push_str_cells(&mut cells, "hi", &CellStyle::default());
        s.draw_row(0, 0, &cells);
        let _ = s.render_diff();
        // Normally same content → no emit.
        s.draw_row(0, 0, &cells);
        s.invalidate();
        let bytes = s.render_diff();
        let out = String::from_utf8_lossy(&bytes);
        assert!(out.contains("hi"), "invalidate should force re-emit: {:?}", out);
    }
}
```

- [ ] **Step 1.3: 把 `pub mod screen` 加到 `render/mod.rs`**

### Phase 2: 单线程 render facade（不 spawn task，先串起来）

**目的**：让 `Screen` 能被现有 event loop 使用，**先不引入异步 render loop**（Phase 5 再引入节奏），保持简单。

**Files:**
- Create: `crates/atomcode-tuix/src/render/retained.rs`
- Modify: `crates/atomcode-tuix/src/render/mod.rs`

- [ ] **Step 2.1: RetainedRenderer impl Renderer**

```rust
use std::io::{BufWriter, Stdout, Write};
use super::cell::{push_str_cells, Cell, CellStyle};
use super::screen::Screen;
use super::theme::{role, Role};
use super::{MenuPayload, Renderer, StatusLine, UiLine};
use crate::sanitize::scrub_controls;
use crate::terminal::TerminalCaps;

const PAD_COL: usize = 2;

pub struct RetainedRenderer<W: Write + Send> {
    out: W,
    caps: TerminalCaps,
    screen: Screen,
    // Widget state: the single mutable "model" layer.
    input_buf: String,
    input_cursor_byte: usize,
    spinner: Option<(String, String)>,
    menu: Option<super::MenuPayload>,
    status: super::StatusLine,
    // Body scrollback: we don't keep it (terminal's own scrollback
    // does). Body writes scroll the screen buffer up by N rows.
}

impl RetainedRenderer<BufWriter<Stdout>> {
    pub fn new(caps: TerminalCaps) -> Self {
        let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
        Self::with_writer(BufWriter::new(std::io::stdout()), caps, w, h)
    }
}

impl<W: Write + Send> RetainedRenderer<W> {
    pub fn with_writer(out: W, caps: TerminalCaps, w: u16, h: u16) -> Self {
        Self {
            out,
            caps,
            screen: Screen::new(w, h),
            input_buf: String::new(),
            input_cursor_byte: 0,
            spinner: None,
            menu: None,
            status: StatusLine::default(),
        }
    }

    fn footer_rows(&self) -> usize {
        // spinner + top rule + middle (wrap aware) + bottom rule + menu + status
        // Simplified constant for Phase 2 smoke test; Phase 3 derives properly.
        5
    }

    fn paint_footer(&mut self) {
        // Stub: only draw a single-row middle with input buf for smoke test.
        let h = self.screen.height() as usize;
        let footer_top = h.saturating_sub(self.footer_rows());
        let mut middle = Vec::new();
        push_str_cells(
            &mut middle,
            &format!("  ❯ {}", scrub_controls(&self.input_buf)),
            &CellStyle::default(),
        );
        self.screen.draw_row(footer_top + 2, 0, &middle);
        // cursor at end of input
        self.screen.set_cursor(
            (footer_top + 2) as u16 + 1,
            (PAD_COL + 2 + self.input_buf.chars().count()) as u16 + 1,
        );
    }

    fn flush_frame(&mut self) {
        let bytes = self.screen.render_diff();
        let _ = self.out.write_all(&bytes);
    }
}

impl<W: Write + Send> Renderer for RetainedRenderer<W> {
    fn render(&mut self, line: UiLine) {
        match line {
            UiLine::InputPrompt { buf, cursor_byte, menu, status } => {
                self.input_buf = buf;
                self.input_cursor_byte = cursor_byte;
                self.menu = menu;
                self.status = status;
                self.paint_footer();
                self.flush_frame();
            }
            // Phase 2 only covers InputPrompt for smoke; other
            // variants are no-ops. Phase 3 adds the rest.
            _ => {}
        }
    }

    fn flush(&mut self) { let _ = self.out.flush(); }
    fn shutdown(&mut self) {
        let _ = self.out.write_all(b"\x1b[?7h\x1b[r\r\n");
        let _ = self.out.flush();
    }
    fn reset(&mut self) {
        self.screen = Screen::new(self.screen.width(), self.screen.height());
        let _ = self.out.write_all(b"\x1b[2J\x1b[H");
        let _ = self.out.flush();
    }
    fn clear_screen(&mut self) { self.reset(); }
    fn suspend_for_external(&mut self) {
        let _ = self.out.write_all(b"\x1b[r\x1b[?7h\r\n");
        let _ = self.out.flush();
    }
    fn resume_from_external(&mut self) {
        self.screen.invalidate();
        let _ = self.out.write_all(b"\x1b[2J\x1b[H");
    }
    fn flush_deferred(&mut self) { self.flush(); }
    fn on_resize(&mut self, cols: u16, rows: u16) {
        self.screen.resize(cols, rows);
        self.paint_footer();
        self.flush_frame();
    }
}
```

- [ ] **Step 2.2: 把 retained 作为 feature-flagged 备选** — 不替换 AnsiRenderer。用环境变量 `ATOMCODE_TUIX_RETAINED=1` 或 startup arg 选择。

```rust
// lib.rs 里 run() 中：
let inner: Box<dyn Renderer> = if caps.tty {
    if std::env::var("ATOMCODE_TUIX_RETAINED").ok().as_deref() == Some("1") {
        Box::new(RetainedRenderer::new(caps))
    } else {
        Box::new(AnsiRenderer::new(caps))
    }
} else { ... };
```

- [ ] **Step 2.3: smoke test** — `ATOMCODE_TUIX_RETAINED=1 cargo run`。只验证能启动 + 看到 input box。body 不画（Phase 4），菜单不画（Phase 3.5），streaming 不画（Phase 4）。验证渲染 pipeline 通了。

### Phase 3: footer widget 完整迁移

**Files:**
- Modify: `crates/atomcode-tuix/src/render/retained.rs`

- [ ] **Step 3.1: `paint_footer` 完整实现**，对齐现有 `AnsiRenderer::draw_footer_here_with_prev_cursor`

把 build_spinner_row / build_rule_row / build_middle_row / build_menu_row / build_status_row 搬过来（cell-based，已有），依次 `screen.draw_row(footer_top + i, 0, &cells)`。

逻辑：
- footer_rows 根据 input buf 跨几行 + menu 是否打开计算
- middle_rows 用 `wrap_with_cursor` 算
- rule_width = screen.width - PAD_COL*2
- footer_top = H - footer_rows

- [ ] **Step 3.2: 所有 footer 相关 UiLine 分发**

`UiLine::Spinner` / `StreamingBox` / `InputPrompt` 都变成 "更新 widget state + paint_footer + flush"。

- [ ] **Step 3.3: 迁移现有 footer 测试** — `menu_toggle_byte_cost` / `footer_shrink_erases_menu_tail` / `keystroke_byte_cost_steady_state` 改跑 `RetainedRenderer`，assert 字节量 **不回退**。

目标：
- menu open ≤ 900 B (现 880)
- menu close ≤ 900 B
- keystroke ≤ 30 B
- menu nav ≤ 300 B

### Phase 4: body widget + scrollback

**Files:**
- Modify: `crates/atomcode-tuix/src/render/retained.rs`

- [ ] **Step 4.1: body 写入用 scroll_up + draw**

```rust
fn append_body_lines(&mut self, lines: &[String]) {
    let h = self.screen.height() as usize;
    let footer_top = h - self.footer_rows();
    let body_bottom = footer_top;
    let w = self.screen.width() as usize - PAD_COL * 2;
    let mut emit_lines: Vec<Vec<Cell>> = Vec::new();
    for line in lines {
        for phys in line.split('\n') {
            for chunk in crate::width::wrap_line_to_width(phys, w) {
                let mut row = Vec::new();
                push_str_cells(&mut row, &" ".repeat(PAD_COL), &CellStyle::default());
                push_str_cells(&mut row, chunk, &CellStyle::default());
                emit_lines.push(row);
            }
        }
    }
    for row in emit_lines {
        self.screen.scroll_up(body_bottom, 1);
        self.screen.draw_row(body_bottom - 1, 0, &row);
    }
}
```

- [ ] **Step 4.2: `UiLine::AssistantText` / `AssistantLineBreak` / `ToolCall` / `ToolResult` / `DiffLine` / `DiffBlock` / `User` / `CommandOutput` / `Welcome` / `TurnSeparator` 都走 `append_body_lines`**

markdown 渲染继续用 `crate::markdown::render_line`，返回的 String 直接喂 append_body_lines。

- [ ] **Step 4.3: streaming 字节 budget 测试迁移**

`streaming_text_delta_byte_cost` 改跑 RetainedRenderer，assert <60 B/delta。

### Phase 5: async render loop + 16ms coalesce

**Files:**
- Modify: `crates/atomcode-tuix/src/render/retained.rs` 或新 `render/frame_loop.rs`

- [ ] **Step 5.1: 把 flush_frame 改成 "标记 dirty"，不立即 emit**

```rust
dirty: bool,

fn paint_footer(&mut self) { /* update screen */ self.dirty = true; }
fn append_body_lines(&mut self, ...) { /* update screen */ self.dirty = true; }
```

- [ ] **Step 5.2: event_loop 里加 16ms tick**，tick 时调 `renderer.flush_deferred()` → 内部判断 dirty → emit。

flush_deferred 目前是旧 InputThrottle 用。改语义为"如果 dirty 则 emit"。

- [ ] **Step 5.3: 消除"输入完输入框没有"4ms 空窗**

测试：render 连续 3 个 UiLine（`User` + `CommandOutput` + `InputPrompt`）后只走一次 emit（因为 dirty 合并），屏幕上不会有"只有 User 没 footer"的帧间状态。

### Phase 6: 切换默认 + 清理

- [ ] **Step 6.1: `ATOMCODE_TUIX_RETAINED` 默认 on**，保留 `=0` 能切回 AnsiRenderer
- [ ] **Step 6.2: 跑全套手测 checklist**（见 Verification 下）
- [ ] **Step 6.3: 删掉 DECSTBM 相关 dead code** — `sync_scroll_region` / `clear_scroll_region` / `emit_footer_absolute` / `emit_footer_diff`
- [ ] **Step 6.4: 删掉 AnsiRenderer 或保留作为 fallback**（看 Phase 6 完测情况）

---

## Verification

**测试 gate**（每 phase 必须过）：

1. `cargo build --workspace` 干净
2. `cargo test -p atomcode-tuix --lib` 全绿（目标保持 170+ 通过）
3. Phase 3 后: byte cost test assert 字节量不回退
4. Phase 4 后: streaming byte test assert 不回退
5. Phase 5 后: 新增"帧间无空窗"测试

**手测 checklist**（Phase 6 必走）：

- [ ] 启动 `atomcode --tuix` — welcome + input box
- [ ] 打字中英文混合 — "你好 hello 世界"
- [ ] `/` 菜单弹出 → Down/Up → Enter
- [ ] `/model` 切换模型
- [ ] `/provider` wizard 全流程
- [ ] `/login` → OAuth → 返回 → 打字（**关键：不再出现幽灵线**）
- [ ] `/logout` → `/login` 再次
- [ ] `/clear` 清屏 → 继续打字（**关键：不再出现"输入框消失"**）
- [ ] `/session` 新会话
- [ ] `/resume` session picker
- [ ] streaming 期间打字（type-ahead queue）
- [ ] agent edit 工具流 spinner 不卡
- [ ] Window resize 拖大拖小 — footer 重画正确
- [ ] panic / Ctrl+C 退出 — shell 滚动正常（DECSTBM 不残留）

**字节量 budget**（Mac Terminal 209 col）：

- keystroke 稳态：≤ 30 B
- menu open/close：≤ 950 B（物理下界约 880）
- menu nav：≤ 300 B
- streaming delta：≤ 60 B
- resize 全量重画：≤ 5000 B（一次性）

---

## Critical files

已熟悉，Phase 执行中主要改这几个：

- `crates/atomcode-tuix/src/render/ansi.rs` — 现有 immediate-mode，Phase 6 后变 dead code 或删
- `crates/atomcode-tuix/src/render/cell.rs` — Cell/diff/serialize，Phase 0 小改
- `crates/atomcode-tuix/src/render/screen.rs` — **新，Phase 1 核心**
- `crates/atomcode-tuix/src/render/retained.rs` — **新，Phase 2-5 核心**
- `crates/atomcode-tuix/src/render/mod.rs` — 加 pub mod，保留 Renderer trait 不动
- `crates/atomcode-tuix/src/lib.rs` — run() 里根据 env 选 renderer
- `crates/atomcode-tuix/src/event_loop/mod.rs` — Phase 5 加 16ms tick 调 flush_deferred

---

## 工期 & 风险

| Phase | 规模 | 风险 |
|---|---|---|
| 0 Cell 小改 | ~50 行 | Low |
| 1 Screen struct | ~300 行 | Medium — 新抽象 |
| 2 RetainedRenderer smoke | ~200 行 | Medium — Renderer trait 适配 |
| 3 footer 迁移 | ~400 行 | **High** — 视觉必须像素级对齐 |
| 4 body 迁移 | ~300 行 | **High** — scrollback 语义 |
| 5 async render loop | ~200 行 | Medium — 时序问题 |
| 6 切换 + 清理 | ~100 行 + 删除 | Low |
| **合计** | **~1550 行新 + ~800 行删除** | ~3 工作日 |

## 检查点（合作节奏）

每 Phase 完成后：
1. Commit + 一行摘要
2. 等你看（你不用读 diff，看摘要 + byte test 数据 + 手测结果）
3. 确认后进下一 Phase

遇到需要你拍板的（比如 Phase 4 的 scrollback 语义、Phase 5 的 flush 时机），我停下问。

**Phase 3 和 Phase 4 是高风险**，我可能要和你多次 sync 视觉细节。

## 不做的

- ratatui 或其他 TUI 框架迁移（我们已经有 cell / diff / serialize，自己写就够）
- 支持 bg color / underline / italic / 256-color（footer 用不到，Cell 结构扩展留给未来）
- 模拟 tmux scrollback（terminal 自己的 scrollback 够用）
- Cross-platform window system support（保持 Unix tty + Windows Console 现有范围）
- 完整 ANSI parser（我们不读 ANSI，只发）
