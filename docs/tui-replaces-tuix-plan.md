# tui 接进 atomcode、替换 tuix:执行计划

状态: 定稿(2026-09-17)。按 ADR [0021](adr/0021-front-end-contracts-split-by-layer.md)–
[0024](adr/0024-the-session-log-is-the-authority.md) 排,取代讨论初期的「第 1–5 步」。四份 ADR 的
「未决」已全部清空,本页不再有待定项;执行中发现新问题,先补 ADR 再改本页。入口统一那部分与
[`assembly-and-host-plan.md`](assembly-and-host-plan.md) 的 W2-rest 重叠,以本页为准;那一页的
W1(配置折树)与本页无依赖。

## 一、目标

`atomcode --tui` 驱动产品装配里的 agent——与 tuix 今天驱动的是同一个 agent、同一份会话——
日用到不想切回 tuix;然后翻默认、删 tuix。

## 二、已定的决定(索引)

| ADR | 要点 |
|---|---|
| 0021 | 会话内走句柄协议,补回执(`Accepted` / `Rejected`,命令带客户端 id)、回合事件带回合号、每回合恰好一个终结;宿主控制单独一份中立契约;runtime 驱动协议只是过渡实现;undo 等进 SDK 面;契约类型放 kernel 分模块(`event` / `session` / `host` / `agent`,`agent.rs` 改目录模块保路径),服务键由消费方声明,两个 `StopReason` 合并,不建新 crate;错误表(第 8 条);不要 generation,改用 session id 寻址 + `based_on: SeqNo` + 回合号 / 提问 id;能力行命令走命令目录 + `Invoke` |
| 0022 | tui 与 agent 分两个 App;会话事实流进句柄协议;重建 App 对前端只以会话身份体现;agent 自述与状态用事件推(`AgentAdded` / `AgentRemoved` / `Described` / `StatusChanged`);每个会话一条流;读写配置树的命令只留 `/effort`;可调布局整体去掉(给模型的、给人的,含 `ctrl-f` / `ctrl-z` / `ctrl-n` / `/mascot`),ADR 0007 作废 |
| 0023 | 「树」= realm;产品 team / task 改用 harness realm 版本;成员面板是切换入口(竖排、「主」在首行、`Tab` 聚焦、方向键 + `Enter`、鼠标点击),切换后输入与流全换成该 agent 的;人可直接对成员说话、lead 知情不被叫醒;统一句柄泵;成员视图操作;取消级联 |
| 0024 | 会话唯一权威是 harness 事件日志;`SessionManager` 改存事件日志、不设从;事件词汇挪 kernel;片段不落盘、取消时半截输出落盘并进上下文;旧会话读时转换;resume 带回团队;租约每个落盘 Agent 一份;成员参数进会话头、stop 是事实;事件日志取代 transcript、记录带时间;旧 journal 丢弃;格式版本与回滚不分叉;撤销 / rewind / 恢复统一为 `Rewound` 事实 |

## 三、里程碑

```
M1 契约地基(kernel)
 ├─> M2 tui 接上 atomcode(最小可用,开始自用)
 └─> M3 会话日志成为权威          ← M2 与 M3 可并行
          └─> M4 团队(4.3 依赖 M3)
               └─> M5 同会话不重建 App + 功能补齐(依赖 M3)
                    └─> M5.6 面板与命令补齐(清单见 docs/plans/)
                         └─> M6 翻默认、迁移、删 tuix
后续(不挡替换):换会话不重建 App、回合引擎改名
```

**为什么 M2 排在 M3 前面**:M2 只开放发消息、回答、取消、压缩、新会话、resume、`/effort`。其中只有
新会话与 resume 会重建 App,而那是「换会话」,前端换一条流即可(0022 第 2、6 节),不依赖日志成为
权威。先接上就能先自用,后面每一步都在真实使用里验证。

### M1 契约地基(kernel)

