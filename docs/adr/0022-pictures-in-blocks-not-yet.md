# 图片块：评估后不做，条件写在这里

状态: 已决定(2026-09-15)——**当前不做**。不是"难",是**没有契约**：唯一可用的
协议与我们自己的滚动模型之间没有定义好的语义。重开条件见最后一节。

## 背景：问题是怎么来的

欢迎块里的吉祥物现在是字符画的。人在问一件合理的事：能不能**直接控制像素**，
而不是字符？那样猫会精细得多。

`caps::Graphics`（`caps.rs:69`）看起来像已经准备好了：`None / Sixel / ITerm2 /
Kitty` 四个变体，`Caps::detect` 也认得出 Kitty(iTerm2/ghostty/WezTerm)。但
`grep -rn "Graphics::" crates/atomcode-tui/src/` 只有枚举定义与 `detect` 的
赋值——**零个绘制点**，而 `Caps.graphics` 自己的 doc 就写着「Nothing draws one
yet——the field is here because it belongs to the shield」。

所以本条要回答的是：**要不要把它接上。**

## 三条协议，逐条核过

### iTerm2（`ESC ] 1337 ; File = …`）

规范原文（iterm2.com/documentation-images.html）：参数只有
`name / size / width / height / preserveAspectRatio / inline`。

**没有 id，没有放置位置，没有源矩形裁剪。** 它在**光标所在处**画，画的框是
`width`/`height`（单位可以是字符格 `N`、像素 `Npx`、百分比 `N%`、或 `auto`），
然后把光标推进到自己占的行列数。

对我们意味着两件事：

1. **不可裁剪**。把一个 9×8 的图钉在某一行、随对话滚走，就得让它随内容移动——
   而协议里没有"移动一个已存在的图"这个动作，只能重画。重画要重新计算光标位置，
   而光标一旦被推走，就得在每个图之后重置一次光标（多一次绝对定位）。
2. **与 `text::for_screen` 冲突**。那个函数（`text.rs:79` → `eat_escape`
   `text.rs:116`）会吃掉 ESC 序列，它的不变量是「我们发出的任何字节都不移动
   光标」。OSC 1337 是一条不移动光标的 OSC……**除了**它画完图会把光标推进。
   iTerm2 的 doc 没说可以禁止这个推进（对比 Kitty 的 `C=1`）。所以这条协议的
   语义与那个不变量直接冲突。

### Kitty（`ESC _G … ESC \`）

规范（sw.kovidgoyal.net/kitty/graphics-protocol/）核到的关键点：

- **有 id 与 placement**，可以给同一个图多次放置。
- **有源矩形裁剪**：`x, y, w, h` 指定要显示的原图区域。
- **有 `C=1`**：放置图像时**不移动光标**（默认 `C=0` 会移）。
- 原文一条硬要求：**「When scrolling the screen (such as when using index cursor
  movement commands, or scrolling through the history buffer), images must be
  scrolled along with text.」**

这一条就是我们自己的模型与它协调不了的地方——见下一节。

### Sixel

`caps.rs` 的注释写得很准：「Sixel cannot be [detected from the environment]：它
需要一次 DA1 查询与回复，那是 I/O」。实现代价最高（调色板量化 + RLE），而且
**Warp 不支持它**（Warp 的图形支持是 iTerm2 协议 2025-03 起、Kitty 协议
2025-05 起）。

## 症结：我们的"滚动"不是终端的滚动

这是本条的实质结论，不是实现难度。

**atui 画的是 alt-screen 上的固定网格，每帧绝对寻址重画。**
`ansi::encode_rows`（`ansi.rs:496`）：每行以 `\x1b[{row};{col}H` 定位，先
`ERASE_LINE` 再写。滚动是**宿主自己算偏移**（`Moment::scroll` / `Host` 的
`scroll_limit`），然后把该显示的行重新画到同样的绝对坐标上。

所以：**我们从不让终端滚动。** 一帧里那些行的**屏幕坐标一个字都不变**，变的
是画在那里的内容。

而 Kitty 协议要求的恰恰是「终端滚动文本时带着图像一起滚」。两条模型对不上：

- 我们从不用终端的滚动 ⇒ 终端**永远不会**替我们移动图像；
- 我们按帧重画 ⇒ 图像若不重画就留在原处（而原处的内容已经换了）；
- 图像不是单元格 ⇒ `ansi::Rows`/`LastPainted` 那套「行字节没变就跳过」的 diff
  对它不成立（载荷几百字节到几 KB，每帧重发不可接受）。

