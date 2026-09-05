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

### 修正:Live 不止一个

一个更早的版本写成 `Stream { live: Option<Block> }`——只允许一个运行态块。
**并行工具调用同时有 N 个未决**,这个形状表达不了。而且它掩盖了一条更重要的不变量:

> **追加顺序是发出顺序,结算顺序是完成顺序,两者不同。**

harness 的循环里这已是既定事实(工具结果按发出顺序写日志,不按完成顺序,
"so the transcript matches what the model asked for, not what happened to finish first"),
屏幕必须一致。

```rust
pub struct Stream { slots: Vec<Slot> }        // 只追加
enum Slot { Live(Block), Settled(Arc<Block>) }
```

不变量:**下标顺序冻结,状态可以乱序从 Live 变 Settled。** 三号槽比五号槽晚结算是合法的。

### turn 和 step 是坐标,不是块

日志里每条事实**已经**带 `(turn, round)`。所以 turn 和 step 不需要成为块,
也不需要开闭配对——它们是块的**坐标**:

```rust
pub struct Block {
    pub id: BlockId,
    pub at: Coord,               // { turn: u64, step: u32 }
    pub content: Arc<dyn Content>,
    state: BlockState,
}
```

四个好处直接兑现:

- **折叠整个 turn** = `Fold(Target::Turn(3))`,块不需要知道自己属于谁
- 分组是**呈现**不是结构,所以换分组方式不动内容,`content_hash` 不变
- 没有「span 没关上」这种状态,少一整类 bug
- `Target` 顺势统一:

```rust
pub enum Target { Block(BlockId), Step(u64, u32), Turn(u64), Kind(&'static str) }
//                                                          ↑ 「折叠所有工具调用」
```

### 六类内容

内容是**语义值**,不是预渲染的行——所以能重折行、能换主题、能算 hash:

| Content | 生命周期 | 来自哪些事实 | 默认呈现 |
|---|---|---|---|
| `UserSaid` | 一次成型 → Settled | `UserMessage` | 展开 |
| `ModelSaid` | Live 累积 → Settled | `AssistantChunk` + `AssistantMessage` | 展开 |
| `ModelThought` | 同上 | `AssistantChunk{reasoning}` | **折叠** |
| `ToolCall` | Live(未决) → 结果到 → Settled | `AssistantMessage.tool_calls` **+** `ToolResultLogged` | 折叠为一行 |
| `Choice` | Live(待答) → 用户答 → Settled | `user-questions` 缝 | 展开,夺焦 |
| `Notice` | 一次成型 | `Notice` | 单行 |
| `Request` | 一次成型 | `RequestHeader` | **隐藏**(调试开关) |

三处需要说明:

**`ToolCall` 是一个块、两条事实。** 生产者按 `call_id` 关联。这正是一致性套件里
「重放等价」要查的东西:增量折叠与批量折叠必须得到同一个块。turn 被取消时
只有 call 没有 result——块停在 Live,渲染为「已中断」,不许 panic。

**`ModelThought` 默认折叠是呈现默认值,不是内容缺失。** 展开随时可得,
`content_hash` 不因折叠而变。

**`Request` 就是「对模型的输入」的可见形态。** 平时隐藏,打开调试开关即可看到
这一轮发了什么模型、第几轮、续接还是新序列。它不该是另一套 wire-log 机制——
**一件事只有一个家。**

### 用户选择走 `user-questions` 缝,不新建机制

`Choice` 是唯一**消费输入**的块。若让流生产者处理按键,流就不纯了(义务 2 破)。

接法是:TUI 的 `UserQuestions` 提供方**就是**这个块的生产者。

```
approval / 任何提问 ──> ctx.service::<UserQuestionsSvc>()
                            ↓
              TUI 提供方: open(Choice) → 声明夺焦 → await → amend → settle
```

三个好处:输入处理只有一处;`Choice` 的所有权明确;换一个 `user-questions`
提供方(比如 JSON-RPC 客户端),流里就**不再出现** `Choice` 块,而空间与时间
两条性质都不用改。

### Todo:同一批事实,两个模块

这是两类模块分水岭的示例:

