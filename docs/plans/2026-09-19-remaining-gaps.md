# 还差什么：2026-09-19 摸底与拍板

承接 [`docs/tui-replaces-tuix-plan.md`](../tui-replaces-tuix-plan.md) 的 M5.6 与 M6。
这份文档做两件事：把「新引擎 + tui 这套方案下还差什么」按**功能**列全，以及记下
2026-09-19 拍的九条决策。

摸底方法：五路独立测绘（tuix↔tui 深度对照、宿主契约能力面、删 tuix 卡点与死码、
配置/provider、多入口矩阵），结论逐条回代码复核过，**有四处推翻了测绘自己的判断**，
见第四节。

---

## 一、2026-09-19 拍板

| # | 问题 | 定的 | 要紧的理由 |
|---|---|---|---|
| 1 | 新机器上怎么加第一个 provider | **TUI 里给一条 OAuth 流**（扫码，全程不碰 api_key） | 「provider 增改删归配置文件」这条边界留了个洞：新机器上 TUI 里没有路能加第一个 |
| 2 | auth 的逻辑住哪 | **住 cli**。cli 实现端口、装配成 tui 的 plugin 行 | 照 `trait Settings`（`tui/settings.rs:539`）现成的形状：屏幕知道的产品信息都从缝过来，不从产品自己的服务里拿（0022 §3） |
| 3 | 先翻默认还是先补引导 | **向导做不完就不翻** | 翻默认只要一行，但翻了之后开箱不可用的锅要自己背 |
| 4 | 首启引导的深度 | **四步向导**（Intro / Language / Setup / Confirm + 扫码） | 对齐 tuix 的 `onboarding_wizard.rs`（2336 行）所覆盖的场景 |
| 5 | 向导的步骤是数据还是代码 | **数据**。tui 出通用 `Wizard` overlay，cli 喂步骤定义 | 「上游把本该是数据的东西写成常量」是下游 fork 90% 的定制成本（见开放性测绘）；`/setup` 将来直接复用 |
| 6 | 二维码谁画 | **tui 画**。cli 给 URL + code | 二维码是 presentation，归屏幕；URL / 轮询 / token 归 cli |
| 7 | `atomcode-auth` 拆两半这笔债 | **顺势拆**：Product 的那半（gateway_crypto / oauth / openrouter）与 Host 的那半（凭据文件读写） | 接的时候不拆，以后就是拆两遍 |
| 8 | `coding/src/team/`（1466 行）的去向 | **随 6.4 删 tuix 一起摘掉**，团队能力只留 harness 的 realm 版 | 新 tui 零消费团队事件；旧 team 面板是 0023 之前的形态 |
| 9 | `/plugin` 市场面板归 CLI 的判定 | **维持归 CLI** | 为它给 coding 开 capabilities 的 `plugin` feature，等于把市场/git 那套拉进 agent 进程，只为重复 CLI 已有的动作 |

**决策 2 + 决策 6 合起来划的那条线**：凭据、认证状态判定（有没有可用 provider、有没有本地
OAuth）、OAuth 轮询、步骤定义 —— 全在 cli；浮层机制、二维码渲染、按键 —— 全在 tui。
tui 今天**没有任何认证代码**，这条线是新划的，不是迁移。

---

## 二、顺序变了：引导先于翻默认

原计划 M6 是 6.1 自用 → 6.2 翻默认。决策 3 把引导插在前面：

```
A 引导（Wizard overlay + cli 的 auth 行 + auth 拆两半）
        │
        ▼
B ACP 命令投影（解开 6.3 的尾巴）／ C 便宜的修 ／ D /config 收尾   ← 可并行
        │
        ▼
E 6.1 自用 + 真模型冒烟  →  翻默认  →  soak
        │
        ▼
F 删 tuix + 摘 team/  →  daemon 迁契约  →  动 DriverCommand
        │
        ▼
G 体验项（/usage 画图、/resume 预览、输入历史落盘 …）
```

G 类七条**刻意排在翻默认之后**：自用会告诉我们哪几条是真缺的，预补出来的是「我以为要」的。

---