结论：**这不是"实现难"，是"没有定义好的语义"。** 三步可以把它变得有定义
（给 `Frame` 加不透明载荷、按帧重建 placement/相对锚点、把载荷排除出 diff），
但那是**渲染管线的第二条通路**，不是欢迎块的一个细节，而在动手之前没有一个人
验证过「在 Warp 上这么画到底会怎么样」。

## Warp 的额外一层：它的渲染模型是逐块的

Warp 不是一个纯网格终端：输出被切成**块**（ScriptExecution / output grid /
prompt-header grid），它在自己的 issue 与 PR 里明确把图像当作「可见的网格内容」
来撑块高、并把图像完成事件路由到输出块（warpdotdev/warp PR #10478）。

而我们的 TUI 是**在固定网格上每秒重画多次**的 alt-screen 应用。在 Warp 的模型
里，这更像一个持续重绘的块——图像 placement 的滚动与裁剪语义在这个交叠处
**两边的规范都没有定义**。

Warp 的 Kitty 支持本身也有已知边界：静态放置可以，`a=f`/`a=a`（动画帧与动画
控制）**明确不支持**，Unicode 占位符也不支持（warpdotdev/warp 的 issue 里维护者
自己说「there isn't much support internally for pushing kitty image protocol
forward」）。我们不需要动画或占位符，但这是个信号：这条路径在 Warp 上会长期
处于「能用但边缘」的状态。

## 业界怎么做的（2026-09-15 查证）

**结论先说：主流 Rust TUI 画图的做法是「图像是一块 TUI 必须跳过的格子矩形」，而且它不滚。**
这条查证把上面的结论说清了——契约不是"普遍不存在"，而是**只对 rect-anchored
的图存在，对"随内容滚"的图不存在**。

### ratatui-image（Rust TUI 画图的事实标准）

`Picker` 探测 → `ProtocolType { Halfblocks, Sixel, Kitty, Iterm2 }` → 每个协议一个
编码器模块。它的 doc 里有一段正是我们这个问题：

> Some protocols, like Sixels, are essentially "immediate-mode", but we still need
> to avoid the TUI from overwriting the image area, even with blank characters.
> Other protocols, like Kitty, are essentially stateful, but at least provide a way
> to re-render an image that has been loaded, at a different or same position.
> **Since we have the font-size in pixels, we can precisely map the
> characters/cells/rows-columns that will be covered by the image and skip drawing
> over the image.**

**「跳过那块格子的绘制」是它的核心机制**，而且它靠两样支撑：

1. **字体像素尺寸**（`[16t]` 查出来）→ 才能算出图覆盖哪些行列；
2. **图像是 widget（有自己的 rect），不是滚动内容**。所以它不需要回答
   「图怎么跟着滚」——它的图不滚，位置是布局给的。

### 能力探测的完整配方（`cap_parser`，ratatui-image 内部模块，非独立 crate）

