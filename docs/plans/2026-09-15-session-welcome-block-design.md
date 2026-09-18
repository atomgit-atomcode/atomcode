# 新 session 开局的欢迎区块（atui）

状态：已定设计，待实施

日期：2026-09-15

基线：`0f7efde1`（分支 `feat/session-welcome-block`，worktree
`.worktrees/session-welcome-block`）

## 背景

`atomcode-tuix` 每次开新 session 会在会话流的第一段画一块欢迎内容：品牌行、
吉祥物、工作目录与模型、以及几条上手命令提示
（`crates/atomcode-tuix/src/render/retained.rs` 的 `push_welcome`，
内容由 `build_welcome_rows` 拼出，tip 池在
`crates/atomcode-tuix/src/render/welcome_tips.rs`）。

`atomcode-tui`（下称 atui，二进制 `atui`）是接替 tuix 的那个前端，它**现在完全
没有 welcome 字样**：`grep -rn "welcome" crates/atomcode-tui/src/` 无命中。新
session 开局是一屏空白，只有 composer 和 status 行。

本任务：在 atui 里补上这块内容，位置与语义对齐 tuix，并且把它做成一个
**可以方便替换、覆盖、重写的插件化**部件。

## 决策摘要

设计过程中定了四个问题，四个答案共同决定了后面的全部结构：

| 问题 | 决定 | 决定的原因 |
|---|---|---|
| 位置语义 | **随对话滚动**，做成流的头部内容 | 与 tuix 一致；滚上去还能回看 |
| 插件化粒度 | **一个行**（`tui-panel-welcome`） | `Op::Insert` 同 id 替换整行 ⇒ 换整块天然免费，够用 |
| 吉祥物 | **把终端能力接进块**（`RenderCtx`），画只前景色的半块猫 | 像素猫的存在与否取决于终端能力，而块现在读不到 |
| 产出时机 | **流为空时就产**，不引入新状态 | resume 有历史 ⇒ 流非空 ⇒ 自然不产 |

其中第三条是本次唯一的**结构性**改动：它不只服务欢迎块，而是给「块的形状取决
于终端能力」这件事开一条通用缝。第一条排除了「做成视图模块」的方案（见「放弃
了什么」）。

## 现状事实（探查结论，含落点）

设计基于以下已核对的事实，不是推测：

- **流的行只有两种来源。** 流生产者（`crate::module::Producer`）追加不可逆块；
  视图模块（`crate::module::View`）每帧全量重画、无历史。划分见
  `docs/adr/0004-tui-stream-is-an-irreversible-block-sequence.md`。
- **块拿不到终端能力。** `Content::lines(&self, width: u16)`
  （`crates/atomcode-tui/src/block.rs:97`）只有宽度。`Caps` 在 `Moment` 里
  （`moment.rs:213`），只有视图模块的 `Viewport` 读得到
  （`moment.rs:268` 的 `Viewport { rect, moment }`）。
- **但字符级降级已经在统一的出口做了。** `ansi::write_line`
  （`crates/atomcode-tui/src/ansi.rs:279`）对每一段文本调 `caps.text(_)`，
  把 `✓ ✗ ─ ┌ ◆ ▸ …` 换成 ASCII。所以块里画一个能降级的字形**不需要** caps。
  块真正需要 caps 的只有一种情形：**整块的存在或形状取决于终端能力**——ASCII
  替换救不了它。
- **`◆` 已有降级。** `caps.rs::ascii_for` 把 `\u{25C6}`/`\u{25CF}`/`\u{25CE}`
  映射成 `*`。所以品牌行的 `◆` 写字面是安全的。
- **`∙` 没有降级，而且被门数。** `gates/tui-layers.sh` 把
  `[┌┐└┘─│├┤┬┴┼✓✗⋯▸•]` 的字面出现次数记成棘轮 `literal_glyphs`；`U+2022`/
  `U+2219`（`•` `∙`）在 `ascii_for` 里映射成 `*`，但**不在棘轮的字形集里**。
  结论：`∙∙` 这类 bullet 走 `Caps::g(Glyph::Bullet)`，不写字面。
- **`Caps` 是 `Copy + PartialEq + Eq`**（`caps.rs:82` 的 derive），新位可以
  不新增，缓存键带上它也不贵。
- **颜色不是块决定的，是上屏时解析的。** 块产出 `Color::Role(Role)`
  （`frame.rs:149`、`frame.rs:154` 的 `Color::role`），真颜色由
  `ansi::encode_with` 拿 caps 解析。`Color::Role` 的 doc 把这条说得最清楚：
  「a module has no idea whether the terminal is light or dark, and threading
  that answer through every `render` and every `Content::lines` would mean
  every one of them could get it wrong. Instead they state the role and
  `encode_with` — which does know — resolves it.」**这句 doc 直接约束了本次的
  `RenderCtx`：它只能传「决定形状/存在」的能力，不能传 `palette`。** 详见
  设计 §三。
