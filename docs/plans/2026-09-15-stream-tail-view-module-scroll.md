# Stream 尾部：视图模块的行进入滚动序列

**决策依据:** [ADR 0020](../adr/0020-view-module-rows-can-join-the-scroll-tail.md)

**目标:** `todo`(任务清单)与 `live`(这一轮在做什么)的行成为流内容的一部分,
读者往回滚时跟内容一起上移,而不是钉在屏幕底部。

**不是目标:** 不改 `status` / `input` / `tip`;不改块序列的任何语义
(不可逆、`Live → Settled`、`content_hash` 冻结);不改布局树持久化
(布局树不落盘,`SessionEvent` 里没有 layout 变体 —— 已核实)。

---

## 进度与基线(2026-09-15 更新)

| | |
|---|---|
| branch | `feat/plexus-plugin-architecture` |
| 开工时 SHA | `72b9e431` |
| **当前 HEAD** | `fdbbe5d9` + 本轮评审修复 |
| 判据基线 | `gates/tui-test-count.baseline` = **495**(开工时 468),棘轮**只能升** |
| worktree | 只剩 `Cargo.lock`(见下)与未跟踪的 `docs/atomcode-crate-deps.html` |

**已提交:**

| commit | 内容 |
|---|---|
| `875a7465` | ADR 0020 + 本计划 + `tui-composability.md` 索引(rebase 前是 `c6ef4656`) |
| `0868f2c6` | 接缝 S1–S4 折回本计划(rebase 前是 `5a3feced`) |
| `1e35d91c` | 对比度下限抬高(`theme.rs` / `markdown/mod.rs`,另一件事,已单独提交) |
| `25a09e40` | **Step 1**:`El::Stream { tail }`,行为不变 |
| `9b47cb0b` | **Step 2**:pane 几何与签名,行为不变 |
| `b7fb3829` | 评审修复 #1(pin 只走一半)+ #2(swap 丢 tail / resize 假成功) |
| `b453eb4e` | 评审修复 #4(徽标锚点)+ #5(tail 总高封顶) |
| `c94a301a` | **Step 3**:`todo` 进 tail,三个构造点收成唯一入口 |
| `2b9580b2` | **Step 4**:`live` 进 tail,删掉让位判据 |
| `4247ed59` | **Step 5**:框架不动 / 封顶一致 / wide 下列宽 —— 三条判据 |
| `fdbbe5d9` | 本计划进度同步 |

**Step 3–5 已完成,ADR 0020 落地。** 剩下的都不是本次范围:

- 评审 #7 的宽度不一致(`scroll_limit` 按整屏宽量、画时用 stream 的 `rect.w`)
  仍在,`wide` 下最明显。计划 Step 5 一节记了它。
- `wide` 的侧栏 `findings` 从不挂载、`team` 没有成员时不占位 —— 这两件都是既有
  行为,写进 `the_wide_layout_puts_the_tail_in_the_conversations_column` 的注释了。

**约束:** `Cargo.lock` 不得提交 —— 本机 `crates/atomcode-codingplan-crypto/`
是私有覆盖(skip-worktree + gitignore),那份 lock 差异是 `hkdf`/`hmac`/`subtle`/
`zeroize` 这些私有 crate 的真依赖,而公开仓库的占位符并不声明它们。

---

## 几何(所有步骤共用)

设 pane 拿到 H 行,内容 = 块(B 行)+ 尾部(T 行):

```
tail_full    = Σ 各 tail 模块的 height()
visible_tail = (tail_full - scroll).clamp(0, H)
block_scroll = (scroll - tail_full).max(0)
block_rect.h = H - visible_tail
scroll_limit = (B + tail_full) - H
```

尾部内部按「距底部的偏移」自底向上分配;某模块 `visible == 0` 时**不 place**
(即 `part(...).is_none()`,与今天语义一致)。

尾部交付的是**可见高度**,模块自己排版(不刚性切行)——`todo::window` 会保住
frontier 并标 `+N 更多`,`live::render` 会在放不下 margin 时丢 margin 保文字。

---

## 接缝(已核实,动手前定死)

### S1. `tail` 必须是 id 列表,不是子节点

