# 交接:plexus 线,2026-09-12(2026-09-13 夜续)

分支 `feat/plexus-plugin-architecture`,整条线自 `origin/release/v5.1.0` 起 103 个 commit
未 push,远端没有这条分支。(起手先 `git log --oneline origin/release/v5.1.0..HEAD`
看真实 HEAD;下面的形状与验证一节随 commit 更新。)

**这棵树同时被两个会话写。** 09-12 夜起 tui 那条线由另一个 atui 会话在推进,工作区
里常年有它在飞的改动。碰共享文件之前先 `git status` 看一眼,提交用显式路径而不是
`git add -A`——今晚已经出过"它的半成品让树编译不过"和"差点把它没写完的功能扫进我的
commit"各一次。

## 先读什么

按这个顺序,20 分钟能接上:

1. [`architecture-target.md`](./architecture-target.md) —— 目标形态一页总览、五个部分、
   24 个缝、三份契约、迁移里程碑。
2. [`adr/0013`](./adr/0013-agent-product-host-ui.md) —— Agent / Product / Host / UI 四层与
   coding / review 对 harness 的缺口清单(逐项到文件:行号)。
3. [`adr/0014`](./adr/0014-an-agent-owns-its-session-and-world.md) → [`0015`](./adr/0015-utility-model-seam-and-session-titles.md)
   → [`0016`](./adr/0016-agent-team-inbox-wake-and-peer-messages.md) —— 本次落地的三块。
4. `git log e1cfa439~1..HEAD` —— 每个 commit 的正文写了为什么。

## 已定的决策(不要重议)

| 决策 | 在哪 |
|---|---|
| `atomcode-tui` 替换 tuix,不看 tuix、不拿它当标尺 | 0012 |
| 四层:Agent(机制 + 能力行)/ Product / Host / UI;Host 唯一认识全部四层 | 0013 |
| 进程外 SDK 的 wire 用 ACP;SDK 面只定义一次 = 句柄协议(8 命令 / 25 事件) | 0013 §SDK |
| 不引入 session 实体:agent 拥有会话,session id 就是 agent 身份 | 0014 |
| header 不是事件(JSONL 第一行);标题是事件 `Titled` | 0014 补 |
| `llm-utility` 旁路模型缝;消费者不回退到 `llm` | 0015 |
| 默认不开模型起名;patch 两行启用 | 0015 |
| 不做「回复摘要给人看」 | 0015 |
| 同伴消息是双方日志里的事实;成员只认 lead(靠构造) | 0016 |
| 角色是 markdown 数据;写隔离走 git worktree;不按型号名选模型 | 0016 补 |
| fs 世界默认不围栏,给 root 才围;敏感路径由 `sensitive-paths` 行经审批缝把关 | commit 06ec153c / 361b3e71 |
| tui 控制面走 `AgentHandle`,渲染订阅日志;不翻协议客户端,直到有 daemon 背后的 tui 需求 | 0013 §6 |

## 现在的形状(改了什么,在哪)

**Agent 机制**(`crates/atomcode-harness/src/`)
- `agent.rs`:`Agents::create(ctx, CreateAgent)`(id / cwd / parent / seed+seed_len / resume /
  persist / setup),发布前装配完;`current()` / `scoped()` / `as_agent()` 任务局部「当前 agent」;
  `OnlySession` 给单 agent 树的测试与回合外自述用;`MessageOrigin::Peer`。
