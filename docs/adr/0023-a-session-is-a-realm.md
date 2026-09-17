# 一个会话一个 realm:产品委派改用 realm 版本,成员可切换、可直接对话、日志保留

状态: 已决定(2026-09-17)。承接 [`0014`](./0014-an-agent-owns-its-session-and-world.md)
(agent 拥有会话与世界);修订 [`0016`](./0016-agent-team-inbox-wake-and-peer-messages.md)
(成员只认 lead)与 [`0022`](./0022-tui-and-agent-in-separate-apps.md) 第 2 节(同会话
不重建 App)。

## 背景

用户的模型(原话):「同一个会话,就是一棵树。换会话就换树。team Agent 里的 subAgent,
也拥有不同的树,可以为 subAgent 渲染界面,在 tui 里切换。」并定:**「树」是 realm**,
不是 plexus App。

**harness 已经是这个模型。** `Agents::create` 给每个 agent 一个 realm,里面是它自己的
会话日志、工具、提示词、模型与 cwd(0014)。team 成员按 `<lead>/<name>` 建、
`persist(false)`、`keep_driven` 驱动(`harness/plugins/team.rs:651-654`),`stop` 时
移除(`:882`);`task` 子 agent 跑完即移除(`harness/plugins/subagent.rs:258`、`:307`)。
tui 的监听器其实已经收到了成员的事实,只是按 session id 过滤掉了
(`tui/plugin.rs:333-348`);0016 的「未做」里写着「tui 的成员面板行未做」。

**产品装配不是。** `runtime.rs:7768-7769` 禁掉 `subagent-in-process` 与
`team-in-process`,挂 coding 自己的 `task` / `team`;成员是 kernel 的
`Agent::builder()`(`coding/team/runner.rs:110`、`capabilities/tools/task.rs:1522`),
不在 plexus 树里,对外只有 `TeamEvent` 的进度——开始、排队、一行 activity、结束摘要
(`capabilities/team.rs:335-373`)。没有成员的对话可以渲染。两套同名不同契约
(0016 补节):coding 的 `team` 是 `run_id` + delegate / wait / result,`task` 带
`subagent_type`;harness 的 `team` 是 delegate / tell / status / stop,`task` 是
`task` / `instructions` / `model` / `effort`。

**消息来源已有区分。** `MessageOrigin` 有 `User` / `Harness` / `Internal` / `Peer`
(`harness/agent.rs:344-358`)。0016 定「成员只认 lead」:成员的 `tell_parent` 只捏着
lead 的 id,成员到成员靠构造不可能;同伴消息标成非用户来源,不当授权。

## 决策

### 1. 一个会话 = 一个 agent realm

- **换会话 = 在同一个 App 里另建顶层 agent 的 realm**,再移除旧的。目标形态下 runtime 不再为
  换会话重建 App。过渡期(每会话状态还没下沉到 realm:provider 的 session 绑定、hooks、
  租约、快照写入器、按目录解析的 MCP 与 skills)仍重建 App,前端只看到 session id 变(0022)。
- **同一会话的操作不换 realm、不重建 App**:撤销、恢复快照做成日志事件;重载配置/skills
  走 control patch;登出/登录只换模型行,不拆 agent。这一条取代 0022 第 2 节表第二行原先的
  「用旧日志做种子重建」。
- **用词**:文档里「树」专指 agent 的 realm;plexus 的整个替换写作「重建 App」。

### 2. 产品的 team / task 改用 harness 的 realm 版本

- 产品树挂 `team-in-process`、`subagent-in-process`;coding 的 `task` / `team` 不再作为
  host tool 挂载(`CodingParts::host_only_tools`)。coding `team/`(2,369 行)与
  `capabilities/tools/task.rs` 的委派部分切换后不再被产品挂载,删不删在删 tuix 那一步一起
  清点。
- **切换前先对齐差异**:把 coding team / task 的现有测试原样挂到 realm 版本上跑,红的逐条
  判断是补 harness 还是有意不要——收双引擎时「旧测试挂新装配最能抓 bug」的做法。已知差异
  至少有:动作集(wait / result 对 tell / status / stop)、`subagent_type`、角色与
  difficulty 映射、成员继承的中间件(`runner.rs` 的 `inherited_worker_middlewares`、
  `credential_shell_policy`)、scope 不重叠校验对 worktree 隔离、`CodingRuntimeEvent::Team`
  进度事件的现有消费方(tuix、daemon)。