- **`Caps` 没有「终端能画 cell background」这一位。** tuix 的像素猫是
  `▀` + fg(上) + 逐格 background(下)，它的门是
  `colors && unicode_symbols && (modern_emulator || jediterm)`——两个环境变量
  启发式。atui 的 `Caps` 只有 `unicode`/`colors`/`palette`/`graphics`。
- **缓存键是宽度，两处。** `Settled.rows: RwLock<Option<(u16, usize)>>`
  （`block.rs:214`）与 `CachedRender.width: u16`（`block.rs:183`）。
- **滚动上界含全部 slot 的行数。** `Host::stream_height`（`host.rs:1939`）
  与 `scroll_limit`（`host.rs:1917`）走同一套算术；一个 0 行的块仍占 slot，且
  `blank_between`（`host.rs:278`）会给它与邻居之间留一行空白。
- **流与宿主是分开的。** `StreamWriter`（`block.rs:384`）是生产者拿到的能力，
  `Host::new` 里 `stream: RwLock::new(Stream::new())`（`host.rs:944`）是私有
  字段，生产者行够不到流本身——「流空不空」的判据在宿主手里。
- **resume 的历史是先折再跑。** `plugin.rs` 的 `Facts::catch_up`
  （`plugin.rs:215`）把 `client.session().events()` 折进模块，历史由
  `SessionLog::restore` 静默恢复。`Tui::run`（`plugin.rs:267`）里
  `catch_up` 在事件循环开始前调用（`plugin.rs:368`）。
- **命令表是活的。** `Commands::all()`（`crates/atomcode-tui/src/command.rs:138`）
  返回 `Vec<Command>`，`Command { name, about, takes }`。atui 的命令集里
  `login` 不存在，而 tuix 的 tip 池把它钉在第一位
  （`welcome_tips.rs` 的 `PINNED`）。
- **`Moment.cwd` 在 `plugin.rs:295` 写入**（`std::env::current_dir()`），
  `bin/atui.rs:176` 另有一处；两者都是启动时一次。
- **`conformance::facts()`**（`crates/atomcode-tui/src/conformance.rs:27`）
  是「由事实造出的块」的穷举，欢迎块不由事实而来，不会被它自动覆盖。

## 设计

### 一、新增面

| 位置 | 是什么 | 为什么在这一层 |
|---|---|---|
| `content::WelcomeBlock` | 一个 `Content` 实现，`kind = "welcome"`，`always_open()` | 块的内容与形状是「屏幕上长什么样」，属于 `content.rs` 那一层 |
| `block::RenderCtx` | `{ width: u16, caps: crate::caps::Caps }`，`Content::lines` 的新参数 | 能力必须在块能看见的签名里，否则整块取舍无法表达 |
| `module::Opening` | 问生产者要不要开场时给它的输入 | 让欢迎读者与「宿主有什么」解耦：生产者不该 `require::<Host>` |
| `Producer::opening` | 带默认实现的关联方法，`Option<Arc<dyn Content>>` | additive：`Transcript` 不动，未来别的生产者可以自行开场 |
| `Host::open_conversation` | 流为空时按挂载顺序问生产者要一个开场块 | 「流空不空」这个判据只在宿主手里 |
| `modules/welcome.rs` | `welcome::Welcome` 生产者（id `"welcome"`） | 一行一模块，与现有 9 个模块同构 |
| `rows.rs` 的 `tui-panel-welcome` | 挂上那个生产者 | 一格一行的插件化单位 |

`Opening` 的字段：

```rust
// crate::module
pub struct Opening {
    /// 这次会话的工作目录，**已经是显示用的字符串**（home 已折叠）。
    ///
    /// 折叠由调用方 `Tui::run` 做，不在这里做：`Tui::run` 所在的
    /// `plugin.rs` 本来就在读环境（`plugin.rs:295` 的
    /// `std::env::current_dir()`），而模块**一个环境变量都不该读**——
    /// `gates/tui-layers.sh` 的 `os_probes` 棘轮禁的是「上层直接探测环境」，
    /// 而 ADR 0008 的理由更直接：读环境的代码在开发机上永远绿。折叠做在上游，
    /// 模块就保持纯函数、可单测。
    pub cwd: String,
    /// 当前模型名。现取，不从 `Moment` 读缓存——`--model` 改的是 `llm` 那一行。
    pub model: Option<String>,
    /// 版本号，`env!("CARGO_PKG_VERSION")` 的读法固定在这里。
    pub version: &'static str,
    /// 屏幕上真实可用的命令，`Commands::all()` 给什么就是什么。
    pub commands: Vec<crate::command::Command>,
}
```

