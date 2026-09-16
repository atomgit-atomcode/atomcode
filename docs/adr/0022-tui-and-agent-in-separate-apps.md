# tui 与 agent 分两个 App,重建 App 对前端只以会话身份体现

状态: 已决定(2026-09-17)。补上 [`0021`](./0021-front-end-contracts-split-by-layer.md)
未决的「渲染面」;重议 `architecture-target.md` §6 的「不翻协议客户端」。服务于
[`0012`](./0012-atomcode-tui-replaces-tuix.md) 的替换路线。第 2 节表的第二行已由
[`0023`](./0023-a-session-is-a-realm.md) 修订。

## 背景

**术语。** 「树」专指 agent 的 realm(0023)。本文的「重建 App」指 runtime 挂一个新的
plexus App、替掉它持有的旧 App(`build_agent`,`coding/runtime.rs:7703-7709`),新 App 里
是一个新的 agent 句柄。它**不是** runtime 的 `generation` 加一,两者相交但互不包含:
`generation` 有 11 处加一,其中只有 4 处重建 App(`:4625`、`:5339`、`:5499`、`:5687`);
其余 7 处不重建(`/model` 原地 patch、登出、暂停压缩、Stop / Replace / Shutdown)。反过来,
撤销和恢复失败后的回滚(`:5554`、`:5755`)重建 App 但不加一。

tui 要接到产品装配(`runtime::mount`)上。过渡期里 runtime 仍会重建 App:

| 类 | 操作 | 位置 |
|---|---|---|
| 换会话 | 新会话、resume、切目录 | `coding/runtime.rs:5102` 的 `changes_session` |
| 同会话 | 撤销、恢复快照、重载配置/skills、登出后恢复 | `:5383`、`:5599`、`:7176`/`:7195`、`:4614` |

重建 App 之后,屏幕「接着画什么」按今天的代码是错的,两种做法(UI 单独一个 App、UI 行
每次重建 App 时重挂)都一样:

1. **新 App 里的日志从原生快照重建。** `session-native` 用 `seed_from_snapshot(snapshot, 1)`
   (`coding/host_rows.rs:210`),只产 6 种事件:`TurnStart`、`UserMessage`、
   `AssistantMessage`、`ToolResultLogged`、`Injected`、`TurnEnd`。提问与回答、Notice、
   Usage、标题、限速暂停、中断、策略干预、工具开始都没了,压缩变成一条 `Injected`。
2. **序号重起。** 从 1 起;跟随日志里已有该会话时接着文件最大序号编
   (`harness/agent.rs:744-750`)。
3. **屏幕按序号去重。** `Facts` 只折 `seq > high` 的事实(`tui/plugin.rs:188-203`)。

合起来:序号接着文件编 → 整段历史**重复画一遍**;序号从 1 起 → 历史被跳过、之后的新事实
在超过旧 `high` 之前被丢掉,**静默冻结**。事实里也没有任何一条说「撤销了」。
另外 `plugin.rs:331` 的注释「resume 与实时画面一样」在产品装配上不成立:原生重建有损。

同时,tui 今天直接读 agent 侧服务,正是 0018 §3(c) 说的「UI 知道 Agent 细节」:

| 服务 | 用途 | 位置 |
|---|---|---|
| `AgentsSvc` | agent 与成员状态 | `tui/plugin.rs:708` |
| `LlmSvc` | 模型名、能否看图(贴图前判断) | `plugin.rs:1714` |
| `CompactionSvc` | 有没有压缩策略(`/compact`) | `commands.rs:141` |
| `ToolsSvc` | `/tools-list`;注册 `adjust_layout` | `commands.rs:216`、`plugin.rs:1875` |
| `ControlSvc` | `/rows`、`/patch`、`/audit` | `commands.rs:236` |
| `SystemPromptSvc` | 贡献 `tui-layout` 提示词 | `plugin.rs:1867` |

## 决策

### 1. 渲染契约是会话事实流,属于句柄协议

句柄协议对每个 agent 的形状:**命令进、状态事件出、会话事实出**。事实带序号,可从某个
序号开始补。内容只走事实流,状态只走事件——沿用 tui 今天的分法(`plugin.rs:318`:
「内容从不走句柄,屏幕是日志的折叠」),也符合架构原则 4「UI 画的内容从日志推导」。

事实流与命令都按 session id 寻址,团队成员(`<lead>/<name>`)同样适用(0023)。

事实词汇就是今天 harness 的 `SessionEvent`,要挪到中立位置。放哪与 0021 未决的「宿主
控制契约放哪」是同一个问题,一起答。

ACP `session/load` 的历史重放是事实流的投影;cli 的 ACP 实现今天已经在重放
(`cli/src/acp/sessions.rs:100`)。

### 2. 重建 App 对前端只以会话身份体现,契约里没有 generation