`El::Stream{tail}` 仍是**叶子**:`place_into`(`el.rs:401`)照旧交出整个矩形就停。
切 pane 的是 **host**:compose 的循环绑定了 `region` 本身(`host.rs:1179`),
所以它可以从同一个值里读到 `tail`,自己算出 `block_rect` + N 个尾部矩形。

**不能把尾部做成 `Stream` 的子节点。** 那样布局引擎会按 flex 给每个子节点一个
矩形 —— 永远全部 N 个可见,而我们要的恰恰是「能被部分滚出视野」。滚动概念一旦进
引擎,就得在引擎里重做一遍偏移算术。

> 一句话:**引擎对滚动一无所知**,`tail` 只是一个 id 列表。

### S2. 只有两个函数要真正理解 `tail`

`El::Stream` 落在 `prune` 的 `other => other.clone()`(`el.rs:371`)—— 原样返回、
不修剪;`modules()` 只递归 `Flex`/`Stack` 的子节点。所以新逻辑只在:

| 函数 | 改法 |
|---|---|
| `modules()`(`el.rs:299`) | **要改** — 收集 `tail` 里的 id。否则 `/hide todo` 报 `NotOnScreen`(`layout.rs:216` 查的就是 `tree().modules()`) |
| `prune()`(`el.rs:334`) | **要改** — 过滤 `tail` 列表(未挂载的剔除) |
| `place_into`(`el.rs:401`) | **不动** — 仍交出一个 rect |
| `wanted`(`el.rs:505`) | **不动** — 仍报 1,靠 `grow` 吃满 |

```
tail 声明了 todo,而树里仍有 El::Module("todo")  →  被渲染两次
```

**这是 Step 2/3 必须守住的不变量**:同一模块不得同时出现在 `tail` 和树中。
(见 Step 5.4 的判据)

### S3. tail 声明是「第三条路」,放在 `default_layout()`

面板定位自己的位置今天有两种做法:

| | 怎么定位 | `inject()` |
|---|---|---|
| `MascotPanel`(`rows.rs:249-266`) | **用 `LayoutOp::Show{side: Top}` 自己占位** | `["tui-modules","tui-layout"]` |
| `TodoPanel`(`rows.rs:365-375`) | **什么都不做**,只 `add_view` | `["tui-modules"]` |

`TodoPanel` 的注释就是 ADR 0020 的出发点:

```rust
// Mount only. Where it goes is the composer's to write — `LayoutOp::Show`
// has no side that means "above the field".
```

所以 `composer()` 是「宿主替这类面板写位置」的**唯一**地方。tail 声明放在同一层
(`default_layout()` 里构造 `Stream{tail}`),不是新机制,是那条既有约束的延伸。
ADR 里应把它描述成第三条路:**不是**「用 op 自己占位」,**也不是**「宿主写死一个
rect」,而是「宿主写死它属于可滚动尾部」。

### S4. `/hide` `/show` `/swap`:无新缺口,但有一条既存别扭(不修)

逐条核过 `Layout::apply`(`layout.rs:194-236`):

| op | 行为 | 结论 |
|---|---|---|
| `Hide{todo}` | 先查 `on_screen.contains(module)`(`:216`) | tail id 必须进 `modules()` —— 即 S2 ✅ |
| `Show{todo}` | 已在屏上时返回 `AlreadyOnScreen`(`:201`) | todo 在 tail 里时被正确拒绝 ✅ |
| `Swap{Target::Stream}` | `matches()` 只认叶子模式 | ⚠️ **这条原来写"语义不变 ✅"是错的** —— `swap` 当时按 `Target` 重建节点,tail 会丢;已由 `b7fb3829` 改成找到原节点整体克隆,并拒绝以 tail id 为目标。见下面"已修的地基缺陷"第 ② 条 |

**既存别扭(不是本次引入):** `/hide todo` 之后 `/show todo`,它作为**浮动兄弟
节点**插回来(`Region::split`,`size` 默认 1 行),**不会回到 tail**。今天也一样:
`/hide todo` 从 `composer()` 的 flex 里剪掉它,`/show todo` 同样插成浮动兄弟。
**不是回归**,可靠的回退路径一直是 `Undo`。**本次不碰**,写在这里免得下一个人
以为自己弄坏了什么。

