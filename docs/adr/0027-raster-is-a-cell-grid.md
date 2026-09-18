# 位图是字符格，不是像素

状态: 提议(2026-09-15),未实现。承接 [`0026`](./0026-pictures-in-blocks-not-yet.md),
补它"路一"的字符格版本。

## 背景:0026 否决了图形协议,但没回答"那密集图形怎么办"

0026 的结论是:真图形协议(Sixel / iTerm2 / Kitty)在我们的滚动模型下**没有定义好
的语义**——我们从不让终端滚动(`ansi::encode_rows` 是每帧绝对寻址重画),而 Kitty
规范要求"终端滚文本时图跟着滚";`text::for_screen` 的整体不变量"我们发出的任何
字节都不移动光标"又和它们直接冲突。

它给出的重开条件里,"路一"是:**图是 rect-anchored 的一块,TUI 跳过它**。但那条
路解决的是"图**不滚**",没解决"图**怎么画**"——而画法这件事,同一个生态里已经有
一个成熟的答案。

**Claude Code 的 `Raster` 没有用图形协议。** 它是:

```ts
{ key, columns: 1..512, rows: 1..256,
  cells: base64(columns*rows 个小端 u32 三元组 [codePoint, fg, bg]) }
```

配一个 `$.ui.blit({ requestId, key, cells })` 只更新变化的格。**那个"在终端里玩
Tetris"的 demo 是这么画出来的**,不是像素协议。一条说明:`cells` 的码点必须是
"a printable width-1 BMP character (blocks, box drawing, braille too)"。

**这条路的全部好处是:它就是格子。** 滚动、diff、能力降级、containment——
我们已有的东西一件都不用改。

## 决策

**位图是一个一等元素:一张字符格网格,每格是 `(码点, 前景色, 背景色)`。**

我们的 `Span { text, style }` 与 `Style { fg, bg, .. }`(`frame.rs:76-83`)
**恰好就是这三样**,而 `sgr()` 真的把 `48;5;n` / `48;2;r;g;b` 写到终端
(`ansi.rs:253-256`)。所以这不是新建渲染能力,是**给已有的三样一个网格形状**。

配套决定六条,每条都有一条硬事实撑着:

**① 位图住在视图模块,不住流块。** 因为**已结算的块内容冻结**是类型性质
([`0004`](./0004-tui-stream-is-an-irreversible-block-sequence.md)):`Settled` 不给
`&mut`,`StreamWriter::amend` 对已结算块返回 `false`。位图住在流块里就只能活在
`Live` 期间,一个 turn 边界把它冻死。视图模块的定义就是"每帧全量重画、无历史"
(`module.rs:1-9`),正是位图要的。

**并且它不能是「骑流尾部」的模块。** [`0020`](./0020-view-module-rows-can-join-the-scroll-tail.md)
规定 tail 里的模块**不再有自己的布局矩形**;而位图要写进某一格就必须有一个**稳定的
矩形**,否则"第几格"每一帧都在变。所以位图的矩形只能来自布局树
(`Region::view(id)` 或 `LayoutOp::Show { side, size }`)。