| 问什么 | 发什么 | 收什么 |
|---|---|---|
| Kitty 图形 | APC `_Gi=31…` | `\x1b_Gi=31;OK\x1b\` |
| Sixel / 矩形操作 | DA1 `\x1b[c` | `\x1b[?62;4c`（4=sixel）、`\x1b[?62;28c`（28=rect ops）|
| 单元格像素尺寸 | `\x1b[16t` | `\x1b[4;H;Wt` |
| 查询结束 | DSR `\x1b[0n` | `\x1b[0n` |

两条实操约束：**必须在进 alt screen 之后、开始读终端事件之前发**（否则吞掉输入）；
并内置一份**已知有 bug 的协议黑名单**——`Konsole` 的 Sixel、`WezTerm` 的 Kitty
（ratatui-image 的兼容表明确写着 WezTerm「would support Sixel and Kitty, but only
iTerm2 actually works bug-free」）。

### 能引用的编码器（这一层是真能抄/能引的）

| 协议 | 纯 Rust 件 | 形态 |
|---|---|---|
| Sixel | **`icy_sixel`** | 100% Rust，无 C 依赖；`sixel_encode(&rgba, w, h, &opts)`，量化用 quantette（Wu + Floyd-Steinberg），可配像素长宽比与透明 |
| Kitty | `kitty-graphics-protocol`、`little_kitty`、`kittage` | APC 命令构造；`kittage` 覆盖除动画帧外的全部客户端侧 |
| iTerm2 | 无需依赖 | base64 + PNG——本 crate 已有 `base64` 与 `png`（粘贴图片在用）|

**最值得照搬的是形态**，`indexable-inc/index` 的 `packages/kitty` 把这件事说清了：

> it turns image bytes into the `APC _G ... ST` escape sequences ... **and nothing
> else. It does not open a terminal, decode images, or do I/O**: callers own those
> concerns and decide where the returned `String` is written.

**编码器是纯函数，I/O 与放置归调用方。** 这就是"引用一部分能力"该有的边界——也
正是本条说"可以引"的那一层。

### 终端侧：那份"图跟着滚"的完整模型在哪

`qwertty_term_vt::kitty`（ghostty 的 Zig 实现移植到 Rust，作者自标为
"flagged library-extraction candidate"）里有 `storage::ImageStorage`——per-screen
image map、placement map、字节上限淘汰、delete 分发，以及 `exec`（光标跟踪的放置）、
`unicode`（`U=1` 占位符）。

**它证明「滚动 + 图像」在终端侧是有完整模型的**（pin-anchored placements，正是
kitty 规范那句"必须随文本一起滚"的实现）。但它是**终端侧**：输入是客户端发来的
字节，输出是"画什么"。我们是客户端，引不到它，也不需要——我们的答案是"不让它滚"。

### 由此修正本条的结论

原稿说"没有定义好的语义"。更准确的是：

| 图的形态 | 有契约吗 |
|---|---|
| **rect-anchored**（一块固定矩形，TUI 跳过不画）| **有**，而且业界标准做法明确（ratatui-image）。落地要三件：`Frame` 支持"这块 rect 不画"、编码器、探测 |
| **随内容滚**（我们的欢迎块要的）| **没有**便携契约。Kitty 可靠 id 重新放置 + 删除旧 placement 做到；Sixel 无 id，只能重发，而"清掉旧的"在格子模型里不可靠 |

**于是有一条决策相关的事实**：**半块（`▀` + `bg`）是唯一保住"随对话滚走"的路**，
因为它就是字符——不需要跳过矩形、不需要探测、不需要编码器、滚动免费。任何像素路
都强制把图变成 rect-anchored 的一块，而那与我们选定的语义（随对话滚动，见
`docs/plans/2026-09-15-session-welcome-block-design.md` §一）直接冲突。

**所以：欢迎块的猫永远走半块。像素路只对"固定位置的图"成立**——若哪天要画工具
输出里的图表，那是一个**独立于欢迎块的问题**，且那时应当按 ratatui-image 的形态做
（图 = rect-anchored widget + 跳过矩形），而不是把它做成流内容。

### 与决策的关系：不变

上面没有推翻"不做"的决定，只是把它说得更准，并**给出一条更便宜的重开路径**：
若将来真要做，第一个可用的形态是 **rect-anchored 图**（不是滚动内容），而那一步
需要的东西是三件明确的工程，不是"先验协议"。

## 决策

**不做。** 具体地：

1. **`caps::Graphics` 保持现状**：枚举与 `detect` 不动，`Caps.graphics` 继续报
   它测到的值（iTerm2/Kitty），但**没有任何绘制点**。它不是"半成品",它是一块
   已经量好、还没接线的表——改动它之前先读本条。
2. **欢迎块（`docs/plans/2026-09-15-session-welcome-block-design.md`）用字符**，
   精度靠"一格多像素"而不是图形协议：
   - `█` → 一格一像素（9×4）；
   - `▀▄█` → 一格两像素（9×8）。**（更正）** 本条早先说这些字形是
     "Unicode-Neutral 宽度，没有终端会把它们拉伸"——按 UAX #11
     `2580..258F` 是 **A（Ambiguous）**，不是 Neutral。它们与 UI 其余部分
     同一假设：**全部框线** `2500..254B` 也是 Ambiguous，所以用它们并不比
     画一个面板边框更危险。
   - Braille（U+2800–28FF）→ 一格 **2×4 个点**（9×16）。
     **一处更正（2026-09-15 查证）**：本条早先写「Braille 是 Ambiguous 宽度，
     iTerm2 默认拉双宽，所以必须是一个 caps 门」——**那句是错的**，来源是
     `crates/atomcode-tuix/src/render/qr.rs` 的一条错注释。按 UAX #11
     `EastAsianWidth.txt`，`2800..28FF` 是 **N（Neutral）**；反倒是
     `2580..258F`（`█▀▄`）与**全部框线** `2500..254B` 是 **A（Ambiguous）**。
     所以 Braille **不需要**为宽度加 caps 门——它是候选里最安全的一档；
     而 `▀▄█` 与 UI 其余部分同一假设（框线也是 Ambiguous）。详见
     [`0023`](./0023-raster-is-a-cell-grid.md) 决策③。
   这三档全部落在 [`0021`](./0021-blocks-may-shape-by-terminal-capability.md)
   那条缝里——不碰 `Frame`、不碰 `for_screen`、不碰 diff、不碰滚动。
   **这与业界一致**：ratatui-image 自己的 `Halfblocks` 兜底也是这一档（它注明
   "should work in all terminals, even if the font size could not be detected,
   with a 4:8 pixel ratio"），opencode 的 logo 也是半块 + 投影。
3. **真要更精细，缺的是一位 caps bit 而不是新管线**：`Style` 已支持逐 span 的
   `bg`（`frame.rs:77`，`ansi::sgr` 会写出来）。半块加投影（opencode 那种：
   `_`=空格+bg 投影、`^`=`▀` fg 主体 + bg 投影）只多需要「这个终端能画格背景」
   一位。这比图形协议便宜两个数量级。
4. **重开条件与第一步见下。** 本条不是"永远不做"，是"没有验证前不做"。


## 重开条件

查证之后分岔成两条，它们的门槛不同，所以分开写。

### 路一（便宜，且业界已走通）：图是 rect-anchored 的一块，TUI 跳过它

**先决条件不是技术，是一个产品选择：图能不能不随内容滚？** 能，这条路就是已知的
三件工程，没有未知：

1. `Frame` 支持"这块 rect 不画"——`encode_rows`（`ansi.rs:496`）对图占用的格子
   **连 `ERASE_LINE` 都不发**（发了就把图擦了，这是 ratatui-image 那段 doc 的
   原话所指）；
2. 编码器：Sixel 引 `icy_sixel`；Kitty 引 `kittage`/`little_kitty`；iTerm2 自己拼
   （base64 + `png`，两个依赖都已在）；
3. 探测：照 `cap_parser` 的配方自己写（`surface.rs` 已经有读终端应答的机制——
   OSC 11 读背景色用了 libc `poll`/`read` + 超时，同一套），**在进 alt screen
   之后、读事件之前发**。

**唯一还需要观察的**（而这个只能靠看）：`ERASE_LINE` 与图的关系、以及图会不会把
光标推走。一个约 20 行的最小复现就够——依次发三个协议各一条最小放置序列，每次之后
自己重画固定网格，观察图是移动、复制还是残留，以及光标是否被推走。

### 路二（贵，且有未知）：图要随内容滚

**先决条件：有人能说清"图跟着内容滚"在至少两个终端上的实际行为。** 这条路的
关键是 Kitty 能用 id 重新放置 + 删除旧 placement，而 **Sixel 没有 id**：只能重发，
且"清掉旧的"在格子模型里不可靠——所以路上大概率会收敛成"Kitty 才行"，
那就等于放弃了 Sixel 与 iTerm2 终端。

**这是两条路里更贵的那条，因为未知在"清旧的"而不在"画新的"。** 先做路一的复现，
再决定要不要走路二——路一的观察同时是路二的前置。

## 放弃了什么

**"先随便接一个能用的协议"**（比如在 Warp 上接 Kitty，因为 Warp 支持它）。
放弃是因为**在我们的滚动模型下它没有定义好的行为**——接上去大概率表现为
「滚动时图撕裂/残留/复制」，而那时最难的不是修，是**说不清哪里错了**，因为
没有一条规范管这个交叠处。而且查证表明：**问题不是"选哪个协议"，是"图该不该滚"**
——先定这个，协议只是实现细节。

**把 ratatui-image 当依赖引进来。** 它是 ratatui 的 widget（`Image`/`StatefulImage`
实现 `ratatui::Widget`），而我们的帧是自己的 `Frame`/`Line`/`Span`，中间隔着一整套
ratatui 的类型。**能引的是它下面的编码器与探测配方，不是它这一层。**

**给 `Caps` 加一个"图像协议已探到"的字段并让欢迎块按它分支。** 现在 `Graphics`
已经承担这个角色；加第二份是同一个事实的第二个来源——而且现在知道**能探到的
东西比 `Graphics` 一个枚举多**（单元格像素尺寸、矩形操作、文本缩放），真要探测
就该一次探清并一起上报，而不是逐个加字段。

**把 9×4 的字符猫当成"够用了"不再想这件事。** 本条明确留了口子：`▀▄█`
（9×8）与 Braille（9×16）都是零风险的精度提升，且不依赖任何本条的结论。
`▀▄█` 应当做；Braille 做成 caps 门。**而这两位之上还有一档**：半块 + 投影
（opencode 的做法）。

**（已落地，2026-09-15）** 这一档做了：`Caps::cell_background` 与
`ShapeCaps::cell_background` 已加（见 [`0021`](./0021-blocks-may-shape-by-terminal-capability.md)
的字段说明），欢迎块的猫用它画一格两像素的半身，缺这一位时退化成"一格一像素"的
实心块。判断环境的方式照 tuix 原样搬（`WT_SESSION` / `TERM_PROGRAM` / `jediterm`）。

## 失效条件

- 若某天 atui 改为**使用终端自身的滚动**（进 scrollback、用 `IND`/`RI`/`SU`
  随文本滚图），那么 Kitty 协议那条「随文本滚」的要求第一次变得**可满足**，
  本条的结论要重新论证。这是最可能改变结论的一条路。
- 若 Warp 支持了 Sixel，或把它的块模型换成纯网格，则"逐块渲染"那一层约束消失。
- 若出现**必须在块里画真图**的硬需求（比如工具输出里的图表、图片 diff），
  这条路线会从"锦上添花"变成"必须",那时先做第一步观察而不是先写编码器。
- 若 `text::for_screen` 的不变量被改写（不再吃掉所有 ESC），第二条通路的一半
  障碍就没了。

## 相关

- [`0021`](./0021-blocks-may-shape-by-terminal-capability.md) —— 本条要的精度，
  在"字符多像素"这一档上走的就是它那条缝
- [`0004`](./0004-tui-stream-is-an-irreversible-block-sequence.md) —— `Frame`
  是「行」的序列，本条讨论的正是一个不是行的东西
- [`0006`](./0006-tui-is-full-screen-not-inline.md) —— alt-screen 全屏，这是
  「我们从不让终端滚动」的由来
- `crates/atomcode-tui/src/caps.rs` 的 `Graphics` 与 `Caps.graphics` 的 doc
- `crates/atomcode-tui/src/ansi.rs` 的 `encode_rows` —— 滚动是重画，不是滚动
- `crates/atomcode-tui/src/text.rs` 的 `for_screen` —— 唯一挡住 ESC 的那道门
- `crates/atomcode-tuix/src/render/qr.rs` —— 字符多像素的两个先例（`▀▄█` 与
  Braille）。**注意它的注释里有一条错的**：它说 Braille 是 Ambiguous 宽度，
  按 UAX #11 是 N（Neutral）；见 [`0023`](./0023-raster-is-a-cell-grid.md) 决策③
- `docs/plans/2026-09-15-session-welcome-block-design.md` §四 —— 猫的形状

查证过的外部件（引用前先读本条"业界怎么做的"）：

- `ratatui-image`（Rust TUI 画图的事实标准）—— 「图像是 TUI 必须跳过的格子矩形」
  那段 doc，以及它每个协议一个编码器模块的结构
- `cap_parser` —— **ratatui-image 的内部模块，不是独立 crate**；它的查询序列
  （DA1 / `[16t` / `\x1b_Gi=31`）与已知 bug 黑名单（Konsole 的 Sixel、
  WezTerm 的 Kitty）是可抄的配方
- `icy_sixel` —— 100% Rust 的 Sixel 编解码，无 C 依赖
- `kittage` / `little_kitty` / `kitty-graphics-protocol` —— kitty APC 的客户端侧
- `indexable-inc/index` 的 `packages/kitty` —— **"只编码、不碰 I/O"** 的形态范本
- `qwertty_term_vt::kitty`（ghostty 的 Zig 实现移植到 Rust）—— 终端侧那份
  "图跟着滚"的完整模型（`storage::ImageStorage`、pin-anchored placement）；
  我们引不到，但它证明那件事在终端侧是有解的
- `opencode`（sst/opencode）—— 半块 + 投影的 logo（`packages/tui/src/logo.ts`
  的 `_`/`^`/`~` 模板 + `run/splash.ts` 的 `cells()`），4 行高，无条件用格背景
