# AtomCode 目标架构

状态:2026-09-12 定稿,描述 plexus 线的**目标形态**。当前生产架构(coding 栈)见
[`architecture.md`](./architecture.md);迁移完成后本文取代它。底座概念(插件、
缝、realm、可逆效应)的定义在 [`plexus-plugin-architecture.md`](./plexus-plugin-architecture.md),
这里不重复。决策依据见文末的 ADR 索引。

## 0. 一页总览

```text
                 ┌──────────────────────── Host ────────────────────────┐
                 │ 读配置 · 选产品 profile · 挂 UI 行 · 起 App · 进程生命周期 │
                 │ cli │ daemon │ embed(库调用) │ headless(评测/CI)        │
                 │ 共享库: launch · config/distribution/endpoints · auth   │
                 │        updater · telemetry 发送 · store(会话磁盘/租约)   │
                 └───┬──────────────────┬──────────────────┬──────────────┘
                     │ 装配              │ 装配              │ 装配
        ┌────────────▼──────┐  ┌────────▼────────┐  ┌──────▼──────────────┐
        │      Product      │  │       UI        │  │  Agent 能力行        │
        │ profiles/*.toml   │  │ ui 缝的提供方    │  │ tools · codeintel    │
        │ + 薄 crate:       │  │ tui · web · acp │  │ skills · mcp · web   │
        │ 配额 hook·品牌     │  │ (repl/oneshot/  │  │ memory · review 行   │
        │ persona·init 提示  │  │  quiet 内置)    │  │ team 行 · 纪律行     │
        │ 渠道工具·i18n 文案 │  │                 │  │ 工作区 gate 行       │
        └────────┬──────────┘  └────────┬────────┘  └──────────┬──────────┘
                 │ 只依赖 ↓             │ 只依赖缝 ↓            │ 只依赖 ↓
        ┌────────▼─────────────────────▼───────────────────────▼──────────┐
        │                        Agent 机制                                │
        │  plexus(插件运行时) · kernel(消息/工具/provider 类型) · harness    │
        │  (24 个缝 · agent-loop · session 日志与投影 · handle 泵 · catalog) │
        │  谁也不依赖                                                       │
        └──────────────────────────────────────────────────────────────────┘

契约只有三份:①句柄协议 8 命令/25 事件(SDK 面,ACP 是它的 wire 投影)
              ②缝 trait(提供方替换面)  ③配置树 insert/patch/remove(组合面)
```

## 1. 四条原则

1. **没有特权核心。** 模型适配、工具目录、审批、agent 循环本身都是行,每个注册都是
   可撤销的效应。能力靠加行,不靠改核心。
2. **Host 是唯一认识全部四层的。** UI 只认缝;Product 只认 Agent;Agent 谁也不认。
   依赖方向反过来一次,就是老栈的形状。
3. **契约是协议,不是签名。** 跨层交互走三份显式契约(句柄协议、缝 trait、配置树
   操作),不走某个 crate 的内部函数。SDK、前端、产品用的是同一份。
4. **日志是真相。** 模型看到的消息、UI 画的内容、持久化的东西、遥测的数据,全部从
   同一份追加式会话日志推导。压缩改投影不改历史,恢复等于重放。

## 2. 五个部分

### 2.1 Agent 机制

| | |
|---|---|
| 职责 | 提供缝、跑回合、记日志、把命令翻成动作、把事实投影成事件 |
| 装什么 | `atomcode-plexus`(App / Context / Plugin / 事件 / realm / 配置树)、`atomcode-kernel`(Message / ToolDef / LlmProvider / AgentCommand / AgentEvent 等中立类型)、`atomcode-harness`(`seams.rs` 24 个缝、`agent-loop` 行、`session.rs` 日志与投影、`handle.rs` 命令泵、`plugins::catalog()`) |
| 不装什么 | 任何产品判断、任何前端、任何磁盘路径与端口、launcher |
| 今天 | 已有。harness 里的 `ui*.rs`、`launch.rs`、`bundle.rs` 的 `*_APP` profile 要移出 |
| 允许依赖 | 无 atomcode 内部依赖 |