| 情况 | 操作 | 处理 |
|---|---|---|
| 换了会话 | 新会话、resume、切目录 | 新 session id 就是新事实流(0014:session id 是身份),前端换流、从头补。目标形态是同一个 App 里另建 realm、不重建 App(0023);过渡期仍重建 App,前端看不出区别 |
| 同会话,内容不变 | 重载配置/skills、登出后恢复 | **不重建 App**:重载走 control patch(`/model` 已是先例),登出/登录只换模型行、不拆 agent。*(0023 修订;本行原为「宿主拿旧 App 的日志原样当新 App 的种子」)* |
| 同会话,内容变了 | 撤销、恢复快照 | 做成日志事件,改投影不改历史,不重建 App |

**不变式**:同一 session id 的事实流,序号单调、只追加。任何改动让同一 session id 下出现
序号回退、或历史被整段替换而没有一条事实说明,即违反契约。

### 3. tui 与 agent 分两个 App

- **UI App** 持有 surface、面板、命令、布局,重建 agent 的 App 时不受影响。
- **agent 的 App** 归宿主(过渡期是 runtime)。每重建一次,宿主重新接线:
  - 把句柄(命令、状态事件、事实流)交给 UI 的 `tui-agent-client`;
  - (UI 对 agent 的贡献——`adjust_layout` 工具与 `tui-layout` 提示词——随第 8 节去掉,宿主不再需要装。)
  - 把 App 里的事实转发出来:带着实例、每个新 App 都挂一份的行,先例是
    `native-compaction-checkpoint`(`host_rows.rs:1608`)。
- **tui 直读的 agent 侧服务改走契约**:
  - `AgentsSvc` → 句柄协议里的 agent 与成员状态事件(带父子关系);
  - `LlmSvc` / `CompactionSvc` / `ToolsSvc` 的只读用途 → agent 自述(模型名、能否看图、
    有无压缩、工具列表),形状未决;
  - `ControlSvc`:见第 7 节,读写配置树的命令只留 `/effort`,改走宿主控制契约。
- `ui-tui2` 不再自己起泵(`plugin.rs:1851`),不再 `inject` `agents` / `agent-loop`。

`architecture-target.md` §6 当初列过翻成协议客户端的三笔代价,这里逐笔认:日志到协议的
投影(第 1 节,SDK 反正要做);`adjust_layout` 这类 UI 工具的通道(第 8 节把它去掉了,这笔
不用付);realm 隔离降为 session id 约定——tui 今天实际上已经按 session id 过滤
(`plugin.rs:343-349`),这笔已经付过。

目标形态下 runtime 不再重建 App(0023),「活过重建」这个理由会消失;分两个 App 仍由
0018 的分层成立。

### 4. tui 功能分期

第 2 节表里第二、三行落地之前,tui 只开放:发消息、回答提问、取消、压缩、新会话、resume。
**不开放**:撤销、rewind、恢复快照、重载配置/skills、登出/登录。

### 6. 每个会话一条流,切换是换显示哪一条

ADR 0004 定了屏幕是不可逆块序列、块属于流;tui 的 Host 今天只有一条流
(`tui/host.rs:748`);tui 的注释写着「一块屏幕一段对话」,把两段混进一条流就是两个 agent 抢着说话
(`tui/plugin.rs:333-340`)。定为:

- **每个会话一条流**,各自内部仍不可逆(0004 不受影响);切换不改任何已定下的块。
- **新会话 / resume**:新建一条流;旧会话的 realm 已移除,它的流随之丢弃。
- **团队成员**:第一次切过去时从事实流序号 0 补历史建流,之后留着,切回不重建。
- **折叠状态与滚动位置按流分开**:0004 的 `Presentation` 按 `BlockId` 索引,改为按 `(会话, BlockId)`。
- **切换时 tip 行提示一句**(复用现有的三秒 tip)。
- 不在同一条流里画分隔后接着画:来回切几次,同一条流里就会反复出现不同的对话。

### 7. 读写配置树的开发命令:只留 `/effort`

`tui-commands-tree` 这一行(`tui/commands.rs:183-198`)有 `/rows`、`/rows-list`、`/tools-list`、
`/audit`、`/effort`、`/patch`,全经 `ControlSvc` 直接改运行中的配置树;`/patch`、`/rows` 能在运行时
关掉审批、敏感路径这类安全行,绕过 runtime 直接改还会让 runtime 的记账与树不一致。定为:

- **只留 `/effort`**,改走宿主控制契约(「切推理强度」,0021 第 2 条)。
- **`/rows`、`/rows-list`、`/tools-list`、`/audit`、`/patch` 删掉**,UI 不再需要配置树操作接口。
- 因此 `Described` 里的工具名列表(第 5 节)没有消费方了,先不做。

### 8. 可调布局整体去掉,等想清楚再说