| | 流里的 todo | 面板里的 todo |
|---|---|---|
| 形态 | `Notice{"新增 3 项"}` 一行,不可逆 | 当前清单,每帧重画 |
| 模块类型 | `StreamProducer` | `ViewModule` |
| 有历史 | 有 | 无 |
| 事实来源 | **同一批** `ToolResultLogged{name:"todo"}` | **同一批** |

**同一批事实,两个模块,一个入流一个不入流。** 这就是必须分两类的理由:
todo 清单塞进流,每勾掉一项就追加一份新清单,流被刷屏;反过来,变更记录只存在
面板里,滚回去就看不见「什么时候加的」。

---

## 二、空间:区域树 × realm 可见性,两条正交轴

### 前提:全屏。整条流必须可寻址

这不是从现状推的(现状是流式),是从需求推的:

| 需求 | 推出什么 |
|---|---|
| **终态可以折叠、隐藏**(§零) | 折叠一个 turn、展开一条旧 reasoning——要求**整条流可寻址** |
| **模块占据窗口一部分,可随意组合、变换** | 「随意」排除流式:流区必然满宽置顶(scrollback 天生满宽),findings 放不到 transcript 右边 |
| **自然语言调布局** | 一半请求得到「这个模式下做不到」,这个入口的价值就塌了 |
| **`ToolCall` 折叠为一行、`ModelThought` 默认折叠** | 折叠是默认呈现,用户随时要展开旧的;流式下滚过去就锁死 |

四条指向同一件事:**应用必须自己持有整条流的渲染缓冲**。终端原生 scrollback
不可寻址,所以流式满足不了。

**流式形态不是被砍掉,它是另一行。** 日志形态的体验由 `ui-repl` 承担——它已经
存在、已经在跑。TUI 这一行不兼职做第二种模式:两种视图是**两行填同一个 `ui` 槽**,
不是一个模式开关。这正是这套架构自己的答案。

结论让设计变简单,这通常是推导对了的信号:`Slot` 只有 `Live | Settled`
(不需要第三个 `Flushed` 态)、布局无形状限制、headless 只记一样(帧)、
折叠覆盖**全部**而非一个滑动窗口。

代价是四条义务,见 §四.7–10。

### 布局本身是数据

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

### 布局是运行时状态,不是挂载期配置

区域树由配置**初始化**,之后是**状态**:可以在运行时调整,而调整不得打断任何模块。

变更是 `LayoutOp`,不是整棵树替换——因为 op 可以被校验、被撤销、被记录、被描述:

```rust
pub enum LayoutOp {
    Split  { at: Target, dir: Dir, size: Constraint, with: ModuleId },
    Close  { at: Target },
    Swap   { a: Target, b: Target },
    Resize { at: Target, size: Constraint },
    Show   { module: ModuleId },      // 放到它声明的默认位置
    Hide   { module: ModuleId },
    Focus  { at: Target },
    Preset { name: String },
    Undo,
}
```

### 三条入口,一个词汇表

调整布局有三种方式,它们**必须产出同一个值**,否则就是三份布局编辑实现:

```
快捷键     Keymap binding             ──┐
菜单命令   /layout split right findings ─┼──> LayoutOp ──> apply()
自然语言   模型调 adjust_layout 工具    ──┘
```

于是需要重测的只有 `apply` 的性质;三条入口各自只需一个薄翻译测试,
外加一条**防分叉闸门**:同一个意图分别走三条入口,产出的 op 必须相等。

### 模型那条:感知用现成机制

**感知 = 一个 prompt 片段。** 布局行往 `PromptRegistry` 贡献;而 `PromptRegistry`
每次请求现读,所以模型看到的永远是当前布局,不可能漂移:

```
## 屏幕布局
当前:  transcript(流区) │ findings(视图 30%)
       status(1 行) / input(3 行)
可用但未显示: usage、todo、diff
用 adjust_layout 调整。「把 findings 收起来」= Hide{module:"findings"}。
```

**动作 = 一个工具。** 布局行往 `ToolsSvc` 注册 `adjust_layout`,风险 `Safe`
(不改任何持久物且可 Undo),不走审批。

两件事由**同一行**提供,且都从**同一份布局状态**导出——描述与渲染不可能说两样。

### 布局变更必须入日志

被「屏幕可见即已记录」逼出来的:布局决定屏幕上有什么,不记录则恢复会话看到的
屏幕与之前不同——**时间可组合性破了**。

