# Raster 与 blit：终端上的字符格位图

状态：设计（未实现）。配套 ADR：[`docs/adr/0023`](../adr/0023-raster-is-a-cell-grid.md)。

日期：2026-09-15

分支：`feat/session-welcome-block`（worktree `.worktrees/session-welcome-block`）

## 目标

**在终端里画一块密集、可原地刷新的图形，并且只重画变化的部分。**

这一版的驱动用例是**原地动画 / 可交互小部件**（Claude Mods 里那个
「Terminal 里玩 Tetris」的形状）。选它作为第一条验收场景有理由：它同时压到三
个方向——**格数要够多**、**刷新要够频繁**、**必须只重画变化的部分**——而只要
这三条同时立住了，静态密集图形（图表、可视化）自动也在范围里。

**不在本版范围**：随对话滚动的位图。理由见决策一，那是一个独立的、更贵的决定。

## 背景：为什么不是图形协议

`docs/adr/0022` 已经论证过：真图形协议（Sixel / iTerm2 / Kitty）在我们的滚动
模型下没有定义好的语义，且 `text::for_screen` 的整体不变量（**我们发出的任何
字节都不移动光标**）与它们冲突。那条路的重开条件写在那里。

而 Claude Code 的做法给出了第三条路：**它没有用图形协议**。它的 `Raster` 是一
个**字符格位图**——

```ts
RasterProps = {
  key: string
  columns: number   // 1..512
  rows: number      // 1..256
  cells: string     // base64: columns*rows 个小端 u32 三元组 [codePoint, fg, bg]
}
```

配一个 `$.ui.blit({ requestId, key, cells })` 只更新变化的格。**Tetris 是这么画
出来的**，不是像素协议。这条路的全部好处是：它就是格子，所以滚动、diff、
能力降级、containment 这些我们已有的东西**一件都不用改**。

## 现状事实（逐条核对过，不是推测）

| 事实 | 落点 |
|---|---|
| 一格有前景色与背景色 | `Style { fg, bg, bold, italic, underline, reverse }`（`frame.rs:76-83`）|
| 两者都真的写到终端 | `sgr()` 写 `38;5;n`/`38;2;r;g;b` 与 `48;5;n`/`48;2;r;g;b`（`ansi.rs:238-256`）|
| 帧里的一块是「矩形 + 行」 | `Placed { owner: String, rect, lines }`（`frame.rs:324-328`）；`Frame::place(owner, rect, lines)`（`frame.rs:348`）；`Frame::part(owner)`（`frame.rs:357`）|
| **行级增量已经免费** | `Lines::of(frame, caps, prev)` 按 `Line` 值比对算 `dirty[]`，**未变的行复用上一帧那份字节**，`patch_from` 只发脏行（`ansi.rs:403-465`）|
| 那条不变量有尺子 | `ROWS_ENCODED`（test-only，`ansi.rs:~487`）：**未变的帧一行都不许重新编码**——而输出相等看不出来 |
| 一帧 = 每行「擦 + 绝对定位 + 画」 | `encode_rows`（`ansi.rs:496-517`）：`ERASE_LINE` 然后 `\x1b[{row};{col}H` |
| 视图模块每帧重画、无历史 | `View::render(state, &Viewport)` 纯函数；`Viewport { rect, moment }`（`module.rs:41-73`）|
| 可要求固定矩形 | `Height::Fixed(n)`（`module.rs:31-38`），或 `LayoutOp::Show { side, size }` |
| 外部可变状态、模块每帧读（**要照的形状**） | `Host.asks: Arc<Asks>`（`host.rs:759`）；`Asks` 是 `Mutex<Vec<..>> + AtomicU64 + wake`（`ask.rs:56-59`）|
| base64 已可用 | `base64` 依赖（剪贴板图片在用，`surface.rs:899`）|
| **braille 已有先例决定** | `SPINNER` 是 braille，**故意不进降级表**（`caps.rs:325-332`、`586-591`）：「没有一格 ASCII 能读作运动」，所以改由 `Caps::spinner()` **按终端选整组** |
| 宽度以 `unicode-width` 为权威 | `width.rs:1-21` 的 doc：「an implementation-independent oracle」|