- **安全边界要在产品树上重新立判据。** harness 已证「根上的闸门看得到子 agent 的调用、
  委派逃不掉根上的拒绝」(`harness/tests/subagent.rs:203-222`、
  `a_root_denial_cannot_be_escaped_by_delegating`),但那是 harness 的树;产品树的审批、
  敏感路径、凭据 shell、工作区围栏对成员同样生效,要在产品树上各立一条。

### 3. tui 可以切到任一成员的视图

- 事实流与命令按 session id 寻址(0022 第 1 节),成员 `<lead>/<name>` 同样适用;agent
  注册事件带父子关系与状态,tui 据此列出可切换的对象。
- 切换 = 换所看的 session id 并补历史;lead 与成员用同一套面板。
- **成员面板是切换入口**(`tui/modules/team.rs`,`tui-panel-team`):竖排列表,第一行是「主」(lead),
  其后是成员;可键盘导航、可鼠标点击。选中一行即切换,**整个输入框与流都换成那个 agent 的**——
  输入发给它(第 4 节),流、状态栏、任务清单、steering、提问面板都显示它的。选「主」切回 lead。
  面板今天的文档写着「刻意不显示成员自己的流」,随本决定改写。
- **键盘与鼠标**:`Tab` 把焦点移到成员面板(`Tab` / `Shift-Tab` 终端能收到,`tui/surface.rs:1497-1498`,
  键位表里还没绑);焦点在面板时 `↑` / `↓` 移动、`Enter` 切换、`Esc` 或再按 `Tab` 回输入框不切换。
  鼠标悬停高亮、点击直接切换。选中行只有一个状态,键盘与指针共用(同提问面板)。面板里标出
  当前看的是谁,状态栏也写上当前 agent 名。
- 屏幕上每个会话一条流,切换是换显示哪一条(0022 第 6 节)。

### 4. 人可以直接给成员发消息(修订 0016)

- 0016「成员只认 lead」收窄为 **agent 之间**的规则:成员的 agent 通道仍只指向 lead,成员到
  成员仍不可能。**人是另一类发送方**,可以对团队里任一 agent 说话。
- 进成员日志的是 `MessageOrigin::User` 的消息——人说的话,带人的授权分量;不是 `Peer`。
- 人发的消息经成员的句柄泵(第 6 节)进 inbox,唤醒成员起回合。

### 5. 成员结束后日志保留

`task` 子 agent 跑完、team 成员被 `stop` 之后,仍能切过去看它的完整对话。移除 realm
不等于丢日志。成员会话落盘(0024)。

### 6. 所有 Agent 用同一种泵:句柄泵

今天三种驱动各不相同:

| | 接命令(发消息、取消、回答提问) | inbox 唤醒 |
|---|---|---|
| lead:句柄泵(`harness/plugins/handle.rs:840-935`) | 有;取消时拒掉挂着的提问(`refuse_all`) | 有(`:840`) |
| 成员:`keep_driven`(`harness/plugins/agent_loop.rs:954`) | 无 | 有 |
| `task` 子 agent:task 工具直接 `driver.drive`(`subagent.rs:302`) | 无 | 无 |

人要能对成员发消息、取消它的回合、回答它的提问与审批,只有句柄泵有这些。定为:

- 句柄泵能接管一个已存在的 agent(今天的 `spawn` 只收 `CreateAgent`,自己建 agent)。
- `keep_driven` 删掉,成员由句柄泵驱动。
- task 工具改成:建子 agent → 发任务 → 等它这一回合的终结事件。
- 取消某个 agent 只拒掉**它自己**挂着的提问。提问已经带着是谁问的(`Question.asker`,
  `harness/seams.rs:609-611`);缺的是拒绝按 agent 分:`refuse_all` 今天把所有挂着的提问一起拒掉
  (`handle.rs:531-541`)。

### 7. 人对成员说话:lead 知情,但不被叫醒

用途是**纠偏或补充信息,lead 仍统筹**。

今天成员每结束一个回合、且这一回合没主动对 lead 说过话,team 行替它给 lead 发一条汇报
「[X finished turn N: …] + 最后一句」(`harness/plugins/team.rs:1160-1195`)。它用的是
`send_from`,是**消息**,会叫醒 lead 开一个回合(`harness/agent.rs:409-416`)。人直接对成员
说话时照这个走,lead 会在不知道人说过什么的情况下被叫醒,读到一句没头没尾的回复。定为:

