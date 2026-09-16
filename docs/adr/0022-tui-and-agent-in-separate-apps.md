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
  - 装 UI 对 agent 的贡献:`adjust_layout` 走 `HostState.tools`(`coding/on_harness.rs:1278`,
    已有);`tui-layout` 提示词片段要新开入口;
  - 把 App 里的事实转发出来:带着实例、每个新 App 都挂一份的行,先例是
    `native-compaction-checkpoint`(`host_rows.rs:1608`)。
- **tui 直读的 agent 侧服务改走契约**:
  - `AgentsSvc` → 句柄协议里的 agent 与成员状态事件(带父子关系);
  - `LlmSvc` / `CompactionSvc` / `ToolsSvc` 的只读用途 → agent 自述(模型名、能否看图、
    有无压缩、工具列表),形状未决;
  - `ControlSvc` → 配置树操作契约。进程外是否开放仍按 0013 未决;tui 在进程内。
- `ui-tui2` 不再自己起泵(`plugin.rs:1851`),不再 `inject` `agents` / `agent-loop`。

`architecture-target.md` §6 当初列过翻成协议客户端的三笔代价,这里逐笔认:日志到协议的
投影(第 1 节,SDK 反正要做);`adjust_layout` 这类 UI 工具的通道(第 3 节由宿主每个 App
装一次);realm 隔离降为 session id 约定——tui 今天实际上已经按 session id 过滤
(`plugin.rs:343-349`),这笔已经付过。

目标形态下 runtime 不再重建 App(0023),「活过重建」这个理由会消失;分两个 App 仍由
0018 的分层成立。

### 4. tui 功能分期

第 2 节表里第二、三行落地之前,tui 只开放:发消息、回答提问、取消、压缩、新会话、resume。
**不开放**:撤销、rewind、恢复快照、重载配置/skills、登出/登录。

## 权衡过、没做的

- **UI 行每次重建 App 时重挂,屏幕状态放 App 外。** 接线少;但把上表六处直读固化下来,
  而且序号与有损重建的问题一样在。保留为退路,见失效条件。
- **契约带 generation。** 能表达任何重建;但把重建实现写进契约,与 0021「不暴露会话
  模型」冲突。
- **等所有 App 重建都消掉再接 tui。** 最干净,但替换要排在最大那块欠账(每会话状态下沉到
  每个 realm)之后。换会话类在过渡期可以继续重建 App,前端只看到 session id 变。

## 未决

- ~~事实词汇放哪~~:已由 [`0024`](./0024-the-session-log-is-the-authority.md) 定,挪到 kernel。
- 换会话在屏幕上怎么表现:清屏重画,还是在不可逆流里加一条分隔再画新会话(ADR 0004)。
- ~~resume 画面有损~~:已由 [`0024`](./0024-the-session-log-is-the-authority.md) 取消——会话权威改为 harness 日志,resume 即重放,不再从快照重建。
- agent 自述在契约里的形状(位置已定:`kernel::agent`,0021 第 6 条)。
- `tui-layout` 提示词片段的宿主入口。

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