## 决策一：位图住在**视图模块**，不是流块

**决定：`Raster` 由视图模块拥有。** 它的矩形来自布局（`Region::view(id)` 或
`LayoutOp::Show`），内容每帧从一张外部表里取。

理由是一条硬约束：**已结算的块内容冻结**。ADR 0004 把「内容冻结」做成了类型
性质——`Settled` 不给 `&mut`，`StreamWriter::amend` 对已结算块返回 `false`。
所以位图若住在流块里，它只能活在 `Live` 期间，**一个 turn 边界就把它冻死**，
之后再也刷不动。而视图模块的定义就是「每帧全量重画、无历史」
（`module.rs:1-9`）——位图要的正是这个。

**另一条同源的约束：位图不能是「骑流尾部」的模块。** ADR 0020 规定 tail 里的模块
**不再有自己的布局矩形**（它的行成为滚动内容）。而位图要 blit 就得有一个**稳定的
矩形**，否则「写进哪一格」每一帧都在变。所以位图模块的矩形只能来自布局树
（`Region::view(id)` 或 `LayoutOp::Show { side, size }`），**不得进 `El::Stream`
的 `tail`**。这一条要写进 row 的 doc 与 `--audit` 的判据里，否则下一个人会顺手把
它挂上尾部。

**代价（必须说清，这是真实的选择代价）**：视图模块的矩形是**固定的**，所以
**位图不会随对话滚走**。

而我们先前为欢迎块那条猫做的选择恰恰是「随对话滚动」（见
`docs/plans/2026-09-15-session-welcome-block-design.md` §一）。**两者不冲突，是
两个不同的东西**：

| | 随内容滚的图形 | 原地刷新的小部件 |
|---|---|---|
| 例子 | 欢迎块的猫、工具输出里的图 | Tetris、状态动画、可点的图例 |
| 归宿 | 流块（不可逆、有历史） | **视图模块（每帧重画、无历史）** |
| 能否原地刷新 | ❌ 冻结后不能 | ✅ 每帧可换 |
| 本版 | 不在范围 | **在范围** |

「能滚的图」若将来要原地刷新，等于要求「一个不冻结的流块」——那是一条独立的、
更大的决定（会碰 ADR 0004 的核心不变量），本版不做，记在失效条件里。

## 决策二：**v1 只做行级增量**，格级列为二期

**决定：`blit` 换掉位图内容，重画按 `Lines::of` 已有的行级 `dirty[]` 走。这一版
不碰 `encode_rows` 的不变量。**

理由：

1. **行级已经免费，且已被判据钉住。** `Lines::of` 按 `Line` 值比对，未变的行
   复用字节；`ROWS_ENCODED` 就是那把尺子。位图落进 `Frame` 之后什么都不用做
   就享受到它。
2. **格级要动的是一条不变量，不是加一段代码。** `encode_rows` 的注释写着：
   「Erase first, draw second, one row at a time. **Erasing a row cannot scroll
   the screen and cannot reach the scrollback**」。做到格级就得放弃「整行擦 +
   整行画」，改成「把光标移到变了的格、只覆盖那几个格」——**erase 没了**，而
   「一帧 = 一组独立行载荷」这个模型上建着 diff、scrollback 安全论证和几条既有
   判据。
3. **代价是可算的，而且对驱动用例不大。** 行级重画的代价 = 脏行的**全部格**字节。
   设位图 48×20（见决策四），一行 48 格 × 每格约 8–20 字节 SGR ≈ 0.5–1KB；一帧
   动 2 行 ≈ 1–2KB。这与 26k 行会话里一次流式 chunk 的代价同量级。

**引入格级的前提是量化，不是直觉。** 触发条件写成可测的：

> 当**每帧传输字节数**成为可观测瓶颈时：用 `ROWS_ENCODED` 的同类计数器（加一个
> `CELLS_ENCODED`）统计「一帧重编码了多少格」，测出一个 48×20 的位图每帧变化
> 稀疏（例如 <10% 格）时的实际字节数。**先量**，再决定要不要动 `encode_rows`。