## 三、待办清单

判据规矩不变（0019）：**先写判据、摘掉被测代码证伪一次**，反证写进 commit；
`gates/tui-test-count.baseline` 随判据上抬。

### A. 引导（挡住翻默认）——**已完成，2026-09-19**

六条都落地了，顺序与当初写的不同：A6 先做，因为它是 A2 的触发时机。

- [x] **A1 tui 的通用 `Wizard` overlay**（决策 5）。步骤是数据：id、标题、已排好的
      body 行、四种要法（读一段 / 选一个 / 打一段 / 等宿主）。这个文件里没有
      onboarding、没有语言、也不知道屏上那个码是用来扫的。等待是它比 `Picker` 多
      出来的那件事：宿主留着 `Arc`，进行中 `say()`、落地 `resolve()`。
      顺带给 `Overlays` 开了 `finish(id, value)`——按名字关，因为登录迟到而人已经
      打开了别的东西时，按「当前开着的那个」关会关错。
- [x] **A2 tui 的 QR 渲染**（决策 6）。一张位图不是一串转义：颜色不走角色（扫码器
      要的是浅底深块），一格用 `▀` 装上下两个模块（否则码是宽高比 2:1，扫不出来）。
      终端不画单元格背景时一张都不画——半张码比没有更坏。
- [x] **A3 cli 侧的 auth 行**（决策 2）：`tui_onboarding`，四步（说清现状 / 语言 /
      扫码登录 / 看一眼结果），登录那步做的是 `atomcode login` 浏览器回来后的同样
      三件事减去打印。**这里一行都不能往 stdout 打。**
- [x] **A4 触发判定在 cli**：readiness 对 `NotConfigured` 点名 `onboarding`；行无
      条件挂上，跑不跑由启动时问宿主的那一答决定。
- [~] **A5 拆 `atomcode-auth`**（决策 7）——**做了，然后回退了（2026-09-19）**。
      理由见下面「A5 为什么退回去」。这条债从此按「不做」记，别再照着交接文档里
      那句话做第二遍。
- [x] **A6 提交前预检**：`HostCommand::Readiness` 一问一答。这是「首启没引导」的
      根因：新桥只有提交后报错，旧 driver 协议的三个预检一个都没接。

### A5 为什么退回去

拆是拆成了：`atomcode-credentials` 拿走凭据文件、锁和 `AuthInfo`，`atomcode-auth`
只剩协议，方向还上了闸门。全部测试绿，闸门三条都证伪过。**然后退了**，因为核完
账发现没有收益：

| crate | 拆之后还依赖 auth 吗 | 还依赖 credentials 吗 |
|---|---|---|
| capabilities / daemon / codingplan / cli / tuix | **都还依赖** | 都依赖 |

**一个调用方都没变轻**，每个反而多一条 manifest 边。当初写在提交说明里的那句
「任何只想知道谁登录着的地方不用再拉 HTTP 客户端」，描述的是形状，不是今天任何人
收到的好处——今天没有这样的调用方。

提这件事时给的理由也没站住：我说「A3 正在碰 auth，顺势拆掉，否则以后拆两遍」。
实际上 A3 只是**调用** auth（`start_login` / `spawn_poller` / `finish` / `save_auth`），
一行内部实现都没改，所以它根本不是顺势，是单独一件事。

**留下的判断**：这笔债要不要还，取决于**有没有一个只要存储那半的调用方**。今天
没有；等真出现一个（比如某个只读"谁登录着"的小工具，或下游 fork 要换认证），
那时拆才有人收得到好处。在那之前，它只是多一个 crate。

回退掉的 commit 是 `6ecd9047`，自包含，退得干净。

**这一节顺带补的机制**（不在原计划里，是做的过程中缺的）：
- tui 的 `Repaint` 缝：循环外的活改了屏幕要能让它重画；登录轮询正是这种活，而恰恰
  在「等它」的那一步，下一次按键可能永远不来。
- `deliver()`：命令被「选中」和被「打出来」原本走两条路，选中那条把 `Outcome::Open`
  静默丢掉——选中项若要再开一个浮层，什么都不会发生。