| # | 内容 | 出处 |
|---|---|---|
| 1.1 | `kernel/agent.rs` 改目录模块:现有内容挪 `agent/engine.rs`,`agent/mod.rs` 里 `pub use`,调用方不改 | 0021 §6 |
| 1.2 | 两个 `StopReason` 合并进 kernel:先查泵(`harness/plugins/handle.rs`)今天怎么映射,再按含义合并、全仓替换 | 0021 §6 |
| 1.3 | 会话词汇挪 `kernel::session`:`SessionEvent`、`LoggedEvent`、`SessionHeader`、`InjectionOrigin`、`HeaderReason`、`derive_messages*`、`renumber`,连带 `Question` / `Answer` / `AboutCall` / `RateLimitPause`。内存日志 `SessionLog` 留 harness;harness 过渡期 `pub use` 保旧路径 | 0024 §6 |
| 1.4 | 句柄协议补强:命令带客户端 `id`;`Accepted { command, turn, steered }` / `Rejected { command, error }`;`TurnStarted { turn }` / `TurnComplete { turn, reason }` / `Steered { turn, … }`;每回合恰好一个终结。泵实现 | 0021 §7 |
| 1.5 | 会话事实流进句柄协议:按 session id 订阅、从某序号补。泵实现 | 0022 §1 |
| 1.6 | `kernel::agent` 契约:`AgentAdded` / `AgentRemoved` / `Described`(身份、模型 id 与能否看图与推理档、有无压缩、命令目录) / `StatusChanged` | 0022 §5 |
| 1.7 | `kernel::host` 最小集:新会话、resume、切推理强度的命令 / 事件 / 错误 / trait;错误按 0021 §8,寻址与防过期按 0021 §9 | 0021 §2、§8、§9 |
| 1.8 | 命令目录契约类型:命令描述、`Invoke { id, session, name, args }`、`Invoked { id, output }` | 0021 §10 |

**判据**

- kernel 不依赖 plexus(读 `Cargo.toml` 的守卫)。
- 只有一个 `StopReason`。**今天红。**
- 宿主控制契约每个变体 serde 往返一致。
- 回合归属可判定:steer 进当前回合的消息只看事件流就知道由哪个回合号收尾。**今天红。**
- 每个 `TurnStarted { turn }` 后恰好一个同号 `TurnComplete`(含取消、出错、关闭)。

### M2 tui 接上 atomcode(最小可用)

| # | 内容 | 出处 |
|---|---|---|
| 2.1 | 宿主侧 adapter:`kernel::host` 最小集 → `CodingRuntimeHandle` | 0021 §5 |
| 2.2 | 事实转发行:每个 App 挂一份,把会话事实与 agent 状态、自述事件转发到宿主持有的流(先例 `native-compaction-checkpoint`,`coding/host_rows.rs:1608`);coding 装配的推理档不经 `reasoning-effort` 行,由这一行在 `agent/describe` 上填 | 0022 §3 |
| 2.3 | tui 拆成独立 UI App:`ui-tui2` 不再起泵、不 `inject` `agents` / `agent-loop`;`tui-agent-client` 接宿主给的句柄与事实流;6 处 agent 侧服务直读改走契约 | 0022 §3 |
| 2.4 | 每个会话一条流:`Presentation` 按 `(会话, BlockId)`;新会话 / resume 建新流、丢旧流;切换时 tip 行提示 | 0022 §6 |
| 2.5 | 删可调布局:`tui-layout` 提示词片段、`adjust_layout`(`layout_tool.rs`)、`tui-commands-layout` 行、`ctrl-f` / `ctrl-z` / `ctrl-n`、`/mascot`、布局操作日志与撤销;面板行挂载时的 `LayoutOp::Show` / `Hide` 保留 | 0022 §8 |
| 2.6 | 删读写配置树的命令:`/rows`、`/rows-list`、`/tools-list`、`/audit`、`/patch`;`/effort` 改走宿主控制契约 | 0022 §7 |
| 2.7 | 入口 `atomcode --tui`:cli 在 `main.rs:2427` 分支(默认仍 tuix);`atui` 的 flag 搬进 cli;删 `tui/src/product.rs` 自拼的 coding 装配,改挂 `runtime::mount` + UI overlay;删 `atui` 二进制;`gates/tui.sh:80` 改调新入口;`launch.rs` 的 `the_full_screen_front_end_is_not_a_row_here` 不反向——0022 §3 定了屏幕是独立 App、tui 依赖 harness,harness 挂不了它;只把拒绝提示改为指向 `atomcode --tui` | 0018 §5、0022 §3 |
| 2.8 | 功能第一批:发消息、回答提问、取消、压缩、新会话、resume、`/effort` | 0022 §4 |

**判据**

- 装配入口唯一:`CODING_DEFAULTS` / `coding_overlay` / `CODING_ROWS` 的生产引用只在
  `atomcode-coding` 内部。**今天红**(`tui/src/product.rs`)。
- 会话互通:产品路径写下的会话,`atomcode --tui --resume` 之后屏幕上有历史。**今天红**
  (`atui` 读 harness JSONL)。
- tui 不依赖驱动协议(读源码守卫)。
- UI 不直读 agent 侧服务(读源码守卫)。**今天红**(6 处)。
- 屏幕跨会话切换不重不冻(headless 端到端)。
- 可调布局已删:`crates/atomcode-tui/src` 里没有 `adjust_layout`、`LayoutOp::Undo`、`tui-commands-layout`;
  键位表没有 `ctrl-f` / `ctrl-z` / `ctrl-n` 的绑定(读源码守卫)。