```rust
SessionEvent::LayoutChanged { op: LayoutOp }
// is_model_visible() == false —— 与 Notice 同类:给屏幕看的事实,不给模型看
```

四个后果都是白送的:恢复会话自动还原布局;**Undo = 重放到第 N 条**,不需要单独的
撤销栈;场景构造器能构造任意布局状态;连续 `Resize` 同一目标由 fold 合并,
不会撑爆日志。

变体的初始布局仍是配置值(`code-review` 带 findings、`longcode` 不带),
运行时 op 叠在其上。

### 布局引擎必过的四条

| # | 性质 | 为什么 |
|---|---|---|
| 1 | **全函数** 任意 op × 任意布局 → 合法布局或**带原因的拒绝**,永不 panic、永不产出非法树 | 模型会发垃圾 |
| 2 | **可逆** `apply(op); undo()` 回到逐字节相同的帧 | Undo 是入口之一 |
| 3 | **不触碰模块状态** `state_after == state_before`,`stream_after == stream_before` | **空间变换与时间流正交**——把 findings 从左挪到右,不许清空它 |
| 4 | **模块缺席可降级** 布局引用了未挂载的模块 → 该区收缩,不 panic | `code-review` 的布局在 `longcode` 下也要能用 |

第 3 条是两条约束交汇处的关键:它把「空间随意变换」与「时间不可逆」的正交性
变成一条可断言的等式。

失败必须带**模型能用的**原因:

```rust
pub enum LayoutError {
    NoSuchModule(String),              // 「没有叫 foo 的模块,可用的有:…」
    TooSmall { need: u16, have: u16 },
    WouldOrphanFocus,
}
```

---

## 三、缝

| 缝 | 模式 | 说明 |
|---|---|---|
| `surface` | Seam,**单提供方,只在根 realm 绑定** | 终端是物理单例 |
| `tui-stream` | 注册表 | 流生产者:日志事实 → 块 |
| `tui-panel` | 注册表 | 视图模块:状态 → 行 |
| `tui-keymap` | 注册表 | 键 → `Action` |
| `tui-command` | 注册表 | 斜杠命令 → `Action` |
| `tui-layout` | Core 服务 | 区域树 + `apply(LayoutOp)`;初值来自配置,变更入日志 |

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
   见 §十 装置 1。这条是时间可组合性的前提,不是美观问题。

6. **每个模块必须过一致性套件。**
   (`tui_conformance!` 宏;未注册的模块行在 `tests/tui/every_module_is_covered.rs` 里判红)
   模块不写自己的性质测试——套件是长出来的,不是写出来的。见 §六。

全屏(§二)不是白拿的,它换来四条义务:

7. **退出时把整条流回吐到正常缓冲区。**
   (`tests/tui/exit_dumps_transcript.rs`)
   最容易漏掉的一条,而它决定了「退出后还能不能看见刚才发生了什么」——
   对开发者工具是硬需求。

8. **自己实现滚动。**
   (一致性套件:滚动位置在 `Moment` 里,且不影响任何块的 `content_hash`)

9. **自己实现选择与复制。**
   (`/copy` `/save` 的命令行为测试;逻辑从 tuix 搬)

10. **`Drop` 必须恢复终端,panic 路径也要。**
    (现有 `Screen` 的 `Drop` 模式 + 一条 panic 注入测试)
    留一个 raw mode 的 shell 给用户,比没有 UI 更糟。

11. **`render` 永不读系统时钟。**
    (`tui` crate 内禁 `Instant::now` / `SystemTime::now` 的 lint;
    时间从 `Moment.tick` / `Moment.now` 注入)
    一次墙钟读取就**静默**毁掉整个闭环——测试还绿,只是同一份输入不再给
    同一份输出。见 [ADR 0008](./adr/0008-animation-time-is-injected-not-read.md)。

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

### 流的下标与内容

```
prop  并行 N 个工具、任意结算顺序 → 块的下标顺序恒等于发出顺序
prop  ∀ target. fold(target) 改变行数，不改变任何块的 content_hash
prop  fold_incremental([call, result]) == fold_batch([call, result])
test  只有 call 没有 result（turn 被取消）→ 块停在 Live，渲染「已中断」，不 panic
test  折叠整个 turn 后展开 → 逐字节回到原帧
test  换掉 user-questions 提供方 → 流里不再出现 Choice 块，其余帧逐字节相同
test  同一批 todo 事实 → 流得到 1 个 Notice 块，面板得到 3 项清单
```

