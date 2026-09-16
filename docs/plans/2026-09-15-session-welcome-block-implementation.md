# 新 session 欢迎区块 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 atui 新 session 开局时，于流（对话）的头部插入一块欢迎内容，并把它做成一行插件、可整体替换/拿掉。

**Architecture:** 欢迎内容是一个**流生产者**产出的不可逆块（`kind = "welcome"`），走 `Producer` 而不是视图模块——因为它「发生一次、有历史」，滚上去还要看得见。产出时机是「流为空时」，由 `Host::open_conversation` 在 `Tui::run` 折完 resume 历史之后问每个生产者要一个开场块。为了让块能按终端能力决定形状（像素猫只在能画的终端上出现），`Content::lines` 从收 `width` 改成收 `RenderCtx { width: u16, caps: ShapeCaps }`，渲染缓存的键同步从 `width` 变成 `(width, ShapeCaps)`。

**Tech Stack:** Rust 2021、`atomcode-plexus` 插件树、`atomcode-harness` 的 session 事实、`cargo nextest`、`gates/tui.sh`。

**Worktree:** `/Users/lichao/project/gitcode/ai/atomcode/.worktrees/session-welcome-block`，分支 `feat/session-welcome-block`，基线 `0f7efde1`。

**Spec:** `docs/plans/2026-09-15-session-welcome-block-design.md`
（设计文档放 `docs/plans/` 而非 skill 默认的 `docs/superpowers/plans/`：后者整个目录在 `.gitignore:89` 里，受跟踪的设计文档一直落在 `docs/plans/*-design.md`。）

**ADR:** `docs/adr/0021-blocks-may-shape-by-terminal-capability.md`

**每条命令都在 worktree 根目录跑。** 全部命令前面都有 `cd /Users/lichao/project/gitcode/ai/atomcode/.worktrees/session-welcome-block &&` 的隐含前缀。

---

## 文件结构

| 文件 | 职责 | 本计划里怎么变 |
|---|---|---|
| `crates/atomcode-tui/src/block.rs` | 块的生命周期、渲染缓存、`Content` trait | `ShapeCaps`/`RenderCtx` 落在这里；缓存键换 | 
| `crates/atomcode-tui/src/caps.rs` | 终端能力探测、字形表 | 抽出 `pub fn glyph(unicode, Glyph)`，供 `ShapeCaps::g` 用 |
| `crates/atomcode-tui/src/host.rs` | 宿主：布局、行数、绘制、滚动 | `RowIndex` 加 caps 键；`compose`/`stream_height`/`scroll_limit` 造并传 ctx；新增 `open_conversation` |
| `crates/atomcode-tui/src/module.rs` | `View` / `Producer` 两个 trait | 新增 `Opening`、`Producer::opening` |
| `crates/atomcode-tui/src/content.rs` | 所有 `Content` 实现 | 11 个实现的签名跟着改；新增 `WelcomeBlock` |
| `crates/atomcode-tui/src/modules/welcome.rs` | 欢迎块的生产者 | **新建** |
| `crates/atomcode-tui/src/modules/mod.rs` | 模块清单 | 加 `pub mod welcome;` |
| `crates/atomcode-tui/src/rows.rs` | 一行一插件 | 加 `TuiWelcomePanel` + `SCREEN` 里一行 + `catalog()` |
| `crates/atomcode-tui/src/plugin.rs` | 装配与事件循环 | `Tui::run` 里构造 `Opening` 并调 `open_conversation` |
| `crates/atomcode-tui/src/text.rs` | 文本卫生 | 新增 `collapse_home_with` / `collapse_home` |
| `crates/atomcode-tui/tests/e2e.rs` | 端到端 | 加一条：开局有欢迎块、随对话滚走 |

---

## Phase 1 — 缝：`RenderCtx` 与缓存键

这三个 task 是**纯结构改动，不含任何新行为**。第 1 个 task 之后树必须仍然编译、测试全绿、行为逐字节不变（`ShapeCaps` 传下去但没人用）。

### Task 1: 引入 `ShapeCaps` / `RenderCtx`，`Content` 换签名

**Files:**
- Modify: `crates/atomcode-tui/src/block.rs:87-135`（`Content` trait）、`block.rs:259`（`rows_at`）
- Modify: `crates/atomcode-tui/src/caps.rs:360-404`（抽 `glyph()`）
- Modify: `crates/atomcode-tui/src/content.rs`（11 个 impl 里的 10 个 + 调用点）
- Modify: `crates/atomcode-tui/src/modules/steering.rs:140,208,265`、`modules/transcript.rs` 测试
- Modify: `crates/atomcode-tui/src/plugin.rs:1630,2012-2020`
- Modify: `crates/atomcode-tui/src/host.rs:1582,1623,2969,3058`

- [ ] **Step 1: 在 `block.rs` 里加 `ShapeCaps` 与 `RenderCtx`**

插在 `Content` trait 定义之前（`block.rs:82` 那句 doc 的上方）：

```rust
/// 块能看到的那部分终端能力：**只放决定形状或存在的位**。
///
/// 不含 `palette`。颜色不是块决定的：块写 `Color::Role(Role)`，真颜色由
/// [`crate::ansi::encode_with`] 上屏时解析——`frame.rs:143-149` 的 doc 为这条
/// 写了理由（块不知道终端是明是暗，把那个答案穿进每个 `lines` 只会让它们每一个
/// 都可能弄错）。而且 `Palette` 由 `measure_palette()` 测出来、随终端主题变，
/// 缓存键带上它就等于**切一次主题击穿全部已结算块的缓存**。
///
/// 将来给 [`crate::caps::Caps`] 加位时问一句「它决定形状吗」：是，加到这里
/// （并跟着缓存键走）；否，别加。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShapeCaps {
    pub unicode: bool,
    pub colors: crate::caps::Colors,
}

impl ShapeCaps {
    /// 从终端测得的完整能力里取形状那部分。
    pub fn of(caps: &crate::caps::Caps) -> Self {
        Self {
            unicode: caps.unicode,
            colors: caps.colors,
        }
    }

    /// 装饰字形在**这个**终端上的写法。
    ///
    /// 与 [`crate::caps::Caps::g`] 同一张表，见那里的 doc。块里写装饰符必须走
    /// 这里，不能写字面——`gates/tui-layers.sh` 数上层源码里的字面装饰符。
    pub fn g(&self, glyph: crate::caps::Glyph) -> &'static str {
        crate::caps::glyph(self.unicode, glyph)
    }
}

/// 渲染一个块所需要的全部输入。
///
/// 两样，都是枚举得出的：宽度，和决定形状的那部分能力。**不收 `Moment`**——
/// 那会把输入、滚动位置、动画相位一起拖进来，而那些不该影响块画什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderCtx {
    pub width: u16,
    pub caps: ShapeCaps,
}

impl RenderCtx {
    /// 只要宽度的那条路：形状能力取「全都能画」。
    ///
    /// 给测试与那些不关心形状的 `Content` 用。这不是一个偷偷降级的默认——
    /// `Caps::default()` 的语义是「现代终端」，见 `caps.rs:105-108`。
    pub fn bare(width: u16) -> Self {
        Self {
            width,
            caps: ShapeCaps::of(&crate::caps::Caps::default()),
        }
    }
}
```

- [ ] **Step 2: 在 `caps.rs` 里把字形表抽成自由函数**

把 `Caps::g`（`caps.rs:360-404`）整个替换成两步。先加自由函数（放在 `impl Caps` 之后）：

```rust
/// 一个装饰字形在「支持/不支持 unicode」两种情况下的写法。
///
/// 抽成自由函数是因为 [`crate::block::ShapeCaps`] 也要用它：块拿不到完整的
/// `Caps`（里面还有 palette），但字形只取决于 `unicode` 一位。
///
/// 两张表都是**一列进、一列出**——这是替换完还能保持对齐的前提，也是这张表
/// 只收窄字符的原因（见 `ascii_for` 的 doc）。
pub fn glyph(unicode: bool, glyph: Glyph) -> &'static str {
    // 原 `Caps::g` 的两个 match 分支原样搬过来，`self.unicode` 换成 `unicode`。
    /* 整块照抄 caps.rs:360-404 的 match，不改一个字形 */
}
```

然后 `Caps::g` 变成一行委托：

```rust
    pub fn g(&self, glyph: Glyph) -> &'static str {
        glyph_(self.unicode, glyph)
    }
```

**注意命名冲突**：自由函数叫 `glyph`，参数也叫 `glyph`。用 `use crate::caps::glyph as glyph_of;` 或在 `impl Caps` 内部写全路径 `crate::caps::glyph(self.unicode, glyph)`。**别用下划线后缀糊过去**，写全路径最清楚。

- [ ] **Step 3: 改 `Content` trait 的三个方法**

```rust
    /// Render at a width, on a terminal with these capabilities. Called every
    /// frame; must be pure.
    ///
    /// The capabilities are here for one thing only: a block whose *existence or
    /// shape* depends on what the terminal can draw. Glyphs that merely need
    /// downgrading do not need this — [`crate::ansi::write_line`] swaps them on
    /// the way out, one column in and one column out. See `docs/adr/0021`.
    fn lines(&self, ctx: &RenderCtx) -> Vec<Line>;
```

`summary` 的默认实现（`block.rs:121-123`）：

```rust
    fn summary(&self, ctx: &RenderCtx) -> Line {
        self.lines(ctx).into_iter().next().unwrap_or_default()
    }
```

`Slot::rows_at`（`block.rs:259`）：

```rust
    pub fn rows_at(&self, ctx: &RenderCtx) -> (usize, Option<Arc<Vec<Line>>>) {
```

函数体内**本 task 不做缓存键改动**，把 `width` 换成 `ctx.width` 即可（`CachedRender.width`/`Settled.rows` 的键保持 `u16`，留给 Task 2）。三处替换：
- `block.rs:263` → `let lines = b.content.lines(ctx);`
- `block.rs:273` 的 `c.width == width` → `c.width == ctx.width`
- `block.rs:284,295` 的 `render_settled(..., width, base)` → `ctx.width`
- `block.rs:297,313` 的 `width` → `ctx.width`
- `block.rs:317` → `let lines = s.block.content.lines(ctx);`

- [ ] **Step 4: 机械迁移 11 个 `Content` 实现**

**只有 10 个在 `content.rs`**（第 11 个是 `plugin.rs:2012` 的测试用 `ToolOutput`）。每个的改法完全一样：签名换掉，函数体第一行加一句取出宽度。