- `gates/tui.sh` 全过。删测试导致 `gates/tui-test-count.baseline` 下降时,在同一个 commit 里写明是
  随功能删除。
- `gates/differential.baseline` 只降不升。

**M2 完成后开始每天用 `atomcode --tui` 写这个仓库。**

### M3 会话日志成为权威

| # | 内容 | 出处 |
|---|---|---|
| 3.1 | 落盘记录外层加提交时间(注入时钟) | 0024 §14 |
| 3.2 | `SessionManager` 存事件日志:`<id>.events` + `<id>.index`,追加 `append_events`(与 `append_jsonl_line` 同样的保证);租约每个落盘 Agent 一份,追加前校验;写失败即停;删 `session-journal` 行 | 0024 §5、§12、§16、落地补充 |
| 3.3 | 片段不落盘;人取消时半截输出合并成事实;投影带半截文本(只文本) | 0024 §7–9 |
| 3.4 | resume = 重放事件日志,`session-native` 不再 `seed_from_snapshot` | 0024 §2 |
| 3.4a | runtime 的撤销 / rewind / 恢复落成事实:`Rewound { to, scope }` 与投影规则、非前缀的恢复整段撤回再提交、重建失败截回(从 5.3 提前) | 0024 §17、落地补充 |
| 3.5 | 读时转换旧原生会话:补旧 transcript 时间戳;旧文件加 `.migrated` 挪开(含 `.ui.json`) | 0024 §10、§14、§16 |
| 3.6 | 返回 `SessionSnapshot` 的读接口改为事件投影(daemon / ACP 读路径不动) | 0024 §5 |
| 3.7 | recall / worklog / `list_sessions` / 网页历史读事件;Claude Code hooks 的 `transcript_path` 指事件日志;删 `SnapshotHook` / `TranscriptHook` / presentation 写入 | 0024 §14、迁移写路径 |
| 3.8 | 格式版本:读到更新版本列出并拒 resume;半截输出等新事件种类随版本升 | 0024 §16 |
| 3.9 | 旧 `sessions/harness/` journal 不再写、不导入 | 0024 §15 |
| 3.10 | `AGENTS.md` 第 39、42 行改写成新现状 | 0024 |

**判据**:0024 闸门——只有一个权威、resume 无损(除片段外)、片段不落盘、半截输出进上下文、
一个会话只有一个写者、新版本会话被拒而不拖垮目录、回滚不分叉、记录带时间、旧 journal 不被读、
旧会话读时转换。

**依赖与冲突面**:依赖 M1.3。与 M2 并行时,两边都会动 `coding/host_rows.rs`(M2 加转发行,
M3 改 `session-native`),分 worktree 时先约定合并顺序。

### M4 团队

| # | 内容 | 出处 |
|---|---|---|
| 4.1 | 统一句柄泵:泵接管已存在的 agent;删 `keep_driven`;task 工具改为发任务 + 等终结;取消只拒该 agent 自己的提问 | 0023 §6 |
| 4.2 | 产品改用 `team-in-process` / `subagent-in-process`:先把 coding team / task 的现有测试原样挂到 realm 版本上跑,逐条对齐差异;在产品树上立成员的安全判据;撤掉 coding 的 `task` / `team` host tool | 0023 §2 |
| 4.2a | harness 委派边界:敏感路径硬拒、`.git` 与工作区外写拒、成员无 shell / 委派、角色文件 `tools:` / `model:` 受限、写 scope、执行限制只由 lead 更新 | 0023 落地补充 |
| 4.2b | 体验与配置:风险按参数、补齐 14 角色、轮数 / 并发 / 开关接配置、子 agent 计费、tuix `Team` 事件与 `task` 进度行过渡适配、登出结束成员 | 0023 落地补充 |
| 4.3 | 成员落盘:会话头 `member` 字段、「已停止」事实、resume 带回没被 stop 的成员;权限按当前角色重算 | 0024 §11、§13 |
| 4.4 | 命令目录:harness 加 `commands` 核心注册表;泵处理 `Invoke`;team 行登记 `stop`;tui 斜杠菜单合并 UI 自己的、宿主控制的、目录里的命令 | 0021 §10 |
| 4.5 | 人对成员说话:`User` 来源;lead 注入(新来源「人对成员说」);人发起回合的汇报改注入;带 lead 消息的回合与 `tell_parent` 照旧叫醒 | 0023 §4、§7 |
| 4.6 | 成员视图操作:lead 视图显示并回答成员提问;取消成员;人停成员(经 4.4);压缩成员 | 0023 §8 |
| 4.7 | 取消级联:task 子 agent 级联;「全部停下」命令;lead 回合被撤回时停掉这一回合新 delegate 的成员 | 0023 §9 |
| 4.8 | 成员面板改切换入口:竖排,「主」在首行;`Tab` 聚焦、`↑` / `↓` 移动、`Enter` 切换、`Esc` / 再按 `Tab` 回输入框;鼠标悬停高亮、点击切换;选中行一个状态键盘与指针共用;标出当前 agent、状态栏写名字;切换后输入与流全换成该 agent 的(成员流第一次切过去时从序号 0 补) | 0023 §3、0022 §6 |