---

## Step 1 — `el.rs`:`Stream` 带 `tail`(行为不变) —— 已落地 `25a09e40`

```rust
// el.rs:127 现状
Stream,
// 改为
Stream { tail: Vec<String> },
```

`El` 已 derive `Clone, Debug, PartialEq`(`el.rs:117`),加字段无需额外 derive。

**机械编译修复**(`El::Stream` 变结构体后所有构造/匹配点):

- `el.rs`: 294(`stream_over`)、312(`has_stream`)、401(`place_into`)、
  505(`wanted`)、563(`lay`)、896(测试 helper)、
  923、942、946、956、994、1014、1073、1133(测试)
- `layout.rs`: 128、141(preset 构造)、288(`matches`)、297、301(`swap`)、548(测试)
- `host.rs`: 1180(compose)、1323(`stream_rows`)、1522(`default_layout`)

新逻辑只落在 `modules()` 与 `prune()` 两个函数上(理由见 S2);`place_into` 与
`wanted` **不动**。

**完成判据:** `cargo nextest run -p atomcode-tui` 全绿 — tail 处处为空,行为不变。

---

## Step 2 — `host.rs`:几何与签名(行为不变) —— 已落地 `9b47cb0b`

> **下面的代码片段是当时的计划,不是现在的代码。** 签名此后变过两次
> (`stream_height` 从 `width` 到 `size` 再到 `room: Rect`;`last_width`/`last_size`
> 并成 `last_room`)。要读实现请读文件:几何在 `Host::pane_geometry` 与
> `Host::cap_tail`,补偿在 `Host::pinned`。这一节留着的价值是**为什么**,
> 不是**怎么写**。

### 2a. 尾部几何

新增纯函数(便于单测,不碰锁):

```rust
struct Pane {
    block_rect: Rect,
    block_scroll: usize,
    tail: Vec<(String, Rect)>,   // (id, 可见矩形);visible == 0 的不在此列
}

fn tail_geometry(pane: Rect, scroll: usize, heights: &[(String, u16)]) -> Pane
```

### 2b. `stream_lines` 收 offset

`stream_lines(&self, rect)`(`host.rs:965`)→ `stream_lines(&self, rect, scroll: usize)`。
现在它内部从 `moment.read()` 取 scroll(`host.rs:1001`),改成参数。

**倒走循环、`window_into`、`lids`、`blank_between` 一行不动。**

### 2c. `compose` 接线

`host.rs:1180` 的 `Region::Stream` 分支:

```rust
let pane = self.scroll_pane(rect, &moment);
frame.place("stream", pane.block_rect, pane.block_lines);
for (id, r) in &pane.tail {
    frame.place(format!("stream.tail.{id}"), *r, lines);
}
*self.hits.lock() = Hits { rect: pane.block_rect, rows: pane.owners, .. };
```

`Hits` 只索引块区,尾部的 y 落在 `block_rect` 之外 → `block_at` 返回 `None`
→ 点尾部什么都不发生(与今天一致)。**`RowOwner` 不需要新变体。**

### 2d. `stream_height` 收 `&Moment`

```rust
pub fn stream_height(&self, width: u16, moment: &Moment) -> usize
```

尾部高度依赖 `Moment`(`live` 由 `activity` 决定),而调用方可能正持写锁
(`host.rs:1329-1330` 的契约;`plugin.rs:876`、`915` 确实如此),所以不能内部读锁。

调用点(全部已核实可以传引用):

| 位置 | 处理 |
|---|---|
| `host.rs:1333`(`scroll_limit` 内) | 本就有 `moment` 参数,直接传 |
| `host.rs:734`、`762`(`absorb`) | 需小重构,见 2e |
| `plugin.rs:911`、`913` | 此处 `m` 已于 `:900` drop,不持锁;注意 `:914` 会再取写锁,须在此前完成调用 |
| 测试 `1651`、`1655`、`1666`、`1676`、`2334`、`2340`、`2987` | 传 `&Moment::default()` 或局部 moment |

### 2e. `absorb` 需要屏幕尺寸