- **人的原话进 lead 日志,作注入。** 人对成员发消息时,在 lead 日志提交一条注入,新来源
  「人对成员说」(今天的注入来源是 `Peer`、`Memory`、`Reminder`、`Continuation`、
  `InternalNudge`、`CompactionSummary`),写明对哪个成员、说了什么。注入不起回合(0016)。
  渲染给 lead 的模型时写明:这是人对该成员的纠偏或补充,成员正照此执行,不是对 lead 的
  指令,lead 统筹时不要推翻它。
- **人发起的成员回合,结束时的汇报改成注入。** 内容是成员这一回合的最后一句,不叫醒 lead,
  lead 下一回合读到。
- **带了 lead 消息的回合照旧叫醒。** 一个回合里只要有 lead 发来的消息(成员日志里来源为
  lead 的 `Peer` 注入,`agent_loop.rs:488-491`),结束时照旧以消息汇报——那是 lead 委派的
  工作,lead 在等结果。
- **成员主动 `tell_parent` 照旧叫醒 lead。** 那是成员判断 lead 现在就该知道。

### 8. 成员视图里能做的事

前提是所有 Agent 都由句柄泵驱动(第 6 节),每个成员接同一套命令。

| 操作 | 定为 | 要不要告诉 lead |
|---|---|---|
| 发消息 | 可以(第 4 节) | 按第 7 节 |
| 回答成员的提问与审批 | 可以。**lead 视图里也显示成员的提问**,标明是谁问的(`Question.asker`),也能直接回答——免得成员卡在审批上没人看见 | 不用 |
| 取消成员当前回合 | 可以,只停它、只拒它自己的提问 | 按第 7 节:回合里有 lead 的消息就照旧以消息汇报(带 `Cancelled`)并叫醒 lead,否则作注入 |
| 停掉成员 | 可以。今天只有 lead 能经 `team` 工具调 `stop`(0016 判据);agent 之间这条不变,人经前端命令也能停 | 成员手上有 lead 委派、还没汇报的活 → 叫醒 lead;否则只注入。lead 在等它的结果,不叫醒会一直干等 |
| 压缩成员上下文 | 可以,同一个命令 | 不用 |

人停成员经命令目录发出:team 行把 `stop` 登记进 `commands` 注册表,前端经 `Invoke` 调用
(0021 第 10 条)。

### 9. 取消的级联

今天工具执行拿到的是 lead 的取消令牌(`harness/exec.rs:53-73`),但 task 工具不看它
(`harness/plugins/subagent.rs` 里没有 cancel),所以 lead 被取消后要等子 agent 自己跑完,
取消才生效。0016 定了 lead 被取消不停成员。定为:

| | 级联 | 理由 |
|---|---|---|
| `task` 子 agent | **级联**:lead 被取消时给子 agent 发取消,task 工具返回已取消 | 它是 lead 这一回合里一次同步的工具调用,本就是这一回合的一部分 |
| team 成员 | **不级联** | 成员异步、跨回合长驻;人取消 lead 常常只是嫌 lead 啰嗦 |
| 「全部停下」 | 另给一个前端命令:给 lead 与每个成员各发取消,不 `stop` 成员 | 句柄协议已有取消,不需要新契约命令 |
| lead 的回合被撤回时(`keep_interrupted_context = false`) | 这一回合里新 delegate 的成员一并 `stop` | lead 的上下文里已经没有派它们去的记录 |

## 落地时的补充:切换前对齐的差异(2026-09-17,M4.2)

逐条对照了 coding `team/`(32 条测试)、capabilities `tools/task.rs`(40 条)与 harness 两行。结论按第 2 节
「红的逐条判断」定下来;安全相关的在切换前补齐,不以「以后再补」换切换。

**委派出去的 agent 的边界(team 成员与 `task` 子 agent 一样),补进 harness,切换前必须有:**

- **敏感路径对委派 agent 是硬拒**,不是询问。产品今天就是硬拒(子 agent 结束回合);harness 根上的
  `sensitive-paths` 只问,自动 / 绕过模式下等于放行,lead 的「总是允许」还会盖到成员。
- **写 `.git/` 内部一律拒**(git hook 会跑 shell,等于绕开「成员没有 shell」)。
- **工作区外的写一律拒**,解析 `..`、绝对路径与符号链接;委派 agent 不问人。
- **成员与子 agent 永远没有 `bash`、`team`、`task`。** 网络读工具照给:名单按父 agent 的工具集解析,
  父 agent 自己就能不经询问调用,给成员不是放宽(`harness/plugins/team.rs` `EXPLORE_TOOLS` 的既有决定)。
  角色文件的 `tools:` 只能在允许集里挑;仓库内角色文件(`<project>/.atomcode/agents`)的 `model:` 按模型自选
  (`Chose::Model`)的规矩解析,只有用户目录下的才算人选。