**home 折叠这个帮手目前在 atui 里不存在。** `collapse_home` 只在
`crates/atomcode-tuix/src/platform.rs:38`，那是另一个 crate，本条**不依赖它**
（atui 与 tuix 是两个前端，互相引用会把两个产品绑在一起）。所以要新增一个
小的折叠函数；落点是 `crates/atomcode-tui/src/text.rs`（已有的文本工具模块），
调用方 `Tui::run`。它需要一个显式传入的 home 目录，以便单测——tuix 的
`collapse_home_with(path, home)`（`platform.rs:45`）就是这个形状，照它写。

`Producer` 的加法：

```rust
// crate::module::Producer，带默认实现，object-safe
fn opening(&self, _at: Coord, _open: &Opening) -> Option<Arc<dyn Content>> {
    None
}
```

### 二、数据流与唯一调用点

```
Tui::run（plugin.rs:267）
  ├─ feed.catch_up(history)              ← 先折历史；resume 的会话在这里有了流
  ├─ Host::open_conversation(&Opening)   ← 新增，全仓唯一调用点，在 catch_up 之后
  │     ├─ 流非空 → 返回 false（resume 不产欢迎块）
  │     └─ 流为空 → 按 producers() 的顺序问每个 Producer::opening(at)
  │           ├─ Some(content) → emit(at) + settle，然后 break
  │           └─ None → 问下一个
  └─ 返回 true 表示欠一帧
```

**`break` 而不是「每个生产者都产」。** 规则是「流为空时就产」——第一个回答的
生产者产完之后流就不再为空，循环自然只有一个赢家。这样不需要「只允许一个欢迎
块」这种额外概念去拦第二个，规则本身就是拦。

**绕开 `Host::absorb`，是刻意的。** `open_conversation` 直接写 `self.stream`。
`absorb` 那条路对应一条**已提交的日志事实**，而这里合成的是一次开局表现，不是
事实：走 `absorb` 会污染 `SessionLog`，并在回放时重新走一遍。所以新开一个入口，
只写流、只写这一处。

**零行不产出。** 极端窄终端或 `colors == None` 且宽度装不下任何一段时，
`opening` 返回 `None`，**不是返回一个空块**。理由见现状事实：0 行的块仍占一个
slot，`blank_between` 还会再给它留一行空白，屏幕会多出一条莫名其妙的空行。

**补记（2026-09-16）：开局块落在对话区的哪一行，当时没写，实测与 tuix 不一致。**
「位置语义对齐 tuix」在本文只写了「随对话滚动」这一维，而二维上 atui 当时是
**短内容贴底**（`Host::stream_lines` 把剩余空行补在内容**之前**），于是新会话的
欢迎块贴着输入框、空行全在它上面——tuix 不是这样，它是 inline + 终端 scrollback，
正文与 footer 从屏幕第一行往下排（`retained.rs` 的「footer sits directly below the
last body row, not pinned to the screen bottom」）。已改成**空行补在内容之后**，
即对话从对话区顶部起排、向下生长。

这不是把滚动反过来：内容多于一屏时 rect 本来就是满的，两边都没有空行；而低于
那个阈值时 `scroll_limit` 为 0，读者本来就无处可滚。**「最新一行永远是屏幕最后
一行」只在「还没有一屏对话」时不再成立**，而那正是它不该成立的时候。两条判据钉住
它：`host.rs` 的 `a_conversation_shorter_than_its_pane_starts_at_the_top_of_it`
（含「满屏时没有空行」的阴性对照）与 e2e 的
`a_new_session_opens_with_the_welcome_and_it_then_scrolls_away`（欢迎块从对话区
第一行开始）。

这一维原先**一条判据都没有**，所以它跑偏了而全套仍然全绿——补判据时先验过：把
补空行改回前置，新判据判红（`left: 19, right: 0`）。

### 三、`RenderCtx` 与缓存键