### B. 解开 6.3 的尾巴 —— **已完成（2026-09-20）**

- [x] **B1 ACP 命令改投影**。两半一起落地了，而**卡住它的从来不是那两个"设计决定"，
      是一处接线漏了**——前两版诊断都没找对，记在这里免得后人照着错的方向走。

**真正的根因（第三次才挖到）**：ACP 起运行时时 `PrepareOptions.front_end` 是 `None`
（`acp/engine.rs` 走 `..default()`），而它的 `FrontEnd` 是**运行时起好之后**才建的。
于是 `front-end-feed` 那条行根本没挂（它只在 `host.front_end.is_some()` 时插），
`FrontEnd::app` 永远是空，每一次 `Subscribe` 都被拒，`Described` 永远不来——
所以这个通道对"自己 agent 注册了哪些命令"一无所知。tui 是在 `main.rs` 先建
FrontEnd 再传进 prepare 的，所以它一直是对的。

修法：`spawn_session` 自己建 FrontEnd、传进 `PrepareOptions`、连同运行时一起返回；
`register_session` 收下它，不再自己造一个。判据
`the_commands_the_agent_registered_are_advertised_here_too`（把 front_end 从 prepare
里摘掉就判红）。

**执行那半**按当初写的 C 方案做了：`AgentEvent::Invoked` 多一个 `queued: bool`，
由 `run_catalog_command` 读 `agent.inbox().has_waking_input()` 现场作答。只答的命令
在收到 `Invoked` 时就收口，排了活的接着等那个回合的 `TurnComplete`——两种都错不得：
早收口，那个回合的事实会落到**下一次** prompt 上（turn.rs 通篇在防的就是这件事）；
干等，请求就挂住。判据 `an_agent_command_runs_in_the_agent_and_ends_where_it_ends`
两种形状都钉了。

名字冲突的规矩：两边都有的名字算**这个通道自己的**——它才是真正会跑的那个，
拿 agent 的句子去描述一条被本通道截走的命令，说的是客户端永远拿不到的东西。

### C. 便宜且独立（各 30 分钟量级，随时插队）

- [x] **C1 `/init` 登记**（2026-09-19）。`/worklog` 的同一个形状：`InitPlugin` 在
      `coding/host_rows.rs`，提示词由 `build_init_prompt` 按语言取，人自己的
      `init_prompt_file` 在**命令运行时**读——配置被改过就该生效，理由同设置面板
      持路径而不持已加载的 `Config`。
- [ ] **C1 余下的 `/setup` 与 `/guide` —— 清单写错了，摸完的事实如下**：
      - `/setup` **不需要新写一条命令**。种子 skill
        `assets/setup-seeds/skills/atomcode-automation-recommender/SKILL.md` 的
        frontmatter 就是 `name: setup` + `user_invocable: true`，而
        `harness/plugins/capabilities.rs:132` 把每个 user_invocable skill 自动
        登记成一条命令。**装过种子的机器上，新前端已经有 `/setup` 了。**
        真正的缺口只剩「全新机器上第一次输 `/setup` 时自动装种子」这一件，而
        CLI 的 `atomcode setup` 已经能装。**收益小，待定。**
      - `/guide` 在旧前端是**一张写死的 i18n 菜单**（13 个 `Msg::Guide*` 串）
        加上带参数时展开一个叫 `ask` 的 skill——清单写的「都是展开一个 skill」
        对 `/guide` 不成立。新前端已经有 `/help` 列命令；再搬一套 13 条文案的
        第二份帮助，**收益存疑，建议不做**（真要做，`ask` skill 装上以后自己
        就是一条命令）。
- [x] **C2 失败的压缩会说话**（2026-09-19）。**没有**按原计划加 `NoticeKind`：手动
      压缩走的是 coding runtime 的 `CompactionFinished`，不经 harness 的 notice
      那条路，加一种反而是第二条路。改成屏幕认 `AgentEvent::CompactionFailed`
      （原来只认 `Compacted`，失败那支掉进 wildcard，全链路静默）。
      为它给 e2e 补了一条事件注入缝（`start_with_agent_events`）——"agent 说了 X
      屏幕怎么办"这类判据都用得上，而 fixture 造不出这种事件。
