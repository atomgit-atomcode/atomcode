# 前端契约按层拆:会话内走句柄协议,宿主控制单独一份

状态: 已决定(2026-09-17)。修订 [`0013`](./0013-agent-product-host-ui.md)「SDK 与协议」
一节:undo / rewind / 恢复快照**进** SDK 面。细化 [`0018`](./0018-host-contract.md)
原则 7(「UI 通过界面调用 Agent 的命令」)。

## 背景

tui 替换 tuix(0012),第一个要回答的问题是 **tui 与 agent 之间认哪份契约**。
今天仓库里有两份:

| | 句柄协议 | runtime 驱动协议 |
|---|---|---|
| 位置 | `atomcode-kernel` | `atomcode-coding` |
| 形状 | `AgentCommand` 8 种(`kernel/event.rs:126`)、`AgentEvent` 26 种(`:213`) | `DriverCommand` 21 种(`coding/runtime.rs:529`)、`CodingRuntimeEvent` 34 种(`:51`)、`CodingRuntimeHandle` 约 40 个方法、`RuntimeError` 15 种 |
| 过线 | `Serialize` + `non_exhaustive` | 只有 `Clone` / `Debug` |
| 回执 | 命令没有回执;`TurnStarted` / `TurnComplete` 不带 turn id(`:215`、`:304`) | `SubmitReceipt { generation, turn_id }`、类型化错误、phase |
| 消费者 | tui、harness 自带前端 | tuix(231 处)、daemon(197)、cli/acp(44)、clix(18) |

(`AgentEvent` 此前各文档写作 25 个,2026-09-17 实数为 26。)

**两份不在同一层,所以不是二选一。**

- 句柄协议是「agent ↔ 驱动它的一方」,而且给宿主留了叠加的位置:
  `SendSyntheticMessage` 的文档写明用于「goal 模式的自动续跑」(`event.rs:145`),
  `SendMessageWithContext` 是「宿主拥有的合成上下文」(`:134`)。
- 驱动协议是「产品宿主 ↔ 前端」,里面装着三层东西:转发给句柄协议的会话内操作
  (`Submit` 最后发出的是 `AgentCommand::SendMessage`,`runtime.rs:516`)、宿主操作
  (会话切换、撤销、模型、MCP、重载)、能力行控制器(goal / loop / 策略干预)。
  0013 的缺口清单已经按 Host 与能力行分过这 28 个方法。

驱动协议不适合当契约,四条都可以在代码里指出来:

1. **位置**:在产品 crate 里。前端依赖它就是依赖产品。
2. **载荷把宿主与产品的决定推给前端**:`ReloadProvider(CodingAgentConfig)`、
   `ReprepareConfig(CodingAgentConfig)` 要前端拼一份完整配置;`RestoreSnapshot`
   要前端递整份对话。这是 0018 列的越界症状「UI 里有产品判断」。
3. **不可序列化**:daemon 在它前面自己又翻译了一层(`daemon/src/live_api.rs`)。
4. **重建 App 的模型写进了契约**:`generation`、`Reconfiguring` / `Reconfigured` 是
   「原地换会话、整棵重建」这个实现的投影;事件里还夹着产品专有的
   `VisionPreprocess*`、`Team`(直接用 capabilities 的类型)。

句柄协议单独也不够当契约:它只有会话内操作;而 0013 定的「undo / rewind / snapshot
恢复不补,任何 SDK 都没有」,会让 ACP 移植时丢掉今天已有的 `/undo`
(`cli/src/acp/commands.rs:235`),也会让 tui 替换 tuix 时丢掉撤销。

## 决策

### 1. 会话内操作:句柄协议

发消息、回答提问、取消、压缩、快照、关闭,走 `AgentCommand` / `AgentEvent`。

补三条语义。它们是驱动协议被四个客户端磨出来的正确性保证,不是冗余:

- **命令回执**:一条消息是开了新回合,还是被 steer 进了哪个回合。
- **回合事件带 turn id**。
- **每个被接受的回合恰好一个终结事件**。

只加字段与变体(`#[serde(default)]`),旧 wire 照常反序列化。

### 2. 宿主控制:单独一份中立契约

覆盖:新会话、resume、切工作目录、撤销、rewind(含可 rewind 的点)、恢复快照、
切模型与推理强度、provider 失效与恢复、MCP 状态与撤回、重载能力。