### 布局

```
prop  apply 全函数：任意 op 序列，永不 panic，树永远合法
prop  可逆：∀ op. undo(apply(op)) == 原帧（逐字节）
prop  正交：∀ op. 模块的 State 与 Stream 逐字段不变
test  同一意图分别走 键位 / 命令 / 工具 → 三个 op 相等   ← 防三份实现分叉
test  布局引用未挂载的模块 → 该区收缩，其余逐字节不变
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

## 六、模块级一致性套件:测试不是写出来的,是长出来的

「每个模块都能单独测试」不能靠自觉,否则第 14 个模块的测试一定比第 1 个差。
所以模块**不写自己的性质测试**,而是注册时自动获得一整套:

```rust
tui_conformance!(StatusBar,  View);      // 展开成空间模块那 6 条
tui_conformance!(Transcript, Stream);    // 展开成流模块那 6 条
```

新增一个模块 = 一行宏 = 完整性质套件。这同时满足「方便扩展」和「每个模块单独可测」——
**它们是同一件事的两面。**

### 空间模块必过的 6 条

| # | 性质 | 阴性对照 |
|---|---|---|
| 1 | **位置无关** `render(m, A) == render(m, B)`（同尺寸） | 一个读全局/读兄弟状态的模块 → 红 |
| 2 | **不越界** 合成后每个属于 m 的单元格都在 m 的矩形内,宽度 1..=200 | 吐 500 行 / 超宽行的探针 → 红 |
| 3 | **全函数** 任意 (宽,高,状态) 不 panic | 宽度 0、状态 `Default` 的边界 |
| 4 | **确定性** 连画两次逐字节相同 | 掺时间戳的模块 → 红 |
| 5 | **无副作用** 类型上拿不到 `Context`,不是 async | 编译期 |
| 6 | **空状态可画** `Default::default()` 不 panic | |

### 流模块必过的 6 条

| # | 性质 | 阴性对照 |
|---|---|---|
| 1 | **冻结性** 任意后缀事件下 `content_hash` 恒定 | settle 后偷改内容的探针块 → 红 |
| 2 | **前缀性** `blocks(log)` 是 `blocks(log ++ more)` 的前缀 | 会重排块的模块 → 红 |
| 3 | **重放等价** 增量折叠 == 批量折叠 | 依赖挂载时刻的模块 → 红 |
| 4 | **卸载不追回** 卸载生产者,已落定的块原样留下 | |
| 5 | **呈现可变内容不变** 折叠/换宽度改行数不改 hash | |
| 6 | **全函数** 任意事件序列不 panic | proptest 生成器 |

**一个只在合格对象上跑绿的判据没有鉴别力。** 所以每条都配了已知不合格的探针,
探针必须判红——探针本身是 crate 里的行,不是测试里的临时结构。

---

## 七、UI 侧的自动化:外部权威是终端仿真器

功能侧靠上面的性质就够了。**UI 侧不能用我自己的渲染器判自己的渲染器**——
同一个人按同一份理解写出的测试和代码,一致是必然的,而这个必然没有信息量。

所以 UI 断言走一条独立回路:

```
Frame(我的模型)  ──surface──> ANSI 字节 ──vte 解析──> 单元格网格(真终端会显示的)
                                                          │
                            断言在这里,不在 Frame 上 ◄────┘
```

`vte` 是外部权威。它一致就是我的渲染器错了。由此可以机器判定一批**本来只能靠眼睛**的事:

| 断言 | 挡住的错误 |
|---|---|
| 每行占用单元格 ≤ 宽度 | 折行错乱、把终端撑破 |
| CJK / emoji 占格数 == `unicode-width` | 中文半个字、emoji 压字 |
| 属性(色/粗/反显)落在正确单元格 | 转义序列泄漏、颜色串行 |
| 光标终点位置 | 输入行错位 |
| 每个模块的单元格都在自己矩形内 | **模块画出自己的框** |

最后一条是空间可组合性在像素层面的验收,而且它**天然是每模块独立的**。

---

## 八、闭环:一条命令,一个退出码

```
./gates/tui.sh                      # 唯一入口,退出码即结论
  ├ cargo test -p atomcode-tui-render         差分 oracle（32,283 行旧测试）
  ├ cargo test -p atomcode-tui                一致性套件 + 模块单测
  ├ cargo test -p atomcode-tui --test e2e     端到端
  ├ ./gates/tui-negative.sh                   阴性对照:每条都必须红
  └ ./gates/tui.sh --repeat 20                抗抖:20 次结果必须一致