这条写进 ADR 作为二期的门槛：**没有量化数据就不做格级**。

## 决策三：寻址是 `(模块 id, key)`

Mods 的寻址是 `(component, requestId, key)`，并且**能写进别人的渲染点**（一个
插件可以改写 `ToolUse` 那一行的树）。

**我们只做 `(模块 id, key)`**：一个位图属于某个模块，`key` 是它在这块矩形里的
地址（一块矩形里可以放多个位图）。**不接受按任意 owner 字符串写**——那等于让
一个模块改另一个模块的矩形，与 `Frame::containment_violations` 保证的
「一个模块画不出自己的矩形」直接冲突。

**代价**：比 Mods 弱一档，改不了别人的东西。**换来的是**：矩形是真几何，
containment 逐块检查继续生效，`--audit` 能看出「挂了但没画」与「画了但没挂」。

## 决策四：上限是**算出来的**，不是抄来的

Mods 的上限是 512 列 × 256 行。他们没说为什么，我们算一遍：

- 一格是 **3 个 u32 = 12 字节**（码点、前景、背景）；
- 512×256 = 131,072 格 = **1.5 MiB** 原始数据，base64 之后约 **2 MiB**；
- 一帧传输（落进 `Frame` → `Line` → SGR 编码）比这还要大：512×256 的位图若整块
  脏，编码出的字节是 MB 量级。

**所以 v1 的上限定得更小，理由是可算的**：位图的设计目的是「一块小部件」而不是
「一整屏」。建议 **`columns ≤ 128`、`rows ≤ 64`**（= 8192 格 = 96 KiB 原始
数据），并**把上限写进拒因**：超限不是截断，是拒绝并说明原因——截断会让调用方
以为自己画上了。

这个数字是**建议值**，实现时可以调；重要的是它由算式得出，且判据里有一条钉住
「超限被拒」。

## 决策五：字形与宽度——宽度判据取 crate 既有权威，可画性靠整组换

这是本设计里最容易埋暗雷的一处，而仓库里**已经有一半的答案**。

### 5.1 不能靠降级表救

`caps.rs:586-591`（既有判据的 doc）已经把这件事说清了：

> The braille set is deliberately absent from the downgrade table — it trades one
> column for one column, and there is no one-cell ASCII stand-in that reads as
> motion. So the *set* has to follow the terminal, or the shield that exists to
> stop tofu draws eight of them.

**位图与 spinner 是同一类东西**：它的字符（`█▀▄░▒` 与 braille）里，braille 不在
`ascii_for` 的降级表里，所以**没有「上屏时自动降级」这条退路**。结论：

**`Raster` 必须有一个 caps 门。** `caps.unicode == false` 时：要么不画（诚实，
与欢迎块那只猫同一处理），要么提供一份**降级安全的字符集**
（`█▀▄░▒` 都在降级表里，会变成 `#`/`.`）。v1 取前者——一个变成马赛克的「动画」
没有意义，而 spinner 的先例说明这个仓库倾向于**整组换掉而不是事后改写**。

（`Caps::plain()` 与 `ATOMCODE_ASCII` 是这条门的两个触发点。）

### 5.2 宽度：判据是「宽度 1」，**不是「非 Ambiguous」**

这一节推翻了本设计初稿的写法（初稿说「必须宽度 1 且非 Ambiguous，braille 暂拒」）。
**查证推翻了它**，而且牵出一条既有的、全 UI 范围的事实。

**权威数据**（UAX #11 `EastAsianWidth.txt`，本机实测 `unicode-width` 两个约定与它一致）：

| 码点 | EAW |
|---|---|
| `2800..28FF` braille | **N**（Neutral） |
| `2580..258F` `█▀▄` | A（Ambiguous） |
| `2592..2595` `▒` | A |
| **`2500..254B` 全部框线** `─│┌┐└┘├┤┬┴┼` | **A** |
| `25C6` 品牌记号 `◆` | A |

判据：`unicode-width` 的 `width()` 把 Ambiguous 当 **1** 列、`width_cjk()` 当 **2**
（它的 doc 明说），所以 **`width != width_cjk` 就是「Ambiguous」**。