pin 要 clamp 到新的 `scroll_limit`,而 `absorb` 目前只有 `last_width`。
**决定:新增 `last_size: (u16, u16)`**,由 `compose` 与 `last_width` 并列写入
(与 `plugin.rs:915` 的 click 路径语义一致,也避免留下「scroll 短暂越界」的
不可能状态)。

`absorb` 顶部改成 clone 一份 moment(现在 `:733` 的 `read()` 当场释放):

```rust
let now = self.moment.read().expect("moment poisoned").clone();
let held = self.last_size.0 > 0 && now.scroll.0 > 0;
let before = if held { self.stream_height(self.last_size.0, &now) } else { 0 };
// ... 分发 ...
if held {
    let grew = self.stream_height(width, &now) as i64 - before as i64;
    if grew != 0 {                                  // 现在: if grew > 0
        let max = self.scroll_limit(self.last_size, &now) as i64;
        m.scroll = ScrollPos((m.scroll.0 as i64 + grew).clamp(0, max) as usize);
    }
}
```

**注:** 空 tail 时 `grew` 的行为与今天等价,所以本步仍全绿。

**「行为不变」这句话与 2e 的符号化有张力,如实记下(评审第 6 条)。**
`if grew > 0` 改成 `if grew != 0` 只有在「块高在一次 absorb 里不会减少」时才等价,
而这一点当时**没有证明、也没有判据**。实测(一次性探针,非判据)把
`conformance::facts()` 逐条 absorb、每步测高,没有任何一次下降 —— 证据倾向于等价,
但 `Live → Settled` 换渲染器、工具调用合并折叠这些机制并没有被单独钉住。

所以结论分两层:Step 2 **对每个装配出来的树**行为不变(`with_no_tail_the_split_is_the_identity`
钉住的是切分,而这句说的是 pin);但「pin 从这个提交起就与从前等价」是一个当时
**没有根据**的断言,`9b47cb0b` 的 message 里把它写成了事实。真正修掉这条的是
`b7fb3829` —— 它把补偿收成一个方法并给了 activity 翻转的判据,那时符号化才有意义。

**完成判据:** 全绿。

---

## 已修的地基缺陷(外部评审,2026-09-15)

两条会在 Step 3/4 落地后发作为真 bug 的问题,已经在 Step 2 之后、Step 3 之前修掉
(`b7fb3829` / `b453eb4e`)。它们的判据现在是地基的一部分:

**① 补偿只挂在 `absorb` 上,而 tail 高度有一半不经过 `absorb`。**
`state.turn` 走事实(absorb),`Moment::activity` 由事件循环写(从不经过 absorb)。
先后无保证,所以读者回滚时每轮开始/结束屏幕会跳 tail 那两行。
**修法**:补偿收成 `Host::pin` 一个方法,五条路径共走(`absorb` /
`Host::set_activity` / Cancel ×2 / 点击折叠)。判据用 `set_activity` 触发 ——
这正是 bug 的要害所在。

**② `swap` 按 `Target` 重建节点,于是 tail 静默消失,`swap todo stream` 还会
把对话整个删掉并返回 `Ok("换位")`;`resize` 命中不到时返回原树 + `Ok`。**
**修法**:`swap` 改为找到并整体克隆命中的原节点;tail 里的 id 作目标直接拒绝
(新 `LayoutError::TailIsNotATarget`);`resize` 返回 `(tree, found)`,未命中报
`NotOnScreen`。

**③ 徽标锚点**:`stream_rect` 锚 `pane.block_rect`,不是整个 pane —— 否则
`0 < scroll ≤ T` 时它盖住尾部的最后一行右半截。

**④ tail 总高封顶**:`Host::cap_tail` 把总高封在 `pane_h - 1`,自底向上分配。
几何和 `stream_height` **必须用同一个封顶**,因此 `stream_height` 的签名是
`size: (u16, u16)`。不封顶时 tail 高于 pane 会让屏幕在 `[0, T−H]` 冻住
(实测 T=20/H=19 时 scroll 0 与 1 的 rect 完全相同)。

---

## Step 3 — 先接 `todo` —— 已落地 `c94a301a`

**三个构造点,一个入口。** 尾部要在三处出现:`host::default_layout()`(`host.rs:1517`)、
`layout.rs` 的 `focus`(`:144`)与 `wide`(`:157`)两个 preset。**漏掉一个,那个 preset
里 todo 就悄悄消失**,而 `named_twice` 照样绿 —— 它只防「画两遍」,不防「一遍都没画」。