由宿主填缝,UI / ACP / 进程内 SDK 经缝调用,**不认识宿主**(0013 的依赖方向)。

三条形状约束:

- **载荷是意图,不是实现。** 「换到模型 X」,不是整份 `CodingAgentConfig`;
  「撤销到第 N 个 prompt」,不是递一份对话。
- **不暴露会话模型。** 契约里没有 generation、没有「重建 App」;撤销在实现上是日志事件还是
  sidecar(`architecture-target.md` §11 未决 1)不进契约。
- **有回执与类型化错误。** 以 `RuntimeError` 的 15 种为起点,逐条判断是契约语义
  (忙、会话被占用、越界)还是重建 App 这个实现的投影。

### 3. goal / loop / 策略干预 / 本地上下文排队:归能力行

按 0013 缺口清单的归层。它们不进宿主契约,也不塞进句柄协议的核心。

### 4. 修订 0013:SDK 面 = 句柄协议 + 宿主控制契约

0013 原文「28 个无对应的句柄方法直接是 SDK 的能力上限:undo / rewind / snapshot
恢复不补」**不再成立**。改为:两份契约各只定义一次,ACP 行、进程内 facade、
客户端库都是它们的投影,不各自长方法。undo / rewind / 恢复快照属于宿主控制契约,
因此进 SDK 面。

「SDK 面只定义一次、各前端是投影」这条原则不变。

### 5. runtime 驱动协议:过渡期的实现,不是契约

- 新前端(tui)不直接依赖 `CodingRuntimeHandle` / `DriverCommand` /
  `CodingRuntimeEvent`。
- 过渡期由宿主侧 adapter 把两份契约翻译到 `CodingRuntimeHandle`;runtime 拆成行之后
  只换 adapter。
- 新能力**契约先行**(`architecture-target.md` §10):先在契约里加类型,再由 adapter
  实现,不先长在驱动协议上。
- tuix 删除后,daemon / ACP / clix 逐个迁到两份契约上。

### 6. 契约类型放哪

契约是三种东西,位置分开:

| | 例子 | 放哪 |
|---|---|---|
| 数据类型 | 命令、事件、会话事实、错误 | kernel |
| 接口 | 宿主控制的 trait、agent 自述的查询 | kernel(已依赖 `async-trait`) |
| 服务键 | `plexus_service!` 声明的名字 | **消费方声明**,宿主填实现 |

- **kernel 按契约分模块:**
  - `kernel::event`:句柄协议(已有),加第 1 条的三条语义补强;
  - `kernel::session`:会话事实词汇与投影,从 harness 挪来(0024 第 6 条);
  - `kernel::host`(新):宿主控制的命令、事件、错误与 trait;
  - `kernel::agent`:agent 的对外契约——`AgentHandle`(今天已在这里,`kernel/agent.rs:847`)、
    agent 自述、agent 与成员的状态事件(0022)。
- **`kernel/agent.rs` 改成目录模块。** 它今天 6,176 行,主要是回合引擎(`Agent`、`AgentBuilder`、
  `Outcome`、`AutoRespond`、`ToolLoopPolicy`),全仓约 102 处引用。现有内容挪到
  `agent/engine.rs` 并在 `agent/mod.rs` 里 `pub use`,调用方一处不改;契约类型放单独文件,与
  引擎按文件分开。回合引擎改名(解决与 harness `Agent` 撞名)留到它在外部的用户变少之后——
  team / task 换成 realm 版本(0023)会去掉一批。
- **服务键由消费方声明。** kernel 与 plexus 互不依赖(两者 `Cargo.toml` 的依赖都只有 tokio、
  serde 一类外部库),kernel 不为此去依赖 plexus。先例是 tui 在自己 crate 里声明的 5 个服务键
  (如 `AgentClientSvc`)。几个消费方要在同一棵树里共用一个键时,再挪到它们共同依赖的位置。
- **两个 `StopReason` 合成一个,放 kernel。** harness 的(`harness/seams.rs:829`,10 种)是回合结束
  事实用的,kernel 的(`kernel/event.rs:88`,11 种)是句柄协议 `TurnComplete` 用的,只有 6 种
  重合,前端两份都会看到。按含义合并(`InputRejected` / `PromptRejected`、`StoppedByPolicy` /
  `PolicyDenied` 可能是同一件事,`RunawayFuse` 与 `MaxRounds` 要核实)。合并前先查泵
  (`harness/plugins/handle.rs`)今天怎么把前者映射成后者。
