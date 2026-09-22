# Raster v1 Implementation Plan

> **For agentic workers:** 按任务顺序执行，每个任务以「写判据 → 跑红 → 实现 → 跑绿 → 提交」推进。

**Goal:** 在终端里画一块字符格位图，可原地刷新，且**只重画变化的行**。

**Design:** `docs/plans/2026-09-15-raster-blit-design.md`
**ADR:** `docs/adr/0027-raster-is-a-cell-grid.md`

**Worktree:** `.worktrees/session-welcome-block`，分支 `feat/session-welcome-block`

**测试：** `cargo nextest run -p atomcode-tui`（不用 `cargo test`）。门：`gates/tui.sh --fast`、`gates/tui-layers.sh`。

**本版范围（重要）**：**行级**增量；位图住**视图模块**（矩形来自布局）；不碰
`encode_rows` 的「先擦后画」不变量；不做格级 diff；不做键盘交互。

---

## Task 1：`raster.rs` —— 类型、校验、排版

**Files:** Create `crates/atomcode-tui/src/raster.rs`；Modify `lib.rs`（加 `pub mod raster;`）

### Step 1：先写判据

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Rect;

    /// 一格 `(码点, 前景, 背景)` 编成 12 字节小端。
    fn cell(ch: char, fg: u32, bg: u32) -> [u8; 12] {
        let mut out = [0u8; 12];
        out[0..4].copy_from_slice(&(ch as u32).to_le_bytes());
        out[4..8].copy_from_slice(&fg.to_le_bytes());
        out[8..12].copy_from_slice(&bg.to_le_bytes());
        out
    }

    fn b64(bytes: &[u8]) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn a_one_cell_raster_decodes_and_draws() {
        let cells = b64(&cell('\u{2588}', 0x00ff8800, 0x0100_0000));
        let r = Raster::decode(1, 1, &cells).expect("valid");
        let lines = r.lines_in(Rect::sized(1, 1));
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].plain(), "\u{2588}");
        // 背景是 `0x0100_0000`（只有 bit 24）＝终端默认色，不该写成某个颜色。
        assert_eq!(lines[0].spans[0].style.bg, None);
        assert_eq!(
            lines[0].spans[0].style.fg,
            Some(crate::frame::Color::rgb((0xff, 0x88, 0x00)))
        );
    }

    #[test]
    fn a_payload_that_is_not_base64_is_refused() {
        assert_eq!(Raster::decode(1, 1, "!!!!").unwrap_err(), RasterError::BadBase64);
    }

    #[test]
    fn a_payload_of_the_wrong_length_names_both_numbers() {
        let cells = b64(&cell('x', 0, 0)[..6]);
        assert_eq!(
            Raster::decode(1, 1, &cells).unwrap_err(),
            RasterError::BadLength { got: 6, want: 12 }
        );
    }

    #[test]
    fn an_illegal_cell_names_its_index() {
        // 第二格是宽字符 `中`。
        let mut bytes = cell('x', 0, 0).to_vec();
        bytes.extend_from_slice(&cell('\u{4E2D}', 0, 0));
        let err = Raster::decode(2, 1, &b64(&bytes)).unwrap_err();
        match err {
            RasterError::BadCell { index, why } => {
                assert_eq!(index, 1, "拒因必须点名是哪一格");
                assert!(why.contains('2'), "理由该说实测宽度: {why}");
            }
            other => panic!("expected BadCell, got {other:?}"),
        }
    }

    #[test]
    fn braille_and_ambigious_blocks_are_both_accepted() {
        // 5.2 那次更正的可执行版本:窄约定下宽度 1 就收。
        // braille 是 EAW=N(最安全的一档);`█` 是 A —— 与 UI 的框线同一假设。
        for ch in ['\u{2800}', '\u{2588}', '\u{2580}', '\u{2591}'] {
            let cells = b64(&cell(ch, 0, 0));
            Raster::decode(1, 1, &cells)
                .unwrap_or_else(|e| panic!("{ch:?} 应当被接受,却 {e:?}"));
        }
    }

    #[test]
    fn the_size_limit_is_a_hard_gate() {
        let ok = b64(&vec![0u8; MAX_COLUMNS as usize * 1 * 12]);
        assert!(Raster::decode(MAX_COLUMNS, 1, &ok).is_ok(), "上限本身要通过");
        let over = MAX_COLUMNS + 1;
        assert_eq!(
            Raster::decode(over, 1, "").unwrap_err(),
            RasterError::TooLarge { columns: over, rows: 1, max_columns: MAX_COLUMNS, max_rows: MAX_ROWS }
        );
    }

    #[test]
    fn only_the_rect_is_rendered_so_a_frame_costs_the_screen() {
        // 一块 4×4 的位图放在 2×2 的矩形里:只排 2 行、每行只截到 2 格。
        let mut bytes = Vec::new();
        for _ in 0..16 {
            bytes.extend_from_slice(&cell('\u{2588}', 0x00ff0000, 0x0100_0000));
        }
        let r = Raster::decode(4, 4, &b64(&bytes)).unwrap();
        let lines = r.lines_in(Rect::sized(2, 2));
        assert_eq!(lines.len(), 2);
        for line in &lines {
            assert_eq!(line.width(), 2, "按矩形裁列");
        }
    }

    #[test]
    fn adjacent_cells_of_one_style_become_one_span() {
        // 不是优化癖:一排同色实心块合成一个 Span,Span 数就从格数降到段数。
        let mut bytes = Vec::new();
        for _ in 0..4 {
            bytes.extend_from_slice(&cell('\u{2588}', 0x00ff0000, 0x0100_0000));
        }
        let r = Raster::decode(4, 1, &b64(&bytes)).unwrap();
        let lines = r.lines_in(Rect::sized(4, 1));
        assert_eq!(lines[0].spans.len(), 1, "同一样式的相邻格该合成一个 Span");
    }
}
```

### Step 2：跑红

```bash
cargo nextest run -p atomcode-tui -E 'test(raster)'
```
Expected: 编译失败（`cannot find type Raster`）。

### Step 3：实现

```rust
//! 字符格位图：一块 `(码点, 前景, 背景)` 的网格，可原地刷新。
//!
//! 不是图形协议。滚动、diff、能力降级、containment 都按**格子**走，见
//! `docs/adr/0027`。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;