### 2.2 Agent 能力行

| | |
|---|---|
| 职责 | 把一种能力做成一个或一组 `Plugin` 行,挂到机制的缝上 |
| 装什么 | 工具(fs/bash/grep/glob/ast_grep + world 缝)、codeintel 符号层与图层、skills、mcp、web、memory、recall、五道工作区 gate、编码纪律(VerifyCadence、exec行策略、todo、plan mode)、review 行(rules 注入、diff 标注、impact plan、scope 围栏、扇出 + verify)、team 与 goal 控制器、编排原语 |
| 不装什么 | 产品文案、配额与计费、品牌渠道、前端 |
| 今天 | `atomcode-capabilities`(含混层模块,见 §9)、`atomcode-review`;coding 里的 discipline / todo / plan_mode / team / controllers 待拆成行 |
| 允许依赖 | Agent 机制 |

判据:「CodeReview 会想要它吗」。会,就是能力行;只有一个产品要,就是 Product。

### 2.3 Product

| | |
|---|---|
| 职责 | 选行、配行、加只有这个产品才有的行 |
| 装什么 | `profiles/*.toml` overlay(开哪些行、round 上限、压缩阈值、工具集);一个薄 crate:CodingPlan 配额 hook、AtomGit 网关签名与 REST 工具、品牌 persona 片段、init 提示词、i18n 文案、模型特化补丁(skill-first) |
| 不装什么 | 能力实现 |
| 今天 | 散在 coding 的 rate_limit / provider_factory / skill_first / init_prompt / persona、capabilities 的 atomgit;被删的四个产品变体证明差异只有 150 行 TOML |
| 允许依赖 | Agent 机制 + Agent 能力行 |
| 判据 | Product crate 超过一两千行,就是机制漏进去了 |

### 2.4 Host

| | |
|---|---|
| 职责 | 拥有进程:读配置、选产品 profile、挂 UI 行、起 `App`、管信号退出码、持有磁盘会话存储与租约、调度、自更新、遥测发送、鉴权 |
| 装什么 | 宿主共享库:launch、config(distribution / endpoints / proxy / tls / schedule / i18n 加载)、auth、codingplan client、updater、telemetry sender、store(会话磁盘格式、undo sidecar、OS 租约、MCP 生命周期);宿主二进制:`atomcode`(cli)、`atomcode-daemon` |
| 形态 | cli(终端进程)、daemon(常驻,多会话,挂 web + acp 行)、embed(别人的进程,`embed` profile)、headless(评测/CI) |
| 今天 | cli / daemon / config / auth / updater / telemetry / codingplan;coding `runtime.rs` 的持久化与重配部分 |
| 允许依赖 | 全部 |

### 2.5 UI

| | |
|---|---|
| 职责 | 填 `ui` 缝;把人翻成命令,把事件翻成画面 |
| 装什么 | `atomcode-tui`(产品 TUI,0012)、`ui-web`、`ui-acp`(SDK 与 IDE 的 wire)、内置的 repl / oneshot / quiet |
| 不装什么 | 驱动逻辑(回合、steering、取消、压缩排队归 handle 泵)、任何产品判断 |
| 今天 | tui 在;web / jsonrpc 在 harness 里待移出;acp 在老栈 cli 里待移植 |
| 允许依赖 | Agent 机制的缝与协议;不认 Host,不认 Product |

## 3. 运行时:一棵树

**缝**(`harness/src/seams.rs`)。`Seam` 类型可被用户 patch 文件替换提供方,`Core`
类型是机制自己的注册表:

| 缝 | 类型 | 装什么 |
|---|---|---|
| llm | Seam | 模型适配器 |
| llm-utility | Seam | 旁路模型:标题、摘要、建议,程序消费结果;不回退到 llm(0015) |
| fs · shell · opener | Seam | 执行世界:文件、进程、呈现给人 |
| compaction · session-title · session-persistence | Seam | 压缩策略、命名、持久化后端 |
| approval · user-questions | Seam | 审批策略、问人 |
| ui · agent-handle · agent-loop | Seam | 前端、句柄源、回合驱动 |
| findings · subagents | Seam | 结构化结论汇集、委派 |
| tools · system-prompt · operations · skills · mcp · code-index | Core | 活的目录与注册表 |
| sessions · session-projections | Core | 日志与增量投影;`sessions` 由 `Agents::create` 填进每个 agent 的 realm,树根没有(0014) |
| session-defaults | Core | `session` 行:前端自己那个 agent 的 id 与是否 resume |
| agents · control | Core | agent 注册表、运行中重配 |

**事件**:`tools/execute` 与 `agent/request` 是 waterfall,策略行(参数修复、审批、结果封顶、
压缩、溢出重试、截断续写)是挂在上面的监听器,顺序由行序表达。

**realm**:每个 agent 一个 realm,子 agent fork 自父;服务表与事件总线用同一个
`visible_from`,子 agent 看不到父的服务,也画不到父的屏幕。**agent 拥有自己的会话与
世界**(0014):`Agents::create(ctx, CreateAgent)` 在发布前把日志、cwd 和 `setup` 装进
realm;树级监听器通过任务局部的「当前 agent」(`agent::scoped`)解析日志,不再有全局
的一份。session id 就是 agent 的身份,ACP 的 `session/new` 直接映射到它。

**配置树**:四层叠加,内置 bundle → `$ATOMCODE_HOME/profiles/` → `harness.patch.toml`
→ 命令行 overlay。操作词汇 insert / patch / remove / disabled。`--dump-config` 与
`--audit` 是必需品:树越能 patch,「为什么我的 agent 是这样」越要能回答。

## 4. 三份契约

| 契约 | 内容 | 谁用 | 变更规则 |
|---|---|---|---|
| **句柄协议** | `AgentCommand` 8 个 + `AgentEvent` 25 个 | tui 控制面、daemon、SDK、ACP | 新命令只由 Agent 机制加;所有前端与 SDK 是它的投影,不各自长方法 |
| **缝 trait** | `Seam` 类型的 trait 签名 | 能力行、Product、Host 填提供方 | 加缝用 `plexus_service!`;改签名要过所有提供方 |
| **配置树操作** | insert / patch / remove / disabled + 行名 | Product、用户、Host | 行名是公开地址,改名等于破坏兼容 |

**ACP 是句柄协议的 wire 投影**(2026-09-12 决定)。做成 `ui-acp` 行,把老栈
`cli/src/acp/`(7.7k 行)移植过来;`ui-jsonrpc` 不再扩展。进程内 SDK 是 Agent 机制
的公开面(`embed` profile + `App` / `Context` + `AgentHandle` + `Plugin`),建议加一个
薄 facade crate 只做 re-export 与语义化版本。

句柄协议的能力上限就是 SDK 的能力上限:缺口清单(0013)里 28 个无对应的句柄方法不补,
任何 SDK 都没有。

## 5. 数据流

一条用户消息的一生:

```text
人敲键盘 ─► UI 行翻成 AgentCommand::SendMessage
         ─► handle 泵:回合在跑则 steering,否则起回合;Compact/Snapshot 排在回合后
         ─► agent-loop:从日志 derive_messages() 组请求
         ─► agent/request waterfall:压缩阈值 → 溢出重试 → 超时 → 限速 → llm 缝
         ─► 流式回来:AssistantChunk 逐条 commit 进日志
         ─► 工具调用:tools/execute waterfall:参数修复 → 审批(问 user-questions 缝)→ 执行(fs/shell 缝)→ 结果封顶
         ─► ToolResultLogged commit 进日志
         ─► 回合结束:TurnEnd commit;投影更新
日志每 commit 一条 ─► SessionEventCommitted 事件 ─► UI 渲染 / session-persistence 落盘 / telemetry / 投影
```