```rust
    fn lines(&self, ctx: &RenderCtx) -> Vec<Line> {
        let w = ctx.width;
        /* 原函数体一字不改 */
    }
```

`content.rs` 里 10 处（行号是 `fn lines` 的位置）：`108`、`150`、`179`、`536`、`664`、`689`、`781`、`851`、`1059`，外加 `TurnEndBlock`（`1025` 那个 impl 里的 `lines`）。每处的参数名原本是 `w: u16`——**直接改成 `ctx: &RenderCtx` 并在体内 `let w = ctx.width;`**，这样函数体一个字都不用动。

`content.rs` 顶部加 `use crate::block::{Content, ContentHash, RenderCtx};`（按现有 use 的实际写法调整）。

同样处理：
- `block.rs:483-490` 的测试用 `Text`
- `plugin.rs:2012-2020` 的测试用 `ToolOutput`
- `host.rs:2969`、`host.rs:3058` 的测试用块

- [ ] **Step 5: 机械迁移调用点**

| 位置 | 改成 |
|---|---|
| `block.rs:122` | `self.lines(ctx)` |
| `block.rs:263`、`block.rs:317` | `…lines(ctx)` |
| `block.rs:511` 等 `block.rs` 测试 | `…lines(&RenderCtx::bare(80))` |
| `plugin.rs:1630` | `.lines(&RenderCtx::bare(width.max(20)))` ← 此处在测试里 |
| `host.rs:1582` | `block.content.summary(&ctx)` |
| `host.rs:1623` | `block.content.lines(&ctx)` |
| `modules/steering.rs:140,208,265` | `UserSaid(…).lines(&RenderCtx::bare(width))` |
| `modules/transcript.rs` 的 5 处测试 | `…lines(&RenderCtx::bare(80))` 等，宽度照原值 |
| `content.rs` 的 ~20 处测试 | 同上，宽度照原值 |

**迁移时注意**：`host.rs` 的 `render_rows`/walk 里已经有局部变量 `room: u16`（如 `host.rs:1574`），而 `summary`/`lines` 需要的宽度就是 `room`。本 task 用 `RenderCtx::bare(room)` 过度；Task 3 再把真正的 ctx（含 caps）从上游传进来。**先 bare，后真值**，是为了让这个 task 的 diff 全是机械替换、行为零变化。

**别碰**：`ask.rs`、`commands.rs`、`surface.rs`、`text.rs`、`width.rs` 里的 `.lines()`——那是 `str::lines`/`String::lines`。判据：`grep -rn "\.lines(" crates/atomcode-tui/src/` 里剩下的 4 处 `text.lines()`/`s.lines()`/`.lines()`。

- [ ] **Step 6: 编译 + 全测试**

```bash
cargo nextest run -p atomcode-tui
```

Expected: 全部通过，条数与改动前一致（这一步不加也不减判据）。

若报 `expected u16, found &RenderCtx` 之类，说明漏了一个调用点——**按报错补，不要改用 `From`/`Into` 让两种都能收**：那样旧写法会活得比这次迁移久。

- [ ] **Step 7: 确认行为逐字节不变**

```bash
cargo run -p atomcode-tui --bin atui -- --demo > /tmp/after.txt
git stash && cargo run -p atomcode-tui --bin atui -- --demo > /tmp/before.txt && git stash pop
diff /tmp/before.txt /tmp/after.txt && echo "逐字节相同"
```

Expected: `逐字节相同`。（`--demo` 是 headless 打一帧，见 `lib.rs` 的 doc。）

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -F - <<'EOF'
refactor(tui): 块渲染收 RenderCtx 而非裸宽度

纯结构改动,行为零变化(拿 --demo 的前后两帧比过,逐字节相同)。

`Content::lines`/`summary` 与 `Slot::rows_at` 改收
`RenderCtx { width, ShapeCaps }`。`ShapeCaps` 只含 `unicode` 与 `colors`
两位,是**决定形状或存在**的那部分能力;不含 `palette`,因为颜色不该由块
决定——`frame.rs:143-149` 已经写下这条禁令,而 `Palette` 每次测量都会变,
缓存键带上它就等于切一次主题击穿全部已结算块的缓存。

缓存键这一步没动(仍是宽度),因为此刻没有任何块按能力变形状;下一个 commit
换键并配判据。在那之前这个改动是可证零行为的。

`caps.rs` 的字形表抽成自由函数 `glyph(unicode, Glyph)`,好让 `ShapeCaps`
也用同一张表——块拿不到完整的 Caps,但字形只取决于 unicode 一位。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

---

### Task 2: 渲染缓存换键，配两条守卫判据

**Files:**
- Modify: `crates/atomcode-tui/src/block.rs:182-190`（`CachedRender`）、`block.rs:209-215`（`Settled`）、`block.rs:259-322`（`rows_at`）
- Test: `crates/atomcode-tui/src/block.rs` 的 `mod tests`

先写失败的判据。这两条是整个改动里唯一会变成静默 bug 的地方，所以它有两条。

- [ ] **Step 1: 写判据一（换能力必须重渲染）**

加进 `block.rs` 的 `mod tests`：

```rust
    /// 一个只在「终端能画 unicode」时才有两行的块。
    ///
    /// 用真的会变的行为来测缓存键:不用它,判据会在「缓存键没换」时也绿。
    #[derive(Debug)]
    struct TwoRowsWhenUnicode;

    impl Content for TwoRowsWhenUnicode {
        fn kind(&self) -> &'static str {
            "two_rows"
        }
        fn content_hash(&self) -> ContentHash {
            // 形状不进 hash:与宽度同类,见 Content 的契约。
            hash_of(&["two_rows"])
        }
        fn lines(&self, ctx: &RenderCtx) -> Vec<Line> {
            let n = if ctx.caps.unicode { 2 } else { 1 };
            (0..n).map(|_| Line::raw("x")).collect()
        }
    }

    #[test]
    fn a_change_of_terminal_capability_is_not_a_cache_hit() {
        let mut s = Stream::new();
        let id = s.writer("test").emit(Coord::default(), Arc::new(TwoRowsWhenUnicode));
        s.settle_all();

        let unicode = RenderCtx {
            width: 40,
            caps: ShapeCaps {
                unicode: true,
                colors: crate::caps::Colors::Ansi256,
            },
        };
        let ascii = RenderCtx {
            caps: ShapeCaps {
                unicode: false,
                ..unicode.caps
            },
            ..unicode
        };

        let slot = s.get(id).unwrap();
        assert_eq!(slot.rows_at(&unicode).0, 2);
        assert_eq!(
            slot.rows_at(&ascii).0,
            1,
            "同一宽度、不同能力必须重新渲染,而不是命中缓存的行数 —— 换终端后\
             仍是旧行数,而且它看起来是好的"
        );
        // 换回去仍然是 2,说明两次都不是撞上的。
        assert_eq!(slot.rows_at(&unicode).0, 2);
    }
```

- [ ] **Step 2: 跑判据一，确认它失败**

```bash
cargo nextest run -p atomcode-tui -E 'test(a_change_of_terminal_capability_is_not_a_cache_hit)'
```

Expected: FAIL，`assertion left == right failed: 2 != 1`（缓存键还是宽度，ascii 那次命中了 unicode 的旧答案）。

- [ ] **Step 3: 换缓存键**

`CachedRender`（`block.rs:182-190`）的 `width: u16` 改成：

```rust
    /// 渲染这些行时用的 key：宽度 + 形状能力。
    ///
    /// 两者都是「换一个就是另一个问题」。**不含 palette**——颜色不在这里决定
    /// （见 [`ShapeCaps`]），所以切主题不会走到这个分支。
    key: (u16, ShapeCaps),
```

`Settled`（`block.rs:209-215`）的 `rows: RwLock<Option<(u16, usize)>>` 改成：

```rust
    /// `(width, caps, rows)`。三个数一起走，所以换终端或换宽度都是**另一个
    /// 问题**，而不是一个过期的答案。
    rows: RwLock<Option<(u16, ShapeCaps, usize)>>,
```

`rows_at` 里对应的比较与写入：
- `Some(mut c) if c.width == width && text.starts_with(&c.source)` → `Some(mut c) if c.key == (ctx.width, *ctx.caps_ref())`，并在命中分支里 `c.key.0` 或局部 `let width = ctx.width;` 供 `render_settled` 用
- 构造 `CachedRender` 的 `width,` → `key: (ctx.width, *ctx.caps_ref()),`
- `Settled` 那条：`if let Some((w, n)) = *measured { if w == width …` → `if let Some((w, c, n)) = *measured { if w == ctx.width && c == *ctx.caps_ref() { return (n, None); } }`，写入 `*measured = Some((ctx.width, *ctx.caps_ref(), rows));`

`RenderCtx` 加一个小访问器（或直接 `ctx.caps` 是 `ShapeCaps: Copy`，写 `c.key == (ctx.width, ctx.caps)` 即可）。**用后者**，不需要 `caps_ref`。

- [ ] **Step 4: 跑判据一，确认通过**

```bash
cargo nextest run -p atomcode-tui -E 'test(a_change_of_terminal_capability_is_not_a_cache_hit)'
```

Expected: PASS。

- [ ] **Step 5: 写判据二（阴性对照：换主题不得击穿缓存）**

```rust
    #[test]
    fn a_theme_change_does_not_invalidate_a_settled_block() {
        // 主题住在 palette 里,而 palette 不在 RenderCtx 里 —— 所以换个主题,
        // 同一个块的 `rows_at` 必须命中缓存。这条是 ShapeCaps 而非 Caps 的
        // 理由的可执行版本:缓存键若带上 palette,切一次主题就要重渲染全部
        // 已结算块,而屏幕上没有一个字变了。
        let mut s = Stream::new();
        let id = s.writer("test").emit(Coord::default(), Arc::new(TwoRowsWhenUnicode));
        s.settle_all();

        let a = RenderCtx::bare(40);
        let slot = s.get(id).unwrap();
        assert_eq!(slot.rows_at(&a).0, 2);
        // `bare` 已经固定了 ShapeCaps,所以两次调用之间唯一可能变的是时间——
        // 于是断言的是「缓存确实生效」:第二次不重新渲染。
        assert_eq!(slot.rows_at(&a).1, None, "已结算块在同一个 key 上必须命中缓存");
    }
```