**判据**:0023 闸门全部,加 0024 的「成员落盘」「成员参数可恢复」。

### M5 同会话不重建 App + 功能补齐(**已完成**,2026-09-18)

| # | 内容 | 出处 |
|---|---|---|
| 5.1 | 重载配置 / skills 走 control patch(**已做**:配置没动且无 MCP 时原地重读技能、不重建;配置动了或有 MCP 仍重建——边界见 0022 §2 的「重载的边界」) | 0022 §2、0023 §1 |
| 5.2 | 登出 / 登录只换模型行,不拆 agent;同一 App 内模型或推理档变了重发 `Described`(M1.6 只做了订阅时发)(**已做**) | 同上、0022 §5 |
| 5.3 | 撤销、rewind、恢复快照:`Checkpointed { turn, id }`;todo 跟投影走;屏幕把被撤的块标成已撤销并加标记块(`Rewound` 事实与投影已在 3.4a)(**已做**) | 0024 §17 |
| 5.4 | 宿主控制契约其余项 + adapter + tui 命令:撤销、rewind、恢复、切模型、MCP 状态与撤回、重载、登出 / 登录(**已做**) | 0021 §2 |
| 5.5 | goal / loop / 策略干预 / 本地上下文排队作为能力行,命令登记进命令目录;策略干预的两种错误归该行(**已做**) | 0021 §3、§8、§10 |

**判据**:同会话操作不重建 App(0022);撤销是投影的事、撤销后 todo 回退(0024);每个新命令的契约
serde 往返 + adapter 行为判据。

### M5.6 面板与命令补齐(2026-09-18 新增,排在 M6.1 之前)

> **🔴 2026-09-18 晚:面板这一块已交接出去**,入口是
> [`docs/handoff-tui-panels-2026-09-18.md`](handoff-tui-panels-2026-09-18.md)。
> 交接文档里写明了一件要紧事:**本节下面两轮对照用错了尺子**(问「有没有入口」
> 而不是「深度够不够」),所以那两张表不可信;能用的是清单里「按深度再对一遍」那节。
> 非 UI 的部分(6.3 迁移、分层债、两条契约缺口)不在交接范围,仍按原计划走。

清单在 [`docs/plans/2026-09-18-tui-panels-and-commands-inventory.md`](plans/2026-09-18-tui-panels-and-commands-inventory.md)
——两半来源:**能力面打底**(宿主控制契约 13 个变体、runtime 句柄 43 个方法、设置目录 14 个键、
会话日志 26 种事实,逐个问「屏幕上有没有入口」)+ **tuix 对照查漏一次**(0012 经本人解禁,只用于
找能力面推不出的交互形态,不作标尺)。

| # | 批次 | 内容 |
|---|---|---|
| 5.6a | **P0** | 接上 askpass(`sudo` / `ssh` 要密码时现在会干等);首启没有 provider 时的登录引导 |
| 5.6b | **A** | 13 条能力面缺口:模式切换、`/cd`、`/config`、会话改名、5 种事实上屏、轮次上限、`/mcp tools`,以及两条契约缺口(列模型、用量额度) |
| 5.6c | **B1** | 6 组由**能力行自己登记目录命令**(`/review`、记忆三条、`/skills`、`/init`、`/worklog`、`/setup` 与 `/guide`)——tui 侧零改动,可与 5.6b 并行 |
| 5.6d | **B2** | 16 条屏幕自己的活:`/diff` 浏览器、多会话、`/model` 选择器、`/provider`、`/copy` / `/save` / `/view`、`/think on\|off`、`/paste`、上沿 rule、goal / loop 状态行、@ 与 $ 补全、ghost 提示、终端标题等 |
| 5.6e | **O** | 开放性:把「一切都是插件」补齐到 tui 最后几处硬编码。清单见 [`docs/plans/2026-09-18-tui-openness-inventory.md`](plans/2026-09-18-tui-openness-inventory.md) |