**结论：`非 Ambiguous` 不能当判据。**

- 它会拒掉 `█▀▄▒`——**以及这个 UI 已经在用的每一条框线**。面板边框、表格横线、
  `◆` 品牌记号全是 Ambiguous。
- 而「Ambiguous 被终端当双宽」是**全 UI 范围的既有暴露**：开了那个设置的终端上，
  每一个边框今天就已经错位，**与 Raster 无关**。
- 在一个已经全是 Ambiguous 网格的 UI 里，单独拒收 Raster 里的 Ambiguous 字符
  **既不保护什么，又让 Raster 无字符可用**。

**所以 v1 的校验回到这个 crate 既有的权威**：`unicode-width::width(c) == 1`
（窄约定）+ 可打印 BMP 字符。**braille 因此可用，而且是这批候选里最安全的一档**
（Neutral）——初稿把它当危险项是反的。

（顺带更正一处仓库注释：`crates/atomcode-tuix/src/render/qr.rs` 说 braille 是
"Unicode-Ambiguous width"——按 UAX #11 它是 **N**，那句注释是错的。它把 Braille
版 QR 做成 opt-in 的理由因此不成立；opt-in 本身无害，不必动。）

**真要防 ambiguous-as-wide，那是另一件事**：给 `Caps` 加一位，并在 `Caps::g` 里
退回 ASCII 框线（`+ - |`）——形状与既有降级机制完全一样，收益是全 UI 的。
本设计**不做**，但把它记进「相关」，因为它是这次查证的副产品。

## 新面

### `raster.rs`（新文件）

```rust
/// 一块字符格位图。
pub struct Raster {
    pub columns: u16,   // ≤ 上限
    pub rows: u16,      // ≤ 上限
    /// row-major，`columns * rows` 个 `(码点, 前景, 背景)`。
    pub cells: Vec<Cell>,
}

pub struct Cell {
    /// 一个**可打印、宽度 1** 的 BMP 码点（窄约定，见 5.2）。
    pub ch: char,
    /// `None` = 用终端默认色（对应 Mods 的 `0x01000000`）。
    pub fg: Option<Color>,
    pub bg: Option<Color>,
}

impl Raster {
    /// 从 base64 解码并校验。
    ///
    /// 拒因**点名到格**（下标），与 Mods 的 "naming the cell's index" 同：
    /// 一个坏格而只说"非法"会让调用方在一万格里靠猜。
    pub fn decode(columns: u16, rows: u16, cells: &str) -> Result<Raster, RasterError>;

    /// 排成 `rows` 行，每行 `columns` 格。
    ///
    /// 每一格是**一个 `Span`**——不是把整行拼成一个 Span：字形的 fg/bg 逐格
    /// 不同，而 `Span` 正是「一段共享同一 style 的文本」。
    pub fn lines(&self) -> Vec<Line>;
}
```

`RasterError` 的变体，每条都有理由（这些会成为拒因文案）：

| 变体 | 触发 |
|---|---|
| `BadBase64` | 载荷本身不是合法 base64 |
| `BadLength { got, want }` | 解码后不是 `columns * rows * 12` 字节 |
| `BadCell { index, why }` | 不可打印 / 不是 BMP / **宽度 ≠ 1**（宽字符、零宽、控制符）|
| `TooLarge { columns, rows, max }` | 超上限——**拒绝而不是截断** |
| `NotMounted { module, key }` | `write` 到一块没挂载过的位图——**不隐式挂载** |
| `SizeMismatch { want, got }` | `write` 的尺寸与挂载时不符（换尺寸要先卸载再挂）|

### `Rasters`：内容**经 `Moment` 到达模块**（这一条是初稿漏掉的）

初稿写「照 `Asks` 的形状——外部可变、模块每帧读」，**那是错的**，因为
`View::render(state, viewport)` 拿不到任何服务。trait 的 doc 把这条说得很硬：

> [`View::render`] takes `&State`, not `&self`. A module therefore *cannot* capture
> a `Context`, a service handle, a channel or a clock — the signature makes the
> "no side effects, no IO, not async" obligation unrepresentable rather than
> merely discouraged.