```

### 四个确定性来源(缺一条就会 flaky)

**人有先验知识能过滤噪音,Agent 没有**——它会把抽风当真信号去修不存在的 bug,
或者直接删掉那个测试。所以确定性是构造出来的,不是祈祷来的:

| 不确定源 | 消除方式 |
|---|---|
| 模型 | `llm-replay` 脚本化 provider |
| 终端与键盘 | `surface-headless` + 脚本按键 |
| 时间 | `#[tokio::test(start_paused)]`;限流等待用 `advance()` 瞬间跳过 |
| 调度 | `settle()` 等静默判据 + **硬超时判红**(不许超时算过) |

还有一条前提是实测出来的:全内存的 turn 一次 poll 就跑完、永不让出执行权,
按键根本插不进去,交错测试会**全部假绿**。`agent_loop` 里那个 per-round
`yield_now()` 是这条闭环成立的物理前提(见装置台账 4)。

### 黄金帧的再生也必须是命令

```
./gates/tui.sh --bless        # 重新生成黄金帧,不需要活人看一眼
```

**接手点里的每件事,Agent 必须能独立完成。** 一个需要人肉截图确认的黄金帧,
会让严格遵守流程的 Agent 正确地卡住。

---

## 九、诚实边界:论证与闭环各自在哪里**不**成立

### 论证不成立的地方

1. **可折叠覆盖整条流,代价是应用要留住整条流。** 全屏(§二)让所有终态块都可寻址,
   所以「可折叠」没有滑动窗口的限制——但内存里要留住全部块。长会话的
   内存占用与 O(块数) 成正比。缓解手段(块的惰性内容、超长块降级为摘要)是优化,
   带正确性义务(降级后 `content_hash` 必须不变),现在不做,记在账上。

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

### 闭环闭不掉的地方

| 判不了 | 现状 | 补偿 |
|---|---|---|
| **好不好看** 留白、对齐美感、颜色搭配 | 黄金帧只测「变了」 | 人的 review;这是本层真正的质量下限 |
| **跨终端差异** iTerm / Windows Terminal / tmux 的怪癖 | `vte` 只是一种仿真规则 | 宽度权威覆盖了最大的一类(CJK/emoji);其余人工抽查 |
| **交互手感** 延迟、闪烁、滚动跟手 | 无 | 人工 |

前两类各自有明确边界,第三类完全在闭环外。**声明它们,是为了让「跑绿了」这三个字
不被读成「做对了」。**

---

**声明它们,是为了让「跑绿了」这三个字不被读成「做对了」。**

## 十、装置台账

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
| 7 | `tui_conformance!` 宏而非手写性质测试 | 逐模块手写必然衰减:先写的有好测试,后写的凑合 |
| 8 | vte 断言层 | 用自己的渲染器判自己的渲染器,一致是必然的,没有信息量 |
| 9 | `./gates/tui.sh --bless` | 需要人肉看一眼的黄金帧,会让严格遵守流程的 Agent 正确地卡住 |
| 10 | `Vec<Slot>` 而非单个 `live` | 并行工具调用同时有 N 个未决,单 live 表达不了 |
| 11 | 三条入口的 op 相等断言 | 键位/命令/自然语言三条路,天然会长成三份布局编辑实现 |
| 12 | 同一 kind 连续块数上界断言 | 把 todo 清单整个塞进流会刷屏,而「只增不减」仍然绿 |
| 13 | 退出回吐测试 | 全屏会吞掉退出后的可见输出,而这是开发者工具的硬需求 |
| 14 | panic 注入 + `Drop` 恢复终端 | 留一个 raw mode 的 shell 给用户,比没有 UI 更糟 |
| 15 | 禁 `Instant::now` 的 lint | 一次墙钟读取静默毁掉闭环:测试还绿,但同一输入不再给同一输出 |
| 16 | `frame(Idle, 0) == frame(Idle, 5)` 断言 | 闲着也重画的动画会白烧 SSH 带宽,而「真的在动」那条测试仍然绿 |

