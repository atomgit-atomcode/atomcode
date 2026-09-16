# 块可以按终端能力决定形状

状态: 提议(2026-09-15),尚未实现。承接 [`0004`](./0004-tui-stream-is-an-irreversible-block-sequence.md),
补它没回答的一件事。

## 背景:0004 划了两类,但没回答"块能不能看终端"

0004 把屏幕分成两类:**流生产者**(追加不可逆块)与**视图模块**(每帧全量重画、
无历史)。它把"块的形状由谁决定"这件事只答了一半:

- 主动权一路给了视图模块:视图模块拿 `Viewport`(`moment.rs:268` 的
  `Viewport { rect, moment }`),而 `Moment` 里**有** `caps`(`moment.rs:213`)
  和 `cwd`(`moment.rs:205`)。所以一个视图模块想说"这个终端画不了猫,就不画",
  它做得到。
- 块不行。`Content::lines(&self, width: u16)`(`block.rs:97`)只有宽度。同样
  一句判断,视图模块写得出,块写不出。

这不是疏忽,是当时刻意的收窄:**"字面装饰符降级"已经有一个统一的出口**。
`ansi::write_line`(`ansi.rs:279`)对屏幕上每一段文本调 `caps.text(_)`,把
`✓ ✗ ─ ┌ ◆ ▸ …` 换成 ASCII(`caps.rs::ascii_for` 那张表)。所以块里画一个
**能降级的字形**,本来就不需要 caps——它上屏时会自己降级,而且宽度同一列
换一列,对齐不受影响(`caps.rs:196-208` 记着这条性质)。

真正需要 caps 的是另一类:

> **整块的存在或形状取决于终端能力**——ASCII 替换救不了它。

一个半块像素猫就是这一类的样本。它靠 `▀` + 前景色 + **逐格背景色**凑出两个
纵向像素;终端不画 cell background 时,上半身还在、下半身消失,画面是碎的。
换 ASCII 也救不了:**没有等价的"一列两像素"表示**。唯一正确的处理是整块不画。

于是现状是两个坏选项:要么块里硬写一只可能画碎的猫,要么放弃这类块。

## 决策

**`Content::lines` 收一个 `RenderCtx`,它带**只决定形状**的那部分终端能力。**

```rust
// block.rs
/// 块能看到的那部分终端能力:**只放决定形状或存在的位**。
///
/// 不含 `palette`,理由见 §"为什么不是整个 Caps"。将来给 `Caps` 加位时问一句
/// "它决定形状吗":是,加到这里(并跟着缓存键走);否,别加。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShapeCaps {
    pub unicode: bool,
    pub colors: crate::caps::Colors,
    /// 这一位是那条"问一句"的规则的第一次真实使用,记在这里当范例。
    ///
    /// `cell_background`(终端画不画格的背景色)**决定形状**而不是颜色:半块靠
    /// 背景色画下半像素,不画时"猫的下巴"整片消失。所以它进来,并跟着缓存键走
    /// —— 换了终端就该重画,而不是沿用旧的形状。
    pub cell_background: bool,
}

pub struct RenderCtx {
    pub width: u16,
    pub caps: ShapeCaps,
}

pub trait Content {
    fn lines(&self, ctx: &RenderCtx) -> Vec<Line>;   // 原 lines(&self, width: u16)
    fn summary(&self, ctx: &RenderCtx) -> Line;      // 原 summary(&self, width: u16)
}

impl Slot {
    pub fn rows_at(&self, ctx: &RenderCtx) -> (usize, Option<Arc<Vec<Line>>>);
}
```

**渲染缓存必须跟着换键,否则这是一个静默 bug。**

- `Settled.rows`(`block.rs:214`):`RwLock<Option<(u16, usize)>>` →
  `RwLock<Option<(u16, ShapeCaps, usize)>>`
- `CachedRender.width`(`block.rs:183`):`u16` → `(u16, ShapeCaps)`;
  `starts_with` 那条前缀检查不变
- `RowIndex` 的失效判据:`width` + presentation revision →
  `width` + `ShapeCaps` + presentation revision

一行行的形状一旦依赖能力而缓存只按宽度索引,"换了终端仍是旧行数"会稳定复现,
**而且看起来是好的**。`ShapeCaps` 是 `Copy + Eq`,没有成本。