控制面:`control` 缝的 patch 在运行中卸载重挂任意行;`describe_self` 让模型自述活树。

## 6. 前端形态

两种合法形状:

- **树内前端**:填 `ui` 缝,从 `Context` 自己拿服务。repl、oneshot、quiet、web 是这种。
  便宜,同进程,零翻译;代价是焊在进程内 API 上。
- **协议客户端**:在句柄协议另一端。tuix 与 daemon 今天是这种,ACP 客户端将来是。

**tui 的结构**(0010、0012):自己的 plexus 树管 surface / modules / layout / commands,
每个面板、命令集、键位是一行;Host 只独占 surface、事件循环、布局仲裁、焦点。

**已做(2026-09-12):tui 的控制面收到 `AgentHandle` 上,渲染面继续订阅日志。**
此前 tui 自己写了一份 150 行驱动,与 `handle.rs` 分叉两处:cancel 少做 `refuse_all`,
`/compact` 不排在回合后且不发事件。现在 `handle.rs` 把泵抽成 `wire()` + `spawn()`,
tui 在 `tui-agent-client` 行里持有命令通道,面板与命令只发 `AgentCommand`;驱动只剩
一份、在差分闸门下,tui 与 agent 的契约就是句柄协议,以后翻 ACP 客户端只换这一个行。
不翻协议客户端,直到真的需要 daemon 背后或远端的 tui:那时要付的代价是一层
日志到协议的投影(SDK 反正要做)、`adjust_layout` 这类模型可调 UI 工具需要客户端
工具通道、realm 隔离从类型保证降为 session id 约定。

## 7. 宿主形态

| 宿主 | 挂什么 UI 行 | 特有的事 |
|---|---|---|
| cli(`atomcode`) | tui;`--ui` 可换 repl / web / acp | 终端、信号、单会话 |
| daemon | web + acp | 端口、鉴权、token 文件、多会话、调度 |
| embed | quiet | 调用方驱动;进程内 SDK |
| headless | oneshot + 无持久化 | 评测、CI、`--audit` |

四种宿主共用 `launch.rs`(参数、profile 解析、preflight、mount、hand_over)。宿主之间
不共享进程状态;共享的是磁盘上的会话存储与配置,由 store 与 config 两个库定义格式。

## 8. 产品形态

```text
products/<name>/
  profiles/*.toml          # 开哪些行、旋钮、工具集
  crate/                   # 只有这个产品需要的行,通常 < 1k 行
    codingplan_quota.rs    # 429 → 拉配额窗口
    brand_persona.rs       # 贡献给 system-prompt 的片段
    init_prompt.rs         # /init 文案
    channel_tools.rs       # 渠道 REST 工具
```

私有产品同一形状,放自己的仓库:一个 binary 调 `plugins::catalog()` 再 `register`
自己的行,加 profiles 目录;改名改地址替换 `distribution.rs` 与 `endpoints.rs`;
auth / updater / telemetry 在 Host 层,按需换。插件编译期链接,装插件等于重编译,
进程外扩展只有 MCP 与 ACP 两条路。

## 9. crate 与目录布局

目标:

```text
crates/
  agent/    kernel  plexus  harness
  rows/     capabilities  review  team  discipline
  product/  atomcode
  host/     config  auth  codingplan  updater  telemetry  store  cli  daemon
  ui/       tui  web  acp
  legacy/   coding  tuix  cli-old  daemon-old        # 绞杀中:只删不加
gates/layers.sh                                      # 用 cargo metadata 断言依赖方向
```