**位图内容经 `Moment` 到达模块,不经服务。** 这一条是初稿漏掉的,补它的过程值得
记:`View::render(state, viewport)` **拿不到任何服务**——trait 的 doc 把这条说得很
硬("a module therefore *cannot* capture a `Context`, a service handle, a channel
or a clock — the signature makes the ... obligation unrepresentable rather than
merely discouraged")。所以"位图住在视图模块里"还不够,必须回答"它怎么看见位图"。

答案是仓库已有的那个形状:`Moment` 是"不是事实、但此刻为真"的那一半
(`cwd`/`caps`/`members`/`notice` 都在里面),而 `refresh_members`(`plugin.rs:694`)
正是"一个 row 读服务、把活数据写进 `Moment`"的先例。

于是 `Moment` 多一个字段、宿主的 `compose` 每帧把快照放进去:

```rust
pub rasters: RastersView,      // 不可变快照;里面的 Raster 是 Arc<Raster>
```

**两条由这个选择推出的约束:**

- **`Rasters::view()` 必须 O(1)**(把存着的 `Arc` 克隆一份)。重建整张表只发生在
  `write` 上——热路径(每帧)免费、冷路径(写入)付费,与 `LiveCache`/`Settled`
  同一取舍。
- **位图模块的高度必须来自配置**(`Height::Fixed`),不能按位图内容算。因为
  `View::height` 收的是**调用方手上那份** `Moment`(`stream_height` 等路径传来的
  那份里 `rasters` 可能是空的),按内容算高度会让高度取决于"快照在不在"。

**代价说清:视图模块的矩形是固定的,所以位图不随对话滚。** 这与我们为欢迎块那只
猫做的选择(随对话滚动)不冲突——**它们是两个不同的东西**:

| | 随内容滚的图形 | 原地刷新的小部件 |
|---|---|---|
| 归宿 | 流块(不可逆、有历史) | **视图模块(每帧重画、无历史)** |
| 能否原地刷新 | ❌ 冻结后不能 | ✅ 每帧可换 |

"能滚的图还能原地刷新"等于要求**一个不冻结的流块**——那会碰 0004 的核心不变量,
是一条独立且更大的决定,本 ADR 不做。

**② 寻址是 `(模块 id, key)`,不写别人的矩形。** Mods 能写进别人的渲染点
(`component + requestId`);我们不。理由:`Frame::containment_violations` 保证的是
"一个模块画不出自己的矩形",而按任意 owner 写会直接废掉那条保证。代价是比 Mods
弱一档,换来的是几何是真的、`--audit` 仍能看出"挂了没画"与"画了没挂"。

**③ 字形的判据是「宽度 1」,不是「非 Ambiguous」。braille 可以用。**

这一条**推翻了本 ADR 最初的写法**(原稿说"必须宽度 1 且非 Ambiguous,braille 暂拒"),
理由是查证时发现的一件**既有的、全 UI 范围**的事实。先给权威数据
(UAX #11 `EastAsianWidth.txt`):

| 码点 | EAW | |
|---|---|---|
| `2800..28FF` braille | **N** | **Neutral** |
| `2580..258F` `█▀▄` | A | Ambiguous |
| `2592..2595` `▒` | A | Ambiguous |
| **`2500..254B` 全部框线** `─│┌┐└┘├┤┬┴┼` | **A** | **Ambiguous** |
| `25C6` 品牌记号 `◆` | A | Ambiguous |

`unicode-width` 的 `width`/`width_cjk` 正是这个分类的两种读法(它的 doc 明说
`width` 把 Ambiguous 当 1 列),所以 **`width != width_cjk` 就是"Ambiguous"的判据**。

**于是「非 Ambiguous」不能用**:它会拒掉 `█▀▄▒`,**以及这个 UI 已经在用的
每一条框线**。而"Ambiguous 被终端当双宽"是一个**全 UI 范围的既有暴露**——开了
那个设置的终端上,每一个面板边框、每一条表格横线**今天就已经错位**,与 Raster
无关。在一个已经全是 Ambiguous 网格的 UI 里,单独拒收 Raster 里的 Ambiguous
字符既不保护什么,又让 Raster 无字符可用。

**判据因此回到这个 crate 既有的权威**:`width.rs` 的 `unicode-width::width(c) == 1`
(窄约定),加上"可打印的 BMP 字符"。这与 UI 其余部分的假设**完全一致**。

**braille 因此是安全的,而且是这批候选里最安全的一档**(Neutral);原稿把它当危险
项是**反的**。顺带更正一处仓库注释:`crates/atomcode-tuix/src/render/qr.rs`
说 braille 是 "Unicode-Ambiguous width"——按 UAX #11 它是 N,**那句注释是错的**
(它把 Braille 版 QR 做成 opt-in 的理由因此不成立;不过 opt-in 本身无害)。

**这一条不改 Raster 的设计,但改了一条既有隐患的记录**:ambiguous-as-wide 是
UI 级暴露。真要防它,正解是给 `Caps` 加一位并在 `Caps::g` 里退回 ASCII 框线
(`+ - |`),形状与既有降级机制完全一样——**那是独立的一件事**,不在本 ADR。

**④ 能力门是必须的,因为位图**没有**降级退路。** `caps.rs:586-591` 已经为
braille 写下这条理由:

> The braille set is deliberately absent from the downgrade table — it trades one
> column for one column, and there is no one-cell ASCII stand-in that reads as
> motion. So the *set* has to follow the terminal, or the shield that exists to
> stop tofu draws eight of them.

位图的字符与 spinner 同类:**整组换,不是事后改写**。所以 `caps.unicode == false`
时不画(与欢迎块的猫同一处理),而不是指望 `ascii_for`——它里面没有 braille,
画上去就是 tofu。

**⑤ v1 只做行级增量;格级要量化先行。** 行级**已经免费**:`Lines::of` 按 `Line`
值比对算脏行,未变的行复用上一帧字节,`patch_from` 只发脏行(`ansi.rs:403-465`),
而 `ROWS_ENCODED` 就是钉住这件事的尺子。位图落进 `Frame` 之后自动享受到它。

格级要动的是一条**不变量**,不是加代码:`encode_rows` 的注释写着"Erase first,
draw second, one row at a time. **Erasing a row cannot scroll the screen and
cannot reach the scrollback**"。做到格级 = 放弃整行擦、改成只覆盖变了的格,
**erase 没了**,而 diff、scrollback 安全论证与几条既有判据都建在"一帧是一组独立
行载荷"上。

**所以格级有一个可测的门槛**:先用计数器(照 `ROWS_ENCODED` 加一个
`CELLS_ENCODED`)量出"一帧重编码了多少格"与实际字节数;**没有量化数据不做格级**。

**⑥ 上限由算式给出,不抄别人的。** 一格是 3 个 u32 = 12 字节。Mods 的
512×256 = 131072 格 = 1.5 MiB 原始数据(base64 后约 2 MiB)。位图的设计目的是
"一块小部件"而不是"一整屏",所以 v1 取更小的上限(建议 128×64 = 8192 格 =
96 KiB),**且超限是拒绝而不是截断**——截断会让调用方以为自己画上了。

**⑦ 名字不叫 `blit`。** 它是图形学的词(位块传送),而这里没有位块、没有传送,
只有"换掉一块字符格"。叫 `Rasters::write`。这不是吹毛求疵——[`0025`](./0025-blocks-may-shape-by-terminal-capability.md)
那条缝的存在理由正是"块的存在或形状取决于终端能力",而"blit"会让下一个读代码的
人以为我们在往终端写像素。

## 交互:一半有路,一半是硬缺口

驱动用例含"可交互",而它要拆成两半:

**指针有路。** `Action::ClickAt(u16, u16)`(`keymap.rs:41`)已是单元格坐标,
`Host.hits` 是一张"这个矩形是什么"的表。一个模块的矩形登记进 `hits` 就能收到点击。

**键盘今天没有缝。** 三条核实结论一起看:`Moment.focus: Option<String>`
(`moment.rs:163`)**声明了但全仓没有读者**;`Action` **没有任何"交给模块"的变体**;
按键路径是固定的 `keys.resolve(press)` → `Action` → `Tui::act` 的大 match。

**所以不能声称本设计能做出"能玩的小部件"**——能做**动画**与**能点的**东西,
不能做靠键操作的。补这条缝有现成先例可照:`Host::context_menu_key`
(`host.rs:1235-1243`),它的 doc 一句话说清了形状:

> `None` when nothing is open, so the caller falls through to the ordinary keys —
> **focus is arbitration, and this is where the menu is given it or not.**

即"先问持有焦点者;它不接,才落到普通键"。给模块做同一条是**独立工作**,
本 ADR 不做,只点明 `Moment.focus` 是它的落点。

## 放弃了什么

**图形协议(Sixel / Kitty / iTerm2)。** [`0026`](./0026-pictures-in-blocks-not-yet.md)
已论证:没有契约,且与 `for_screen` 的不变量冲突。本 ADR 是那条路的**字符格替代**,
不是绕过。

**把位图放进流块,好让它随对话滚。** 放弃是因为已结算块冻结(0004)——放进去
只能活在 `Live` 期间。要两者兼得必须先重新论证 0004,那是一件比本 ADR 大得多的事。

**Mods 式的"写进别人的渲染点"。** 放弃是因为它会废掉 containment 保证
(决策②)。我们换到的是一档更弱的能力 + 一条仍然有效的几何保证。

**格级 diff(暂)。** 见决策⑤:先量化。这条不是"不做",是"没有数据不做",
门槛写死在 ADR 里。

**v1 收 braille。** 见决策③——而且**推翻**了本 ADR 最初"拒收 braille"的写法:
按 UAX #11 它是 N(Neutral),是候选里最安全的一档;我们最初以为它 Ambiguous,
来源是仓库里一条错的注释。

**`blit` 这个名字。** 见决策⑦。

## 失效条件

- **若位图必须随对话滚**:视图模块的选择失效,出路是给流块一个"不冻结"的类别,
  或让位图成为流块里可变的特例——**那要重新论证 0004**(内容冻结是它的核心),
  不是加标志位。
- **若行级重画的字节数成为实测瓶颈**:开格级,动 `encode_rows` 的先擦后画。
  前置是决策⑤那条量化,且要重新论证"erase 整行"与 scrollback 安全。
- **若"Ambiguous 当双宽"在目标终端上成为常态**:那是 **UI 级**问题(每个面板边框、
  每条表格横线都会错位,不止位图),正解是给 `Caps` 加一位并在 `Caps::g` 里退回
  ASCII 框线(`+ - |`)。**在 Raster 里单独处理它没有意义**——Raster 用的字符与
  框线同类(都是 Ambiguous)。
- **若出现第二个"外部往已挂载的东西写、模块每帧读"的场景**:`Asks` 与 `Rasters`
  会长成两种同形的东西,那时该抽一个共用的形状,而不是并排第三张表。
- **若 `Caps` 探测升级到拿单元格像素尺寸**(`[16t`):Raster 是**字符格**的,
  不需要它;那只对 0026 那条路有意义。**不要把像素尺寸混进这个模型**——它会把
  "格"变成"格里的坐标",而那正是 0026 那一整套麻烦的开端。

## 相关

- [`0026`](./0026-pictures-in-blocks-not-yet.md) —— 图形协议为什么不做;本 ADR 是
  它"路一"的字符格版本
- [`0004`](./0004-tui-stream-is-an-irreversible-block-sequence.md) —— 内容冻结,
  本 ADR 选视图模块的直接原因
- [`0025`](./0025-blocks-may-shape-by-terminal-capability.md) —— 块的能力缝;
  本 ADR 的能力门与它同源
- [`0020`](./0020-view-module-rows-can-join-the-scroll-tail.md) —— 视图模块的行与
  滚动的关系;位图**不进** tail(它有固定矩形)
- `crates/atomcode-tui/src/ansi.rs` —— `Lines::of`(行级增量)、`ROWS_ENCODED`
  (尺子)、`encode_rows`(先擦后画的不变量)
- `crates/atomcode-tui/src/caps.rs` —— `SPINNER` 与它的两条判据(braille 为什么
  不进降级表)
- `crates/atomcode-tuix/src/render/qr.rs` —— **它说 braille 是 Ambiguous,而按
  UAX #11 是 N(Neutral)**;决策③更正了这一点
- UAX #11 `EastAsianWidth.txt` —— 决策③那张表的权威来源
- `crates/atomcode-tui/src/plugin.rs` 的 `refresh_members` —— "row 读服务、把活数据
  写进 `Moment`" 的先例,位图内容正是走这条路进模块
- `crates/atomcode-tui/src/ask.rs` 的 `Asks` —— 新表要照的形状
- `crates/atomcode-tui/src/host.rs` 的 `context_menu_key` —— 键盘焦点那条缝的
  先例
- `docs/plans/2026-09-15-raster-blit-design.md` —— 设计、判据与实施顺序