**`content_hash()` 不算形状能力。** 形状决定"画不画猫",与宽度同类——宽度
也不进 hash。`Content` 的契约写着"hash 覆盖语义内容,永不覆盖渲染出的字节"
(`block.rs:91`),而"猫有没有画出来"是渲染出的字节。

## 为什么不是整个 `Caps`

`RenderCtx { width, caps: Caps }` 更直白,但它是错的,而且这条禁令**已经写在
仓库里**。`Color::Role` 的 doc(`frame.rs:143-149`):

> a module has no idea whether the terminal is light or dark, and threading
> that answer through every `render` and every `Content::lines` would mean
> every one of them could get it wrong. Instead they state the role and
> [`crate::ansi::encode_with`] — which does know — resolves it.

`Caps` 里装着 `palette`(`caps.rs:97`),而 `Palette` 是**每次测量都会变的值**:
`measure_palette()`(`surface.rs:703`)按终端实际回答的背景色填它,随终端主题
变化。缓存键一旦是 `(width, Caps)`,**切一次主题就击穿全部已结算块的渲染缓存**
——一个 26k 行的会话要在下一帧全部重渲染,而屏幕上没有一个字变了。

`ShapeCaps` 只有两个字段,只有"终端能不能画 unicode / 有没有颜色"变了才失效。
这是**真实**会变的东西(换终端、`NO_COLOR`、`ATOMCODE_ASCII`),而主题不是。
代价是给 `Caps` 加位时要多问一句"它决定形状吗"——那句话写进了 `ShapeCaps`
的 doc。

## 放弃了什么

**让块自己去读环境。** 最省事,一行 `std::env::var("TERM")` 就完。放弃是因为
`gates/tui-layers.sh` 的 `os_probes` 棘轮禁的正是"上层直接探测环境",而
`docs/adr/0008` 给了理由:探测会让判据在开发机上永远绿、在别人的终端上错,
且没有测试会说话。`caps.rs` 是**探测的那一层**,`surface.rs` 是 I/O 屏蔽层,
其余一律收答案。

**给块一条"整块隐藏"的独立通道(比如 `fn hidden(&self, caps) -> bool`)。**
形状与存在是同一件事的两面,分成两个方法会长出"隐藏了但仍被量成 1 行"这类
不一致。一个 `RenderCtx` 同时回答"画不画"和"画多宽"。

**把 caps 塞进 `Moment` 再让块读。** 块的 `lines` 是纯函数,收 `Moment` 会把
输入、滚动位置、动画相位一起拖进来——那些**不应该**影响块画什么。`RenderCtx`
只给两样,正是为了让"块能看到什么"是枚举得出的。

**什么都不做,让这类块活不下去。** 现状就是这条,成本已经写清楚:要么画一只
会碎的猫,要么这类块根本不存在。

## 失效条件

- 若出现**必须在块里解析颜色**的需求(比如块要按终端明暗选两套不同的字形),
  `ShapeCaps` 两个字段不够,那时要重新论证"颜色只在上屏时解析"这条是否该让位。
- 若出现**按终端能力改变字形而非隐藏整块**的需求(比如宽字符降级成两格),
  形状能力可能要成为三元组(是否支持宽字符),`ShapeCaps` 的字段要重审。
- 若缓存键带 `ShapeCaps` 之后出现"能力频繁变"的真实场景(远程终端反复重连),
  需要一条失效策略;目前 `caps` 是一次探测、进程内不变,所以不需要。

## 相关

- [`0004`](./0004-tui-stream-is-an-irreversible-block-sequence.md) —— 本条补的是它没回答的第三件事:块能不能看终端
- [`0008`](./0008-animation-time-is-injected-not-read.md) —— 能力与时间同理:注入而不是读
- [`0020`](./0020-view-module-rows-can-join-the-scroll-tail.md) —— 视图模块与流的位置关系,与本条正交
- `crates/atomcode-tui/src/block.rs` 的 `Content`、`Slot::rows_at`
- `crates/atomcode-tui/src/frame.rs` 的 `Color::Role` doc —— 本条沿用它的论证
- `crates/atomcode-tui/src/ansi.rs` 的 `write_line` —— 字符级降级的统一出口
- 落地用例:`docs/plans/2026-09-15-session-welcome-block-design.md`
