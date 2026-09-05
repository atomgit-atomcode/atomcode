# TUI 的时间与空间可组合性

**状态:设计,未实现。** 这份文档论证一件事——把 TUI 拆成插件之后,
[`plexus-plugin-architecture.md`](./plexus-plugin-architecture.md) 里那两条可组合性
在**只有一块屏幕**的地方还成不成立。

realm、fiber、可逆效应这些底座概念不在这里重复定义,它们的家在那份文档里。
这里只处理一个问题:**屏幕是全局共享的物理资源,凭什么还能组合。**

---

## 零、两条性质,按 TUI 自己的性质定义

不是从 cordis 类推的,是从终端本身的性质来的:

**时间。** 用户看到的流不可逆。它有**运行态**和**终态**:运行态可以变,终态不能变,
但可以折叠、隐藏。只能顺序增加。

**空间。** TUI 分成若干模块,每个占据一部分窗口。模块可以随意组合、变换。

这两条比"屏幕是日志的投影"更强,而且推翻了本设计的前一版(见 §八)。

---

## 一、时间:不可逆由借用检查器执行

### 模型

流是**块序列**,每块单向走完一次生命周期:

```
Live ──settle──> Settled            单向,无返回边
```

不可逆不能靠约定。**未被执行的规范约等于不存在**,所以它必须落在类型上:

```rust
pub struct Stream {
    settled: Vec<Block>,          // 只有 push
    live:    Option<Block>,       // 唯一可变的那一个
}

impl Stream {
    pub fn open(&mut self, block: Block);
    pub fn amend(&mut self) -> Option<&mut Block>;   // 只借得到 live
    pub fn settle(&mut self);
    // 刻意没有:任何能拿到 &mut settled[i] 的东西
}
```

「终态不能变」这条约定的执行者是 **`&mut` 的可达性**——不是 review,不是注释。

### 「不能变」与「可以折叠」如何共存

把两件事分开就没有矛盾:

| | 可变吗 | 谁决定 | 量具 |
|---|---|---|---|
| **内容** 块里记录的事实 | **否**,settle 后冻结 | `fold` | `content_hash` 恒定 |
| **呈现** 折叠/隐藏/折行宽度 | 是 | `render` | 行数、可见性 |

于是 resize 不违反不可逆:同一内容在新宽度下重新折行,是**同一个块的另一个呈现**。
被禁止的是内容变成别的内容、顺序乱掉、已落定的块消失。

**`content_hash` 是这一节的量具。** 它在 settle 之后必须恒定,而行数、折行、
可见性都可以变。这把两条口语化的要求各自变成了一个可断言的数。

### 卸载生产者不追回历史

块属于**流**,不属于面板。面板是块的**生产者**。

卸载一个生产者只是停止产出新块,不能撤走旧块——和卸载插件不会把它发过的事件
收回来是同一条规则。这不是选择,是「不可逆」这条性质推出来的。

### 由此强制分出的两类模块

上一版把这两类混成了一种,这是它最实质的错误:

| | **流生产者** | **视图模块** |
|---|---|---|
| 例子 | transcript、findings 明细 | 状态栏、输入行、token 表、当前 todo |
| 输出 | 追加块,不可逆 | 每帧全量重画 |
| 有历史 | 有 | 无 |
| 判据 | 块序列 + 冻结性 | 状态字面量 → 行 |

混在一起会长出"状态栏也想追加历史"和"transcript 被整体重画"这两类错误。
分开之后两边的判据都变简单。

---

## 二、空间:区域树 × realm 可见性,两条正交轴

「随意组合、变换」要求**布局本身是数据**,而不是四个写死的槽:

```rust
enum Region {
    Leaf(PanelId),
    Split { dir: H | V, ratio: Ratio, a: Box<Region>, b: Box<Region> },
    Stack(Vec<Region>),          // 层叠,overlay 用
}
```

区域树是配置值,所以它能被 `--patch` 换掉、能按变体不同、能在运行时变换。

它和 realm 的关系是**正交**的,这一点是整节的核心:

> **区域树说「在哪」,realm 可见性说「哪些」。**
> `compose(realm, layout) -> Frame` —— 布局给矩形,realm 过滤面板。