**这条判据的诚实边界**：`ShapeCaps` 里根本没有 palette 字段，所以它测不出「palette 变了会不会击穿」——它测的是**类型里没有 palette 这件事的后果**（同 key 命中）。真正的保险是 `ShapeCaps` 的定义本身 + `frame.rs:143-149` 的 doc。**别把它写成一条它做不到的判据**（比如伪造一个 palette 不同的 `Caps`），那会让判据看起来覆盖了更多。

- [ ] **Step 6: 跑判据二，确认通过**

```bash
cargo nextest run -p atomcode-tui -E 'test(a_theme_change_does_not_invalidate_a_settled_block)'
```

Expected: PASS。

- [ ] **Step 7: 跑该 crate 全部测试**

```bash
cargo nextest run -p atomcode-tui
```

Expected: 全绿。

- [ ] **Step 8: Commit**

```bash
git add -A
git commit -F - <<'EOF'
fix(tui): 渲染缓存按 (宽度, 形状能力) 键,并配两条守卫

块的行数从此可以取决于终端能力,而缓存只按宽度索引的话,"换了终端仍是旧
行数"会稳定复现 —— 而且它看起来是好的。所以键跟着换:`CachedRender` 的
`width` 与 `Settled.rows` 的 `(width, rows)` 都变成带 `ShapeCaps` 的元组。

两条判据,一条一个方向:
- 换 ShapeCaps 必须重渲染(用一个只在 unicode 下才两行的块来测,不用真块
  —— 否则缓存键没换时它也绿);
- 同一 key 必须命中缓存(阴性对照,否则"每次重渲染"也能让上一条绿)。

第二条的边界写进注释了:它测的是"类型里没有 palette"的后果,不是"palette
变了会怎样"。后者靠 ShapeCaps 的定义本身。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

---

### Task 3: 把真正的 `RenderCtx` 从宿主送到块

Task 1 用 `RenderCtx::bare` 过度，本 task 让 caps 真的流进来。

**Files:**
- Modify: `crates/atomcode-tui/src/host.rs:72-77,124-126`（`RowIndex`）、`host.rs:2004-2071`（`row_index`）、`host.rs:1519`（调用点）、`host.rs:1700-1710`（`compose`）、`host.rs:1885-1945`（`stream_rows`/`scroll_limit`/`stream_height`）、`host.rs:1582,1623`、`host.rs:1951`（`forget_row_index`）

- [ ] **Step 1: 写判据（能力影响行数时，`stream_height` 跟着变）**

加进 `host.rs` 的 `mod tests`：

```rust
    /// 在 `ShapeCaps` 变化时行数会变的块。
    struct CapsSized;

    impl crate::block::Content for CapsSized {
        fn kind(&self) -> &'static str {
            "caps_sized"
        }
        fn content_hash(&self) -> crate::block::ContentHash {
            crate::block::hash_of(&["caps_sized"])
        }
        fn lines(&self, ctx: &crate::block::RenderCtx) -> Vec<Line> {
            let n = if ctx.caps.unicode { 3 } else { 1 };
            (0..n).map(|_| Line::raw("sized")).collect()
        }
    }

    #[test]
    fn the_row_index_is_keyed_on_capability_not_only_width() {
        // 行数的缓存若只按宽度键,换一套能力之后 `stream_height` 会报旧行数,
        // 而画面已经按新行数画了 —— 滚动上界与画面对不上,正是这条棘轮存在
        // 的理由(同一个数字有两个答案)。
        let mods = Arc::new(Modules::new());
        mods.add_producer(transcript::Transcript::new()).unwrap();
        let h = Host::new(mods, default_layout());

        h.stream
            .write()
            .expect("stream poisoned")
            .writer("test")
            .emit(Coord::default(), Arc::new(CapsSized));

        let mut m = h.moment.write().unwrap();
        m.caps.unicode = true;
        drop(m);
        let unicode = h.stream_height((40, 10), &h.moment.read().unwrap());

        let mut m = h.moment.write().unwrap();
        m.caps.unicode = false;
        drop(m);
        let ascii = h.stream_height((40, 10), &h.moment.read().unwrap());

        assert_eq!(
            (unicode, ascii),
            (3, 1),
            "换终端能力后行数必须重算:只按宽度键会让第二个数仍是 3"
        );
    }
```

**若 `h.stream` 不是 `pub`**：`Host` 的 `stream` 字段是私有的（`host.rs:944`）。改用测试所在的同一模块可见性——`host.rs` 的 `mod tests` 在同一个文件里，`use super::*` 之后就够得着私有字段。上面写的是 `h.stream`，若编译器说不可见就写 `h.stream.write()` 的等价路径；**不要为了测试把字段改成 `pub`**。

- [ ] **Step 2: 跑判据，确认它失败**

```bash
cargo nextest run -p atomcode-tui -E 'test(the_row_index_is_keyed_on_capability_not_only_width)'
```

Expected: FAIL，`assertion failed: (3, 1) == (3, 3)`（第二个数仍是缓存的 3）。

- [ ] **Step 3: `RowIndex` 加 caps 键**

`RowIndex`（`host.rs:72-77` 附近）在 `width: u16` 旁边加一个字段：

```rust
    /// 行数缓存的另一半键。与 `width` 同类:换一个就是另一个问题。
    ///
    /// 少了它,一块的行数按能力变时 `stream_height` 会报旧行数而画面按新行数
    /// 画 —— 滚动上界与画面对不上。
    caps: crate::block::ShapeCaps,
```

初始化（`host.rs:961-968`）里加 `caps: crate::block::ShapeCaps::of(&crate::caps::Caps::default()),`。

`forget_row_index`（`host.rs:1951`）把 `width = 0` 之外**同时**把 `caps` 设成一个「与任何真实终端都不同」的值。**别只把 width 设 0 指望它够**——那正是「缓存只按宽度失效」的老毛病。改法是给 `ShapeCaps` 加一个测试专用的哨兵：

```rust
impl ShapeCaps {
    /// 一个任何真实终端都不会等于的值,给"这条缓存作废了"用。
    ///
    /// 不用「width 设 0」那招的替代品宽度 —— 作废必须是**两个键都作废**,
    /// 否则改能力的那条路会命中一个按宽度算有效的旧答案。
    pub const NONE: ShapeCaps = ShapeCaps {
        unicode: false,
        colors: crate::caps::Colors::None,
    };
}
```

**注意**：`Caps::plain()`（`caps.rs:120`）也正好是 `unicode: false, colors: None`——所以 `NONE` 与「一个纯 ASCII 无色彩终端」撞了，那么一个真的在那种终端上的回合会每次都重算行数（正确但慢）。**这不接受**。改成给 `ShapeCaps` 加第三个私有字段做哨兵，或让 `forget_row_index` 直接 `idx.measured.clear(); idx.rows.clear();` 而非依赖键不等——**用后者**：清空本来就等价于作废，而它不引入任何假值。于是 `forget_row_index`：

```rust
    fn forget_row_index(&self) {
        let mut idx = self.row_index.lock().expect("row index poisoned");
        idx.width = 0;
        // 清空才是真的作废。上一版只把 width 设 0 就指望它够 —— 而行的形状
        // 现在也取决于能力,所以"哪一维变了"不再是一个可以逐个比的判断。
        idx.measured.clear();
        idx.rows.clear();
        idx.skip_from.clear();
        idx.total = 0;
    }
```

（`ShapeCaps::NONE` 那条就不加了——上面这段是取代它的方案。**若你在实现时发现 `forget_row_index` 的语义需要缓存留着**,改回加哨兵字段,但别用会和真实终端撞的值。）

- [ ] **Step 4: `row_index` 收 ctx**

签名（`host.rs:2004-2009`）`width: u16` → `ctx: &crate::block::RenderCtx`；失效判据：

```rust
        if idx.width != ctx.width || idx.caps != ctx.caps || idx.presentation != pres.revision() {
            idx.measured.clear();
            idx.rows.clear();
            idx.width = ctx.width;
            idx.caps = ctx.caps;
            idx.presentation = pres.revision();
        }
```

内部的 `let room = width.saturating_sub(inset(b.kind()));` → `ctx.width.saturating_sub(...)`；`lid_row(...)` 与 `rows_at` 的调用改成收 `ctx`（`lid_row` 的 `room: u16` 参数保持 `u16`，传 `room` 即可）。

`lid_row`（`host.rs:583-595` 附近）里的 `slots[i].rows_at(room).0` 改成接收 ctx 再传——把 `lid_row` 的签名从 `(…, room: u16, …)` 改成 `(…, ctx: &RenderCtx, …)`，内部自己算 `room`。

- [ ] **Step 5: 造 ctx 的地方只有三处，都从手上的 `moment` 造**

`compose`（`host.rs:1700`）在 `let moment = …clone();` 之后：

```rust
        // 一个块的形状可能与终端能力有关,所以这一帧的 ctx 从这一份 moment 造
        // 一次、往下传 —— 不在 `rows_at` 里读锁,也不每处各造一个(`stream_height`
        // 的契约是调用方可能正持 moment 写锁,`plugin.rs` 的两处确实如此)。
        let ctx = crate::block::RenderCtx {
            width: /* 该处的宽度,即原来传的 `room` 的来源 */,
            caps: crate::block::ShapeCaps::of(&moment.caps),
        };
```

**宽度那一步要看清**：`row_index` 用的是 `rect.w`（`host.rs:1519`：`self.row_index(rect.w, stream.slots(), &pres)`），而每一块自己的 `room` 是 `rect.w - inset(kind)`。所以边界在 `row_index(rect.w, …)` 与块自己那句 `let room = rect.w.saturating_sub(pad);`（`host.rs:1573-1574`）之间：**`row_index` 收的 ctx 用 `rect.w`，块渲染时把 `room` 换成一个只改宽度的 ctx**（`RenderCtx { width: room, ..ctx }`）。两处宽度不同是**故意的、本来就有的**——`host.rs:2033-2035` 的注释写着「a row count is only the painter's if it was measured at the width the painter draws at」。

- [ ] **Step 6: `stream_height` / `scroll_limit` / `stream_rows` 传 ctx**

这三个的签名保持收 `&Moment`（`docs/adr/0020` 定的契约），内部从 `moment` 造 ctx 再调 `row_index`。`stream_rows`（`host.rs:1885`）里若有 `rows_at`/`lines` 的调用，同样换成 ctx。

`host.rs:1623` 的兜底 `Arc::new(block.content.lines(&ctx))` 用**块自己的那个 ctx**（宽度是 `room`），不是 `row_index` 的那个——否则会造出「缓存说 3 行、实际画 5 行」。

- [ ] **Step 7: 跑判据，确认通过**

```bash
cargo nextest run -p atomcode-tui -E 'test(the_row_index_is_keyed_on_capability_not_only_width)'
```

Expected: PASS。

- [ ] **Step 8: 全测试 + 行为不变**

```bash
cargo nextest run -p atomcode-tui && cargo run -p atomcode-tui --bin atui -- --demo | head -20
```

Expected: 全绿；`--demo` 打出一帧（与 Task 1 的 `/tmp/after.txt` 相同——caps 在 headless 下是默认值，没有块按形状变）。

- [ ] **Step 9: Commit**

```bash
git add -A
git commit -F - <<'EOF'
fix(tui): 行数缓存也按能力键,并且真的作废