所以抽一个**唯一入口**,三个构造点都走它:

```rust
// el.rs 或 host.rs，一处定义
pub fn scroll_region() -> Region { Region::stream().with_tail(TAIL) }   // 实际落地是这个
```

判据改成 **「每棵出厂的树里 todo 和 live 恰好出现一次」**,而不是「没有出现两次」。
后者对「一次都没有」是绿的,前者两边都拦。

其余:

- 拆 `host::composer()`(`host.rs:1497`):滚动部分(`todo`)与框架部分(`tip` + `input`)。
  **注意这时候 todo 不再是树的子节点**,所以它从 `composer()` 里移出不是「少一行」,
  而是「换个地方声明」(见 S3)
- **守住 S2 的不变量**:移走树里的 `El::Module("todo")` 与声明 `tail` 必须同时发生，
  否则 todo 被渲染两次
- 新增判据:**pin 符号对称** — 用 activity 翻转触发(`set_activity`),不用「todo 变短」
  —— 后者走 absorb 路径,而 absorb 那条路在 `b7fb3829` 之前就已经有判据了,
  盖不住 activity 那条

**`wide` 下 tail 会跟着 stream 缩进左边 65% 那一列**,今天是整宽。这是行为变化,
不是回归,写在这里免得被当成 bug。

**会红的测试:** `part("todo")` **不用改名** —— owner 就是裸 id(评审第 8 条,
`b7fb3829` 已改)。受影响的只有那些断言 `stream` rect 高度的用例:现在流区分出去
了尾部那几行。

---

## Step 4 — 接 `live`,删让位 —— 已落地 `2b9580b2`

- `tail: vec!["todo".into(), "live".into()]`
- `modules/live.rs`:**删** `showing()` 里的 `if !moment.scroll.is_at_bottom() { return None }`
- `modules/live.rs`:**删**测试 `the_line_gets_out_of_the_way_while_the_reader_is_scrolled_up`(`live.rs:567`)
- `host.rs:2488` `a_scrolled_up_reader_is_not_shown_the_live_line`:**改写** —
  它锁的正是被删的行为。新语义:滚上去后 live 仍在滚动内容里,再滚一行才出视野
  (替代原来的「整体消失」断言)
- `host.rs:2420` / `2463`:原断言「live 令 stream 高 -2」不再成立(现在 live 在 pane 内),按新几何改

**这一步完成后:视图模块不再读 `scroll`**(全仓仅 `live` 曾有此特权)。

---

## Step 5 — 补齐无兜底的风险 —— 已落地 `4247ed59`

**夹具先要真的挂上尾部。** `host.rs` 的 `host()` 只挂了 `transcript`/`status`/`input`,
没挂 todo/live,也不声明 tail —— 用它写尾部判据会**恒真**:`tail_heights` 返回空,
几何退化成恒等,断言什么都过。Step 2 已经为此建了 `host_with_peek_tail()`(带一个
`Peek` 仪器模块,高度只跟 activity 走)。Step 3/4 需要一个挂**真** todo/live 的版本
(即 Step 3 之后的 `default_layout()` 本身),判据从这里出发。

1. **pin 符号对称** —— 已在 `b7fb3829` 落地(用 activity 翻转触发)。Step 3 接上真
   模块后,判据应当仍走 `set_activity` 这条路,而不是靠「todo 变短」
2. **框架模块不受 scroll 影响**:`status` / `input` / `tip` 的 rect 在任何 `scroll` 下不变
   — 守住「输入框永不移动」(`modules/tip.rs:10-22`)
3. **`stream_height` 与实画一致**(把 `host.rs:2294` 的模式扩到尾部):
   块区行数 + 各 tail 模块实际 place 的行数 == 该高度(**都用封顶后的值**)
4. **同一模块不得同时出现在 `tail` 和树中**(S2 的不变量)。
   判据:构造一棵**故意**同时含 `tail: ["todo"]` 与 `El::Module("todo")` 的树,
   断言它被判红。这条已经落地(`El::named_twice` + `Layout::apply` 拒绝 +
   `the_layout_this_build_ships_names_no_module_twice`)