- [x] **C3 删 `CodingRuntimeHandle::reprepare`**（2026-09-19）。连带摘掉
      `ReprepareTarget::Exact`（唯一构造者就是它）、它的 match 臂，以及
      `ReprepareInput` 的公开再导出——少一个公开 API、一个枚举变体、一处分支。

### D. `/config` 收尾（只剩两样）

- [x] **D1 恢复默认**（2026-09-20）。契约加 `HostCommand::ResetSetting`，宿主加
      `reset_setting`，端口加 `Settings::reset`，面板给**两次 Delete**：这是这个
      面板里唯一会扔掉东西的手势，一按就生效的键是有人去按 Backspace 路上会误触
      的键。确认随行消失——armed 是 `take()` 不是 `clone()`，否则移到别的行再按
      Delete 会把那一行恢复掉。
      **是删键，不是写当前默认值**：删掉的键从此跟着这个构建走，写成今天的默认值
      就不跟了；人说「恢复默认」指的是前者，而两者当天看起来一模一样。
- [x] **D2 按 provider 变的动态 retry 项**（2026-09-20）。它进不了静态目录是因为
      它读写的位置在**当前选中项**底下（`[models.<id>]` 或 `[providers.<id>]`），
      而那随 `/model` 变，没有哪一条静态路径能指到它。所以由 `tui_settings` 按
      选中项现造一行，走 `patch_selection_retry_max_attempts`（那个函数自带重置，
      且刻意不碰凭据）。没选模型就不造这一行——一条讲「当前模型」而没有当前模型
      的行，值是谁也解释不了的。
      顺带修了一条判据的表述：原来断言「行数 == 目录条数」，现在有一行本来就
      进不了目录，只断言条数会变成**禁止**那一行或什么也没说。

### E. 自用与翻默认

- [ ] **E1 真模型冒烟**：codingplan-crypto 临时拷贝流程，跑完还原，**绝不提交**
- [ ] **E2 翻 `Screen::Default`**：`cli/src/lib.rs:112` 一行，`:361-371` 的断言同步改，
      tuix 留 `--classic` soak

### F. 删（E 之后）

- [ ] **F1 删 tuix**：`main.rs` 的 26 处 `--classic` 分支（**唯一真卡点**）+ workspace member
      + `cli/Cargo.toml:21,34` 的 `distro-pm` 转发。daemon 那 4 行、`acp/commands.rs:5`
      都是注释，不是卡点
- [ ] **F2 随 F1 摘 `coding/src/team/`**（决策 8）：半活半死，`stop_all` 还绑在
      `runtime.rs:4229/8965/9033` 三处 `quiesce_current_agent` / `stop_current_agent` 上，
      单独砍会断
- [ ] **F3 daemon 迁两份契约** —— `DriverCommand` 能不能删的前置。它**今天不是死码**：
      daemon 的 `live_hub.rs` / `kernel_runtime.rs` / `native_live.rs` / `live_api.rs`
      仍大量挂载（tui 侧已有判据禁止碰它：`tui/tests/guards.rs:101`）
- [ ] **F4 `daemon/src/legacy_convert.rs`（4485 行）**：唯一的历史 core JSON 单向 importer。
      消费者归零前不动；**在任何报告里都不得称「格式已删除」**

### G. 体验项（翻默认后，按自用暴露的顺序排）