`RowIndex` 加上 `caps` 作为第二个键。少了它,一块的行数按能力变时
`stream_height` 报旧行数而画面按新行数画 —— 滚动上界与画面对不上,正是
"同一个数字有两个答案"那类 bug。

`forget_row_index` 原来只把 width 设 0 就指望它够;行的形状现在也取决于
能力,所以"哪一维变了"不再是能逐个比的判断,改成清空。

ctx 从手上的 moment 造一次往下传,不在 `rows_at` 里读锁 —— 与 `stream_height`
收 `&Moment` 是同一条契约(调用方可能正持 moment 写锁)。

宽度有两处且不同是本来就有的事:`row_index` 用 `rect.w`,块自己用
`rect.w - inset(kind)`,而块的行数只有按它自己被量的宽度算才是画家的数。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

---

## Phase 2 — 生产者、行与装配

### Task 4: `modules/welcome.rs`：生产者，含 tips 的筛选与摇号

**Files:**
- Create: `crates/atomcode-tui/src/modules/welcome.rs`
- Modify: `crates/atomcode-tui/src/modules/mod.rs`（加 `pub mod welcome;`）
- Modify: `crates/atomcode-tui/Cargo.toml`（加 `rand`）

- [ ] **Step 1: 加依赖**

`crates/atomcode-tui/Cargo.toml` 的 `[dependencies]` 里，挨着其他第三方依赖：

```toml
# 摇提示用。与 tuix 同一个版本 —— 两个前端对同一件事用不同大版本，会让
# 这个 workspace 里同时存在两套 rand（tuix 已在用），而收益是零。
rand = "0.8"
```

- [ ] **Step 2: 写失败判据**

`modules/welcome.rs` 底部：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::Command;

    fn cmds(names: &[&'static str]) -> Vec<Command> {
        names
            .iter()
            .map(|n| Command::new(n, "说明"))
            .collect()
    }

    #[test]
    fn tips_only_name_commands_the_screen_actually_has() {
        // 这是这个模块存在的理由:tuix 的池子钉着 `/provider`、`/webui`、
        // `/plan` 这些 atui 未必挂上的命令,照搬会推荐不存在的命令。
        let tips = super::choose_tips(&cmds(&["resume", "help"]), "~/proj");
        for (cmd, _) in &tips {
            assert!(
                ["/resume", "/help"].contains(&cmd.as_str()),
                "推荐了屏幕上没有的命令: {cmd}"
            );
        }
    }

    #[test]
    fn the_pinned_first_choice_yields_when_the_screen_has_no_such_command() {
        // tuix 把 /login 钉在第一位;atui 的命令集里没有 login。钉住的位必须在
        // 命令不在时让出来,而不是推荐一条不存在的命令。
        let without = super::choose_tips(&cmds(&["resume", "help"]), "~/proj");
        assert!(
            !without.iter().any(|(c, _)| c == "/login"),
            "屏幕上没有 /login,不该出现: {without:?}"
        );

        let with = super::choose_tips(&cmds(&["login", "resume", "help"]), "~/proj");
        assert_eq!(with[0].0, "/login", "有这个命令时它仍然排第一");
    }

    #[test]
    fn the_same_directory_picks_the_same_tips() {
        // 摇号只读了 cwd 一次,所以同一目录同一二进制每次一致 —— 否则每帧摇一次,
        // 块会自己变(Content 的契约要求纯),tuix 的 `welcome_tip_indices` 就是
        // 为这件事存在的。
        let all = cmds(&["resume", "help", "rows", "skills", "mcp", "model", "audit"]);
        let a = super::choose_tips(&all, "~/proj");
        let b = super::choose_tips(&all, "~/proj");
        assert_eq!(a, b);
    }

    #[test]
    fn tips_are_distinct_and_capped() {
        let all = cmds(&["resume", "help", "rows", "skills", "mcp", "model", "audit"]);
        let tips = super::choose_tips(&all, "~/proj");
        assert!(tips.len() <= super::MAX_TIPS + 1, "至多 4 条(1 固定 + 3 随机)");
        let mut names: Vec<&str> = tips.iter().map(|(c, _)| c.as_str()).collect();
        let n = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), n, "不重复: {tips:?}");
    }

    #[test]
    fn an_empty_command_table_produces_no_tips() {
        assert!(super::choose_tips(&[], "~/proj").is_empty());
    }

    #[test]
    fn the_description_comes_from_the_command_itself() {
        // 说明文字取 `Command::about`,不另写一份 —— 否则命令的帮助改了、欢迎页
        // 还是旧的。
        let tips = super::choose_tips(&cmds(&["resume"]), "~/proj");
        assert_eq!(tips[0].1, "说明");
    }
}
```

- [ ] **Step 3: 跑，确认失败**

```bash
cargo nextest run -p atomcode-tui -E 'test(tips) or test(welcome)'
```

Expected: FAIL，`cannot find function choose_tips`。

- [ ] **Step 4: 实现**

```rust
//! 新 session 开局的那一块，作为一个流生产者。
//!
//! 一个块，一个行：`tui-panel-welcome`。整块换掉是 `Op::Insert` 同 id 替换整行，
//! 于是第三方要换一块完全不同的欢迎内容，不需要改这个 crate —— 见设计文档
//! `docs/plans/2026-09-15-session-welcome-block-design.md` §"插件化"。

use std::sync::Arc;

use atomcode_harness::session::SessionEvent;

use crate::block::{Content, Coord, StreamWriter};
use crate::command::Command;
use crate::content::WelcomeBlock;
use crate::module::{Opening, Producer};

/// 随机提示的条数。固定位之外的那几条。
const MAX_TIPS: usize = 3;

/// 钉在第一位的那条命令。
///
/// **也要过筛**：atui 的命令集里现在没有 `login`，而 tuix 把它钉在第一位。
/// 钉住的位在命令不在时让出来，比推荐一条不存在的命令好。
const PINNED: &str = "login";

/// 候选命令，按"对新来的人有用"排过。
///
/// 只是一份**名字清单**：说明文字从 `Command::about` 取（见 `choose_tips`），
/// 而清单里没有的命令会被 `Commands::all()` 过滤掉。于是这份清单可以宽松——
/// 多写一个名字只会什么都不发生，而不是推荐一条不存在的命令。
const CANDIDATES: &[&str] = &[
    "login",
    "resume",
    "model",
    "skills",
    "mcp",
    "rows",
    "help",
    "layout",
    "audit",
    "compact",
    "context",
    "transcript",
    "reasoning",
    "tools",
    "mouse",
];

/// 筛出屏幕上真有的候选，钉住在位，然后按 `seed` 摇出其余的。
///
/// `seed` 由调用方从 cwd 算出来（见 `Welcome::opening`），**不是每次调用现取**
/// —— `Content::lines` 每帧调一次，而每帧摇一次会让块自己变，违反 `Content`
/// 的纯契约，也会让 `content_hash` 每帧变。tuix 的 `welcome_tip_indices` 就是
/// 为这件事存在的；这里靠"只摇一次"从根上避开。
pub(crate) fn choose_tips(commands: &[Command], seed: &str) -> Vec<(String, String)> {
    let has = |name: &str| commands.iter().find(|c| c.name == name);

    let mut tips: Vec<(String, String)> = Vec::new();
    // 钉住的位，命令不在就让出来（不做任何补偿——让给随机）。
    if let Some(c) = has(PINNED) {
        tips.push((format!("/{}", c.name), c.about.to_string()));
    }

    let mut rest: Vec<&Command> = CANDIDATES
        .iter()
        .filter(|n| **n != PINNED)
        .filter_map(|n| has(n))
        .collect();

    // 由 seed 定的顺序。种子稳定 ⇒ 同一目录同一二进制每次一致。
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed_of(seed));
    use rand::seq::SliceRandom;
    rest.shuffle(&mut rng);

    for c in rest.into_iter().take(MAX_TIPS) {
        tips.push((format!("/{}", c.name), c.about.to_string()));
    }
    tips
}