- **挪 `SessionEvent` 连带挪的纯数据类型**:`Question`、`Answer`、`AboutCall`
  (`harness/seams.rs:545-625`)、`RateLimitPause`(`harness/events.rs:56`)。
- **不建新 crate。** `AGENTS.md:54`:只有出现稳定跨进程 schema、非 Rust codegen 或独立版本
  契约的真实需求时才拆纯协议叶子。今天三条都不满足(ACP 在 Rust 里投影,没有 codegen,SDK
  facade 未做)。任一出现时再拆。
- 宿主控制要用到的类型今天都在 coding(`ProviderUnavailableReason` `runtime.rs:648`、
  `RewindCatalog` `:236`、`McpStatusSnapshot` `:178`、`RuntimeMode` `:321`、`ReconfigureKind`
  `:164`);契约里写中立版本,不原样搬(载荷是意图,第 2 条)。

## 对 tui 的直接后果

- `tui-agent-client` 继续认句柄协议;另认宿主控制缝。
- tui 要用的撤销、resume、切模型等,等宿主控制契约里对应项落地(类型 + adapter)再做。
- 过渡期宿主仍会为 resume / 新会话 / 切目录 / 撤销 / 恢复 / 登出后恢复重建 App
  (`runtime.rs:5057`、`:5383`、`:5599`、`:4614`),屏幕要能活过重建——已由
  [`0022`](./0022-tui-and-agent-in-separate-apps.md) 定:tui 与 agent 分两个 App,重建 App 对前端只以
  会话身份体现。

## 权衡过、没做的

- **驱动协议当契约。** 覆盖全、零迁移;不取的理由是背景里的四条。
- **宿主操作塞进句柄协议,只留一份。** 协议少一份,但 Agent 层要认识会话存储、
  provider 生命周期这些宿主概念,0013 的依赖方向反过来。
- **维持 0013 原判(undo 等 SDK 不补)。** ACP 移植丢 `/undo`,tui 替换 tuix 丢撤销。

## 未决

- ~~宿主控制契约放哪~~:定为第 6 条,kernel 分模块、服务键由消费方声明、不建新 crate。
- 句柄协议三条语义补强的具体字段。
- 能力行的命令(goal / loop / 策略干预)怎么被前端发现与调用。
- `RuntimeError` 里哪些进契约。
- **`generation` 今天承担的「拦过期请求」由什么接替。** 契约里没有 generation,但它在
  runtime 里有实际用途:每个控制消息带上调用方看到的 generation,对不上就回 `Busy`
  (`runtime.rs` 里 26 处 `request_generation != generation`)。契约要用别的东西表达同一
  保证,候选是会话 id 加 turn id 或对话版本号(撤销今天已经另带 `expected_revision`)。
- ~~**渲染面不在这两份里。**~~ 已由 [`0022`](./0022-tui-and-agent-in-separate-apps.md) 定:
  会话事实流属于句柄协议(命令进、状态事件出、会话事实出,可从某序号补)。

## 闸门

- **tui 不依赖驱动协议**:读源码守卫,`crates/atomcode-tui/src` 除注释外不出现
  `CodingRuntimeHandle` / `DriverCommand` / `CodingRuntimeEvent`。落地时加一处真实
  引用证伪一次。
- **宿主控制契约可过线**:每个命令与事件变体 serde 往返一致。
- **kernel 不依赖 plexus**:读 `crates/atomcode-kernel/Cargo.toml` 的守卫,依赖里没有
  `atomcode-plexus`(也没有任何 `atomcode-*`)。
- **只有一个 `StopReason`**:全仓 `pub enum StopReason` 只有 kernel 那一个。今天是红的(两个)。
- **回合归属可判定**:一条被 steer 进当前回合的消息,只看事件流就能知道它由哪个
  turn id 的终结事件收尾。今天判不了(`TurnComplete` 不带 turn id)。

## 失效条件

- 宿主控制契约长成 `DriverCommand` 的改名版——载荷里出现 `CodingAgentConfig`、
  整份 `SessionSnapshot` 或 generation——重议。
- 某个前端必须绕过两份契约直接调 runtime 才能做成一个功能:那是契约缺口,补契约,
  不开后门。