use crate::frame::{Color, Line, Rect, Span, Style};
use crate::width;

/// 上限由算式给出，不要往上调就完事：一格 3 个 u32 = 12 字节，
/// 128×64 = 8192 格 = 96 KiB 原始数据。位图是「一块小部件」，不是一整屏。
pub const MAX_COLUMNS: u16 = 128;
pub const MAX_ROWS: u16 = 64;

/// 一格。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    /// 可打印、**窄约定下宽度 1** 的 BMP 码点（见 ADR 0027 决策③）。
    pub ch: char,
    /// `None` = 终端默认色（载荷里的 `0x0100_0000`）。
    pub fg: Option<Color>,
    pub bg: Option<Color>,
}

/// 一块位图。不可变：`write` 换掉整块，而不是就地改。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Raster {
    pub columns: u16,
    pub rows: u16,
    cells: Vec<Cell>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RasterError {
    BadBase64,
    BadLength { got: usize, want: usize },
    BadCell { index: usize, why: &'static str },
    TooLarge { columns: u16, rows: u16, max_columns: u16, max_rows: u16 },
    NotMounted { module: String, key: String },
    SizeMismatch { want: (u16, u16), got: (u16, u16) },
}

impl Raster {
    /// 解码并校验。拒因**点名到格**：一个坏格只说「非法」会让调用方在一万格里靠猜。
    pub fn decode(columns: u16, rows: u16, payload: &str) -> Result<Self, RasterError> {
        if columns == 0 || rows == 0 || columns > MAX_COLUMNS || rows > MAX_ROWS {
            return Err(RasterError::TooLarge {
                columns, rows, max_columns: MAX_COLUMNS, max_rows: MAX_ROWS,
            });
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .map_err(|_| RasterError::BadBase64)?;
        let want = columns as usize * rows as usize * 12;
        if bytes.len() != want {
            return Err(RasterError::BadLength { got: bytes.len(), want });
        }
        let mut cells = Vec::with_capacity(columns as usize * rows as usize);
        for (index, chunk) in bytes.chunks_exact(12).enumerate() {
            let word = |at: usize| u32::from_le_bytes(chunk[at..at + 4].try_into().expect("4 bytes"));
            let cp = word(0);
            let ch = char::from_u32(cp)
                .filter(|c| !c.is_control() && (c as u32) <= 0xFFFF)
                .ok_or(RasterError::BadCell { index, why: "not a printable BMP character" })?;
            let w = width::char_width(ch);
            if w != 1 {
                return Err(RasterError::BadCell {
                    index,
                    why: if w == 2 { "is 2 cells wide" } else { "is not one cell wide" },
                });
            }
            cells.push(Cell { ch, fg: colour(word(4))?, bg: colour(word(8))? });
        }
        Ok(Self { columns, rows, cells })
    }

    /// 排成 `rect` 能放下的那些行，每行截到 `rect.w` 格。
    ///
    /// **成本是矩形的，不是位图的**：一帧只排看得见的部分，与
    /// `LiveCache`「一帧的成本是屏幕的而不是答案的」同一取舍。
    /// v1 从第 0 行起排（位图内部不滚动）。
    pub fn lines_in(&self, rect: Rect) -> Vec<Line> {
        let rows = (rect.h as usize).min(self.rows as usize);
        let cols = (rect.w as usize).min(self.columns as usize);
        (0..rows)
            .map(|row| {
                let base = row * self.columns as usize;
                let slice = &self.cells[base..base + cols];
                // 相邻同色同字的格并成一个 Span：Span 数从格数降到段数。
                let mut spans: Vec<Span> = Vec::new();
                for cell in slice {
                    let style = style_of(cell);
                    match spans.last_mut() {
                        Some(last) if last.style == style => last.text.push(cell.ch),
                        _ => spans.push(Span::styled(cell.ch.to_string(), style)),
                    }
                }
                Line::from_spans(spans)
            })
            .collect()
    }
}

/// `0x00RRGGBB` 是颜色；`0x0100_0000`（只有 bit 24）是终端默认色；其余非法。
fn colour(word: u32) -> Result<Option<Color>, RasterError> {
    match word {
        0x0100_0000 => Ok(None),
        w if w & 0xff00_0000 == 0 => Ok(Some(Color::rgb((
            (w >> 16) as u8,
            (w >> 8) as u8,
            w as u8,
        )))),
        _ => Err(RasterError::BadCell { index: 0, why: "colour word is neither 0x00RRGGBB nor 0x01000000" }),
    }
}

fn style_of(cell: &Cell) -> Style {
    let mut s = Style::new();
    if let Some(fg) = cell.fg {
        s = s.fg(fg);
    }
    if let Some(bg) = cell.bg {
        s = s.bg(bg);
    }
    s
}
```

**两处实现细节**：`colour` 的 `index: 0` 是错的（调用方丢了下标）——改成把
`index` 传进去。`Line::width()` 要确认存在（`frame.rs:229` 有）。

### Step 4：跑绿 + 提交

```bash
cargo nextest run -p atomcode-tui -E 'test(raster)'
cargo fmt --all && git add -A && git commit -m "feat(tui): 字符格位图的类型、校验与排版 …"
```

---

## Task 2：`Rasters` 表 + `RastersView` 快照

**Files:** Modify `crates/atomcode-tui/src/raster.rs`

### Step 1：判据

```rust
#[test]
fn writing_to_a_raster_nobody_mounted_is_refused() {
    let r = Rasters::new();
    assert_eq!(
        r.write("pane", "main", "AA==").unwrap_err(),
        RasterError::NotMounted { module: "pane".into(), key: "main".into() }
    );
}

#[test]
fn a_write_of_the_wrong_size_is_refused_and_changes_nothing() {
    let r = Rasters::new();
    r.mount("pane", "main", tiny(2, 2)).unwrap();
    let before = r.revision();
    let err = r.write("pane", "main", &one_cell()).unwrap_err();
    assert!(matches!(err, RasterError::SizeMismatch { .. }), "{err:?}");
    assert_eq!(r.revision(), before, "被拒的写入不该让任何东西变");
}

#[test]
fn a_write_that_lands_bumps_the_revision_and_swaps_the_snapshot() {
    let r = Rasters::new();
    r.mount("pane", "main", tiny(1, 1)).unwrap();
    let before = r.view().get("pane", "main").cloned();
    r.write("pane", "main", &b64(&cell('x', 0, 0))).unwrap();
    let after = r.view().get("pane", "main").cloned();
    assert_ne!(before, after, "写进去了就该拿到新的那一块");
    assert!(r.revision() > 0);
}

#[test]
fn a_view_is_a_snapshot_so_a_later_write_cannot_change_it() {
    // 同一份 view 渲染两次必须看到同一幅画面 —— 与 caps/cwd 守的是同一个承诺。
    let r = Rasters::new();
    r.mount("pane", "main", tiny(1, 1)).unwrap();
    let held = r.view();
    r.write("pane", "main", &b64(&cell('y', 0, 0))).unwrap();
    let seen_twice = (
        held.get("pane", "main").map(|x| x.lines_in(Rect::sized(1, 1))[0].plain()),
        held.get("pane", "main").map(|x| x.lines_in(Rect::sized(1, 1))[0].plain()),
    );
    assert_eq!(seen_twice.0, seen_twice.1, "快照必须在两次渲染之间不动");
}
```

### Step 2：实现

```rust
/// 某一帧的位图集合。**不可变**，克隆是 N 次 Arc 计数，不是像素拷贝。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RastersView(Arc<std::collections::HashMap<(String, String), Arc<Raster>>>);

impl RastersView {
    pub fn get(&self, module: &str, key: &str) -> Option<&Arc<Raster>> {
        self.0.get(&(module.to_string(), key.to_string()))
    }
    pub fn is_empty(&self) -> bool { self.0.is_empty() }
}

/// 写入方用的表。宿主持有一个。
pub struct Rasters {
    current: Mutex<Arc<std::collections::HashMap<(String, String), Arc<Raster>>>>,
    revision: AtomicU64,
}

impl Rasters {
    pub fn new() -> Self { Self::default() }

    /// **O(1)**：把存着的那个 `Arc` 克隆一份。每帧调一次，所以重建只能发生在写入上。
    pub fn view(&self) -> RastersView {
        RastersView(self.current.lock().expect("rasters poisoned").clone())
    }

    pub fn revision(&self) -> u64 { self.revision.load(Ordering::Relaxed) }

    pub fn mount(&self, module: &str, key: &str, raster: Raster) -> Result<(), RasterError>;
    pub fn unmount(&self, module: &str, key: &str);
    /// 换掉已挂载位图的内容。未挂载、尺寸不符、校验不过——都拒，且帧不变。
    pub fn write(&self, module: &str, key: &str, payload: &str) -> Result<(), RasterError>;
}
```

`mount`/`write` 的公共部分是「改一格、换整张表」：

```rust
fn swap(&self, module: &str, key: &str, raster: Arc<Raster>) {
    let mut cur = self.current.lock().expect("rasters poisoned");
    let mut next = (**cur).clone();          // N 次 Arc 计数 + 两个 String
    next.insert((module.to_string(), key.to_string()), raster);
    *cur = Arc::new(next);
    self.revision.fetch_add(1, Ordering::Relaxed);
}
```

`write` 先取旧的那块拿尺寸、`decode` 成功后才 `swap`（**失败什么都不动**）。

**`Default for Rasters`** 手写（`Mutex<Arc<HashMap>>` 可以 derive，但显式写更清楚）。

---

## Task 3：接线 —— `Moment`、`Host`、服务、模块、行

**Files:** Modify `moment.rs` / `host.rs` / `plugin.rs` / `modules/mod.rs` / `rows.rs`；Create `modules/raster.rs`

### Step 1：`Moment` 加字段

```rust
// moment.rs
    /// 已挂载的位图，**这一帧的那一份**。
    ///
    /// 快照而不是把手：里面的 `Raster` 不可变，所以同一个 `Moment` 渲染两次看到
    /// 同一幅画面——与 `caps`、`cwd` 守的是同一个承诺。位图**不能**由模块自己去
    /// 表里取：`View::render` 拿不到任何服务（见 `docs/adr/0027` 决策①）。
    pub rasters: crate::raster::RastersView,
```

### Step 2：`Host` 持有表 + `compose` 放快照

```rust
// host.rs 字段
    /// 已挂载的位图。与 `asks` 并列：宿主持有，行通过服务写。
    pub rasters: Arc<crate::raster::Rasters>,
// Host::new
    rasters: Arc::new(crate::raster::Rasters::new()),
// compose
    let mut moment = self.moment.read().expect("moment poisoned").clone();
    // 位图经 `Moment` 到模块（`View::render` 拿不到服务），所以每帧把快照放进去。
    moment.rasters = self.rasters.view();
```

### Step 3：服务 + 模块 + 行

```rust
// plugin.rs
plexus_service!(RastersSvc => crate::raster::Rasters, "tui-rasters", Core,
    "Mounted cell-grid bitmaps, addressed by (module id, key)");
// TuiUiPlugin::apply 里，与其它 tui-* 一起
let _ = ctx.provide::<RastersSvc>(host.rasters.clone()).map_err(|e| e.to_string())?;
```

```rust
// modules/raster.rs
pub const ID: &str = "raster";
pub const KEY: &str = "main";

/// 画一块位图的视图模块。
///
/// 无状态、每帧重画——位图要的正是这个（`stream` 的块会冻结，而位图要能刷）。
pub struct RasterPane;

impl crate::module::View for RasterPane {
    type State = ();
    fn id() -> &'static str { ID }
    fn absorb(_: &mut (), _: &SessionEvent) {}
    fn render(_: &(), vp: &Viewport<'_>) -> Vec<Line> {
        vp.moment.rasters
            .get(ID, KEY)
            .map(|r| r.lines_in(vp.rect))
            .unwrap_or_default()
    }
    // height 用默认 (Fill)：矩形由布局树给（见 ADR 0027 决策①）。
}

#[cfg(test)]
mod tests { crate::tui_conformance!(view RasterPane as raster_pane_conformance); }
```

`rows.rs`：照 `panel!` 宏加一行 `RasterPanel` / `"tui-panel-raster"`，进 `catalog()`
与 `SCREEN`（**`disabled = true`**，opt-in，像 mascot/team）。

### Step 4：判据

```rust
// modules/raster.rs
#[test]
fn nothing_mounted_draws_nothing() {
    let m = Moment::default();
    let vp = Viewport::new(Rect::sized(10, 4), &m);
    assert!(RasterPane::render(&(), &vp).is_empty(), "没挂位图就不该占屏幕");
}

#[test]
fn a_mounted_raster_reaches_the_screen_through_the_moment() {
    let rasters = Rasters::new();
    rasters.mount(ID, KEY, tiny(3, 2)).unwrap();
    let m = Moment { rasters: rasters.view(), ..Default::default() };
    let vp = Viewport::new(Rect::sized(10, 4), &m);
    let lines = RasterPane::render(&(), &vp);
    assert_eq!(lines.len(), 2, "位图几行就画几行");
    assert!(lines[0].plain().contains('\u{2588}'));
}
```

---

## Task 4：验收 —— 只重画变化的行

**Files:** `host.rs` 的 `mod tests`（`ROWS_ENCODED` 已在 `ansi.rs`）

```rust
#[test]
fn an_unchanged_raster_encodes_no_row_and_a_write_encodes_only_its_own() {
    // `ROWS_ENCODED` 是现成的尺子:它量的是"一帧重编码了几行",而输出相等看不出
    // 这件事 —— 重编码一行不变的行会产出同样的字节。
    let mods = Arc::new(Modules::new());
    mods.add_view(Arc::new(Mounted::<crate::modules::raster::RasterPane>::new())).unwrap();
    let h = Host::new(mods, layout_with_raster());
    h.rasters.mount(crate::modules::raster::ID, crate::modules::raster::KEY, tiny(3, 2)).unwrap();

    let caps = crate::caps::Caps::default();
    let a = ansi::Lines::of(&h.compose((40, 12)), caps, None);
    // 没变的一帧:一行都不重编码。
    let before = ansi::ROWS_ENCODED.load(Ordering::Relaxed);
    let b = ansi::Lines::of(&h.compose((40, 12)), caps, Some(&a));
    assert_eq!(ansi::ROWS_ENCODED.load(Ordering::Relaxed), before,
        "位图没变,一行都不该重编码");
    assert!(b.patch_from(Some(&a)).is_empty(), "没变就没有补丁");

    // 只改第一格:只有位图那一行变。
    h.rasters.write(ID, KEY, &b64_one_cell('\u{2580}')).unwrap();
    let c = ansi::Lines::of(&h.compose((40, 12)), caps, Some(&b));
    let encoded = ansi::ROWS_ENCODED.load(Ordering::Relaxed) - before;
    assert_eq!(encoded, 1, "只该重编码位图变化的那一行");
}
```

**`ROWS_ENCODED` 是 `#[cfg(test)]` 的**（`ansi.rs`），所以这条判据必须在
`atomcode-tui` 的 test 构建里跑（是）。

**并配一条能力门的判据**：

```rust
#[test]
fn a_terminal_with_no_unicode_gets_no_raster() {
    // braille 不在降级表里(`caps.rs` 为 spinner 记过同一件事),所以位图没有
    // "上屏时自动降级"这条退路 —— 整组不画,而不是画成 tofu 或马赛克。
}
```

---

## 完成判据

| 承诺 | 去哪看 |
|---|---|
| 一块位图能画出来，逐格带 fg/bg | Task 1 的 `a_one_cell_raster_decodes_and_draws` |
| 坏载荷逐变体被拒，且**点名是哪一格** | Task 1 的 `an_illegal_cell_names_its_index` |
| 宽度判据是「窄约定下 1」而不是「非 Ambiguous」 | Task 1 的 `braille_and_ambigious_blocks_are_both_accepted` |
| 一帧的成本是矩形的不是位图的 | Task 1 的 `only_the_rect_is_rendered_...` |
| 未挂载/尺寸不符的写入被拒且**帧不变** | Task 2 的两条 |
| 快照不可变（同一 view 渲染两次一样） | Task 2 的 `a_view_is_a_snapshot_...` |
| 位图经 `Moment` 到模块 | Task 3 的 `a_mounted_raster_reaches_the_screen_through_the_moment` |
| **只重画变化的行** | Task 4 的 `ROWS_ENCODED` 那条 |
| 门与格式 | `gates/tui.sh --fast`、`gates/tui-layers.sh`（棘轮只许持平或降）、`cargo fmt --all -- --check` |

## 没做的事

- **格级 diff**（ADR 0027 决策⑤：先量化）
- **键盘交互**（`Moment.focus` 那条缝是独立工作）
- **随对话滚动的位图**（要动 ADR 0004）
- **图形协议**（ADR 0026）
- **ambiguous-as-wide 防御**（UI 级问题，独立一件事）