/// cwd → 种子。FNV-1a，与 `block::hash_of` 同一套（那边用来算 content hash），
/// 用同一个常数只是省得再想一个。
fn seed_of(seed: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in seed.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// 产开局那一块。
#[derive(Default)]
pub struct Welcome;

impl Welcome {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

impl Producer for Welcome {
    fn id(&self) -> &'static str {
        "welcome"
    }

    /// 一个欢迎块不折任何事实。它的内容在 `opening` 里一次成型。
    fn absorb(&self, _fact: &SessionEvent, _out: &mut StreamWriter<'_>) {}

    fn opening(&self, _at: Coord, open: &Opening) -> Option<Arc<dyn Content>> {
        // 摇号在这里、只在这里 —— `lines` 每帧调一次，而它必须纯。
        let tips = choose_tips(&open.commands, &open.cwd);
        Some(Arc::new(WelcomeBlock {
            cwd: open.cwd.clone(),
            model: open.model.clone(),
            version: open.version,
            tips,
        }))
    }
}

#[cfg(test)]
mod tests { /* 上面那六条 */ }
```

**注意 `Command::new` 是 `const fn` 且收 `&'static str`**（`command.rs:26`），所以判据里的 `cmds(&["resume", "help"])` 用一个 `&[&'static str]`、返回 `Vec<Command>` 是可行的——`"说明"` 也是 `&'static str`。

**若 `Command.about` 的类型不是 `&'static str`**：看 `command.rs:18-23` 的实际字段类型再定 `.to_string()` 是否必要。

- [ ] **Step 5: 跑，确认通过**

```bash
cargo nextest run -p atomcode-tui -E 'test(tips) or test(welcome)'
```

Expected: 全 PASS。

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -F - <<'EOF'
feat(tui): 欢迎块的生产者,提示从活的命令表筛

一行一个模块:`modules/welcome.rs` 的 `Welcome` 实现 `Producer::opening`,
其余什么都不做(它不折任何事实)。

提示的来源是**活的命令表**:候选只是一份命令名清单,按 `Commands::all()`
过滤,说明文字取 `Command::about`。tuix 的池子钉着 `/provider`、`/webui`、
`/plan` 这些 atui 未必挂上的命令,照搬会推荐屏幕上不存在的命令。

tuix 把 `/login` 钉在第一位,而 atui 的命令集里没有 login —— 所以**钉住的位
也要过筛**,命令不在时让给随机。

摇号只读一次种子(从 cwd 来),于是同一目录同一二进制每次一致。这不是洁癖:
`Content::lines` 每帧调一次,每帧摇一次会让块自己变、以及 `content_hash`
每帧变。tuix 的 `welcome_tip_indices` 就是为这件事存在的持久化;这里靠
"只摇一次"从根上避开。

`rand` 是新依赖(与 tuix 同版本):这条缝是给第三方替换用的,一个只能用 FNV
的仓库等于让每个替换者自己写一遍洗牌。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

---

### Task 5: 行 + 装配 + 调用点（`tui-panel-welcome`）

**Files:**
- Modify: `crates/atomcode-tui/src/rows.rs`（新行 + `SCREEN` + `catalog()`）
- Modify: `crates/atomcode-tui/src/plugin.rs`（`Tui::run` 里造 `Opening` 并调）
- Modify: `crates/atomcode-tui/src/modules/mod.rs`（若 Task 4 还没加 `pub mod welcome;`）

**依赖：** Phase 1 全部 + Phase 3 的 Task 7、Task 8。（`Tui::run` 要 `Opening`，
`WelcomePanel` 要 `welcome::Welcome` 与 `WelcomeBlock`。）

- [ ] **Step 1: 写 `rows.rs` 的行**

`SCREEN` 的**最前面**加一行（它是流的第一块内容，读起来该排在对话那块旁边）：

```toml
# 新 session 的开场块。它是流里的一块（生产者），不是面板 —— 所以它随对话一起
# 滚走：滚上去还看得见，而不是钉在屏幕上占一行。
#
# 整块替换是 `[[patch]] id = "tui-panel-welcome"` 换 `name`；`[[remove]]` 掉它，
# 流就直接从对话开始。
[[insert]]
name = "tui-panel-welcome"
```

`catalog()` 里加 `Arc::new(WelcomePanel),`（放在 `TranscriptPanel` 旁边，两者都是生产者）。

`rows.rs` 里加实现（照 `TranscriptPanel` 的形状——它也是生产者）：

```rust
/// 新 session 的开场块：生产者的行，与 `tui-panel-transcript` 同一类。
///
/// 它**不声称任何位置**：块的顺序是流里的顺序，而流是对话与面板之间那条
/// 分界线本身（`docs/adr/0004`）。所以它既不需要 `LayoutSvc`，也不需要
/// `LayoutOp::Show` —— 与 `todo`/`live` 那两个"挂上但位置由 composer 写"的
/// 面板不同，它压根不在布局树里。
pub struct WelcomePanel;

#[async_trait]
impl Plugin for WelcomePanel {
    fn name(&self) -> &'static str {
        "tui-panel-welcome"
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-modules"]
    }
    fn description(&self) -> &'static str {
        "the opening block of a new session: brand, mascot, where you are, and a few commands worth knowing"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let mods = ctx.require::<ModulesSvc>().map_err(|e| e.to_string())?;
        let producer = welcome::Welcome::new();
        let id = producer.id();
        mods.add_producer(producer)?;
        let m: Arc<Modules> = mods.clone();
        let _ = ctx.effect(move || m.remove_producer(id));
        Ok(())
    }
}
```

`rows.rs` 顶部的 `use crate::modules::{…}` 加 `welcome`。

- [ ] **Step 2: 跑 `rows.rs` 的两条一致性判据**

```bash
cargo nextest run -p atomcode-tui -E 'test(the_screen_only_names_rows_this_crate_ships) or test(every_shipped_row_is_either_mounted_or_deliberately_not) or test(no_two_rows_share_a_name)'
```

Expected: 全 PASS。（`every_shipped_row_is_either_mounted_or_deliberately_not` 会**失败**直到 `SCREEN` 里有那行——这正是它存在的意义。）

- [ ] **Step 3: 在 `Tui::run` 里造 `Opening` 并调**

位置：`plugin.rs` 里 `if feed.catch_up(history) { … }` 那一块**之后**、`let reader = …` 之前：

```rust
        // 让这次会话说第一句话 —— 只在流为空时。
        //
        // **在 `catch_up` 之后**，这个顺序是整件事的关键：一个被 resume 的会话
        // 在这里已经有历史，于是 `open_conversation` 看到流非空、什么都不做。
        // 反过来的话，每个 resume 都会在历史前面顶一条欢迎块。
        {
            let cwd = self.host.moment.read().expect("moment poisoned").cwd.clone();
            // 折叠在上游做（模块不读环境）：这条折的是给人看的字符串，而
            // `Moment::cwd` 存的是全路径。
            let open = crate::module::Opening {
                cwd: crate::text::collapse_home(&cwd),
                // 现取，不从 `Moment` 读缓存：`--model` 改的是 `llm` 那一行。
                model: client
                    .agent
                    .ctx()
                    .service::<LlmSvc>()
                    .map(|p| p.model_name().to_string()),
                version: env!("CARGO_PKG_VERSION"),
                commands: self.host.commands.all(),
            };
            if self.host.open_conversation(crate::block::Coord::default(), &open) {
                let _ = wake_tx.send(Wake::Fact);
            }
        }
```

`LlmSvc` 已经在 `plugin.rs:13` 的 use 里（`use atomcode_harness::seams::{LlmSvc, UiSvc, UserInterface, UserQuestionsSvc};`）。

- [ ] **Step 4: 编译并跑 e2e 全量**

```bash
cargo nextest run -p atomcode-tui
```

Expected: 全绿（此时 e2e 里还没有针对欢迎块的判据，但既有判据不能因为多了一块而红——若某条判据断言"第一行是什么"，它会红，**那是判据在说真话**：去看那条判据是不是假设了空会话的第一行是别的东西，按新事实修正它，而不是让欢迎块绕开）。

- [ ] **Step 5: 提交前核 `Cargo.lock`（AGENTS.md 点名的事）**

```bash
grep -c 'name = "hkdf"\|name = "hmac"\|name = "zeroize_derive"' Cargo.lock
```

Expected: `0`。非 0 就按 AGENTS.md 的做法用干净基线重来（**不要** `cargo update`）。注意 `rand` 是合法新增，别连同它一起剔了。

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -F - <<'EOF'
feat(tui): 欢迎块挂上 —— 一行,由 Tui::run 在折完历史之后问

`tui-panel-welcome` 进 `SCREEN` 与 `catalog()`,照 `tui-panel-transcript`
的形状(两者都是生产者)。它不声称任何位置:块的顺序就是流里的顺序,而流是
对话与面板之间那条分界线本身 —— 所以它既不要 `LayoutSvc` 也不要 `Show`。

调用点在 `catch_up` **之后**,顺序是整件事的关键:resume 的会话到这里已经有
历史,`open_conversation` 看到流非空就什么都不做;反过来的话每个 resume 都会
在历史前面顶一条欢迎块。

`Opening` 三样都现取:cwd 从 moment(并折叠 home,模块不读环境)、model 从
`LlmSvc::model_name()`(不从 moment 读缓存 —— `--model` 改的是 llm 那一行)、
commands 从 `Commands::all()`。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

---

### Task 6: e2e 判据、门、以及 spec 里承诺的收尾

**Files:**
- Modify: `crates/atomcode-tui/tests/e2e.rs`
- Modify: `gates/tui-test-count.baseline`（门自己抬，见下）

- [ ] **Step 1: 写 e2e 判据**

照 `tests/e2e.rs` 里既有用例的形状（`start(tree)` 拿 `Session`，`ui.frame()` 之类看屏幕——**以该文件现有 API 为准**，别凭记忆写）。要断的三件事：

```rust
#[tokio::test]
async fn a_new_session_opens_with_the_welcome_and_then_it_scrolls_away() {
    let root = scratch("welcome");
    let s = start(tree(&root, "", &[])).await;

    // 1. 开局就有,而且是最上面那几行。
    let first = /* 屏幕上第一行 */;
    assert!(first.contains("AtomCode"), "开局第一行该是品牌行: {first:?}");

    // 2. 它是一块已结算的内容,所以往回滚还看得见 —— 这要造出足够长的对话把
    //    它推出视野,再滚回去。
    /* 驱动若干轮对话 */

    // 3. 滚回去之后它还在(它是流内容,不是被丢掉的东西)。
    /* 滚到顶,断言品牌行又在屏幕上 */
}
```

**这条判据的重点是第 2、3 步**：只断言"开局有品牌行"的话，把它做成钉在屏幕上的面板也能绿——而那是设计明确排除的方案（见 spec §"放弃了什么"）。所以**必须**有"滚走再滚回来"这一段。

- [ ] **Step 2: 跑它**

```bash
cargo nextest run -p atomcode-tui -E 'test(a_new_session_opens_with_the_welcome_and_then_it_scrolls_away)'
```

Expected: PASS。（若第 2 步造不出足够长的对话，用 `replay()` 的脚本；`--demo` 那条路只看一帧，测不了滚动。）

- [ ] **Step 3: 跑 tui 门（会自己抬判据数基线）**

```bash
gates/tui.sh --fast
```

Expected: 通过。`tui-test-count.sh` 会**自动把基线往上抬**并把新基线写进
`gates/tui-test-count.baseline`（这是它的设计：升是允许的，降才拦）。按
`0f7efde1` 的先例，这个抬升**单独提交**，不混进功能 commit。

- [ ] **Step 4: 跑分层门，看棘轮有没有被顶**

```bash
gates/tui-layers.sh
```

Expected: 通过，且**没有**任何一个棘轮计数上升（`literal_glyphs` 尤其——`█`
不在它数的字形集里，`∙` 走的是 `Glyph::Bullet`）。若 `literal_glyphs` 涨了，
说明某处写了字面装饰符：`grep -nE '[┌┐└┘─│├┤┬┴┼✓✗⋯▸•]' crates/atomcode-tui/src/` 找它。

- [ ] **Step 5: 格式化（阻塞门）**

```bash
cargo fmt --all && cargo fmt --all -- --check
```

Expected: `--check` 退出 0。若 rustfmt 在某个长字符串匹配臂上不收敛（AGENTS.md 记过的老毛病），按块重写字面量加 `\` 续行，**不要**给门加 `continue-on-error`。

- [ ] **Step 6: Commit（两块，分开）**

先功能与 e2e：

```bash
git add crates/atomcode-tui/tests/e2e.rs
git commit -F - <<'EOF'
test(tui): 钉住"欢迎块随对话滚走"

只断言"开局有品牌行"是不够的:把它做成钉在屏幕上的面板也能让那条断言绿,
而钉住正是设计明确排除的方案。所以这条判据造出一段足够长的对话把欢迎块推出
视野,再滚回去断言它还在 —— 它是流内容,不是被丢掉的东西。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

再基线（单独一个 commit，按 `0f7efde1` 的先例）：

```bash
git add gates/tui-test-count.baseline
git commit -F - <<'EOF'
test(tui): 判据数基线随门自动抬

`gates/tui-test-count.sh` 是"判据只能升不能减"的棘轮,跑一次门就会把落后的
基线抬上去(这是它的设计:升是允许的,降才拦)。与 `0f7efde1` 同例,单独提交,
不混进功能 commit —— 混在一起的话,功能 diff 和基线数字一起评审,而后者只是
一个计数。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

- [ ] **Step 7: 全量复核**

```bash
cargo nextest run -p atomcode-tui && gates/tui.sh --fast && cargo fmt --all -- --check && grep -c 'name = "hkdf"\|name = "hmac"\|name = "zeroize_derive"' Cargo.lock
```

Expected: 测试全绿、门通过、fmt 退出 0、`Cargo.lock` 的那个计数为 `0`。

---

---

## Phase 3 — 开场机制与内容

> **执行顺序（重要）**：本 phase 的 **Task 7 → Task 8** 排在 Phase 2 之后，
> 但它在文件里的位置在 Phase 2 期间被编辑推到了前面 —— **按 task 号执行，不按
> 文档位置**。Phase 2 的 Task 4 只依赖 Phase 1；Task 5 依赖 Task 8（`WelcomeBlock`）。

### Task 7: `Opening`、`Producer::opening`、`Host::open_conversation`、home 折叠

**Files:**
- Modify: `crates/atomcode-tui/src/module.rs`（新增 `Opening`、`Producer::opening`）
- Modify: `crates/atomcode-tui/src/text.rs`（新增 `collapse_home_with` / `collapse_home`）
- Modify: `crates/atomcode-tui/src/host.rs`（新增 `open_conversation`）
- Test: 三个文件各自 `mod tests`

- [ ] **Step 1: 写 `text.rs` 的折叠判据**

```rust
    #[test]
    fn collapse_home_rewrites_the_prefix_and_nothing_else() {
        let home = std::path::Path::new("/home/me");
        assert_eq!(
            collapse_home_with("/home/me/proj/a", Some(home)),
            "~/proj/a"
        );
        // 只有整段前缀才算:一条**前缀相同但不是路径段**的路径不能被改写,
        // 否则 `/home/melon` 会变成 `~on`。
        assert_eq!(
            collapse_home_with("/home/melon/a", Some(home)),
            "/home/melon/a"
        );
        // 不是 home 底下的原样返回。
        assert_eq!(collapse_home_with("/tmp/a", Some(home)), "/tmp/a");
        // 问不出 home 就原样返回,而不是猜一个。
        assert_eq!(collapse_home_with("/home/me/a", None), "/home/me/a");
    }
```

- [ ] **Step 2: 跑，确认失败**

```bash
cargo nextest run -p atomcode-tui -E 'test(collapse_home_rewrites_the_prefix_and_nothing_else)'
```

Expected: FAIL，`cannot find function collapse_home_with`。

- [ ] **Step 3: 实现 `text.rs` 的两个函数**

```rust
/// 把 home 底下的路径写成 `~/…`，给人看的。
///
/// **以路径段为界**，不是字符串前缀：`/home/melon` 在 home 是 `/home/me` 时
/// 必须原样返回，否则会变成 `~on` —— 而这是个只在别人机器上出现的 bug。
///
/// 放在 `text.rs` 而不是某个模块里，因为调用方是 `Tui::run`（它本来就读环境），
/// 而模块一个环境变量都不许读（`gates/tui-layers.sh` 的 `os_probes`）。
pub fn collapse_home(path: &str) -> String {
    collapse_home_with(path, home_dir().as_deref())
}

/// 折叠的实现，home 显式传入 —— 好让判据不依赖跑测试的那台机器。
pub fn collapse_home_with(path: &str, home: Option<&std::path::Path>) -> String {
    let Some(home) = home else {
        return path.to_string();
    };
    let home = home.to_string_lossy();
    if home.is_empty() {
        return path.to_string();
    }
    let rest = if path == home.as_ref() {
        ""
    } else if let Some(rest) = path.strip_prefix(home.as_ref()) {
        // 段边界:紧跟着的必须是分隔符,否则就是把 `/home/melon` 当成了 `/home/me`。
        match rest.strip_prefix(std::path::MAIN_SEPARATOR) {
            Some(rest) => rest,
            None => return path.to_string(),
        }
    } else {
        return path.to_string();
    };
    if rest.is_empty() {
        "~".to_string()
    } else {
        format!("~{}{rest}", std::path::MAIN_SEPARATOR)
    }
}

fn home_dir() -> Option<std::path::PathBuf> {
    // `HOME` 在 unix,`USERPROFILE` 在 windows —— 与 tuix 的
    // `platform::home_dir` 同一套,但这条边不跨 crate。
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}
```

- [ ] **Step 4: 跑，确认通过**

```bash
cargo nextest run -p atomcode-tui -E 'test(collapse_home_rewrites_the_prefix_and_nothing_else)'
```

Expected: PASS。

- [ ] **Step 5: `module.rs` 加 `Opening` 与 `Producer::opening`**

```rust
/// 问一个生产者要不要给这次会话开个头时，给它的东西。
///
/// **只给数据，不给把手。** 生产者够不到 `Host`（它的 `absorb` 只拿得到
/// `StreamWriter`），而它可以拿到 `Host` 的话，一个开场块就能做任何事——包括
/// 往一个已经有人说话的流里插东西。这里列出的是显示一块欢迎内容真正需要的那几样。
#[derive(Clone, Debug, Default)]
pub struct Opening {
    /// 工作目录，**已经是显示用的字符串**（home 已折叠）。
    ///
    /// 折叠由调用方做，不在这里：读环境是 `Tui::run` 的事（它本来就在读 cwd），
    /// 而模块一个环境变量都不该读 —— `gates/tui-layers.sh` 的 `os_probes` 数
    /// 的就是那个。折叠做在上游，这个函数就保持纯、可单测。
    pub cwd: String,
    /// 当前模型名。**现取，不缓存**：`--model` 改的是 `llm` 那一行。
    pub model: Option<String>,
    pub version: &'static str,
    /// 这棵树上真实可用的命令，`Commands::all()` 给什么就是什么。
    pub commands: Vec<crate::command::Command>,
}
```

`Producer` trait 里加（`absorb` 之后）：

```rust
    /// 给这次会话开个头，如果有话要说。
    ///
    /// 只在**流为空**时被问（`Host::open_conversation`），所以它不需要知道
    /// 「现在是不是新会话」——「流是空的」就是那个判断。默认 `None`，于是
    /// 现有的生产者一行都不用改。
    ///
    /// 返回的块由宿主 `emit` 并立即 `settle`：一块开场内容没有"还在长"的阶段。
    fn opening(&self, _at: crate::block::Coord, _open: &Opening) -> Option<std::sync::Arc<dyn crate::block::Content>> {
        None
    }
```

- [ ] **Step 6: `Host::open_conversation` + 它的判据**

先写判据（`host.rs` 的 `mod tests`）：

```rust
    /// 一个只在流为空时开场的生产者。
    struct Opener {
        said: std::sync::atomic::AtomicU32,
    }

    impl Producer for Opener {
        fn id(&self) -> &'static str {
            "opener"
        }
        fn absorb(&self, _: &SessionEvent, _: &mut StreamWriter<'_>) {}
        fn opening(&self, _at: Coord, open: &Opening) -> Option<Arc<dyn crate::block::Content>> {
            self.said.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Some(Arc::new(crate::block::Text::new(format!("hi {}", open.cwd))))
        }
    }

    #[test]
    fn the_first_thing_the_conversation_says_happens_once_and_only_when_it_is_empty() {
        let mods = Arc::new(Modules::new());
        let opener = Arc::new(Opener {
            said: std::sync::atomic::AtomicU32::new(0),
        });
        mods.add_producer(opener.clone()).unwrap();
        let h = Host::new(mods, default_layout());

        let open = Opening {
            cwd: "~/proj".into(),
            ..Default::default()
        };
        assert!(
            h.open_conversation(Coord::default(), &open),
            "空的流上必须开场"
        );
        assert_eq!(h.stream.read().unwrap().len(), 1);

        assert!(
            !h.open_conversation(Coord::default(), &open),
            "流已经不空了,第二块开场内容会把别人的话往后挤"
        );
        assert_eq!(h.stream.read().unwrap().len(), 1, "还是那一块");
        assert_eq!(
            opener.said.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "第二个生产者不该被问 —— 流非空,循环就不该再转"
        );
    }

    #[test]
    fn an_opening_block_is_settled_so_its_content_can_never_change() {
        let mods = Arc::new(Modules::new());
        mods.add_producer(Arc::new(Opener {
            said: std::sync::atomic::AtomicU32::new(0),
        }))
        .unwrap();
        let h = Host::new(mods, default_layout());
        h.open_conversation(Coord::default(), &Opening::default());
        let s = h.stream.read().unwrap();
        assert!(
            s.slots()[0].is_settled(),
            "开场块一落地就该冻结:它不是还在长的东西"
        );
    }
```

**`crate::block::Text` 是 `block.rs` 里 `mod tests` 下的测试块**（`block.rs:483`），别的模块够不着。若它不可见，把 `Opener` 的 `opening` 换成一个最小实现：

```rust
            Some(Arc::new(crate::content::NoticeBlock {
                detail: format!("hi {}", open.cwd),
            }))
```

`NoticeBlock`（`content.rs:653`）是公开的，`detail: String` 是公开字段 —— 用它最省。

`StreamWriter`/`Producer`/`Opening`/`Coord` 要靠 `host.rs` 的 `mod tests` 里已有的 `use super::*` 加上显式 `use crate::module::{Opening, Producer};`、`use crate::block::StreamWriter;`。

- [ ] **Step 7: 跑，确认失败**

```bash
cargo nextest run -p atomcode-tui -E 'test(the_first_thing_the_conversation_says_happens_once_and_only_when_it_is_empty)'
```

Expected: FAIL，`no method named open_conversation`。

- [ ] **Step 8: 实现**

`host.rs` 的 `impl Host` 里（放在 `absorb` 附近）：

```rust
    /// 让这次会话说第一句话，如果有生产者愿意。
    ///
    /// **只在流为空时**。这就是「新会话」的全部判据——一个被 resume 的会话在
    /// `Tui::run` 折完历史之后流已经非空，于是它自然地不产开场块，不需要一个
    /// 单独的"是不是新会话"标志（那个标志会是一个第二真相源）。
    ///
    /// **不走 `absorb`。** `absorb` 那条路对应一条已提交的日志事实，而这里合成
    /// 的是一次开局表现，不是事实：走那条路会污染 `SessionLog`，并在回放时被
    /// 重新走一遍。所以它直接写流，是全仓唯一一处这样做的地方。
    ///
    /// 第一个回答的生产者赢，然后循环就停：规则是"流为空就产"，而它一产完流
    /// 就不空了——不需要第二个机制去拦第二名。
    ///
    /// 返回 `true` 表示产了一块，调用方欠一帧。
    pub fn open_conversation(&self, at: crate::block::Coord, open: &crate::module::Opening) -> bool {
        let mut stream = self.stream.write().expect("stream poisoned");
        if !stream.is_empty() {
            return false;
        }
        for p in self.modules.producers() {
            if let Some(content) = p.opening(at, open) {
                // 生产者自己的 id：块的 `producer` 字段说的是"谁产了它"，
                // 而它将来可能还是要被 settle/清点的那一个。
                stream.writer(p.id()).emit(at, content);
                return true;
            }
        }
        false
    }
```

- [ ] **Step 9: 跑两个判据，确认通过**

```bash
cargo nextest run -p atomcode-tui -E 'test(open) or test(opening) or test(collapse)'
```

Expected: 全 PASS。

- [ ] **Step 10: Commit**

```bash
git add -A
git commit -F - <<'EOF'
feat(tui): 流为空时让生产者开个头

`Producer` 加一个带默认实现的 `opening(at, &Opening)`,默认 `None` ——
现有的生产者一行都不用改。宿主新增 `open_conversation`:流为空时按挂载顺序
问,第一个回答的赢,`emit` 并立即 `settle`。

"新会话"的判据就是"流是空的"。resume 的会话在 `catch_up` 折完历史之后流
已经非空,于是它自然地不产欢迎块 —— 不需要一个单独的是不是新会话的标志,
那会是一个第二真相源。

**绕开 `absorb`**:那条路对应一条已提交的日志事实,而这里合成的是一次开局
表现,不是事实。走它会被写进 SessionLog 并在回放时重新走一遍。这是全仓
唯一一处直接写流的地方,注释里写明了。

`Opening` 只给数据不给把手:生产者已经够不到 Host(它的 absorb 只拿得到
StreamWriter),能拿到就等于一块开场内容可以做任何事,包括往一个已经有人
说话的流里插东西。

`text.rs` 加 `collapse_home`:以**路径段**为界,不是字符串前缀 ——
home 是 `/home/me` 时 `/home/melon` 会变成 `~on`,一个只在别人机器上出现
的 bug。折叠在上游做,模块不读环境(os_probes 棘轮)。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

---

### Task 8: `content::WelcomeBlock`（四段内容与排版）

**Files:**
- Modify: `crates/atomcode-tui/src/content.rs`（新增 `WelcomeBlock` + 它的 `impl Content` + 常量）
- Test: `crates/atomcode-tui/src/content.rs` 的 `mod tests`

- [ ] **Step 1: 写失败判据**

```rust
    fn welcome() -> WelcomeBlock {
        WelcomeBlock {
            cwd: "~/proj".into(),
            model: Some("a-model".into()),
            version: "9.9.9",
            tips: vec![
                ("/resume".into(), "接着上次".into()),
                ("/help".into(), "列出所有命令".into()),
            ],
        }
    }

    fn caps(unicode: bool, colors: bool) -> RenderCtx {
        RenderCtx {
            width: 80,
            caps: crate::block::ShapeCaps {
                unicode,
                colors: if colors {
                    crate::caps::Colors::Ansi256
                } else {
                    crate::caps::Colors::None
                },
            },
        }
    }

    #[test]
    fn the_welcome_draws_the_pieces_it_has() {
        let b = welcome();
        let text: Vec<String> = b.lines(&caps(true, true)).iter().map(Line::plain).collect();
        let all = text.join("\n");
        assert!(all.contains("AtomCode"), "{all}");
        assert!(all.contains("9.9.9"), "{all}");
        assert!(all.contains("~/proj"), "{all}");
        assert!(all.contains("a-model"), "{all}");
        assert!(all.contains("/resume"), "{all}");
    }

    #[test]
    fn the_mascot_needs_unicode_and_colour_both() {
        // 猫靠前景色画全块,而且没有 ASCII 等价物(设计文档:整块形状取决于
        // 能力)。所以两个门各自都能把它关掉。
        let b = welcome();
        let solid = |ctx: &RenderCtx| {
            b.lines(ctx)
                .iter()
                .map(Line::plain)
                .filter(|l| l.contains('█'))
                .count()
        };
        assert!(solid(&caps(true, true)) > 0, "能画的终端上要有猫");
        assert_eq!(solid(&caps(false, true)), 0, "没 unicode 就不画猫");
        assert_eq!(solid(&caps(true, false)), 0, "没颜色就不画猫");
    }

    #[test]
    fn a_terminal_without_colour_still_gets_the_words() {
        let b = welcome();
        let all = b
            .lines(&caps(true, false))
            .iter()
            .map(Line::plain)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("AtomCode") && all.contains("/resume"), "{all}");
    }

    #[test]
    fn nothing_fits_means_no_block_at_all_not_an_empty_one() {
        // 0 行的块仍然占一个 slot,而 `blank_between` 还会再给它留一行空白 ——
        // 屏幕上会多出一条莫名其妙的空行。所以这里必须返回空,而不是一个空行。
        let b = welcome();
        for w in [0u16, 1, 2, 3] {
            assert!(
                b.lines(&RenderCtx {
                    width: w,
                    ..caps(true, true)
                })
                .is_empty(),
                "宽度 {w} 装不下任何一段,就该没有这一块"
            );
        }
    }

    #[test]
    fn the_same_inputs_hash_the_same() {
        assert_eq!(welcome().content_hash(), welcome().content_hash());
    }

    #[test]
    fn a_different_input_hashes_differently() {
        assert_ne!(
            welcome().content_hash(),
            WelcomeBlock {
                model: None,
                ..welcome()
            }
            .content_hash()
        );
    }

    #[test]
    fn the_welcome_cannot_be_folded_away() {
        // 它的全部意义就是"事情发生过"——折成一行摘要就是把这个意思折没了。
        assert!(welcome().always_open());
    }
```

（`ContentHash` 是 `Copy`（`block.rs:56` 的 derive），所以 `assert_eq!` 直接比，不需要 `.clone()`。）

- [ ] **Step 2: 跑，确认失败**

```bash
cargo nextest run -p atomcode-tui -E 'test(the_welcome_draws_the_pieces_it_has)'
```

Expected: FAIL，`cannot find type WelcomeBlock`。

- [ ] **Step 3: 加常量和类型**

`content.rs` 里（放在 `NoticeBlock` 之前）：

```rust
/// 欢迎块左右两侧的对齐留白，与 tuix 的 `PAD_COL` 同值。
const WELCOME_PAD: usize = 2;

/// 猫：tuix 的半块源图，**原文照搬**。
///
/// 4 行 × 18 字符 = 9 格 × (上像素, 下像素)。tuix 靠 cell background 在同一
/// 格里画两层；本 UI 只用前景色，一格一个像素，所以把每格**降采样**成"上下
/// 任一像素是身体就实心"（见 `cell_solid`）。这样不需要手推一张新图——源图
/// 就是 tuix 的常量，而 tuix 自己的测试（`render/mascot.rs` 的
/// `rows_are_well_formed`）已经钉住它是 18 字符宽、字符合法。
const MASCOT_SOURCE: [&str; 4] = [
    "oooo.o.o.o.o.ooooo",
    "ooooooewekooewekoo",
    "ooooookokoookokooo",
    "..o.ooooooooooo...",
];

const MASCOT_W: usize = 9;

/// 一格是不是身体。`.` 是透明，其余（`o`/`e`/`w`/`k`）都是身体。
///
/// 颜色在这里被丢掉（整只猫一个 `Role::Brand`，见设计文档），只留下形状。
fn cell_solid(row: &str, cell: usize) -> bool {
    row.chars().skip(cell * 2).take(2).any(|c| c != '.')
}

/// 新 session 开局的那一块。
///
/// 一个**流生产者**产出的不可逆块，不是视图模块：它"发生一次、有历史"，
/// 滚上去还要看得见（`docs/adr/0004`）。
#[derive(Debug)]
pub struct WelcomeBlock {
    pub cwd: String,
    pub model: Option<String>,
    pub version: &'static str,
    /// 筛过、摇定下来的提示，`(命令, 说明)`。
    ///
    /// **构造时就定下来，不在 `lines` 里摇**：`lines` 每帧调一次，每帧摇会让
    /// 块每次都变（违反 `Content` 的纯净契约），也会让 `content_hash` 每帧变。
    pub tips: Vec<(String, String)>,
}
```

- [ ] **Step 4: 实现 `Content` 与排版**

```rust
impl Content for WelcomeBlock {
    fn kind(&self) -> &'static str {
        "welcome"
    }

    fn content_hash(&self) -> ContentHash {
        // 形状不进 hash：与宽度同类。`Content` 的契约是"覆盖语义内容，永不覆盖
        // 渲染出的字节"，而"猫有没有画出来"是渲染出的字节。
        let mut parts: Vec<&str> = vec!["welcome", self.version, &self.cwd];
        if let Some(m) = &self.model {
            parts.push(m);
        }
        for (cmd, about) in &self.tips {
            parts.push(cmd);
            parts.push(about);
        }
        hash_of(&parts)
    }

    /// 拒绝被折叠。
    ///
    /// 它的全部意义就是"这次会话是这样开头的"——折成一行摘要正是把这个意思
    /// 折没了。与 `SkillLoaded` 同一类（`Content::always_open` 的 doc）。
    fn always_open(&self) -> bool {
        true
    }

    fn lines(&self, ctx: &RenderCtx) -> Vec<Line> {
        let w = ctx.width as usize;
        if w == 0 {
            return Vec::new();
        }
        let content_w = w.saturating_sub(WELCOME_PAD * 2);
        let pad = " ".repeat(WELCOME_PAD);

        // ---- 左列：猫（够格才画）+ 两个 bullet ----
        let show_mascot = ctx.caps.unicode && ctx.caps.colors != crate::caps::Colors::None;
        let mut left: Vec<Line> = Vec::new();
        if show_mascot {
            for row in MASCOT_SOURCE {
                let mut art = String::with_capacity(MASCOT_W);
                for cell in 0..MASCOT_W {
                    art.push(if cell_solid(row, cell) { '█' } else { ' ' });
                }
                left.push(Line::styled(
                    format!("{pad}{art}"),
                    Style::new().fg(Color::role(Role::Brand)),
                ));
            }
        }

        // ---- 右列：标题 + 提示 ----
        let bullet = ctx.caps.g(Glyph::Bullet);
        let mut right: Vec<Line> = Vec::new();
        if !self.tips.is_empty() {
            right.push(Line::styled("快速上手".to_string(), muted()));
            let cmd_w = self.tips.iter().map(|(c, _)| width::str_width(c)).max().unwrap_or(0) + 2;
            for (cmd, about) in &self.tips {
                let gap = cmd_w.saturating_sub(width::str_width(cmd));
                right.push(Line::from_spans(vec![
                    Span::styled(cmd.clone(), Style::new().fg(Color::role(Role::Accent))),
                    Span::raw(" ".repeat(gap)),
                    Span::styled(about.clone(), muted()),
                ]));
            }
        }

        // ---- 两列判定：只有最宽的提示真的放得下时才并排 ----
        //
        // 与 tuix 同一条判据、同一个理由:提示行不截断,放不下还并排会让它越过
        // 右边缘被终端硬折行,把对齐的列打碎。
        let gap = 4usize;
        let left_w = if show_mascot { WELCOME_PAD + MASCOT_W } else { WELCOME_PAD };
        let tips_col = left_w + gap;
        let right_w = right.iter().map(Line::width).max().unwrap_or(0);
        let two_cols = show_mascot && !right.is_empty() && content_w >= tips_col + right_w;

        let mut rows: Vec<Line> = Vec::new();
        // ---- 头行：左品牌，右版本 · 许可证 ----
        let right_txt = format!("v{}  {}  MIT", self.version, ctx.caps.g(Glyph::Separator));
        let head_w = width::str_width("◆ AtomCode") + width::str_width(&right_txt);
        if content_w > head_w {
            let fill = content_w - head_w;
            rows.push(Line::from_spans(vec![
                Span::raw(pad.clone()),
                Span::styled("◆ AtomCode".to_string(), Style::new().fg(Color::role(Role::Brand))),
                Span::raw(" ".repeat(fill)),
                Span::styled(right_txt, muted()),
            ]));
        } else {
            rows.push(Line::from_spans(vec![
                Span::raw(pad.clone()),
                Span::styled("◆ AtomCode".to_string(), Style::new().fg(Color::role(Role::Brand))),
            ]));
            rows.push(Line::styled(format!("{pad}{right_txt}"), muted()));
        }
        rows.push(Line::empty());

        // ---- 两列 或 堆叠 ----
        if two_cols {
            let n = left.len().max(right.len());
            for i in 0..n {
                let mut line = left.get(i).cloned().unwrap_or_else(Line::empty);
                let have = line.width();
                if have < tips_col {
                    line.push(Span::raw(" ".repeat(tips_col - have)));
                }
                if let Some(r) = right.get(i) {
                    for s in &r.spans {
                        line.push(s.clone());
                    }
                }
                rows.push(line);
            }
        } else {
            rows.extend(left);
            if !right.is_empty() {
                for r in right {
                    let mut line = Line::from_spans(vec![Span::raw(pad.clone())]);
                    for s in &r.spans {
                        line.push(s.clone());
                    }
                    rows.push(line);
                }
            }
        }

        // ---- cwd / model：**只在两列块之下**，绝不 zip 进左列 ----
        //
        // tuix 踩过:提示比猫高时,多出的提示行会落到 cwd/model 上,屏幕上出现
        // `∙ proj/goal  set a goal…` 这种一行两事。
        for text in std::iter::once(Some(self.cwd.as_str()))
            .chain(std::iter::once(self.model.as_deref()))
            .flatten()
        {
            rows.extend(wrapped(text, content_w as u16, muted(), &format!("{bullet} ")));
        }

        // 尾空行：免得后续异步输出（MCP 已连接、升级提示）贴上来。
        rows.push(Line::empty());

        // 装不下任何一段时不留一块空行 —— 一个 0 行的块仍然占 slot。
        if rows.iter().all(|l| l.plain().trim().is_empty()) {
            return Vec::new();
        }
        rows
    }
}
```

**要核实的 API**（都以 `content.rs` 顶部的 `use crate::frame::{Color, Line, Span, Style};` 为准）：`Line::empty()`、`Line::from_spans(Vec<Span>)`、`Line::width()`、`Line::plain()`、`Line::styled(impl Into<String>, Style)`、`Line::push(Span)`、`Span::raw/styled`，都在 `frame.rs:200-260` 一带。

**`wrapped` 的签名**（`content.rs:60`）是 `fn wrapped(text: &str, w: u16, style: Style, prefix: &str) -> Vec<Line>`——前缀在调用处拼好，它自己按 `width::str_width(prefix)` 缩进后续行。

**标题是字面中文**：atui 没有 i18n 模块（`grep -rn "i18n" crates/atomcode-tui/src/` 只命中 `product.rs` 的一处注释），不要为了这块引入一个。

- [ ] **Step 5: 跑全部五条判据**

```bash
cargo nextest run -p atomcode-tui -E 'test(welcome) or test(mascot) or test(nothing_fits)'
```

Expected: 全 PASS。若 `nothing_fits_means_no_block_at_all_not_an_empty_one` 失败，检查最后那道 `rows.iter().all(…)` 的门——它是这条判据的**实现**，不是补丁。

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -F - <<'EOF'
feat(tui): 欢迎块的内容与排版

四段:品牌头行、猫、cwd/model、提示。排版判据照 tuix —— 只有最宽的提示真的
放得下才并两列(提示行不截断,放不下还并排会被终端硬折行,把对齐的列打碎),
而 cwd/model 只在两列块**之下**,绝不 zip 进左列(tuix 踩过:提示比猫高时
多出的行会落到 cwd/model 上,屏幕出现"一行两事")。

猫:tuix 的源图原文照搬,按"一格上下任一像素是身体就实心"降采样成 9×4。
tuix 用 cell background 在格子里塞两层像素,本 UI 只用前景色 —— 一格一像素。
用整块 `█` 而不是 `▀`,后者的半格会在逐行叠起来时留空隙,看着是条纹不是猫。
`█` 写字面:它不在 gates/tui-layers.sh 数的字形集里,而 ascii_for 已把它映射
成 `#`(同宽)。

猫的门是 `unicode && colors != None`。两个门都能关掉它,因为整块的存在取决于
能力 —— 这正是 adr/0021 那条缝的用途;能降级的字形不需要它。

`always_open()`:它的全部意义就是"这次会话是这样开头的",折成一行摘要正是把
这个意思折没了。

hash 不含形状:与宽度同类,`Content` 的契约是"覆盖语义内容,永不覆盖渲染出的
字节"。

Co-Authored-By: AtomCode (deepseek-flash) <noreply@atomgit.com>
EOF
```

---

## 执行顺序

**按 task 号执行，不按文档位置。** 文件里的 phase 顺序是为了阅读（Phase 1 是纯结构
改动、Phase 2/3 是功能），但依赖关系是：

```
Task 1 → Task 2 → Task 3         (Phase 1，每个任务后全绿、行为零变化)
Task 4                            (生产者；文案层，不做排版)
Task 7 ─┐
Task 8 ─┴→ Task 5                 (行与装配要 `Opening` 和 `WelcomeBlock`)
Task 6                            (e2e + 门)
```

Task 4 只依赖 Phase 1。Task 5 依赖 Task 4、7、8。Task 6 最后。

---

## 完成判据

实施完成后，下面每一条都能指到一个可查的东西：

| 承诺 | 去哪看 |
|---|---|
| 新 session 开局有欢迎块，且随对话滚动 | `tests/e2e.rs` 的那条 e2e |
| 它是插件化的（可替换/可拿掉） | `tui-panel-welcome` 在 `SCREEN` 里；`[[remove]]` 掉它流就直接开始对话（`open_conversation` 的判据） |
| 块的形状能按终端能力决定 | `docs/adr/0021`；`ShapeCaps`；两条缓存键判据 |
| 换终端能力会重算行数，不命中旧缓存 | `a_change_of_terminal_capability_is_not_a_cache_hit` 与 `the_row_index_is_keyed_on_capability_not_only_width` |
| 主题变化不击穿缓存 | `ShapeCaps` 的定义（不含 palette）+ `frame.rs:143-149` 的 doc |
| 窄终端不留莫名其妙的空行 | `nothing_fits_means_no_block_at_all_not_an_empty_one` |
| 不推荐屏幕上没有的命令 | `tips_only_name_commands_the_screen_actually_has` |
| 猫是 9×4、整块画、只前景色 | `the_mascot_needs_unicode_and_colour_both` 与 spec §四 |

## 没做的事（明确记下）

- **不做「一段一行」**（品牌/猫/cwd/tips 各自成行）。一个行的粒度已能让第三方整体
  替换；拆到段一级会把排版算术暴露成配置。见 spec §"插件化"。
- **不做 caps 的 `cell_background` 位**，所以猫是只前景色的 9×4，不是 tuix 的半块
  9×8。见 spec §"放弃了什么"。
- **不引入 i18n**。标题与说明都是字面中文，与 atui 现状一致（它没有 i18n 模块）。
- **不动 `Moment`**：欢迎块不需要它，能力经 `RenderCtx` 走。