**所以位图不能由模块自己去表里取，必须由宿主送进来。** 仓库里已有的先例正是这个
形状——`refresh_members`（`plugin.rs:694`）：

```rust
let mut members: Vec<MemberNow> = agents.list()...collect();
if moment.members != members {
    moment.members = members;        // 一个 row 读服务，把活数据写进 Moment
}
```

模块这边则从 `viewport.moment.members` 读。**`Moment` 就是「不是事实、但此刻为真」
的那一半**（`cwd`、`caps`、`members`、`notice` 都在里面）。

照办，于是三件事各有归属：

```rust
// moment.rs —— 新增一个字段
pub struct Moment {
    ...
    /// 已挂载的位图，**这一帧的那一份**。
    ///
    /// 快照而不是把手：里面的 `Raster` 是不可变的，所以同一个 `Moment` 渲染两次
    /// 看到同一幅画面——与 `caps`、`cwd` 守的是同一个承诺。
    pub rasters: RastersView,
}
```

```rust
// raster.rs —— 不可变快照 + 可变表
/// 某一帧的位图集合。克隆是 N 次 Arc 计数，不是像素拷贝。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RastersView(Arc<HashMap<(String, String), Arc<Raster>>>);

impl RastersView {
    pub fn get(&self, module: &str, key: &str) -> Option<&Arc<Raster>>;
    pub fn is_empty(&self) -> bool;
}

/// 写入方用的表。宿主持有一个，`RastersSvc` 交给能写的行。
pub struct Rasters {
    /// **每帧读的那条路必须是 O(1)**：`view()` 只是把存着的 `Arc` 克隆一份。
    /// 重建整张表（O(N) 次 Arc 计数，无像素拷贝）只发生在 `write` 上——
    /// 与 `LiveCache`/`Settled` 同一取舍：热路径免费，冷路径付费。
    current: Mutex<Arc<HashMap<(String, String), Arc<Raster>>>>,
    revision: AtomicU64,
}

impl Rasters {
    pub fn view(&self) -> RastersView;                                   // O(1)
    pub fn mount(&self, module: &str, key: &str, raster: Raster) -> Result<(), RasterError>;
    pub fn write(&self, module: &str, key: &str, cells: &str) -> Result<(), RasterError>;
}
```

**宿主侧两处接线**（`host.rs`）：

```rust
// Host::new 里：自家造一个，无需改签名
rasters: Arc::new(crate::raster::Rasters::new()),

// compose：把这一帧的快照放进 moment —— 与 `moment.caps` 同一条路
let mut moment = self.moment.read().expect("moment poisoned").clone();
moment.rasters = self.rasters.view();
```

**为什么 `view()` 必须 O(1)**：`compose` 每帧调一次，`write` 是外部偶发。把重建
放在 `write` 上，帧路径就只剩一次 `Arc` 克隆。

**为什么位图模块的高度必须来自配置**：`View::height(state, moment, width)` 收的是
调用方手上那份 `Moment`，而**不是** `compose` 造的那份——`stream_height` 等路径
传来的 moment 里 `rasters` 可能是空的。所以位图模块**不能**按位图内容报高度，
只能用 `Height::Fixed(n)`（配置给）。这一条避免了「高度取决于快照在不在」的
不确定性。

### `RastersSvc`（新的一格服务）

照 `ModulesSvc` / `CommandsSvc` 的位置加在 `plugin.rs`：

```rust
plexus_service!(RastersSvc => crate::raster::Rasters, "tui-rasters", Core,
    "Mounted cell-grid bitmaps, addressed by (module id, key)");
```

由 `TuiUiPlugin` 与其它 `tui-*` 服务一起 `provide`（`assemble(surface)` 造好
`Host` 之后，`host.rasters.clone()`）。

## 交互：一半有路，一半是**硬缺口**

驱动用例是「可交互小部件」，而「交互」要拆成两半看，结论完全不同。

### 指针：有路