今天要处理的混层:`capabilities` 的 provider / atomgit / marketplace 反向依赖 `auth`;
它内部的 session(14k,harness 刻意不用)、plugin marketplace、setup、askpass、datalog、
team.rs 分别属于 Host 或能力行;`harness` 含 UI 行与 launch;`config` 16.7k 行是 Host
却被 L1 依赖。搬目录前要改流水线:官方构建把闭源 `atomcode-codingplan-crypto`
直接丢进 `crates/`。

不拆 capabilities 成十个 crate。拆 crate 痛点驱动,目录加闸门已能守住层。

## 10. 迁移路径

渐进,每步有闸门,UI 与宿主不阻塞:

| 里程碑 | 内容 | 闸门 | 硅基估算 |
|---|---|---|---|
| M0 | 依赖方向闸门 + 基线 | 只准降 | 1 小时 |
| M1 | harness 钻到 CodingRuntime 底下当引擎,`--engine harness`;21 道 middleware 接成行(gate 实现已在 capabilities) | `differential.baseline` 16 场景不涨;tuix / cli 测试全绿 | 2 到 3 天 |
| M2 | coding 分流:Product 行搬出;纪律与 todo 改事件监听;部分覆盖 11 项补厚;Host 会话(undo / 租约 / MCP 生命周期);编排原语与 team;review 搬成行 | 差分 + 各自单测 | 2 到 3 周,约三分之一等决定 |
| M3 | tui 控制面收到 AgentHandle;tui 补到「日用不想切回 tuix」;翻默认,tuix 留 `--ui tuix` | tui 闸门 + 每日 dogfood | 1 周自用,3 到 4 周翻默认 |
| M4 | `ui-acp` 行;新 daemon 宿主 | 真 IDE 接一次 | 各 3 到 5 天 |
| M5 | 删 legacy | — | 半天 |

两条线并行的规则:契约先行(新命令 / 事件 / 缝先加类型与空实现,一个小 commit);
tui 线永远对着 replay 模型与 Headless surface 开发;新命令只由 harness 线加,tui 线只
消费;各守各的闸门,集成点是每天用 atui 写这个仓库。

## 11. 未决与失效条件

未决,要人拍板:
1. undo 的会话模型:在日志上加事件,还是引入 sidecar。
2. 编排原语的形状:「一个 agent 的结构化结果交给下一个 agent 复核」怎么表达。
3. 配置树改写方法(`harness/patch` 等)是否进公开 SDK。当前:进程内开放,进程外不开。

失效条件:
- Product 层长成大 crate → 查机制泄漏,不接受。
- 某能力必须改 kernel 或 plexus 才能落地 → 机制缺口,单独立 ADR。
- 出现 daemon 背后或远端 tui 的真实需求 → §6 的「不翻协议客户端」重议。
- 替换路线叫停 → 本文降级为对照表,`architecture.md` 继续有效。

## 12. 决策索引

| ADR | 决定 |
|---|---|
| 0004 | TUI 流是不可逆的 block 序列 |
| 0005 | turn 与 step 是坐标 |
| 0006 | TUI 全屏不内联 |
| 0007 | 布局是日志化状态,一套操作词汇 |
| 0008 | 动画时间注入不读取 |
| 0009 | 自述是一扇门不是一句话 |
| 0010 | 每个面板是一行 |
| 0011 | (已取代)tui 是无头前端 |
| 0012 | tui 替换 tuix |
| 0013 | Agent / Product / Host / UI 四层、缺口清单、ACP |
| 0014 | agent 拥有自己的会话与世界;`session` 行只给默认值 |
| 0015 | `llm-utility` 旁路模型缝;会话标题并行起名、落成 `Titled` 事件 |

相关文档:[`plexus-plugin-architecture.md`](./plexus-plugin-architecture.md)(底座与 spike 记录)、
[`tui-composability.md`](./tui-composability.md)(TUI 的时间与空间可组合性)、
`gates/`(差分基线与 TUI 闸门)。
