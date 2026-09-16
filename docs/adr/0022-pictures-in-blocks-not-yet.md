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

## 决策

**不做。** 具体地：

1. **`caps::Graphics` 保持现状**：枚举与 `detect` 不动，`Caps.graphics` 继续报
   它测到的值（iTerm2/Kitty），但**没有任何绘制点**。它不是"半成品",它是一块
   已经量好、还没接线的表——改动它之前先读本条。
2. **欢迎块（`docs/plans/2026-09-15-session-welcome-block-design.md`）用字符**，
   精度靠"一格多像素"而不是图形协议：
   - `█` → 一格一像素（9×4）；
   - `▀▄█` → 一格两像素（9×8），且这些字形是 **Unicode-Neutral 宽度**，
     没有终端会把它们拉伸（tuix 的 `render/qr.rs` 已经踩过这条路）；
   - Braille（U+2800–U+28FF）→ 一格 **2×4 个点**（9×16），但它是
     **Ambiguous 宽度**：iTerm2 默认「ambiguous 当双宽」会把它横向拉 2 倍，
     所以它必须是一个 caps 门，而不是默认。
   这三档全部落在 [`0021`](./0021-blocks-may-shape-by-terminal-capability.md)
   那条缝里——不碰 `Frame`、不碰 `for_screen`、不碰 diff、不碰滚动。
3. **重开条件与第一步见下。** 本条不是"永远不做"，是"没有验证前不做"。

## 重开条件

**条件：有人能在真终端上观察到 Warp/iTerm2/kitty 三者中至少两者的实际行为，
并把观察写下来。** 这一步是整条路线的前置——因为它要回答的问题是「契约是什么」，
而那个问题**只能靠看**。没有这一步，写下的任何编码器都是在猜。

第一步**不是**写编码器，是这份最小复现（约 20 行）：依次发三个协议的各一条
最小放置序列，每一次之后**自己重画固定网格**（模拟我们的一帧），然后观察：

| 要观察的 | 为什么是它 |
|---|---|
| 图像是否随 `\x1b[{row};{col}H` 重画而移动/复制/残留 | 这是"我们从不让终端滚动"的直接后果 |
| 图像是否被 `ERASE_LINE` 清掉 | 我们的每帧都先擦后画（`ansi.rs:501`） |
| 图像是否把光标推走 | 推走了下一行的绝对定位就失去意义 |
| 图像在**光标被推走后**画在哪 | iTerm2 协议没有放置参数，一切取决于光标 |
| 与 `for_screen` 的相对位置 | 确认它确实在吃 ESC（`text.rs:116`），以及需要哪种放行 |

**三条协议都要试**，包括已被判定不可行的 iTerm2——因为 iTerm2 协议是 Warp 上
支持最久的那条，而观察它失败的方式比读文档更能定住问题。

实现上要试的第二件事（**只在第一步有结论之后**）：给 `Frame` 加一种不透明载荷
（一个 `Part`，其行不进 `write_line` 的转义处理），把载荷排除出 `LastPainted`
的逐行 diff。这是三步里唯一有设计工作的一步。

## 放弃了什么

**"先随便接一个能用的协议"**（比如在 Warp 上接 Kitty，因为 Warp 支持它）。
放弃是因为**在我们的滚动模型下它没有定义好的行为**——接上去大概率表现为
「滚动时图撕裂/残留/复制」，而那时最难的不是修，是**说不清哪里错了**，因为
没有一条规范管这个交叠处。先花半天把观察做出来，比先接再查便宜。

**给 `Caps` 加一个"图像协议已探到"的字段并让欢迎块按它分支。** 现在 `Graphics`
已经承担这个角色；加第二份是同一个事实的第二个来源。

**把 9×4 的字符猫当成"够用了"不再想这件事。** 本条明确留了口子：`▀▄█`
（9×8）与 Braille（9×16）都是零风险的精度提升，且不依赖任何本条的结论。
`▀▄█` 应当做；Braille 做成 caps 门。

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
  Braille，含 Braille 的 ambiguous 宽度坑）
- `docs/plans/2026-09-15-session-welcome-block-design.md` §四 —— 猫的形状