**写判据时的两条陷阱(评审第 9、11 条):**

- **别断言「块的行位置不变」。** `scroll` 在 `[0, T]` 区间里 `block_scroll` 确实是 0,
  但 `block_rect.h` 每格长一行、内容底对齐 —— 块在屏幕上**每格下移一行**,顶上同时
  多露出一行更旧的。ADR 里「块纹丝不动」那句说的是偏移,不是屏幕坐标,已改。
  要断言的「不动」是:`scroll` 跟着 tail 的高度变化一起调整,使得**同几行字**仍在
  视野里(这正是 `b7fb3829` 的判据在做的事)。
- **`stream_height` 现在收 `size` 不是 `width`**(`b453eb4e` 改的)。尾部封顶要用
  pane 高度,而 pane 高度是 `stream_rows(size, moment)` 算的。

**已知的宽度不一致(评审第 7 条,本次不修):** `scroll_limit(size, …)` 内部按
`size.0`(整屏宽)量块高,而画的时候用的是 stream 自己的 `rect.w`。`wide` preset 下
两者是 65% 和 100%,所以滚动上界可能比实际少算。`plugin.rs:911/915` 有同一个问题。
这是**既有**缺陷,与本次改动无关,记在这里以免被当成新引入的。修的时机是有人真的
在 `wide` 下滚到顶发现末尾够不着的时候。

---

## 验证

```bash
# 每步之后
cargo nextest run -p atomcode-tui
cargo test --doc -p atomcode-tui        # nextest 不跑 doctest(本 crate 可跑 0,确认没丢)

# 每步收尾
bash gates/tui-test-count.sh            # 判据数只能升(当前 486)
bash gates/tui.sh
bash gates/tui-layers.sh
bash gates/tui-string-slice.sh
bash gates/tui-negative.sh

# 交付前(唯一阻塞门)
cargo fmt --all && cargo fmt --all -- --check
```

**不要** `--workspace`(AGENTS.md:9 个 consumer 各开不同 feature,全量会编 19 次)。
本次无跨 crate 依赖变化,`-p atomcode-tui` 足够。

---

## 退化风险与兜底

| 风险 | 兜底 |
|---|---|
| 尾部绕过 `window_into` → 整块物化 | ✅ `host.rs:1686` `COPIED_ROWS == drawn`(不并入缓冲区时天然成立) |
| 滚回时渲染量随会话增长 | ✅ `host.rs:1614` |
| `height` 与实画行数失衡 | ✅ `host.rs:2294` + Step 5.3 |
| pin 单边补偿 → 抖动 | ✅ `b7fb3829` 的 activity 翻转判据已覆盖 |
| `stream_height` 内部读锁 → 死锁 | ⚠️ 会挂而非静默;`stream_height` 收 `&Moment`,调用方传守卫 |
| **同一模块同时在 tail 与树中 → 画两遍** | ✅ `named_twice` + `Layout::apply` 拒绝 + `every_shipped_tree_rides_the_tail_exactly_once` |
| **一条路径补偿、另一条不补** | ✅ `b7fb3829` 收成 `Host::pinned`,四条路径共走;`opening_a_call_leaves_the_row_that_was_clicked_where_it_was` 钉住点击那条(底部也钉) |
| **封顶时先扣掉 live** | ✅ `b453eb4e` 反向 + `a_capped_tail_keeps_the_live_line_and_cuts_the_plan` |
| 尾部每帧小量渲染 | 可忽略(`scroll == 0` 时与现状相同;滚过之后反而被算术跳过,不再渲染) |

---

## 已知未验证

- **没有实测帧耗时。** 本仓无 bench(`crates/atomcode-tui/benches` 不存在),
  `gates/` 的性能判据都是**计数**类(`COPIED_ROWS`、`asked`),且只数块、不数模块渲染。
  「尾部增量成本可忽略」是从代码结构推的,不是量出来的。
- `absorb` 的 `last_size` 是新字段,需确认 `Host` 的其它构造/测试路径
  (`host.rs:1626`、`2348`、`2402`、`2979`、`2983`、`3095` 直接调
  `scroll_limit` / `stream_rows`)不受影响。