```rust
// block.rs
/// 块能看到的那部分终端能力：**只放决定形状或存在的位**。
///
/// 不含 `palette`，因为颜色不是块决定的：块写 `Color::Role(Role)`，真颜色由
/// `ansi::encode_with` 上屏时解析（`frame.rs:143-149` 的 doc 就是为这条写的，
/// 它明确反对把明暗穿进每个 `Content::lines`）。这里同理，而且顺带换来一个
/// 好处——缓存不必因为切主题而整片失效。
///
/// 将来给 `Caps` 加位时，问一句「它决定形状吗」：是，加到这里（并跟着缓存键
/// 走）；否，别加。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShapeCaps {
    pub unicode: bool,
    pub colors: crate::caps::Colors,
}

impl ShapeCaps {
    /// 从终端测得的完整能力里取形状那部分。
    pub fn of(caps: &crate::caps::Caps) -> Self;
}

pub struct RenderCtx {
    pub width: u16,
    pub caps: ShapeCaps,
}

impl RenderCtx {
    /// 只要宽度的那条老路（测试与不关心形状的实现用）。
    pub fn bare(width: u16) -> Self;
}

pub trait Content {
    fn lines(&self, ctx: &RenderCtx) -> Vec<Line>;   // 原 lines(width)
    fn summary(&self, ctx: &RenderCtx) -> Line;      // 原 summary(width)
    // growing_text / always_open / as_tool_call 不变
}

impl Slot {
    pub fn rows_at(&self, ctx: &RenderCtx) -> (usize, Option<Arc<Vec<Line>>>);
}
```

**为什么是 `ShapeCaps` 而不是整个 `Caps`（这是本次最容易做错的地方）：**

- 传整个 `Caps` 会**把 palette 带进块**，而 `frame.rs:143-149` 已经写下这条
  禁令及其理由。更要紧的是实际后果：`Palette` 是**每次都会变的值**——它由
  `measure_palette()` 测出来（`surface.rs:703`），随终端主题变化。
- 缓存键一旦是 `(width, Caps)`，**切一次主题就击穿全部已结算块的渲染缓存**，
  26k 行的会话要在下一帧全部重渲染。
- 用 `ShapeCaps`（两个字段）则缓存键是 `(width, ShapeCaps)`，只有「终端能不能
  画 unicode / 有没有颜色」变了才失效。这是**真实**会变的东西（换终端），
  而主题不是。

**缓存键必须一起改，否则是静默 bug：**

- `Settled.rows`：`RwLock<Option<(u16, usize)>>` → `RwLock<Option<(u16, ShapeCaps, usize)>>`
- `CachedRender.width: u16` → `(u16, ShapeCaps)`，`starts_with` 那条前缀检查不变
- `RowIndex` 的失效判据：`width` + presentation revision → `width` +
  **`ShapeCaps`** + presentation revision

一旦块的行数依赖形状能力而缓存只按宽度索引，「换了终端能力仍是旧行数」会稳定
复现，而且它**看起来是好的**——这正是要在实施时配一条阴性对照的原因。

**`content_hash()` 不把形状能力算进去。** 形状决定的是「画不画猫」，与宽度
同类——宽度也不进 hash。`Content` 的契约写着「hash 覆盖语义内容，永不覆盖渲染
出的字节」（`block.rs:91`）。

**churn 边界。** `content.rs` 里的自由函数 `wrapped(text, w, style, prefix)`
**保持收宽度**，只有 `lines` 收 `ctx`。于是 `content.rs` 里 15 个 `Content` 实现
的内部基本不动，改的只是签名与调用点。总量约 15 个实现 + 约 20 处调用点（含
测试）。

调用点清单（`grep -rn "\.lines(" crates/atomcode-tui/src/` 的实测结果中属于
`Content::lines` 的那些）：`host.rs:1623`（`block.content.lines(room)`，
`rows_at` 返回 `None` 时的兜底）、`block.rs:122`（`summary` 默认实现）、
`block.rs:263`/`block.rs:317`（`rows_at` 的两条路）、`plugin.rs:1630`，
以及 `block.rs`/`host.rs`/`modules/*.rs` 内的测试。`ask.rs`、`commands.rs`、
`surface.rs`、`steering.rs` 里的 `.lines()` 是 `str::lines`/`String::lines`，
不动。

**`RenderCtx` 的构造点只有一个真源头：** `Host::compose` 本地持有
`moment: Moment`（`host.rs:1703`），`moment.caps` 就是它；`Host::stream_height`
与 `scroll_limit` 的契约是「调用方可能正持 moment 写锁」，所以它们**收 `&Moment`
而不自己读锁**——`RenderCtx` 也照这条走：宿主从手上那份 `moment` 造 ctx 再往下
传，不在 `rows_at` 内部读锁。

**住不进 `rows_at` 的那个调用点。** `host.rs:1623` 的
`None => Arc::new(block.content.lines(room))` 是一条**测试专用的兜底**：生产路径
上 `Slot::rows_at` 在 `width` 已量过时返回 `None`，调用方随后渲染也拿同一个
width，于是永远命中缓存、走不到这条兜底；而 `Host::new` 里缓存的初始 width 是
`0`（`host.rs:962` 的 `RowIndex { width: 0, .. }`），所以一条未量过的路径在测试里
会落到它。改造时它同样要收 `ctx`（width 与 caps 都取当前帧那份），否则会造出
「缓存说 3 行、实际画 5 行」的读数与画面不一致——而这一致的价值比「让兜底也走
缓存」重要。