`compose` 里那一步过滤,调用的是服务表和事件总线用的**同一个** `visible_from(owner, viewer)`,
不是一套相似的规则。于是单向可见性直接落到屏幕上:

| 面板注册在 | 出现在根帧 | 出现在该 agent 的帧 |
|---|---|---|
| 根 realm | ✓ | ✓（子看得见祖先） |
| agent A 的 realm | ✗ | ✓ |
| A 的子 agent 的 realm | ✗ | ✗（父看不见子） |
| agent B 的 realm | ✗ | ✗（兄弟互不可见） |

**这解决的是一个已经发生过的真 bug 的一般形式。** 做 AgentHandle 桥时发现:
子 agent 的 transcript 会漏进父 agent 的事件流,而且已经在写进父 agent 的持久化文件。
当时用 session id 过滤修的——那是特例。realm 化的合成是通例:**被委派的子 agent
画不到父 agent 的屏幕上,因为它的面板本来就在父的可见性之外。**

顺带白送多 agent UI:两个 agent 并排 = 从两个 realm 各合成一帧,由区域树平铺。
谁也不需要知道对方存在。

---

## 三、缝

| 缝 | 模式 | 说明 |
|---|---|---|
| `surface` | Seam,**单提供方,只在根 realm 绑定** | 终端是物理单例 |
| `tui-stream` | 注册表 | 流生产者:日志事实 → 块 |
| `tui-panel` | 注册表 | 视图模块:状态 → 行 |
| `tui-keymap` | 注册表 | 键 → `Action` |
| `tui-command` | 注册表 | 斜杠命令 → `Action` |
| `tui-layout` | 配置值 | 区域树 |

**宿主行只做四件事**:把已提交事实分发给流生产者;按区域树分配矩形;管焦点;
把 `Frame` 交给 `surface`。它不认识任何一个模块。

---

## 四、义务:论证成立的前提

每条义务后面括号里是执行它的机器。**填不出执行者的条目不写进这份文件。**

1. **模块之间没有共享可变状态。**
   (`tui` crate 内禁 `static .*Mutex|LazyLock` 的 lint)
   反例已经存在:`ui_tui.rs` 的 `static CURRENT_INPUT: LazyLock<CurrentInput>` 是全局输入缓冲,
   一个进程里两个 agent 会串台——**「隔离看起来是真的,其实不是」的 UI 版本。**

2. **`render` 不得有副作用,也不得是 async。**
   (类型:`fn render(&State, &Viewport) -> Vec<Line>`,拿不到 `Context`)
   反例已经存在:`ui_tui.rs` 的 `draw()` 在绘制路径里 `titler.title(&log).await`。
   绘制依赖时序,而时序不在日志里。异步产物走 `SessionProjections` 先算好。

3. **合成实时读模块表,不得启动时快照。**
   (`tests/tui/mid_stream_mount.rs`)
   和 `ToolBox::defs()` 每次请求现读是同一条规则。

4. **顺序按声明的 rank,不按激活顺序。**
   (`tests/tui/rank_is_stable_across_mount_order.rs`)
   和 `PromptRegistry` 的显式 rank 是同一条规则,理由也一样。

5. **屏幕可见即已记录。**
   (`SessionEvent::Notice` + 禁 `eprintln!` 的 lint)
   见 §六 装置 1。这条是时间可组合性的前提,不是美观问题。

---

## 五、判据

### 强度声明

**一个未声明强度的闸门比没有闸门更危险,因为它制造虚假安全感。**

| 判据 | 来源 | 强度 |
|---|---|---|
| 渲染引擎行为不变 | 旧实现 + 它的 32,283 行测试 | **强**(迁移场景的免费 oracle) |
| 字符宽度 / CJK / emoji | `unicode-width` + `vte` 解析回单元格网格 | **强,独立于实现** |
| 块冻结性 | `content_hash` 恒定 | **强** |
| 位置无关性 | proptest | **强** |
| realm 单向可见 | 与 plexus 现有测试同构 | **强** |
| 帧布局 | 黄金帧快照 | **弱——只测「变了」,不测「对不对」。这一层的质量下限是人的 review,不是闸门。** |

### 时间