已判定**不做或归 CLI**:`/upgrade`(与 CLI 完全重复)、`/app` / `/desktop`(零屏幕依赖)、
`/webui` 起服务那半、`/think budget N`(字段在 v2 已被丢弃,做了是假的)、`/schedule`(执行靠 OS
调度器)。理由逐条记在清单里。

**判据**:每条一行或一条命令登记(0019),先写判据、摘掉被测代码证伪一次;
`gates/tui-test-count.baseline` 随判据上抬。

**5.6 的进度**(2026-09-18 晚,收口)。a / b / c / e 已完;d(B2)逐条有了结论:

| 做完的 | |
|---|---|
| B2-1 `/diff` | 两级浏览器,工作区快照出 numstat 与单文件 diff |
| B2-2 多会话 | 本来就有(`/new` + `/resume`,有端到端判据);realm 化归「换会话不重建 App」那条后续 |
| B2-3 `/model` 选择器 | A12 时做掉 |
| B2-4 `/provider` | 列表 + 挑一个换过去(就是 `/model <id>`,一个开关) |
| B2-6 `/copy` `/save` `/view` | |
| B2-7 `/language` | `/config language` 的具名入口,同一段实现 |
| B2-8 `/whoami` `/worktree` | worktree 是能力行的目录命令（git 不进中立契约） |
| B2-10 `/think on\|off` | |
| B2-11 `/paste [路径]` | |
| B2-12 上沿 rule | 会话名 + 历史第几条 |
| B2-14 @文件 | `$skill` 判不做:每个可调用 skill 已经是一条 `/` 命令 |
| B2-15 ghost | 来源是本会话历史(fish/zsh 那种),右键接受 |
| B2-16 终端标题 | 顺带发现 `SessionEvent::Titled` 之前没人消费 |

| 有结论但不做 / 没做完 | 为什么 |
|---|---|
| B2-5 `/plugin` 市场面板 | **归 CLI**。coding 没开 capabilities 的 `plugin` feature,为它开等于把市场/git 那套拉进 agent 进程,只为重复 CLI 已有的动作 —— 与 `/upgrade`、`/webui` 同一把尺子 |
| B2-13 goal / loop 状态行 | 做了一半。`/autonomy` 能问到「第几轮、跑了多久」;**常驻状态行**要一条推送通道（运行时每轮发 `GoalChanged`,但那条流屏幕不在上面）,单独排 |
| B2-9 `/sync` | 原定就在 6.3 之后 |

这一轮顺带改对的分层(见 0021 同日修订):宿主控制契约搬出 kernel 成
`atomcode-host-api`,适配器搬出 coding 进 cli,并加了 `gates/layers.sh` 守依赖方向。

**依赖**:B2 的多会话依赖「后续」里的「换会话不重建 App」;`/sync` 与 6.3 同批;
**6.4 删 tuix 必须排在本节之后**——B2 多条是照 tuix 的交互形态补的。

**5.6e 为什么要赶在 6.4 前面做完**:下游 fork 的三份测绘结论是它们 90% 的定制
是「上游把本该是数据的东西写成了常量」。欢迎块与 mascot 刚落地就已经重犯了一次
tuix 的老毛病——现在改是十几行,等下游移植完再改就是他们再扫一遍 165 处字符串。

### M6 翻默认、迁移、删 tuix

| # | 内容 |
|---|---|
| 6.1 | 自用到「不想切回 tuix」;真模型冒烟(codingplan-crypto 的临时拷贝流程,跑完还原,绝不提交) |
| 6.2 | 翻默认:`atomcode` 默认进 tui,tuix 留逃生口 soak(**机制已备好**:`[ui] screen = "default" \| "rows" \| "classic"` + `--tui` / `--classic`,`screen_for` 一处判定;`Screen::Default` 现在解析成 classic,翻默认时只动这一处。等 6.1 自用过关再翻) |
| 6.3 | daemon / ACP / clix 迁到两份契约;ACP 的可用命令改由命令目录投影。**测绘见下** |
| 6.4 | 删 tuix:`cli/src/acp/commands.rs:21` 的 `CommandRegistry`、`cli/Cargo.toml:21` 的 `distro-pm` 转发、`main.rs` 的 tuix 分支、workspace 成员 |
| 6.5 | 清点并删掉不再被挂载的代码:runtime 驱动协议、coding `team/`、capabilities `tools/task.rs` 的委派部分 |

#### 6.3 测绘(2026-09-18)：比表面小很多

先澄清一件事：**删 tuix(6.4)真正卡的只有一处**。全仓对 `atomcode_tuix`
的引用只剩四个文件：