### 四、欢迎块的四段内容与排版

形状照 tuix 的 `build_welcome_rows`，但每个字面装饰符都走已有通道（棘轮门）。

| 段 | 内容 | 约束 |
|---|---|---|
| 头行 | 左 `◆ AtomCode`，右 `v<version>  ·  MIT` | `◆` 已可降级；两者加宽度装不下时拆两行（tuix 的做法） |
| 左块 | 猫（够格才画）+ `cwd` / `model` 两条 bullet | bullet 走 `Caps::g(Glyph::Bullet)`，**不写字面 `•`/`∙`** |
| 右块 | 「快速上手」标题 + 至多 4 条 tip | 见下面的筛选规则 |
| 尾 | 一个空行 | tuix 留着它，免得后续异步输出（MCP 已连接、升级提示）贴上来 |

**两列还是堆叠，判据与 tuix 相同：** 只有当**最宽的那条 tip 真的放得下**时才并
两列。tuix 的注释记下了原因——tip 行不截断，两列模式若在放不下时启用，tip 会越
过右边缘被模拟器硬折行，把对齐的列打碎；它当初用一个偏小的固定估算值，结果在窄
终端上真的发生了。并且 **cwd/model 那两条只在两列块之下**，绝不 zip 进左列：
tuix 踩过，tips 比猫高时多出的行会落到 cwd/model 上，屏幕出现
`∙ proj/goal  set a goal…` 这种一行两事。

**caps 的门只有一个：**

```rust
use crate::caps::Colors;
let show_mascot = ctx.caps.unicode && ctx.caps.colors != Colors::None;
```

（`ctx.caps` 是 `ShapeCaps`，只有这两个字段——见 §三。）

- atui 的 `Caps` 没有「能画 cell background」这一位，所以**不复制** tuix 的
  `modern_emulator || jediterm` 两个环境变量启发式。这是有意的：环境变量推不出
  「背景色能不能画」，而推错的后果是猫的下半身消失、上半身还在（tuix 的注释
  记的正是 FinalShell 这类终端）。
- 只走前景色 ⇒ **一格一个像素**，而 tuix 靠 cell background 在同一格里塞了两个
  纵向像素。这带来一个必须算清的取舍：
  - 保留 tuix 的 9×8 像素图、用整块画 ⇒ 要 **8 个屏幕行**（终端格子约 1:2，
    画面纵向翻倍）；
  - 保留 tuix 的 **4 个屏幕行** ⇒ 纵向分辨率减半，须把 9×8 的源图**降采样成
    9×4**。

  **取后者**：与 tuix 同高，于是竖排算术与两列判据都不用动，而 9×4 的猫仍然
  认得出。降采样按**像素行成对 OR**（相邻两行有任一为实即实），不是简单丢一行
  ——丢行会让耳朵和下巴随机消失。
- **画的是整块 `█`，不是 `▀`。** 一格一像素且要画面连续，就必须用整块；`▀`
  只画格子上半，逐行叠起来会留下半格空隙，看着是条纹不是猫。
- **`█` 写字面，不新增 `Glyph` 变体。** 核对过 `gates/tui-layers.sh`：它数的字形
  集是 `[┌┐└┘─│├┤┬┴┼✓✗⋯▸•]`，**`█`（U+2588）不在里面**，所以写字面不顶棘轮；
  而 `ascii_for` 已经把 `\u{2588}` 映射成 `#`，宽度一列换一列。所以「新增一个
  `Glyph::Pixel`」是多余的——也不必复用 `Glyph::Thumb`（它的语义是「滚动条的
  实心部分」，不对，但也不需要它）。
- **源图直接用 tuix 那份 `MASCOT_ROWS` 常量原文**（4 行 × 18 字符 = 9 格 ×
  上下两像素，tuix 的测试已经钉住它的形状），降采样规则是：**一格里上下任一像素
  是身体（非 `.`）就画成一格实心**。这样不需要手推一张新图，`SOURCE` 就是tuix
  的原文，而格子算法是一行 `any`。
- 颜色不新增 `Role`：猫整体用 `Role::Brand`（「产品自己的标记」），眼睛留透明
  当孔洞。**不用裸索引**（如 `Color::AnsiValue(202)`）——`gates/tui-layers.sh`
  的 `raw_colours` 棘轮正是为禁这件事存在的，route 是「过角色调色板，角色在上屏
  时才解析成颜色，那里才知道明暗」。若日后猫要第二个颜色，正解是加一个 `Role`。