```
prop  settled_blocks(fold(log)) == settled_blocks(fold(log ++ more))[..n]   冻结性
prop  blocks(log) is_prefix_of blocks(log ++ more)                          只增不减
prop  fold_incremental(events) == fold_batch(events)                        重放安全
test  unmounting_a_producer_leaves_its_settled_blocks_intact
test  collapsing_a_block_changes_line_count_but_not_its_content_hash
test  resize_rewraps_settled_blocks_without_changing_their_content_hash
```

### 空间

```
prop  render(m, rect_at(A)) == render(m, rect_at(B))     when same size    位置无关
test  swapping_two_regions_swaps_their_pixels_byte_for_byte
test  a_panel_in_a_child_realm_never_enters_the_parents_frame
test  sibling_realms_do_not_paint_on_each_other
test  a_root_panel_still_appears_in_every_agents_frame
```

### 阴性对照:每个闸门先证明它会失败

**一个只在合格对象上跑绿的判据没有鉴别力。** 每条闸门配一个已知不合格的对象:

| 闸门 | 阴性对照 | 必须 |
|---|---|---|
| 冻结性 | 一个 settle 后偷改内容的探针块 | 红 |
| 位置无关性 | 一个读全局/读兄弟状态的模块 | 红 |
| 宿主裁剪 | 吐 500 行的 `tui-probe` | 被裁到视口 |
| 键位冲突 | 两行争同一个键 | 挂载期红 |
| 场景 builder | turn/round 编号自相矛盾的场景 | 被拒绝 |
| `settle()` | 一个永不静默的场景 | 超时报错,不许假绿 |

---

## 六、诚实边界:论证在哪里**不**成立

1. **滚出视口的块不可寻址。** 落进终端原生 scrollback 的内容,折叠也做不到——
   终端的物理限制。所以「可折叠」的适用范围是**仍在视口内的终态块**。
   写进缝的定义,否则会做出一个在长会话里悄悄失效的功能。

2. **`surface` 是真单例。** 一块终端,一个提供方,只在根 realm 绑定。
   区域树能切分它,realm 不能复制它。

3. **焦点是仲裁,不是组合。** 区域树能表达层叠,不能表达「谁该拿到这个键」。
   宿主做调度器。键位解析是 `resolve(key, mode, focus_realm)`——焦点选 realm,
   realm 选可见绑定;但「谁获得焦点」本身没有可组合的答案。

4. **视口尺寸是环境,不在日志里。** 所以场景构造里它属于「此刻」,
   和 agent 状态、半截输入并列。**分开写是为了让不可推导的部分无处藏身。**

5. **内容哈希恒定 ≠ 字节恒定。** 这是 §一 那个和解的代价:量具查内容,不查像素。
   像素级稳定由黄金帧兜,而黄金帧上面已声明是弱判据。

---

## 七、装置台账

**每个装置都要能追溯到一次真实发生过的麻烦。** 凭空想象的规则拦不住真实错误,
还会挤占本就有限的注意力。

| # | 装置 | 起因(真实失败) |
|---|---|---|
| 1 | `SessionEvent::Notice` + 禁 `eprintln!` 的 lint | `recovery.rs:112/241/514` 往 alternate screen 打 stderr:画面会花,而且面板画不出这些状态,**卸载再挂载恢复不了——时间可组合性在这里破了** |
| 2 | `render` 类型上不给 async | `ui_tui.rs` 的 `draw()` 在绘制路径里 `await` |
| 3 | 禁 `static .*Mutex` 的 lint | `ui_tui.rs` 的 `CURRENT_INPUT` 全局输入缓冲 |
| 4 | per-round `yield_now()` + `settle()` 超时对照 | 全内存 turn 一次 poll 跑完,按键插不进,交错测试会全部假绿 |
| 5 | 产品行/测试行分开统计 | 用 117k 总行估了两次迁移成本,实际产品代码 25,275 行——差三倍 |
| 6 | 区域树而非固定槽 | 上一版判断「可配置布局是在解决还没人提出的问题」,判断错了 |

---

## 八、被推翻的设计(放弃了什么)

**只写选了什么的记录,拦不住下一个人重走弯路。**

| 上一版 | 现在 | 为什么放弃 |
|---|---|---|
| 屏幕是日志的纯投影,每帧从状态重算 | 流是不可逆块序列,只有 live 块可变 | 重算允许改写终态,违反「终态不能变」 |
| 四个固定槽(Header/Body/Footer/Overlay) | 区域树是数据 | 固定槽表达不了「随意组合、变换」 |
| 面板只有 `(fold, render)` 一种形态 | 分流生产者与视图模块 | 不可逆只适用于前者,混同会长出两类错误 |
| 每帧重走整个日志渲染 | 成本 O(live) | 每敲一个键重走一遍日志 |