`Action::ClickAt(u16, u16)` 与 `Action::SelectFrom/SelectTo`（`keymap.rs:41-55`）
已经是单元格坐标，`Host.hits` 是一张「这个矩形是什么」的表（stream 行、字段、
回到底部的徽标都有登记）。**一个模块的矩形登记进 `hits` 就能收到点击。**

### 键盘：**今天没有缝**

核实的结论，三条一起看：

1. `Moment.focus: Option<String>`（`moment.rs:163`）——**声明了，但全仓没有任何
   读者**（`grep` 只命中文档散文、`focus` **布局 preset 名**、以及 `widget.rs`
   自己的 `focused: usize` 参数）。
2. `Action` 枚举（`keymap.rs:15+`）**没有任何「交给模块」的变体**。
3. 按键的路径是固定的：`self.keys.resolve(press)` → `Action` → `Tui::act` 里那个
   大 match（`plugin.rs:613`、`825+`）——即「键 → 已知动作」，不是「键 → 谁有
   焦点」。

**所以我们不能声称 v1 能做出能玩的小部件。** 能做出**动画**，以及**能点的**
东西；**不能**做出靠键操作的东西。

要补这条缝，仓库里**有现成的先例可照**：`Host::context_menu_key`
（`host.rs:1235-1243`）就是那个形状——

> Run a key against the open menu... `None` when nothing is open, so the caller
> falls through to the ordinary keys — **focus is arbitration, and this is where
> the menu is given it or not.**

即：**先问那个持有焦点者；它不接，才落到普通键**。给模块做同一条（`Moment.focus`
填上模块 id，`Tui::act` 先问它）是**一条独立的工作**，不在本版；本版把
`Moment.focus` 这个没人读的字段留原样，并在文档里点明它是这条缝的落点。

## 失败语义

| 情形 | 行为 |
|---|---|
| `caps.unicode == false` | 不画位图（与欢迎块的猫同一处理）。**不是** tofu，也不是半块马赛克 |
| 超上限 | `TooLarge` 拒，说明上限与收到的尺寸 |
| 载荷长度不符 | `BadLength { got, want }` |
| 某一格非法 | `BadCell { index, why }`——**点名下标** |
| 宽度 ≠ 1 的码点（如 `中` U+4E2D，或零宽的组合符） | `BadCell`，理由写明实测宽度 |
| `write` 到未挂载的 `(module, key)` | 拒（`NotMounted`），不隐式挂载 |
| `write` 的尺寸与挂载时不符 | 拒（`SizeMismatch`）——换尺寸要先卸载再挂 |
| 模块被卸载而位图还在 | 模块的 `apply` 里 `ctx.effect` 撤销时一并清掉（照 `rows.rs` 里每个面板的 effect 形状）|
| 位图画超出自己的矩形 | 由 `Frame::containment_violations` 现成抓获 |

## 判据

**每一条都要配阴性对照**——这个仓库的既有做法，且没有对照的判据会在实现坏了
之后照样绿。

1. **校验（逐变体）**：`TooLarge`、`BadLength`、`BadBase64`、`BadCell` 各一条；
   `BadCell` 那条断言**拒因里有点名下标**。阴性对照：一个合法最小位图（1×1）通过。
2. **宽度不是 1 的码点被拒，braille 通过**（这一对就是 5.2 那条更正的可执行版本）：
   - `\u{4E2D}`（`中`，宽字符）被拒，理由提到实测宽度 2；
   - `\u{2800}`（braille 空白）**通过**——它是 Neutral，是这批里最安全的一档；
   - `\u{2588}`（`█`，Ambiguous）**通过**——与 UI 其余部分（框线也是 Ambiguous）
     同一假设，拒它没有意义。
3. **未变的行不许重编码**：一个已挂载且内容未变的位图，连画两帧，
   **`ROWS_ENCODED` 不增**（现成的尺子）。阴性对照：`write` 换掉一行之后，
   **只有那一行**重编码。
4. **能力门**：`ShapeCaps { unicode: false, .. }` 下位图不画（渲染出空，或按
   实现取「降级集」时是降级字符）——断言**不是 tofu**。