屏幕布局分三层:① 给模型的——系统提示里的布局描述(`tui/plugin.rs:1866-1873`,挂载时算一次,
注释说「每次请求重新读取」而代码不是)与 `adjust_layout` 工具(`tui/layout_tool.rs`);② 给人的——
`tui-commands-layout` 行的 `/layout`、`/show`、`/hide`、`/undo-layout`、`/layout-set`
(`tui/commands.rs:340-356`),键 `ctrl-f`(专注预设)、`ctrl-z`(撤销布局)(`tui/keymap.rs:221-233`),
`ctrl-n` 与 `/mascot`(手动显示 / 隐藏吉祥物,走同一套布局操作,`tui/plugin.rs:1594-1614`),
以及为撤销和模型而记的布局操作日志(ADR 0007);③ 渲染本身——区域树决定面板画在哪。

- **去掉 ① 与 ②**(含 `ctrl-n` 与 `/mascot`)。ADR 0007 作废。吉祥物显不显示只由 `--mascot` 决定——
  它打开的是 `tui-panel-mascot` 这一行,不经布局操作。
- **③ 保留。** 面板行挂载时自己用 `LayoutOp::Show` / `Hide` 把自己放上 / 撤下屏幕(如
  `tui/rows.rs:240-290` 的吉祥物)——这是渲染装配,不是人或模型调布局。
- `/effort` 之外的读写配置树命令同时去掉(第 7 节)。
- 删测试会让 `gates/tui-test-count.baseline`(「判据只能增不能减」)变红;降基线在 commit 里写明是
  随功能删除。

### 5. agent 自述与状态:用事件推,不做查询

状态栏的模型名已经从日志事实 `RequestHeader { model }` 读(`tui/modules/status.rs:41`)。要靠自述
提供的是:能否看图(`tui/plugin.rs:1714`)、有无压缩(`commands.rs:141`)、工具列表
(`commands.rs:216`)、agent 状态(`plugin.rs:708`)。`/model` 会在会话中途换模型,自述会变。

- `AgentAdded { description }` / `AgentRemoved { session }`:团队成员列表。
- `Described { description }`:订阅时发一次,变了再发。内容:session id、父会话 id、成员名与
  角色;模型 id、能否看图、推理档;有无压缩;工具名列表(第 7 节删了 `/tools-list`,这一项先不做);**命令目录**(0021 第 10 条)。
- `StatusChanged { session, status }`:空闲 / 工作中 / 停止中,变得频繁,单独拆出。

位置在 `kernel::agent`(0021 第 6 条)。

## 权衡过、没做的

- **UI 行每次重建 App 时重挂,屏幕状态放 App 外。** 接线少;但把上表六处直读固化下来,
  而且序号与有损重建的问题一样在。保留为退路,见失效条件。
- **契约带 generation。** 能表达任何重建;但把重建实现写进契约,与 0021「不暴露会话
  模型」冲突。
- **等所有 App 重建都消掉再接 tui。** 最干净,但替换要排在最大那块欠账(每会话状态下沉到
  每个 realm)之后。换会话类在过渡期可以继续重建 App,前端只看到 session id 变。

## 未决

- ~~事实词汇放哪~~:已由 [`0024`](./0024-the-session-log-is-the-authority.md) 定,挪到 kernel。
- ~~换会话在屏幕上怎么表现~~:定为第 6 节,每个会话一条流。
- ~~resume 画面有损~~:已由 [`0024`](./0024-the-session-log-is-the-authority.md) 取消——会话权威改为 harness 日志,resume 即重放,不再从快照重建。
- ~~agent 自述在契约里的形状~~:定为第 5 节。
- ~~`tui-layout` 提示词片段的宿主入口~~:取消,第 8 节把这个功能去掉了。

## 闸门

- **同会话操作不重建 App**(runtime 侧,不依赖 tui):重载配置、登出再登录前后,当前 agent
  是同一个(同一 realm、同一日志对象),日志序号连续。今天是红的(这几项都走
  `build_agent`)。观察点待定:`CodingRuntime` 公开面今天看不到 App 内的日志——要么经
  第 1 节的事实流,要么经 JSONL 跟随日志文件。
- **屏幕跨会话切换不重不冻**(tui 接上 runtime 之后):headless 端到端,发一条消息 →
  resume 另一个会话 → 屏幕换到新会话、不混入旧会话的块,再发一条消息画得出来。
- **UI 不直读 agent 侧服务**:读源码守卫,`crates/atomcode-tui/src` 不出现对
  `AgentsSvc` / `LlmSvc` / `CompactionSvc` / `ControlSvc` / `ToolsSvc` / `SystemPromptSvc`
  的 `service::<` / `require::<`。今天是红的(背景表里六处)。
- 每条落地时摘掉被测代码证伪一次。

## 失效条件

- 同会话操作做不到不重建 App(例如某项重载必须整个 App 重挂):回到「契约带 generation」
  重议。
- 第 3 节要补的契约项(成员状态、agent 自述、配置树操作)任何一项必须把 agent 内部类型
  暴露给 UI:退到「UI 行每次重建 App 时重挂」。