> **2026-09-20 逐条回代码核过一遍。** 起因是 G3 记错了（见下），而那种错法会让人
> 去重建一个已经有的东西。核的办法是打开实现看它到底做了什么，不是看清单怎么写的。
> 结论：**G1 / G4 / G5 / G7 属实，G2 / G6 记多了，G3 记错了。**
>
> | | 核到的 |
> |---|---|
> | G1 | 属实。`/usage` 只有一段 `Outcome::Said` 文本（`tui/commands.rs:1163`），一根线都没画 |
> | G2 | **搜索已经有了**——`Picker` 的过滤同时匹配 label 与 about（`overlay.rs` 的 `visible()`），打字就在筛。真缺的只有预览和删除 |
> | G3 | **记错了**，见下面那条 |
> | G4 | 属实。`Moment::autonomy` 存着而且有判据（`plugin.rs:3070`/`:3532`），但 `modules/` 里没有任何人读它——数据齐了，没人画 |
> | G5 | 属实。`/view` 的 `Reading` 只吃滚动键，别的键一律关掉（`overlay.rs`），没有搜索也没有语法色 |
> | G6 | 记多了一半。每行已经带 `about` 和「当前这个」的标记（`commands.rs:817-822`）；真缺的是**分组**，而「能力标注」取决于宿主往 `about` 里放什么 |
> | G7 | 属实。全仓搜不到任何书签/收藏的代码 |


- [x] **G1 `/usage` 画图**（2026-09-20）。画在设置面板的「用量」页签里，不再另开命令。
      过程里自己错了三次，都记在这儿，因为每一次都是同一种错法：
      1. **先断言了「整个栈里都没有已用量」**——错的。用户甩了一张 tuix 的图过来，
         上面就有百分比。回去查，`codingplan::types::RateLimitWindow` 一直带着
         `usage_percent` 和 `calls_used`；丢是丢在 `daemon/runtime_host.rs` 那个
         **逐字段手抄**的映射里，抄的时候漏了两个字段，于是整条链下游就真的没有了。
         判据 `a_window_crosses_with_everything_the_service_said_about_it` 把这两个
         字段钉住——再漏抄就判红。
      2. **把页签自己写的占位文案当成了需求**。那句话是占位，不是意图。这一页说的是
         **账号额度**（窗口用了多少、几时重置），不是本次会话烧了多少 token。
      3. **进度条的底和填充用了同一个字**，于是不上色时条条看着都是满的。改成
         `█` 填充 + `░` 底（无 Unicode 时退到 `-`），并用一条断长度的判据钉住。
      画出来是两节：**额度窗口**（每个窗口一根条 + 「用了 N%」+ 「几时重置」，
      学用户给的那张 Claude Code 的样式）和**总览**（总 token/请求数/日期范围、
      每天用量的火花线、各模型的 token/请求/占比条）——tuix 那张图上的信息一条没丢。
      口子开在 `RateLimitWindowSource::fetch_usage()`（默认 `Ok(None)`，
      宿主接不上就只是不画这一节），daemon 那侧用 `client.usage()` 接上。
      新判据 4 条，证伪过 5 种（含「没有额度窗口时提前 return 会吞掉总览那一节」
      ——这条是判据先抓到的，不是看出来的）。
      **当天又返工两轮，两轮都是我先自己发明、用户拿 tuix 的图纠回来的：**
      第一版画成一行 8 级方块的火花线——那答的是「有没有涨」，答不了「涨到多少、
      哪天」，而后者正是这张图存在的理由。第二版补了坐标轴，但画成了填充的柱子；
      用户一句「你不能用区块图，要用线图」。第三版才是折线，用户接着说「跟 tuix
      的对齐」——于是去读 `tuix/modals/usage_render.rs`，发现**那边早就有
      `braille_line_plot`**，几何也早定好了：52×6 格盲文、Y 轴取 1.0/0.6/0.2/0、
      **最底那行本身就是图的一部分**（横贯的点线是值为 0 的折线，不是另画的横杠）、
      X 轴最多 5 个刻度（端点全日期、中间 MM-DD、撞了往右推）、表格列宽 26/10/9/7。
      照抄。
      教训和 G1 正文第一条是同一个：**先去读对面怎么做的，再动手**。三轮返工里
      有两轮是因为我没先读那 444 行。
      顺带又挖出同一个手抄映射丢的第三样东西：`UsageRow.model_tokens`（每天每个
      模型的 token）。tuix 的图是每模型一条线、颜色对应表里的 ●，没有它只能画一条
      总量线。补进契约（`UsageStats.series`）、`daemon` 那侧提成 `usage_from()`
      并配判据 `every_model_keeps_its_own_day_by_day_figures`——那个闭包现在能离线
      判了，这正是 `window_from` 当初提出来的理由。
      颜色走新的 `Role::Series(n)`：**唯一「意义就是区分」的角色**，所以是带下标的
      一个角色而不是六个具名角色——名字会宣称颜色有含义。tuix 那边直接写的 256 色
      号（`[75, 214, …]`），既假设深色终端，也和本前端对每一种颜色的答案都不一致。