| 在哪 | 多少 | 是什么 |
|---|---|---|
| `daemon/src/{lib,trace}.rs` | 3 | **全是注释**，零真实依赖 |
| `cli/src/acp/commands.rs` | 1 | `use atomcode_tuix::commands::CommandRegistry` —— **就这一行** |
| `cli/src/main.rs` | 26 | `--classic` 逃生口，6.2 特意留的 |

所以 6.3 的 ACP 那一半 = 把那一行换掉，而换掉它要 ACP 能拿到命令目录 ——
目录是随 `AgentDescription` 走句柄协议来的，而 ACP 今天直接消费 `CodingRuntimeEvent`。

**改动面量过了，不是 7.7k 行：**

| ACP 今天调的 | 迁到哪 | 处数 |
|---|---|---|
| `respond`、`cancel`、`compact`、`shutdown` | 句柄协议 `AgentCommand` | 12 |
| `reprepare_config`、`undo_to_prompt` | 宿主契约 `SwitchModel`/`SetSetting`、`Undo` | 3 |
| `context_stats` | **契约里没有对应的** —— 要么新开一条，要么走 `AgentDescription` | 1 |
| 事件循环 | 全在 `turn.rs` 一个 match 里，7 处、4 种事件 | 7 |

`cli/src/host.rs` 的 `translate()` 已经把 ACP 用的那 4 种全映射好了，所以事件那一半
是换一个流而不是重写。唯一非机械的一处是 `TurnFinished(TurnCompletion)` —— 契约侧是
`AgentEvent::TurnComplete { turn, reason }`，而 ACP 下游还在用 `TurnCompletion` 本体，要先看它用来干嘛。

**一条真的行为变化，不能悄悄吞掉。** ACP 今天把
`TurnCompletion::SnapshotUnavailable`（回合跑完了、但快照没写成）报成 internal error
（`dispatch.rs:57`）；而契约侧的 `AgentEvent::TurnComplete` 只带 `reason`，适配器里两个
分支已经合并了。也就是说 **tui 今天已经丢了这个信息**，ACP 迁过去也会丢。

三条路：契约里给「回合完了但持久化出了问题」一个位置；或者承认它不该上契约、改成
一条事实（日志才是权威，0024）；或者明确写下「快照写不成不再向前端报」。
**做 6.3 之前先定这一条。**

##### 把这一条摊开(2026-09-18 量过,等拍板)

先把"谁在用它"数清楚,因为丢掉它的代价不在 tui 这边:

| 用它的地方 | 干什么 | 丢了会怎样 |
|---|---|---|
| `cli/src/main.rs:97` | headless 退出码 `current.max(1)` | **脚本会把失败当成功**。`atomcode -p "..."` 快照没写成时现在返回非 0 |
| `cli/src/main.rs:120` | 通知里报 `NotifyStopReason::Error` | 通知变成"完成了" |
| `cli/src/acp/dispatch.rs:57`、`v2.rs:583` | 向 ACP 客户端报 internal error | 客户端以为回合正常结束 |
| tui | **今天就没有** | —— |

头两个读的是 `TurnCompletion` 本体、不过契约,所以**它们不受 6.3 影响**;真正会丢的
只有 ACP 那一路。

再看这三条路各自成不成立:

- **改成一条事实(日志)** —— *自相矛盾,可以直接划掉*。失败的正是"写进日志"这件事;
  写不进快照的时候,再写一条"快照写不进"的事实一样写不进去。
- **不再向前端报** —— 成立,但等于让 ACP 客户端把"记录丢了"的回合当正常结束。
  0024 说日志是唯一权威,那么"权威没记上"这件事不报给任何人,是把唯一权威悄悄打了个洞。
- **契约里给它一个位置** —— 成立,且便宜。

**建议(要你拍)**:走第三条,但**不要动 kernel**。`AgentEvent::TurnComplete` 在
句柄协议里,而 kernel 要保持极稳;持久化是**宿主**的活(存储是它的),所以位置应该在
`atomcode-host-api` 的 `HostEvent` 上,加一条
`PersistenceFailed { session, message }`。这样:回合照常 `TurnComplete`(它确实完成了),
"没记上"单独推一条 —— 两件事本来就是两件事。tui 顺带第一次有了这个信息,ACP 迁过去
也不丢。kernel 一行不动。

**顺序**：ACP 先拿 `HostConnection`(`connect()` 已在 cli 里，和 tui 用的是同一个)→
`turn.rs` 换流 → 7 个方法换契约 → `commands.rs` 改从目录投影、删 tuix 那一行。

##### 进度(2026-09-18)

