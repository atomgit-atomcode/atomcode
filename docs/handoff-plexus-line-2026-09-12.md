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