**tips 的来源改成活的命令表：**

- 候选是一份**策划过的命令名清单**（`login`、`resume`、`rows`、`skills`、
  `mcp`、`model`、`help`、`layout`、`audit`、`compact`、`context`、
  `transcript`、`reasoning`、`tools`、`mouse`…），**按 `Commands::all()` 实际
  挂上的过滤**。
- 每条 tip 的描述直接取那个 `Command` 自带的 `about`，不另写一份文案——否则
  命令的帮助改了、欢迎页还是旧的。

**随机来源：加一个 `rand` 依赖，只读一次种子。** atui **目前没有** `rand`（只有
tuix 有，`crates/atomcode-tuix/Cargo.toml:61`），所以这是一个新依赖。权衡过两条路：

- 用 `cwd` 走 FNV-1a（仓库已有同款算法，`block.rs:67`）做种子，零依赖；
- 加 `rand = "0.8"`，与 tuix 一致。

**选后者**，理由是这条缝的用途：第三方替换欢迎块时会想「真随机挑几条」，而一个
只能用 FNV 当种子的仓库等于让每个替换者自己写一遍洗牌。`rand` 是 workspace 里
已被 tuix 用的成熟依赖，代价只有 Cargo.lock 里多几行。**种子仍然只读一次**
（从 `cwd` 的哈希来），于是同一目录同一二进制每次渲染一致——这点必须保持，否则
每帧重摇的问题会回来，tuix 的 `welcome_tip_indices` 就是为它存在的。

**tips 的条数与「固定位」：** 至多 4 条（照 tuix 的 1 + 3）。tuix 把 `/login`
钉在第一位，但 **atui 的命令集里没有 `login`**（`grep` 实测：`commands.rs` 的
`Command::new` 列表里无此名）。所以固定位也过筛：筛掉之后由随机补足到 4 条这条
正是「筛」的意义——欢迎块永不推荐屏幕上没有的命令。

### 五、插件化：替换 / 覆盖 / 重写分别怎么做到

这是本任务点名要的性质，逐条对应到机制：

| 想要的效果 | 怎么做 | 机制 |
|---|---|---|
| **整体替换** | `[[patch]] id = "tui-panel-welcome"` 换 `name` 成第三方的行 | 行层叠：`Op::Insert` 同 id 替换整行；用户 `harness.patch.toml` 在最后一层（ADR 0017） |
| **整体拿掉** | `[[remove]] id = "tui-panel-welcome"` | 行不存在 ⇒ 生产者不挂 ⇒ `open_conversation` 没有候选，流直接开始对话 |
| **只换 tips 来源** | 不用改行——tips 从 `CommandsSvc::all()` 取，`/patch` 或另一行加命令集就改了 | 数据从缝里来，不从 const 里来 |
| **换内容/配色** | 改 `content::WelcomeBlock` | 这是 Rust 层面的重写，一行 + 一个 impl |
| **重写成完全不同的开局块** | 第三方 crate 实现自己的 `Producer::opening` + 自己的 `Content`，在 `SCREEN` 等价的 const 里插自己的行、`remove` 掉内置那行 | `rows::catalog()` 的注册与 `SCREEN` 的声明分离，正是 ADR 0010 那条「一行一个面板」 |
| **按能力退让** | 块自己读 `ctx.caps` 决定画不画猫 | 本条新增的缝 |

**明确不做的事：** 不拆「一段一行」（品牌/猫/cwd/tips 各自成行）。决定是按
YAGNI 取一个行；`SCREEN` 里一行一模块的粒度已经能让第三方整体替换，而拆到段
一级会把排版算术暴露成配置。

### 六、失败语义

| 情形 | 行为 |
|---|---|
| `caps.unicode == false` 或 `colors == None` | 不画猫；头行 + cwd/model + tips 照画 |
| 宽度装不下任何一段 | `opening` 返回 `None`，流保持空，**不留空行** |
| `LlmSvc` 取不到 model | 省掉 model bullet，不报错、不阻断开局 |
| `CommandsSvc` 为空 | 省掉 tips 整段 |
| resume（`catch_up` 折到历史） | 不产欢迎块。规则就是「流为空」，无需额外判断 |
| **resume 之后，第一次会话那块也没了** | **见下**——这是设计后果，不是遗漏 |
| `catch_up` 与 `open_conversation` 之间落了新事实 | 块被抑制。这不是 bug：此刻会话已不再是全新的 |
| 误调用第二次 | 不产第二块（流已非空） |