- **写入范围(scope)**:写角色的成员没有独立 worktree 时必须声明 scope,写只落在 scope 内,两个
  在跑的写成员 scope 不得重叠(沿用 capabilities `team.rs` 的判定,宁可误拒)。有独立 worktree 时
  scope 可省。
- ~~被策略拦下的委派结果扣住~~:落地时发现在 harness 上走不到——树上唯一提交策略干预的是凭据 shell
  闸门,而委派 agent 的 `bash` 在它之前就被 `delegation-bounds` 拒掉。产品需要它,是因为产品的 worker
  子 agent 有 shell;这里没有,不做。将来给委派 agent 任何会提交策略干预的能力时,先补这一条。
- **本回合的执行限制只由 lead 的请求更新**,成员照 lead 的限制执行:成员的请求不得改写它。

**产品体验与配置,补:**

- 风险按参数算:`task`(子 agent 只读)与 `team` 的 `status` / `tell` / `stop` 是 Safe,派出写角色的
  `delegate` 才是 Risky。否则每次委派都要问、计划模式全挡。
- 补齐产品的 14 个内置角色(缺 planner、architect、rust、tui_ux、debugger、security、performance、
  release_manager、migration_compat);写角色的工具集含 `search_replace`。
- `[subagent].max_rounds` / `ATOMCODE_SUBAGENT_MAX_ROUNDS` 接到两行,0 表示不限;`[subagent].max_concurrent`
  接到 team 同时存在的成员数上限(`max_members`;成员常驻、回合按消息起,没有另设回合排队);`SubagentPolicy::Disabled` / `ATOMCODE_SUBAGENT=0` 时两行不挂。
- **子 agent 的花费照记**:委派 agent 日志里的 `Usage` 事实按模型记进会话的 `detached_model_usage`。
- **前端过渡**:runtime 从成员的 agent 事件与事实合成 `CodingRuntimeEvent::Team`(tuix 团队面板读的那些
  字段),并给 `task` 调用发进度行(daemon / 网页只读这个)。M6 删 tuix 时一并删。
- 登出与重建 App 时成员随 App 结束(过渡期仍重建 App),立判据。

**有意不要:**

- `team` 的 `wait` / `result` / `run_id` 与 JSON 快照:改为成员主动汇报(第 2 节已定的推模型)。
- `task` 的批量、`subagent_type`、`difficulty`、`role`、worker 子 agent(带 `bash`):改动走 `team` 的写角色;
  同一步里多个 `task` 调用本就并行。
- 困难子 agent 瞬时失败回退主模型重试一次:困难档就是主模型,没有可退。
- 工具参数修复(控制字符、未转义引号):不是委派特有,留给全树。
- 成员读也限在 scope 内:不要,读不改东西。
- 结果格式:保留 harness 的「最后一句 + 统计」。

## 落地时的补充:人与成员(2026-09-17,M4.5–4.7)

- **前端怎么对成员说话、取消它、压缩它**:经 lead 那条连接,用 0021 补充里的寻址信封 `To`。
  压缩成员在它空闲时做,忙时回 `Busy`。
- **给 lead 的两种注入**:「人对成员说」(第 7 节)之外再加「关于成员的通知」——人发起的成员回合结束时的
  汇报、人停掉一个没欠 lead 汇报的成员,都用它。两种都**不是 lead 某一回合的工作**:lead 的回合被撤回时
  它们留在投影里(同记忆、压缩摘要),被 `Rewound` 撤掉的那段里的照撤。格式版本随之升到 8。
- **「lead 日志里有两条注入」怎么做到**:lead 空闲时当场提交进它的日志(与回合开始互斥,落在下一个
  `TurnStart` 之前),lead 视图立刻看得到;lead 正在跑时排进它的收件箱,由下一次领取带进日志——
  不在回合中途插进日志,否则会夹在工具调用与结果之间。
- **一回合里有没有 lead 的消息**:看成员这一回合的日志里有没有来源为 lead 的 `Peer` 注入,不另记状态。
- **「成员手上有 lead 委派、还没汇报的活」**(第 8 节,人停成员):成员正在跑的回合里有 lead 的消息,
  或它的收件箱里排着 lead 的消息。有 → 以消息告诉 lead 并叫醒它;没有 → 「关于成员的通知」。