- [ ] G2 `/resume` 预览 + 搜索 + 删除 —— 给 `overlay.rs:32` 的 `Overlay` trait 加 preview
      概念，不是单做一个特化 picker（否则 `/rewind`、`/model` 将来要同样的东西时再加一次）
- [~] **G3 这条记错了（2026-09-20 用判据核实）。** 原文写「只在内存里，重启即丢」，
      依据是那个 push 点——但没看它**从哪儿** push：历史是从**事实**折出来的
      （`tui/host.rs` 的 `Host::fold`，`SessionEvent::UserMessage` 那一支），而
      resume 会重放日志。所以**恢复一个会话，上箭头翻得回上次打的东西**，这已经
      是 0024「日志是唯一权威」的做法了，不用再补。
      判据：`a_resumed_session_remembers_what_was_typed_into_it`（整块摘掉会判红）。
      **真正还没有的是跨会话**：新开一个会话看不到昨天在别的会话里打过的命令
      （tuix 那个 704 行的文件是跨会话的）。要不要做是另一回事——做它要么去读别的
      会话的日志，要么另开一份文件（那就和 0024 拧着了）。今天没人要，先不做。
      写这条判据时自己踩了两次：第一版断言「屏幕上有这句话」，而 resume 的
      transcript 本来就有，摘掉被测代码照样绿；第二版的**证伪**写错了——把分支改
      走 `Titled`，而这个会话的名字恰好就是那句话，于是照样推了同一个字符串。
- [x] **G4 自主状态行**（2026-09-20）。事实一直在存（`Moment::autonomy`，由
      `HostEvent::Autonomy` 每轮喂，还带判据），**没有任何人画它**——人只能靠
      `/autonomy` 主动问，而「它还在跑吗」恰恰是不方便去问的时候才想知道的。
      画进底部状态栏而不是自开一行：一条不跑时就空着的行，等于拿对话的一行去说
      「现在没事发生」。停着的时候把原因也说出来——「登记了但没在跑」正是人会
      干等的那个状态。
      **做它本身越了序**：G 节按决策排在翻默认之后、按自用暴露的顺序做，这条是
      在那个顺序之外做的（小、已验证、不挡任何东西）。不认可就 revert。
- [ ] G5 `/view` 搜索 + 语法色 ｜ G6 `/model` 分组与能力标注 ｜ G7 `/cd` 书签

### H. 记账与既有的债

- [x] **H1 三处报高已改回**（2026-09-20）。改的是清单本身，不是只在别处记一笔——
      清单是后人会照着做的那份，留着报高的格子等于埋一次重复劳动。
      逐条落点：`/init` 标上 C1、`/setup` 那格改成「不用新写命令，种子 skill 自己
      就会被登记」、`/guide` 标「判定不做」；深度表的 `usage` 与 `config_panel`
      两行改成已补；`/config` 差的四样逐条标上 D1 / D2 与 09-18 之后那两个提交。
      计划页那两处同步：5.6c 的警告结清，6.3 的「有一个例外」结清成 B1。
- [x] **H2 追完了，不是竞态写错，是没人等它写完**（2026-09-20）。
      datalog 的写是**后台 OS 线程 + fire-and-forget**：`append` 只往 channel 里塞，
      而回合结束的监听器是同步的，所以它要的那次 flush 是 `tokio::spawn` 出去的
      ——`on_harness.rs` 的注释自己写着「只有 WAIT 被 spawn 了」。并发下线程调度
      晚一点，判据就先读到了还没写完的文件。
      **这不止是判据的事**：进程若在回合后立刻结束（`-p` 跑一次、一个读自己刚要的
      文件的测试），最后一轮的日志就是写线程碰巧赶完的那部分。
      修法：writer 多一个**同步**屏障（`WriteOp::BarrierSync`），树拆的时候
      （`Context::effect`——那是最后一个还能保证的时刻）有界地等它一次（2 秒上限：
      写没了的 writer 不该把收尾拖住，而这里赌的只是一段日志的尾巴）。
      验证：`cargo nextest run -p atomcode-coding` 连跑三次，各 834 全过。