**「首次的那块也会消失」是刻意换来的，e2e 逼出来的发现。** 写判据时才发现：
`open_conversation` **刻意不走日志**（走 `absorb` 会把它写成一条会话事实、被回放、
被 compaction 计入 token），所以它不在 `SessionLog` 里。于是 resume 之后翻开日志
**看不到它**——不是"不产第二块"，而是"一块都没有"。

两条路，选了第一条：

| 选择 | 代价 |
|---|---|
| **不记日志**（现在） | resume 后没有欢迎块。换来：日志里没有模型没看过、却要计入 compaction 的"事实" |
| 记日志 | resume 后仍在。代价：日志多一条假事实，`SessionLog` 的语义被稀释，回放时它会出现两次（一次来自块、一次来自重放） |

这也是为什么第一条 e2e 与第二条 e2e 都要写：一条钉"新会话有且随对话滚走"，
另一条钉"resume 一块都没有"——**后者是前者的代价，不写下来下一个人会当成 bug 去"修"**。

对齐 AGENTS.md 的生命周期不变量：本条不新增 runtime、不改 provider/session
绑定、不碰审批与 cancel；`open_conversation` 不产生 pending 状态，也就不存在
「accepted operation 没有终态」的问题——它要么产出并 settle 一个块，要么什么
都不做，两者都是终态。

### 七、放弃了什么

**把欢迎块做成视图模块、骑在流头部（`El::Stream` 加一个 `head`，ADR 0020
`tail` 的镜像）。** 这是最省事的一条：复用尾部已有的几何与 pin 算术，而且
`Viewport` 里**本来就有** `caps` 与 `cwd`（`moment.rs:268`），`Content` 一行都
不用改。

放弃它的理由有三条，逐条都指向同一件事——欢迎块不是「每帧重画的现在」：

1. **它是「发生一次、有历史」的东西。** ADR 0004 把屏幕分成两类，这一类的归宿
   是流生产者；做成视图模块是借壳。
2. **高度会随宽度/cwd 变，于是顶动下方全部内容。** 这正是 ADR 0020 失效条件里
   点名的那类抖动（「尾部高度变化的频率高到补偿在观感上仍然抖」）。
3. **每帧重摇 tips 的问题会原样回来**，于是又得为它持久化一份索引——tuix 的
   `welcome_tip_indices` 就是那个补丁。

代价：改动跨 `Content` 签名与渲染缓存。收益：顺带开出一条通用缝（能力敏感的
块），这一条比欢迎块本身活得久。

**不做 caps 缝，吉祥物改用 ASCII 行（`(=^·^=)`，atui 已有）。** 最省事，且
`content.rs` 的 15 个实现一行不动。放弃是因为它同时放弃了「后续能力敏感的块有
路可走」，而那正是本次要开缝的理由。

**照搬 tuix 的 tip 池。** 15 条池里有 `/provider`、`/webui`、`/plan`、`/init`、
`/session`、`/goal`、`/loop`、`/language`、`/usage` 等 atui 未必挂上的命令，
照搬会推荐不存在的命令。放弃，改为活命令表筛选。

**复制 tuix 的 `modern_emulator || jediterm` 环境变量启发式来判断「能不能画
cell background」。** 放弃：环境变量推不出这个能力，推错的后果是猫画一半
（tuix 注释里记的 FinalShell），而只走前景色就从根上不问这个问题。

**给 `Caps` 新增一个 `cell_background` 位。** 放弃：新增一位意味着要改终端
探测（`Caps::detect` 那一堆环境变量判断）与所有构造点，而只前景色的猫不需要它。
真需要背景色的那天再谈。

**把整个 `Caps` 传进块（`RenderCtx { width, caps: Caps }`）。** 看起来更直白，
但它会把 `palette` 带进块，而 `frame.rs:143-149` 的 doc 已经为「角色在上屏时
解析」写下了理由；而且 `Palette` 是每次测量都会变的值（`surface.rs:703`），
缓存键带上它意味着**切一次主题就击穿全部已结算块的缓存**。改用只含两位的
`ShapeCaps`（§三）。代价是将来给 `Caps` 加位时要多问一句「它决定形状吗」——
这句话现在写进了 `ShapeCaps` 的 doc 里。

### 八、判据

1. `modules/welcome.rs` 单测：`ShapeCaps` 的四种组合（`unicode` × `colors`）下
   各段的在/不在；
   一张含 `login` 与一张不含的假命令表，验证固定位的两种行为；同 cwd 两次产出
   相同且 4 条不重复；空命令表 ⇒ 无 tips 段；窄宽度 ⇒ 堆叠而非两列。