5. **`write` 的三种拒**：未挂载、尺寸不符、坏格；每种断言**帧没变**
   （`Rasters::revision` 不动）。
6. **containment**：一个大于自己矩形的位图被 `containment_violations` 抓到
   （现成机制，钉住它对新元素也生效）。
7. **上限是硬门**：边界值（恰好等于上限）通过、+1 被拒。

## 不做的事（明确记下）

- **不做格级 diff。** 触发条件与量化方法写在决策二；没有数据不做。
- **不做随对话滚动的位图。** 它要求「不冻结的流块」，会碰 ADR 0004 的核心
  不变量。见失效条件。
- **不做键盘交互。** `Moment.focus` 那条缝是独立工作（见「交互」一节）。
- **不做图形协议。** ADR 0022 已定，重开条件在那里。
- **不做 ambiguous-as-wide 的防御。** 那是**全 UI 范围**的既有暴露（框线全是
  Ambiguous），正解是给 `Caps` 加一位并在 `Caps::g` 里退回 ASCII 框线——
  **独立的一件事**，见 5.2。
- **不做「写进别人的矩形」。** 决策三。

## 失效条件

- **若位图必须随对话滚**：视图模块的选择失效。出路是给流块一个「不冻结」的
  类别，或让位图成为流块里可变的特例——**那要重新论证 ADR 0004**（内容冻结是
  它的核心），不是加个标志位。
- **若行级重画的字节数成为实测瓶颈**：开格级，动 `encode_rows` 的先擦后画。
  前置是决策二里那条量化，且要重新论证「erase 整行」与 scrollback 安全。
- **若「Ambiguous 当双宽」在目标终端上成为常态**：那是 **UI 级**问题（每个面板
  边框、每条表格横线都会错位，不止位图），正解是给 `Caps` 加一位并在 `Caps::g`
  里退回 ASCII 框线（`+ - |`）——**在 Raster 里单独处理它没有意义**。那时
  Raster 不需要改：它用的字符与框线同类。
- **若出现了第二个「外部往已挂载的东西写、模块每帧读」的场景**：`Asks` 与
  `Rasters` 会长成两种同形的东西，那时该抽一个共用的形状，而不是并列第三张表。
- **若 `Caps` 能力探测升级**（比如真的去探 `[16t` 拿单元格像素尺寸）：
  Raster 是**字符格**的，不需要像素尺寸；探测升级只对 ADR 0022 那条路有意义。

## 相关

- [`docs/adr/0023`](../adr/0023-raster-is-a-cell-grid.md) —— 本条的设计决定
- [`docs/adr/0022`](../adr/0022-pictures-in-blocks-not-yet.md) —— 图形协议为什么
  不做；本设计是它「路一」的字符格版本
- [`docs/adr/0004`](../adr/0004-tui-stream-is-an-irreversible-block-sequence.md) ——
  「已结算块内容冻结」是本设计选视图模块的直接原因
- [`docs/plans/2026-09-15-session-welcome-block-design.md`](./2026-09-15-session-welcome-block-design.md) ——
  随内容滚的那一类图形（欢迎块的猫）走的是另一条路
- `crates/atomcode-tui/src/ansi.rs` 的 `Lines::of` / `ROWS_ENCODED` /
  `encode_rows`
- `crates/atomcode-tui/src/frame.rs` 的 `Placed` / `Frame::place` / `Style`
- `crates/atomcode-tui/src/caps.rs` 的 `SPINNER` 与其两条判据（braille 不进降级表）
- `crates/atomcode-tuix/src/render/qr.rs` —— **它说 braille 是 Ambiguous，按
  UAX #11 是错的（N）**；这次的更正见 5.2
- UAX #11 `EastAsianWidth.txt` —— 5.2 那张表的权威来源
- `crates/atomcode-tui/src/ask.rs` 的 `Asks` —— 新表要照的形状
- `crates/atomcode-tui/src/host.rs` 的 `context_menu_key` —— 键盘焦点那条缝的
  现成先例
- Claude Code Mods 的 `RasterProps` / `$.ui.blit` —— 形态参照（`mods/types/
  claude-code.d.ts`）