- **「全部停下」**:tui 命令 `/cancel-all`,给屏上会话与它的每个成员各发一次取消。
- **撤回时停掉新成员**:成员记下是 lead 的哪一回合 delegate 的;lead 日志提交「中断且撤回」时,停掉那一回合
  delegate 的成员。resume 带回的成员不记,不受影响。
- **成员泵的事件没人读**:成员与 `task` 子 agent 的泵只接命令,它产出的事件没有前端在读;`drive` 关掉这条
  事件通道,不让它在成员活着期间一直积压。

## 权衡过、没做的

- **每个会话 / 每个成员一个 App。** 0014 已否:代码索引、MCP 连接、模型客户端每个 App
  各一份,成员共享父工具做不了。
- **保留 coding 的 team / task,让 runner 把成员对话发成事实。** 有日志可渲染,但成员仍在
  树外,与「subAgent 拥有自己的树」不符,而且两套委派继续并存。
- **维持 0016「成员只认 lead」。** 用户定人可以直接对成员说话。

## 未决

- ~~**保留范围**~~ 已由 [`0024`](./0024-the-session-log-is-the-authority.md) 定:会话的权威改为 harness
  日志,成员会话落盘、带 `parent`,重启后仍可看;resume lead 时带回没被 stop 的团队成员(0024 第 11 条)。
- ~~**人对成员说的话,lead 要不要知道**~~:定为第 7 节,知情不打扰。
- ~~**成员视图里还能做什么**~~:定为第 8 节。
- ~~0016「lead 被取消时不级联停止成员」要不要改~~:定为第 9 节。

## 闸门

- **产品只有一套委派**:产品树里的 `task` / `team` 工具来自 `subagent-in-process` /
  `team-in-process` 行;`CodingParts::host_only_tools` 不含 `task` / `team`。今天是红的。
- **成员过全部闸门**:产品树上,team 成员与 `task` 子 agent 的写文件、读敏感路径、带凭据
  shell、工作区外访问,和 lead 一样被拦或被问。
- **人对成员说话**:发往成员 session id 的消息以 `User` 来源进成员日志,并唤醒成员跑回合;
  成员到成员仍不可能(0016 原判据保留)。
- **人纠偏不打扰 lead**:团队空闲时,人对成员 X 发一条消息 → X 跑一回合,lead 不起回合;lead
  日志里有两条注入(人的原话、X 这一回合的最后一句),lead 下一回合的请求里看得到,且渲染里
  写明不是对 lead 的指令。
- **lead 的任务照常叫醒**:lead 发给 X 的消息与人的消息落在 X 的同一回合里 → 回合结束以消息
  汇报并叫醒 lead;X 主动 `tell_parent` 同样叫醒 lead。
- **泵统一**:成员与 `task` 子 agent 由句柄泵驱动(没有 `keep_driven`);对一个成员发取消只停它的回合、
  只拒它的提问,lead 与其他成员挂着的提问不受影响。
- **成员的提问在 lead 视图可答**:成员发起一次审批 → lead 视图的状态事件里有这个提问且标明
  提问者;从 lead 视图回答后成员继续跑。
- **人停成员**:人经前端停掉成员 X → X 日志末尾有「已停止」、realm 被移除;X 有未汇报的 lead
  委派时 lead 被叫醒,没有时 lead 日志只多一条注入、不起回合。
- **task 子 agent 跟着停**:lead 在 `task` 调用进行中被取消 → 子 agent 回合以 `Cancelled` 结束,
  task 工具返回已取消,lead 的回合随即结束、不等子 agent 跑完。今天是红的。
- **team 成员不跟着停**:lead 被取消时正在跑的成员回合继续跑完。
- **全部停下**:命令之后 lead 与所有成员的当前回合都结束,成员仍在注册表里。
- **撤回时停掉新成员**:`keep_interrupted_context = false` 时,lead 被取消的那一回合里 delegate
  的成员被 `stop`,更早回合里 delegate 的成员不受影响。
- **成员结束后可看**:`task` 子 agent 跑完、team 成员 `stop` 之后,按它的 session id 仍能
  补出完整事实流。今天是红的:日志只由 `Agent` 自己持有(`harness/agent.rs:526`),
  移除之后注册表里没有它,`by_session`(`:822`)查不到。
- 每条落地时摘掉被测代码证伪一次。

## 失效条件

- 差异对齐时发现某项 realm 版本补不了、又必须保留(例如成员必须跑在树外):重议第 2 节。