- [~] **H3 不做**（2026-09-20 核实）。`parse_version`（`updater/lib.rs:1102`）剥掉
      `-` 之后的部分是**为 `-beta.1` / `-rc.2` 刻意做的**（Issue #596），而上游从
      没发过带 `-N` 的版本——tag 全是 `vX.Y.Z`，`latest.json` 也是。这条是下游 fork
      报的：他们若用 `-N` 当修订号，要先决定 `v5.1.0-3` 该排在 `v5.1.0` 之上还是
      之下（semver 说 pre-release 在正式版之前，而「修订」的直觉相反）——那是他们
      的产品决定，不是这里能替他们拍的。上游照当前行为是对的。
- [x] **H4 已修**（2026-09-20）。`atomcode-capabilities` 的测试编译在本分支上是红的，且**不是本线改出来的**：
      `session/manager.rs:4769` 的一条测试调 `mgr.append_jsonl_line(…)`，而这个方法
      在 HEAD 的同一文件里根本没有定义（0 处定义、1 处调用）。也就是说
      `cargo nextest run -p atomcode-capabilities` 在这条分支上跑不起来，已经有一阵
      子没人跑过它了。多半是 0024 事件日志迁移时删了方法、漏了这条测试。
      2026-09-19 发现于 A5，2026-09-20 修掉：写那一半的 `append_jsonl_line` 随
      `513e7567`（删 TranscriptHook、转事件日志）一起没了，而**读**那一半还活着——
      daemon 的 transcript 端点就在用（`daemon/lib.rs:2303`）。所以测试改成直接
      把 jsonl 写进文件，测的仍是那个读法。修完 `cargo nextest run -p
      atomcode-capabilities` **930 条全过**，也就是说这批判据从那次提交起一直是
      跑不起来的状态，而没有人发现。

---

## 四、摸底纠正的记账（H1 的内容）

**三处报高**，都已回代码核过：

1. **5.6c 标 ✅ 是错的**。`/init`、`/setup`、`/guide` 在 tui / harness / coding 里
   **零命中**；`commands::register` 全仓只有 6 个调用点（`harness/plugins/capabilities.rs:114/132/476/766`、
   `coding/host_rows.rs:1813/1969`），内置 skill 只有 `atomcode-automation-recommender`
   一个种子。B1 里最便宜的三条从未落地。
2. **`/config` 差四样 → 实际差两样**。清单写于 09-18 晚，之后的提交补上了「预填当前值」
   （`cli/tui_settings.rs:100-135`）与「写完不关连续改」（`:132-140`）。设置目录本身是
   健康的：28 项 1:1 全覆盖，`tui_settings.rs:203` 用 `SETTINGS.len()` 钉住，加新设置不会漏。
3. **6.3「完成」有例外**：ACP 命令投影未做（B1）。计划页 6.3 的进度里自己写了「只做了
   一半」，这次独立测绘扫到同一处。

**一处推翻测绘自己的判断**（记下来免得后人照着修）：测绘报告说
`SessionEvent::PolicyIntervention` 的 doc comment 承诺「resume 后原样再问一次」而
UI 没实现。**核过是错的**：`harness/plugins/handle.rs:422` 有 fact → `AgentEvent`
的回放映射，tui 消费 live 事件即可，承诺是兑现的。

**契约面本身是干净的**：`HostCommand` 28 / `HostReply` 15 / `HostEvent` 3 全部已接；
`AgentCommand` 13 个里 tui 不发的 3 个是宿主内部注入命令，不是缺口。

---

## 五、不在这条线上

- 提交前必核 `Cargo.lock` 的私有依赖（AGENTS.md）。本次摸底时实测为 0。
- 版本号、发布配置不属于本线。