2. **缓存键的守卫**（`block.rs`）：同一宽度下换一套 `ShapeCaps` 必须**重新
   渲染**，而不是命中缓存的行数——并配一条**阴性对照**（`ShapeCaps` 相同则命中、
   不重渲染，走 `LIVE_RESUMES` 那类 test-only 计数器）。另加一条：**同一宽度下
   换 palette 而 `ShapeCaps` 不变，必须命中缓存**——这条钉住「主题切换不击穿
   缓存」那个决定（见 §三种 `ShapeCaps` 而非 `Caps` 的理由）。这是整个改动里
   唯一会变成静默 bug 的地方，所以它值得两条判据。
3. host 层：空流上 `open_conversation` 恰好产出一个**已 settle** 的块；再调一次
   不加东西；`catch_up` 有历史时不产；块 `always_open()`。
4. e2e（`tests/e2e.rs`，headless 帧）：全新会话的首几行是欢迎块；**接着由测试
   驱动若干事实后，它随对话滚走**——滚回去能看见、滚到底后首行不再是它。这条
   直接钉住「随对话滚动」那个决定。（注意 e2e 的驱动模型：模型是回放 fixture、
   终端是录制器、键盘是脚本，所以要构造出足够多的对话才能把欢迎块推出视野。）
5. `content_hash` 在相同输入下两次构造稳定。**注意**：`conformance::facts()`
   覆盖的是由事实造出的块，欢迎块不由事实而来，不会被它自动覆盖，要显式写。
6. 门：`gates/tui.sh --fast` 通过；`gates/tui-layers.sh` 必须**持平或降**（尤其
   `literal_glyphs`——这就是 bullet 走 `Glyph` 的原因）；`tui-test-count` 的基线
   会被门自动抬，按 `0f7efde1` 的先例**单独提交**这次抬升。

### 九、实施顺序与交付

三个 commit，分开审：

1. `RenderCtx` 缝 + 缓存键（`width` → `(width, caps)`）。不含任何新行为——纯
   机械迁移单独成一个 commit，否则 diff 没法评审。
2. `WelcomeBlock` + `Producer::opening` + `Host::open_conversation` +
   `tui-panel-welcome` 行 + 调用点；判据 1、3、4、5。
3. 文档（ADR + 本 spec）+ 判据数基线抬升。纯文档与纯基线各不混进功能 commit。

**ADR。** 为本条缝单独写 `docs/adr/0021`——「块可以按终端能力决定形状」。它比
欢迎块本身活得久（下一个能力敏感的块直接用），而 `Content` 是 ADR 0004 划的那
条线的核心类型，改它的签名要留下论证。欢迎块自己的取舍（位置、粒度、tips 来源、
猫的形状）留在本 spec 里。

**worktree。** 已在 `.worktrees/session-welcome-block`（分支
`feat/session-welcome-block`，基线 `0f7efde1`）。顺带一个好处：主检出的
`Cargo.lock` 有那批私有依赖污染（`hkdf`/`hmac`/`zeroize_derive`），新 worktree
是干净的；但按 AGENTS.md，**push 前仍要核**
`grep -c 'name = "hkdf"\|name = "hmac"\|name = "zeroize_derive"' Cargo.lock`
为 0。

**验证命令。** 按 AGENTS.md 定标：跑测试用 `cargo nextest run -p atomcode-tui`
（该 crate 只有一个 test binary，nextest 与 `cargo test` 基本同速，但输出更
清楚）；`cargo fmt --all -- --check` 必须退出 0（阻塞门）。

### 十、失效条件

- 若出现**在 welcome 块上需要交互**的需求（点击某条 tip 就执行），「它是一个
  已 settle 的块」不够，要回到「每帧重画的视图模块 + 自己的矩形」。
- 若 cat 的像素画要精确复刻 tuix（半块 + 背景色），就必须给 `Caps` 加一位并
  重新论证「环境变量能不能判断背景色支持」。
- 若第三方要**只替换四段中的一段**而保留其余，一个行的粒度不够，那时才谈拆
  「一段一行」。
- 若 `open_conversation` 之外又出现第二处合成非事实的流内容，这个「只写流不写
  日志」的入口需要收敛成一个概念，而不是两处各写一遍。

### 十一、相关

- `docs/adr/0004` —— 流生产者与视图模块的划分，本条把欢迎块归给前者
- `docs/adr/0010` —— 「一行一个面板」，`tui-panel-welcome` 照此
- `docs/adr/0017` —— 配置在 fold 时装配，用户的 home patch 在最后一层（替换能
  生效的前提）
- `docs/adr/0020` —— 视图模块骑流尾部，本条**不采用**它的镜像（见「放弃了
  什么」）
- `crates/atomcode-tuix/src/render/retained.rs` 的 `build_welcome_rows` 与
  `welcome_tips.rs` —— 内容的参考来源，不是要照搬的实现
- `docs/tui-composability.md` —— 可组合性的论证