新模型顺带更省:渲染 O(live) 而非 O(日志);重挂载只重放到最后一个 settled 块的边界,
不必重放全部历史。

**失效条件**(定期复核,防止仪式活得比它的用途长):

- 若终端普遍支持可寻址 scrollback,§六.1 的边界失效,「可折叠」范围应扩大。
- 若出现第二个 `surface` 提供方的真实需求(如同时驱动终端与 SSH 会话),
  §六.2 的单例假设需重新论证。

---

## 九、迁移的规模(已核实)

`atomcode-tuix` **产品代码 25,275 行,测试 92,295 行(78%)**。
下表按产品行统计:

| 堆 | 产品 | 测试 | 去向 |
|---|---:|---:|---|
| 渲染引擎 `render/ width markdown highlight glyph sanitize` | 7,945 | 32,283 | **整体搬**成叶子 crate `atomcode-tui-render` |
| 模态 `modals/` ×17 | 8,634 | 8,206 | **一个一个搬**成模块行 |
| 事件循环 `event_loop/` | 2,805 | 43,051 | **大部分散回各能力行**,不进 TUI |
| 输入 `input/` | 1,193 | 1,161 | 搬(历史、粘贴、图片) |
| 终端控制 | 911 | 1,127 | 进 `surface-crossterm` |
| 命令表 60 条 | 688 | 756 | **分给各能力行** |
| 状态 `state.rs` 142 字段 | 632 | 3,201 | **不搬** |
| 其余 | 1,883 | 2,358 | 分类处理 |

三个结构判断:

1. **渲染引擎是干净的叶子。** 整个 `render/` + `width` + `markdown` + `highlight`
   对外只依赖 `atomcode_coding::GoalPhase` 一个五变体枚举
   (`render/mod.rs:967`、`render/retained.rs:486`)。换本地枚举即零依赖。
   32,283 行测试跟着搬,**它们就是验收**。

2. **`state.rs` 的 142 字段不搬——它正是被替换的东西。** 搬它等于原样复制耦合。
   替代物是每个模块自己的 fold 状态,按 `(realm, module_id)` 存。

3. **60 条命令的行为不属于 TUI。** `/model` `/provider` 属于 `llm` 行,
   `/compact` `/undo` `/rewind` 属于 session 行,`/memory` 属于 memory 行。
   **TUI 只负责发现(斜杠菜单)、渲染(模块/模态)、绑定(键位)** ——
   这才是三角色约定用在 UI 上的样子。

---

## 十、落地顺序

每步单独绿、单独 commit、单独回滚:

```
0  SessionEvent::Notice + 三处 eprintln 改 commit        ← 时间可组合性的前提
1  atomcode-tui-render:整体搬,32,283 行测试全绿          ← 独立验收
2  atomcode-harness-cli:挪出 349 行启动器,破循环依赖
3  Block/Stream/Frame/Action/Region + surface 缝 + headless
4  scene() 场景构造器 + 它的阴性对照                      ← 场景能构造了
5  宿主行 + tui-probe                                     ← 宿主可测
6  tui-transcript(第一个流生产者)                        ← 冻结性判据落地
7  tui-status / tui-input(视图模块)+ tui-keymap
8  tui-command:60 条命令分配到各能力行
9  17 个模态 → 模块行,一次一个
10 tui-findings + 四变体断言 + 端到端                     ← 闭环合上
```

第 1、2 步是机械迁移,独立验收。第 4 步做完「构造特定场景」即成立——那时一个真模块都还没有。

---

## 十一、结论

> **时间 = 块序列(不可逆)× 呈现(可变)**
> **空间 = 区域树(在哪)× realm(哪些)**

四条轴,两两正交,每条都有可证伪的判据和阴性对照。

论证的每一处可证伪点都对应一条判据;而 §四 里的三条违例
(全局输入缓冲、绘制里 await、三处 eprintln)和 §九 的 142 字段 god-struct
**都不是假想的**——它们就是这个论证要求新架构必须消除的东西。