- `session.rs`:`SessionHeader`、`SessionLog::header()/title()`、`SessionEvent::Titled`、
  `InjectionOrigin::Peer { from }`(渲染成「another agent's report, not the user」)。
- `events.rs`:`InboxInserted`。`seams.rs`:`SessionDefaultsSvc`、`LlmUtilitySvc`、`SessionSummary`、
  `SessionPersistence::{begin, header, describe}`。
- `plugins/handle.rs`:`wire()` + `spawn(ctx, wire, answers, CreateAgent)` + `Answers` trait,
  泵有三个唤醒源(命令 / 回合结束 / inbox),注册监听后先看 inbox。
- `plugins/agent_loop.rs`:`drive` 用 `as_agent` 包住;`keep_driven(agent)`;ToolBatch 的
  working_dir 跟随 `agent.cwd()`。
- `plugins/session.rs`:`session` 行只给 defaults;持久化按 `committed.session` 落盘,
  `persist=false` 不落,header 在 `AgentCreated` 同步写。
- `plugins/session_title.rs`(新):first-prompt / model 两个 namer + `session-title-on-first-prompt`。
- `plugins/team.rs`(新):`team` 工具、角色加载(`.atomcode/agents/*.md`)、`tell_parent`、
  沉默代报、worktree。`plugins/llm.rs`:`llm-utility-openai-compat` / `llm-utility-replay`。
- `plugins/policy.rs`:`sensitive-paths` 行(`AsRisky` 视图)。`plugins/world.rs`:无 root 不围。
- plexus:审计只把根 realm 的 provide 算作行的声明面(`owned_by_in`)。

**UI**(`crates/atomcode-tui/`)
- `plugin.rs`:`tui-agent-client` 行持有命令通道与 agent;自己的驱动删了。
- `plugin.rs` 的事实监听器按驱动中 agent 的 session id 过滤(940588e6)。它坐在根 realm,
  realm 听得到子孙,不过滤就是把成员的日志折进 lead 的流——两段对话按 (turn,round) 交错。
- `modules/team.rs` + `tui-panel-team` 行(默认关):成员面板。两个来源——委派/角色/报告
  折自 lead 自己的日志,「此刻在不在跑、第几轮」来自 `Moment.members`(宿主每帧从 agent
  注册表读)。成员自己的流不在屏幕上,这是 0016 的规矩。
- 审批:`seams::Question`(prompt / options / asker / about)取代了一句话 + 两个选项;
  `approval-interactive` 说出是哪个成员在问、记住 `allow_always` 的授权范围
  (`{tool}::{scope}`,来自 `Tool::always_grant_scope`)。屏幕这边 `tui-ask-view` 是缝,
  `tui-ask-card` 行是默认实现(弹框:谁在问、调用做什么、三个答案各覆盖什么);
  拆掉这行,问题退回流脚下的几行纯文本,照样能问能答。

**09-13 夜(两个会话并行)**

Agent 侧(`atomcode-harness`):
- `plugins/ask.rs` / `tool-ask` 行:`ask_user` 让模型能把选择题交给人,走的是审批那条
  `user-questions` 缝。以前问不出来是结构性的——工具没挂,而且 `exec.rs:58` 把
  `requester` 写死成 `None`。每个问题 2..5 个选项;没人答 ≠ 同意,工具结果留一步可走。
- `plugins/todo_reminder.rs` / `todo-reminder` 行:清单里还有活而最近 N 步没人碰,就
  发一条 `<system-reminder>`。折叠用工具自己的 `reduce_todos`,所以工具/面板/提醒
  三方不会有三种说法。
- `persona.rs` 补两条:动手前一句话说要干什么;用人的语言回复**并且思考**(推理画在
  屏幕上,一半是外语就是一半要先翻译的对话)。

UI 侧(`atomcode-tui`,另一个会话):
- `text.rs`:外来文本一进来就消毒(转义序列/CR/TAB)。根因链值得记住——画错的行
  → 重画 diff 跳过"字节没变"的行 → 那行永远不重画 → 只有 ctrl-l 或 resize 能救。
- `modules/todo.rs` / `tui-panel-todo`、`modules/live.rs` / `tui-panel-live`(回合在做
  什么、多久、花了多少)、截图粘贴(`attach.rs` + `Surface::clipboard_image`)、
  回合结尾横线上的花销、`theme.rs` 的 muted 从"猜一个槽位"改成"按终端自报的两端量"。
- `el.rs` 的 `wanted()` 去掉了 `.max(1)`:`Height::Hug(0)` 现在真能隐身,没内容的面板
  不再占一行。

闸门:
- `gates/tui-string-slice.sh`:`clippy::string_slice` 提 deny,每处切片必须写明为什么
  安全。起因是 `subject_of` 按字节切长路径,中文命令把 TUI 打死了四次。
- `gates/tui-layers.sh` 改成只读代码(剥整行注释与文件末尾的 `mod tests`),基线
  43 → 21——一半"存量债"是文档。阴性对照补了"注释里的装饰符不算违规"。
- `conformance::facts()` 带上敌意输入:够长、含 `/`、结尾一整段三字节汉字,于是
  "留最后 N 字节"对任何不是 3 的倍数的 N 都落在字符中间。`a_hostile_argument_is_in_the_corpus`
  守着这条输入不被改软。

**09-13 深夜:单帧成本(UI)与摘要行(Agent)**

UI 侧(`atomcode-tui`),按"什么在变贵"排的:
- `ansi::Lines`(3110386c):diff 挪到 encode **之前**。上一帧每行的 payload 留着,逐行比对,
  没变的行直接沿用——一次 encode 都不做。原来的 `patch_from` 省的是 write,而贵的是
  encode(`write_line` 每 span 都要 escape/clip/分配,与帧内容无关的 ~0.29ms/帧常数)。
  范围:行变不变按**屏幕行**算,而 stream 底对齐,追加一行会让整屏上移、仍全编码;省掉的
  是"行数不变"那些帧(空闲 spinner、输入框打字)。`ROWS_ENCODED` 计数器直接证明跳过的行
  确实没被编码(等值输出证明不了),读它的四条判据共用一把锁(0116b28e)。
- `Slot::Live` 的 `LiveCache` + `markdown::render_settled`(4ceb1d1d):流式块原来每帧整篇重渲。
  现在 `render_settled` 报出**定稿边界**(在行边界上、且不在围栏内),`rows_at` 只在"宽度
  相同且新文本 `starts_with` 上一帧源"时复用整段已渲行。围栏是唯一会回溯的东西,所以边界
  在开口围栏前停住。`Content::growing_text` 是可选声明——只有 `ModelSaid` 实现它。
- `modules/todo.rs` 折一次 + `text::for_screen` 回 `Cow`(e983d984):`render` 与 `height`
  原来各折一遍 `reduce_todos`(扫全部历史调用),现在在 `calls` 真变的那几处折一次存
  `State.items`;`for_screen` 没有控制字符就 `Borrowed`(按字节扫,并按对匹配 C1)。
- 残余:`rows_at` 的契约**没变**,仍是 `(总行数, Option<Vec<Line>>)`——`stream_height` 与
  滚动边界依赖它,所以每帧仍把整屏 `Vec<Line>` 物化一份。见「下一步」第 4 条。

Agent 侧(`atomcode-harness`):
- `plugins/compaction.rs` / `compaction-summary` 行(4defaa25):同一个 compaction 缝的另一半
  ——折**同一个区间**,边界抽成 `settled_span` 两个策略共用;摘要走 `llm-utility`;任何模型
  帮不上忙的路径(没挂 utility / 报错 / 超时 / 答空)都落回模型无关的清单,回退文案与
  `compaction-tail` 逐字相同——provider 挂了降级的是摘要的质量,不是压缩这件事。触发器
  `mount_compaction_trigger` 也抽出来两行共用(否则换进来的策略永远不触发)。一行一个
  provider,所以这是 patch 里 `[[remove]] compaction-tail` + `[[insert]] compaction-summary`
  的**替换**;BASE 默认仍是模型无关那行。

**09-14:思考开关/强度、模型来源收口**(本会话)

Agent 侧(`atomcode-harness`):
- `plugins/llm.rs`:三条填模型的行都多两个原样透传的字段 `thinking_type` /
  `thinking_keep`(Kimi 系 `thinking` 对象的两个键,默认 `None` = 整个对象不发,因为没见过
  这个键的网关会 400),以及 `supports_reasoning_effort`(端点是否吃顶层
  `reasoning_effort`)。**默认 `true`**,`llm-atomcode-config` 行用
  `endpoint_supports_reasoning_effort(resolved.reasoning_effort, reasoning_effort_levels)`
  推——和 CLI/tuix/daemon 同一个函数。默认 `false` 的失败形态是「配了却静默不发」,
  那正是这轮一直在修的坑。
- `model_source.rs`(新):**模型来源的唯一读取点**。`Want`(行声明它要什么:
  `Explicit` / `UserConfig` / `Environment` / `EnvironmentWithModel`)→ `ModelEndpoint`
  (统一形状,含 `origin` 说明这个数从哪来)。`ConfigAndEnv` 是那个实现:行的字段 >
  `~/.atomcode/config.toml`(`Config::load` + `resolve_model`)> `ATOMCODE_*`。三条 `llm*`
  行、`persona.rs`、`home()`、`capabilities.rs` 的 `HOME` 全部改经它;`env::var` 与
  `Config::load` 在别处清零。见「下一步」第 10 条——**它层位不对**(读配置按 ADR 0013
  是 Host 的事),要和 `launch.rs`/`ui*.rs` 一起搬。
- `plugins/reasoning_effort.rs`(新)+ `reasoning-effort` 行(BASE 默认挂,`config = {}`):
  会话的思考强度,`ChatOptions::reasoning_effort` 的每请求注入,走 `agent/request`
  waterfall。**独立成行而不是 `llm` 行的字段**,因为 patch 是整体替换——档位挂在 `llm`
  上会被每一次 `--model` / `/model` 抹掉。没写档位就一个 listener 都不挂。
- `plugins/team.rs`:角色 frontmatter 多一个 `effort` 键(可选,五个内建角色都带:
  explorer/docs_writer=low,reviewer/implementer/tester=max),成员 realm 里
  `on_waterfall(prepend)` 写进它每个请求——所以在成员自己的 realm 上,不跨成员,
  而会话级那行仍给没声明 `effort` 的角色兜底。非法值在**挂载时**报错(mount 就失败,
  不是等第一个回合),和 `permission`/`difficulty` 一致。
- `launch.rs`:`--effort <low|medium|high|xhigh|max>`,patch 的是 `reasoning-effort` 那行。
  `--model` 恢复原样(解析时 push overlay)——两 flag 合成一条 patch 的做法随档位换行
  一起撤了。
- `seams.rs` / `control.rs`:`Control` 多一个 `row_config(id)`(读一行 config 的 JSON),
  `/effort` 无参时要显示当前值。原打算的 `amend_row`(合并写一行)撤了:现在根本不需要。
- `lib.rs`:`REASONING_EFFORT_LEVELS` 转出(档位只有一个定义处)+ `REASONING_EFFORT_ROW`
  常量(行 id 在 bundle/CLI/TUI 三处必须一致,拼错就是静默打空)。

UI 侧(`atomcode-tui`):
- `commands.rs` / `tui-commands-tree` 多一个 `/effort`:无参显示当前档位,有参 patch
  `reasoning-effort` 行。判据在 `tests/e2e.rs`(`the_effort_command_moves_the_row_while_the_screen_runs`)
  ——断言的是**行真的变了**,不是屏幕说了什么。
- 顺带发现 `control` 缝由 `hand_over` 给每个前端填(`launch.rs:371`),所以 TUI 的
  `/rows`、`/patch`、`/effort` 一直都能用。

判据:
- `harness/tests/reasoning_effort.rs`(新):插件的 7 条(未配置=什么都不说、只有档位
  ≠ 开关、每个档位都到得了、非法值不生效)+ `switching_the_model_keeps_the_level`
  (**这条是本设计存在的理由**:按 `--model` 的方式整体替换 `llm` 行,档位必须还在)
  + 两条守卫。
- 两条守卫读源码,不是为了测行为,是为了**防退化**:
  `only_one_module_reads_the_environment` / `only_one_module_loads_the_model_config`,
  禁止 `env::var` / `Config::load` 出现在 `model_source.rs` 之外。判据刻意收窄:
  `current_dir()` / `temp_dir()` / `args()` 是进程事实,不是配置来源,不算违规(第一版
  判宽了,把 9 处无害的 `current_dir()` 也算成违规)。
- `harness/tests/team.rs` 三条:角色的档位真的到请求上(两个角色一起委派,`low` 和
  `max` 都出现)、没声明 `effort` 的角色不被代替说话、非法值在 mount 时被拒。

踩过的坑(这轮教训):
- **`reasoning_effort` 的档位和「关思考」是两回事**,不是一条梯子。DeepSeek 官方
  (chat/completions 文档)写:`thinking.type` 控制开关,`reasoning_effort` 的
  `low/high/max` 是 **enable** thinking 的三档;`medium`/`xhigh` 被接受并映射到 `high`。
  所以「给简单任务低强度」拿到的是「思考开着但便宜」,要真便宜只能关。
- **推理会吃光小预算,而且是静默回退**。起名的默认 `max_tokens = 32`,
  `deepseek-flash` 一个 "say hi" 就烧 22–25 个 reasoning token,于是 32 个 token 在
  推理中途被砍断、一个 `TextDelta` 都出不来、`ask()` 返回 `None`、**静默**回退成
  「第一条提问原样」。同样的成因让压缩摘要在默认 1024 下也落回模型无关清单。
  成因是推理占预算,不是预算小——所以解在 `thinking_type = "disabled"`,不是抬预算。
- **`thinking` 和 `reasoning_effort` 同发时,`disabled` 赢**(实测:`disabled` + `max`
  依然 `reasoning_content` 不出现、无 reasoning tokens),所以两档可以共存,Simple
  角色(走已关思考的 utility provider)不用特殊处理。
- 判据容易搞错的一处:压缩摘要**两条路径都以 `=== EARLIER IN THIS SESSION ===`
  开头**(`compaction.rs:108-114` 的 match 两支都写它),区分回退与模型写的标记是
  **结尾的 footer**「Ask again for any detail…」。拿 header 判会得出相反的结论——
  我第一次就判错了,据此写了一版错误注释。
- 一度把 `model-source` 做成服务并给三行加 `inject`,**全部入口死锁**:
  `Deadlock { pending: [("llm", ["model-source"]), ("agent-loop", ["llm"])] }`。根因是
  时机——`inject` 等的是服务,而 `hand_over` 在 `app.start()` **之后**才 provide。
  已撤,理由写在「下一步」第 10 条。

## 怎么验证

```sh
cargo test -p atomcode-harness -p atomcode-tui --no-fail-fast   # 全绿
cargo clippy --no-deps -p atomcode-harness -p atomcode-plexus --all-targets -- -D warnings
bash gates/tui-layers.sh && bash gates/tui-layers.spec.sh && bash gates/tui-negative.sh
bash gates/tui-string-slice.sh && bash gates/tui-test-count.sh   # tui 单包判据 352
git status --short gates/     # 差分基线 gates/differential.baseline 只准降,不准动
```

`gates/tui-test-count.baseline` 是 352(tui 单包;harness 不计在内),只升不降。

`gates/tui.sh` 的 clippy 步骤会红:tui 的 host.rs / input.rs / theme.rs 三条,kernel 与
telemetry 各几条,都是 rust 1.94 新 lint 的既有问题,不在本次范围。

## 未决(要人拍板)

1. undo 的会话模型:日志上加事件,还是 sidecar。
2. 配置树改写方法(`harness/patch` 等)是否进公开 SDK。当前进程内开放、进程外不开。
3. 生产 profile 要不要默认开模型起名。
4. steering vs 定时:定时消息在回合中到达会折进当前回合,「每小时跑一遍」需要「等回合结束」语义。
5. 成员到成员直接通话不开;ACP 与 tui 里成员输出怎么呈现。
6. capabilities 那三个审批 gate 怎么接(下一步第 5 条):重写成 harness 的 `ToolsExecute`
   监听,还是给 gate 加一个不依赖 kernel `RequestCtx` 的端口。两条都动公开形状,不替人定。

## 下一步(建议顺序)

1. **真模型验证 team**:挂 Claude 和 GLM 各跑一遍 delegate → 收报告 → tell 的完整流程。
   只在 replay 上测过。
2. **`ui-acp` 行**:移植 `crates/atomcode-cli/src/acp/`(7.7k 行)到句柄协议之上;多会话已就绪
   (`CreateAgent` + `by_session`),差 fs/terminal 客户端世界行、RejectAlways、config option 白名单。
3. **`/compact <focus>` 的 focus 进策略签名**:`Compaction::compact(&self, log)`
   (`seams.rs:302`)现在还只吃 log,人给的侧重点到不了策略。摘要行本身已完成
   (`compaction-summary`,见上),这条只剩这个签名。
4. **`rows_at` 返回可见窗口**:live 块增量渲染(4ceb1d1d)省掉的是整篇重解析,每帧那次
   `Vec<Line>` 物化还在。改契约会波及 `stream_height` 与滚动边界算法,是独立一步,也是
   tui 单帧成本这条线上最后一项。
5. **capabilities 的审批 gate 怎么接——要人先拍板**:`ToolMiddleware` 是 **kernel** 的
   trait(`crates/atomcode-kernel/src/middleware.rs:35`),`CredentialBashGate` /
   `WriteApprovalGate` / `BashWorkspaceGate` 实现的正是它(`credential_bash_gate.rs:516` /
   `write_approval.rs:302` / `bash_workspace_gate.rs:791`),`before` 收 `&RequestCtx`
   (`credential_bash_gate.rs:487`)。而 harness 的回合循环是 `agent-loop` 行,它自己的闸门
   是另一套 `Waterfall<ToolsExecute>`(`policy.rs:138` 的 `ApprovalGate`、`:330` 的
   `SensitivePathGate`;`policy_rows.rs:84` 的 `PermissionGate`、`:157` 的 `PlanModeGate`),
   全仓对 `ToolMiddleware` 零引用。所以不是「原样挂上」:要么把三个 gate 重写成
   `ToolsExecute` 监听,要么给 gate 加一个不依赖 `RequestCtx` 的端口。见「未决」第 6 条。
6. **定时与 loop 行**:往 inbox 放 `Harness` 消息即可,机制已备;coding 的 `controllers.rs` 是策略参考。
7. team 剩余:lead 取消级联、`report_finding` 进成员工具集、fork 切片器。
   (成员面板已落地:`tui-panel-team`;审批问题会说是哪个成员在问,见 `tui-ask-card`。)
8. 截图粘贴欠三条(09-13 夜复核 tuix 后列的):剪贴板只有"原始字节"这一级——macOS 上
   Finder ⌘C 与 iTerm2 只给 `public.file-url`,Windows 的 Qt 系截图工具给的 CF_DIBV5
   `arboard` 会拒;发送顺序是"贴入顺序"而不是 marker 在文中的顺序;没有尺寸上限。
   tuix 的 `try_paste_clipboard_image` 是三级回退,每级都写着是哪条真实报告逼出来的。
9. 会话剩余:`delete`、OS 租约、`Titled` 的 `/title` 之外的提交点。
10. **把 `model_source` 搬去宿主 crate(与 `launch.rs`/`ui*.rs` 同一趟)。** 新增的
    `harness/src/model_source.rs`(与本文同批改动,尚未提交)是模型来源的唯一读取点——
    `Config::load` + `resolve_model` 与 `ATOMCODE_*` 的读法都收在这里,三行 `llm*` 只声明
    「我要哪个来源」(`Want::Explicit` / `UserConfig` / `Environment` /
    `EnvironmentWithModel`)。
    但**读配置按 ADR 0013 是 Host 的事**(`docs/adr/0013-*.md:41`),而这个文件现在住在
    Agent 层(harness),和 `architecture-target.md:58` 点名要移出的 `launch.rs`、`ui*.rs`
    同类。搬迁时一起走,别单开一趟。

    **不要再把它做成缝**(试过,已撤):做成 `ModelSourceSvc` + 行 `inject` 会死锁——
    `inject` 让行**等待**服务,而 `hand_over` 是在 `app.start()` **之后**才 provide 的
    (`launch.rs:371`),于是任何自己挂树的入口都挂不上:

    ```
    must mount: Deadlock { pending: [("llm", ["model-source"]), ("agent-loop", ["llm"])] }
    ```

    照 `seam_map::HOST_PROVIDED` 也得不出「它该是代码在宿主层」:`control` 在里面是因为它是
    `hand_over` 填的;而同样被 ADR 称作「Host 填的缝」的 `session-persistence`,实际是
    **bundle 里的一行**填的(`session-persistence-jsonl`),不在那份名单里。ADR 那句话说的是
    「宿主的产品选择决定哪一行填它」,不是「代码位置在宿主层」。

    守卫已经在:`harness/tests/reasoning_effort.rs` 的 `only_one_module_reads_the_environment`
    与 `only_one_module_loads_the_model_config` 读源码,禁止 `env::var` / `Config::load`
    出现在 `model_source.rs` 之外。搬迁后这两条测试要跟着换目录。
11. **行拿到的 config 应当是终态**(原则,尚未做到)。「各种形式的配置(文件、环境、默认)
    应当在同一个地方合成一次,进到 `apply(ctx, config)` 里的就只剩一份真源」——行不该
    再去够任何东西。今天没做到,两个具体缺口:

    **(a) 配置树没有变量展开,所以 bundle 写不出「这台机器的 home」。** base bundle 的
    `skills` 行是 `[[insert]] name = "skills"`,**没有 config**,于是 `row.home` 恒为 `None`,
    `plugins/capabilities.rs:87` 的 `.or_else(|| user_home())` 是**常态路径**而不是兜底。
    `$ATOMCODE_HOME` 这个事实在仓库里被读了**三次**,其中
    `capabilities/skills/registry.rs:264` 在 **L1**(按 ADR 0013 不该读进程环境),
    `skills/render.rs:98` 的注释自己承认这是个靠人记的约定(「the two must stay in step」)。

    统一点是**「层变成树」的那一处**——`ConfigTree::from_layers`(`plexus/src/loader.rs:166`)
    或其上的 `Profiles::resolve`(`harness/src/profile.rs:134`)。在那儿做一次
    `${VAR}` 展开,bundle 才写得出 `home = "${HOME}"`,行才可能只读 config。
    不是小改:展开点要么进 plexus(通用底座),要么进 profile;而**约 20 个测试文件手搓
    配置树**(直接 `ConfigTree::from_layers`),绕开 `Profiles`,展开点选错就会出现
    「测试里的 `${HOME}` 是字面量」。

    **(b) 密钥是例外,而且要说明白为什么。** `api_key` 不能落进配置树——树会被
    `--dump-config` 打印、会被写进日志。所以密钥这条只能是「config 里放**变量名**,
    由一个解析器读」,这正是 `model_source::DEFAULT_API_KEY_ENV` 在做的事。也就是说
    「唯一真源」对**非密钥的机器派生值**是「宿主合成进行 config」,对**密钥**是
    「名字进 config,读的人只有一个」。两条都满足原则的实质(只有一个真源),形式不同。

## 踩过的坑

- 树级监听器用自己的 ctx 找日志会永远找到根:必须 `crate::agent::scoped(&self.ctx)`。
- 泵在 spawn 里注册监听器,消息可能先到:注册后先看 inbox。
- 旁路模型调用不能借主 `llm`:会吃掉 replay 脚本的下一行,而且并行触发时吃哪行不确定。
- 测试断言别假设报告在回合外到达:steering 会把它折进当前回合,用 `heard_by` 那种两态断言。
- patch 的目标 id 是行名(`session-title-first-prompt`),不是缝名(`session-title`)。
- `cargo test --keep-going` 不存在;`cargo build --tests --keep-going` 可以。
- 事件可见性:监听器只看到自己 realm 及子孙 realm 的 emit;根发的事件子 realm 听不到。
- 用 python 脚本批量改文件时,先打印原文再写模式:注释与空行会让模式失配,脚本半途失败会留下半写的文件。
- 回合**中途**不能用 `inbox().inject`:`claim()` 明确规定"没有消息时注入留在原地——
  注入是上下文,上下文不该要求开一个回合",于是中途的注入永远不被认领。要像重复熔断
  那样直接 `session::commit` 成事实(`loop_policy.rs:618`)。
- 在锁里 `commit` 会死锁:commit 发 `SessionEventCommitted`,重入你自己的监听器,而
  `std::sync::Mutex` 不可重入。锁内决定、放锁再说。
- 一条缝只能有一个提供者:把 `user-questions-unattended` 默认挂上,`ui-handle` /
  REPL / 全屏 UI 三种树全都起不来,22 条差分判据当场红。
- 闸门 grep 整个文件就会数到散文:`tui-layers` 因为两条引用界面文案的注释红了,而
  仓库自己写过"首跑即红的闸门会被关掉"。规则说代码,闸门就该只读代码。
- 按字节切 `&str` 是这个 crate 的头号杀手:`&text[text.len() - 44..]` 在中文命令上
  panic,而它跑在渲染里——整个进程当场消失,消息进了 `$TMPDIR/atomcode-tui-<pid>.stderr.log`
  那个没人看的文件。现在有 `gates/tui-string-slice.sh` 和 `surface.rs` 的 panic 钩子
  (panic 时先还屏幕和 stderr,再链到原钩子)。