---

## 十一、被推翻的设计(放弃了什么)

**只写选了什么的记录,拦不住下一个人重走弯路。**

四个非平凡决策已经立了记录,**内容在那边,这里只指路**——同一件事不放两个家:

| 决策 | 记录 |
|---|---|
| 流是不可逆块序列(含两类模块、`Choice` 走 `user-questions`) | [ADR 0004](./adr/0004-tui-stream-is-an-irreversible-block-sequence.md) |
| turn / step 是坐标,不是块 | [ADR 0005](./adr/0005-turn-and-step-are-coordinates.md) |
| 全屏,流式由 `ui-repl` 那一行承担 | [ADR 0006](./adr/0006-tui-is-full-screen-not-inline.md) |
| 布局是入日志的状态,三条入口一个 op 词汇表 | [ADR 0007](./adr/0007-tui-layout-is-logged-state-with-one-op-vocabulary.md) |
| 时间由宿主注入,`render` 永不读系统时钟 | [ADR 0008](./adr/0008-animation-time-is-injected-not-read.md) |

下表是本文档自身的演化,粒度比 ADR 细:

| 上一版 | 现在 | 为什么放弃 |
|---|---|---|
| 屏幕是日志的纯投影,每帧从状态重算 | 流是不可逆块序列,只有 live 块可变 | 重算允许改写终态,违反「终态不能变」 |
| 四个固定槽(Header/Body/Footer/Overlay) | 区域树是数据 | 固定槽表达不了「随意组合、变换」 |
| 面板只有 `(fold, render)` 一种形态 | 分流生产者与视图模块 | 不可逆只适用于前者,混同会长出两类错误 |
| 每帧重走整个日志渲染 | 成本 O(live) | 每敲一个键重走一遍日志 |
| 流式(inline),或流式/全屏双模式 | **全屏,单一模式** | 「终态可折叠」和「随意组合」都要求整条流可寻址,而原生 scrollback 不可寻址。双模式方案需要第三个 `Flushed` 态、模式相关的布局合法性、两套 headless 记录——是过度设计;流式形态由 `ui-repl` 那一行承担 |

新模型顺带更省:渲染 O(live) 而非 O(日志);重挂载只重放到最后一个 settled 块的边界,
不必重放全部历史。

**失效条件**(定期复核,防止仪式活得比它的用途长):

- 若长会话的内存占用成为实际问题(§九.1),块的惰性化需要重新评估,
  且必须先给出「降级不改 `content_hash`」的判据。
- 若 `ui-repl` 被证明满足不了日志形态的需求(例如需要在流式下也有面板),
  §二 的「两行而非两模式」结论需要重新论证。
- 若出现第二个 `surface` 提供方的真实需求(如同时驱动终端与 SSH 会话),
  §九 的 surface 单例假设需重新论证。

---

## 十二、迁移的规模(已核实)

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

## 十三、落地顺序

每步单独绿、单独 commit、单独回滚:

```
0  SessionEvent::Notice + 三处 eprintln 改 commit        ← 时间可组合性的前提
1  atomcode-tui-render:整体搬,32,283 行测试全绿          ← 独立验收
2  atomcode-harness-cli:挪出 349 行启动器,破循环依赖
3  Block/Stream/Frame/Action/Region + surface 缝 + headless
4  vte 断言层 + ./gates/tui.sh 骨架                      ← UI 侧回路先通
5  scene() 场景构造器 + 它的阴性对照                      ← 场景能构造了
6  tui_conformance! 宏 + 两套探针(合格/不合格)           ← 一致性套件先于第一个模块
7  宿主行 + tui-probe                                     ← 宿主可测
8  tui-transcript(第一个流生产者)                        ← 用第 6 步的套件验收,不手写性质
9  tui-status / tui-input(视图模块)+ tui-keymap
10 tui-command:60 条命令分配到各能力行
11 17 个模态 → 模块行,一次一个(每个只需一行宏)
12 tui-findings + 四变体断言 + --repeat 抗抖               ← 闭环合上
```

顺序上有两处是刻意的:

- **第 4 步(vte)先于任何模块。** UI 侧的外部权威必须在有东西可画之前就位,
  否则第一个模块只能拿我自己的渲染器判自己。