- ✅ **`SnapshotUnavailable` 定了**:`HostEvent::PersistenceFailed`,见上一节。卡点解开
- ✅ **`commands.rs` 不再认 tuix**。原打算放在最后,提前做了,因为它是 6.4 唯一的
  非 `--classic` 卡点。**但只做了一半**:表搬进了 `acp/commands.rs` 自己(16 行常量),
  **还没有**改成从 `AgentDescription` 的命令目录投影 —— 那要 ACP 先拿到
  `HostConnection`,也就是下面那三步。搬的时候按原样固定了 wire 可见的 15 条,
  并补了一条判据(advertise 的每一条都得有人能跑;跨 crate 那套写法根本保证不了这件事)。
  搬的过程中既有判据抓到一个真回归:老的 `find` **大小写不敏感**,`/Status` 要认作
  `/status`,我写成了精确匹配
- ⬜ ACP 拿 `HostConnection`
- ⬜ `turn.rs` 换流(7 处 4 种事件;`translate()` 已经把这 4 种映射好了,是换流不是重写)
- ✅ **`context_stats` 的契约缺口补上了**:`HostCommand::Context` /
  `HostReply::Context { window, used, model, working_dir }`。顺带 tui 的 `/context`
  第一次能说出预算 —— 它以前只能数自己看见的,而宿主还打包了系统提示、instructions
  与工具定义,屏幕一条都没见过
- ⬜ 7 个方法换契约(12 + 3 处)

###### 把剩下三步读完之后:**其中两处不是机械搬运**(2026-09-18 更正)

补完 `Context` 当时写的是"剩下的是纯机械搬运"。**那句话是错的**,把七个方法逐个
读过之后有两处不是:

| 方法 | 契约对应 | 是不是机械 |
|---|---|---|
| `respond` / `cancel` / `compact` / `shutdown` | `AgentCommand::Respond` / `Cancel` / `Compact` / `Shutdown` —— 四个全有 | ✅ 直换 |
| `context_stats` | `HostCommand::Context` | ✅ 刚补 |
| **`reprepare_config(next)`** | `SwitchModel` / `SetReasoningEffort` | ⚠️ **不是** |
| **`undo_to_prompt(nth)`** | `Undo { turn, based_on }` | ⚠️ **不是** |

**① `reprepare_config` 收的是一份已经解析好的 `CodingAgentConfig`**,由 ACP 自己
持有的两个闭包(`model_resolver` / `effort_resolver`,见 `options.rs:233`、`:250`)
算出来。契约这边收的是 `model` 名字或 `level`,由**宿主**去解析
(`RuntimeControl::reconfigure` 经 `HostConfig::for_model`)。换过去等于把模型解析
从 ACP 挪回宿主 —— 方向是对的(那本来就是宿主的活),但 `SessionModelResolver`
那条注入链要跟着拆,不是替换一行。

**② `undo_to_prompt(nth)` 的 `nth` 是"往回第几个",契约的 `Undo` 收的是 `turn` 号。**
契约里没有"往回数 N 个"这个说法 —— 这是故意的,`cli/src/host.rs:680` 那段会先查
`rewind_points()` 把 turn 号换成 prompt 序号。所以 ACP 的 `/undo 3` 要变成:
先 `RewindPoints` 拿列表 → 取第 N 新的那个 turn → `Undo { turn: Some(它) }`。

  外加 `based_on: SeqNo`(防的是"你看到的还是不是现在的状态")。`fresh()` 读的是
  **前端自己的会话日志**(`host.rs:547` 的 `front_end.app()`),tui 传的是
  `client.root_high()`。**ACP 今天不跟踪任何 seq**,要开始跟。

**结论**:剩下三步仍然该一次做完(七个文件是一体的),但动手前要先认下这两件事 ——
一件是把模型解析还给宿主并拆掉 `SessionModelResolver` 的注入链,一件是让 ACP 开始
记"我看到的最后一条事实是第几号"。把它们当成机械替换去做,会在半路上才发现。

#### 6.5 测绘(2026-09-18)：今天能删的只有一半,另一半卡在 6.3/6.4 后面

三块逐个追了构造点(不是按名字匹配),结论:

| 块 | 规模 | 生产调用方 | 判定 |
|---|---|---|---|
| **runtime 驱动协议** | `coding/src/runtime.rs` 17,529 行(协议面 ~1,650) | **~440 处**:cli(含 `host.rs` adapter 与 `main.rs:3023/3025`)、acp(6 文件)、daemon(~170)、clix(20)、tuix(~240) | **仍被挂载**。`atomcode --tui` 今天也是经 `cli/src/host.rs` 坐在它上面 —— `tui/tests/guards.rs:101` 那道守卫只保证 tui **crate 内**不出现这三个名字,证明不了协议不可达 |
| `coding/src/team/runner.rs` + `tool.rs` | 919 | **0** —— `TeamTool::new` / `TeamRunnerFactory::new` 只在各自 `cfg(test)` 与 `tests/team_runtime.rs`;`parts.rs:646` 写死 `None`,而那个字段除结构体初始化外无人读 | **已删** |
| `manager.rs` 的 run-store 半边 | ~400 | 0(`store.runs` 只由 `delegate` 填,`delegate` 无生产调用方 ⇒ 生产下 store 恒空) | **不单独删**。`stop_all` 还有 3 处调用且读这个 store,摘它要连带拆 `quiesce_current_agent` / `stop_current_agent` 的 `team_manager` 参数 —— 为 400 行去动回合循环,不划算。随 6.4 整块走 |
| `manager.rs` 的事件中继半边 + `team_progress.rs` | ~320 | `parts.rs:633` + runtime.rs 21 处 | **仍被挂载**:唯一目的是喂 tuix 的 team 面板,随 6.4 走 |
| `capabilities/tools/task.rs` 的委派部分 | ~2,590(含测试) | **0** —— `TaskTool::new` 全部构造点都在 `:1762` 之后的 `cfg(test)` 里;产品挂的是 harness 自己的同名私有工具(`plugins/subagent.rs:458`,经 `subagent-in-process` 挂上) | **已删**,文件 3,190 → 597 |
| `task.rs` 的 `WorkerScopeGate` + `delegated_write_violation` + 路径 helper | ~290 | `harness/src/plugins/policy.rs:450`(`DelegationBoundsPlugin`) | **必留** |

顺带发现:`SUBAGENT_ACTIVITY_MARKER` 留着(tuix 在 strip 它),但**发它的两处都在已删的委派里**,
所以 tuix 那两处 `strip_prefix` 今天已经匹配不到 subagent 活动了 —— 它还能匹配
`review_tool.rs:50` 发的同一个字符,所以前端那段代码本身不算死。

Cargo 影响:无 feature / profile 牵连。唯一陈旧的是
`capabilities/Cargo.toml:35-37` 那条注释——它用"`task` 工具的 `CancellationToken`"
论证 `tokio-util`,现在理由不成立了,但**依赖必须留**(`mcp/transport_stdio.rs:16`、
`codeintel/lsp_tool.rs:262` 都是生产用户)。

### 后续(不挡替换)

- **换会话不重建 App**:每会话状态下沉到 realm——provider 的 session 绑定、hooks、按目录
  解析的 MCP 与 skills(0023 §1 的目标形态)。
- **回合引擎改名**,解决与 harness `Agent` 撞名(0021 §6)。
- **可调布局**:想清楚再说(0022 §8)。

## 三点五、一条既有的红:coding 的 datalog 判据在并发下会挂

`atomcode-coding::mount_wiring::the_configured_datalog_records_the_turn`
(`crates/atomcode-coding/tests/mount_wiring.rs:126`)——断言落盘的 markdown 里有
`**Response:**`。

**不是本线改出来的,已经核过**:2026-09-18 晚把工作树倒回 `d3591cd1`(本线已推送的
那个提交)单独重跑,`cargo nextest run -p atomcode-coding` 全量 612 条同样挂这一条;
而只跑 `mount_wiring` 那一个 binary(4 条)两边都过。**只在并发压力下挂**,单独跑必过
——所以它是个竞态,不是断言写错。

线索:写的那一段在 `capabilities/src/datalog.rs:290`,`if !state.active { return; }`
之后按 `response.tool_calls.is_empty()` 分两种写法,只有空工具调用那一支才写
`**Response:**`。要么是 append 还没落盘测试就读了,要么并发下走到了另一支。
**没有继续追**:不在本线范围,但别让它烂在这里当"偶尔红一下"——那种红最后会训练所有人
无视闸门。

## 四、每步交付前必过的门

```sh
cargo nextest run -p <受影响的 crate>      # 不用 cargo test、不用 --workspace(AGENTS.md)
cargo fmt --all -- --check
bash gates/tui.sh                          # 碰 tui 时
gates/differential.baseline 只降不升        # golden 不可重录
```

新判据落地时先摘掉被测代码证伪一次,反证写进 commit。

## 五、不做

- 不读 tuix 定需求(0012)。
- 不建 `atomcode-protocol` crate(`AGENTS.md:54`)。
- 不重录差分 golden(`AGENTS.md:41`)。
- 不给时间估算:工作量等 M1 与 M3 各做出第一版再估。