- **第 6 步(一致性套件)先于第一个真模块。** 套件在模块之前,模块 #1 和模块 #17
  拿到的验收标准才一样;反过来做,先写的模块会有更好的测试,后写的会凑合。

第 1、2 步是机械迁移,独立验收、可单独回滚。第 5 步做完「构造特定场景」即成立——
那时一个真模块都还没有。

---

## 十四、走一遍:加一个会动的吉祥物

拿一个具体模块把前面每条规则都碰一遍。这个例子挑得刻意——它同时压到
两类模块的分水岭、`Moment` 作为不可推导输入、以及动画如何保持可穷举测试。

### 位置由规则决定,不由喜好决定

- 流块 settle 之后**内容冻结** → 流里的吉祥物永远动不了
- 视图模块**每帧重画** → 它能动

所以会动的吉祥物**必须**是视图模块,占区域树的一块 rect。顺带回答一个隐含问题:
现在 tuix 的吉祥物在欢迎横幅里;横幅若做成流块(挂在对话顶端),它就动不了。
这个限制是规则推出来的,不是谁拍的。

### 状态分两半

**情绪来自事实,相位来自 tick。**

```rust
impl ViewModule for Mascot {
    type State = Mood;                       // Idle / Thinking / Working / Happy / Sad

    fn absorb(mood: &mut Mood, fact: &SessionEvent) {
        *mood = match fact {
            SessionEvent::TurnStart { .. }                        => Mood::Thinking,
            SessionEvent::ToolResultLogged { is_error: true, .. }  => Mood::Sad,
            SessionEvent::TurnEnd { stop: StopReason::Stopped, .. } => Mood::Happy,
            _ => *mood,
        };
    }

    fn render(mood: &Mood, vp: &Viewport) -> Vec<Line> {
        let f = frames(*mood);
        f[vp.moment.tick as usize % f.len()].draw(vp.rect)
    }

    /// 宿主需要一个「没有事实到达也要重画」的理由。
    /// Idle 返回 None —— 静止时不请求节拍，不白烧带宽。
    fn tick(&self) -> Option<Duration> { Some(Duration::from_millis(120)) }
}
```

`render` **仍然是纯函数**:`(mood, tick, rect) → 行`。§六 的一致性套件一条都不用改,
因为 tick 是**输入**,不是隐藏状态。时间必须注入而不能就地读,见
[ADR 0008](./adr/0008-animation-time-is-injected-not-read.md)。

### 动画反而能穷举

这是设计的意外红利。5 种情绪 × 12 相位 = **60 帧,全部可断言**:

```rust
test  每一帧：高度 == 4、不越界、宽度 ≤ 9         // 60 个组合全跑
test  frame(Thinking, 0) != frame(Thinking, 6)    // 真的在动
test  frame(Idle, 0) == frame(Idle, 5)            // 静止时不重画  ← 阴性对照
e2e   start_paused + advance(600ms) → tick 精确 +5，断言第 5 帧
```

第三条是阴性对照:一个「闲着也每 120ms 重画」的吉祥物会白烧带宽,这条把它判红。
比大多数 UI 能做到的都强——因为帧是值,而时间是注入的。

### 加它要动几处

| 动作 | 量 |
|---|---|
| 写模块(`absorb` + `render` + 帧数据) | 一个文件 |
| 注册为行 | 一个 `Plugin` impl |
| 拿到全套性质测试 | `tui_conformance!(Mascot, View)` **一行** |
| 出现在屏幕上 | 变体的初始布局加一个 `Leaf`,或运行时 `Show{module:"mascot"}` |
| 让模型能调它 | **零**——`adjust_layout` 自动认识它,prompt 片段自动列出它 |

最后一行是这套设计真正的回报:**新模块不需要为「能被自然语言调整」做任何事。**

---

## 十五、结论

> **时间 = 块序列(不可逆)× 呈现(可变)**
> **空间 = 区域树(在哪)× realm(哪些)**

四条轴,两两正交,每条都有可证伪的判据和阴性对照。

论证的每一处可证伪点都对应一条判据;而 §四 里的违例
(全局输入缓冲、绘制里 await、三处 eprintln)和 §十二 的 142 字段 god-struct
**都不是假想的**——它们就是这个论证要求新架构必须消除的东西。
