# 除 `/bg` 之外，对着 tuix 还差什么：排期

> 2026-09-23 · 基线 `02b73ada3`(release/v5.2.0) · 分支 `feat/tuix-parity`

承接 [`2026-09-19-remaining-gaps.md`](2026-09-19-remaining-gaps.md) 与
[`handoff-tui-default-2026-09-23.md`](../handoff-tui-default-2026-09-23.md)。

那两份说「剩下唯一没做的功能是 `/bg`」——**在"功能整块"这个粒度上属实**。这份补的是
另一个粒度：五路独立测绘（命令逐名、面板逐键、输入键位、生命周期外围、渲染展示）
把两边的**实现深度**逐条对了一遍，结论是整块缺口还有 4 件、深度缺口约 25 条，其中
4 条是**真回退**（旧前端有、新前端做浅了，且会咬人）。

**`/bg` 按用户 2026-09-23 的决定不在本计划内。**

测绘方法与前几轮一致：不按命令名对上就勾，打开两边实现读。**五处推翻了测绘自己的
判断**，全部记在第零节——那种错法会让人去重建一个已经有的东西。

---

## 零、先说不做的（照着做第二遍最贵）

### 0.1 测绘一度报成缺口，核过是**有的**——别再做一遍

| 报的 | 实际在哪 |
|---|---|
| 自定义 `.md` 命令没了（tuix `custom_commands.rs` 598 行） | 换了 owner：`capabilities/src/skills/registry.rs:276-284` 扫 `.claude/commands` 与 `.atomcode/commands`，`user_invocable` 默认 true(`skills/skill.rs:231`)，`$ARGUMENTS`/`$N` 模板在 `skills/skill.rs:137-155` |
| `/worktree` 零实现 | `coding/src/host_rows.rs:1968-2090`，`create/list/done/cleanup` 四个子命令齐全（测绘只 grep 了 tui/cli 两个 crate） |
| 打字排队（tuix `message_queue`）没了 | 归 kernel inbox：`tui/src/plugin.rs:3708-3729` 直接 `client.send`，折进当前回合还是开新回合由 pump 决定 |
| askpass 没接（09-18 清单的 P0-1） | 已接：`tui/src/plugin.rs:625-669`，启动于 `:994` |
| `/build` 映射错了（`"ask"` 而不是 `"edits"`） | 没错：`RuntimeMode::Build` 就是默认的 ask 档(`coding/src/runtime.rs:400-408`)，Build 与 Ask 是同一个模式的两个名字 |

### 0.2 新前端**更深**的，不要"对齐回去"

代码块语法色（tuix 的 syntect 早已整体移除，`tuix/src/highlight/mod.rs:1-16` 自述）、
Markdown 引用块与 `[text](url)` 链接、OSC 8 可点击超链接、真彩色 + 对比度合成配色、
逐条折叠推理/工具块（tuix 是全局 Ctrl+O 一起开关）、`/toolbox`、`/cd` 书签、
providers 面板的账号分组、settings 的四页签。

### 0.3 已判定不做（继承，附本轮复核）

`/upgrade`（与 CLI 重复）、`/guide`（13 条写死菜单，`/help` 已覆盖）、
`/think budget`（v2 下是死字段）、`/view` 搜索 + 语法色（用户 2026-09-23 判定）、
provider 测试连接（**两边都没有**，不是缺口）。

> **跨会话输入历史从这份名单里拿掉了**（2026-09-23，用户）。它此前被记成"与 0024
> 拧着所以不做"，**那个理由不成立**，见决策 3。

---

## 一、要你拍的三条

这三条不拍，对应的条目没法开工。

### 决策 1：`/diff` 到底在跟谁比

两边**语义已经不同**，不是深浅问题：

| | tuix | 新前端 |
|---|---|---|
| 比的两端 | `git diff HEAD`，整个工作区的未提交改动，与会话无关(`tuix/src/git_diff.rs`) | 「本会话第一句提示词之前的快照」vs 现在(`capabilities/src/session/snapshot.rs:285-312`) |
| 没快照的会话 | 照常显示 | 直接拒绝 |
| 契约带的字段 | 6 种文件状态 + staged/unstaged 分段 | `path/added/removed/binary` 四个(`host-api/src/lib.rs:613-621`) |

**已拍（2026-09-23，用户）：两个都要**。两个问题都真实——「这个 agent 动了什么」
和「我这棵树现在脏成什么样」。**现状（会话快照）留作默认**，因为它答的是前者，
而那是编码会话里更高频的那个；`/diff git` 给后者。

落到 P2-7：`HostCommand::Changes` 加一个作用域参数（**不是**第二条命令——0021 的
理由：一个问题问两个深度已经是它的形状了，再加一个维度仍是同一个问题），
`ChangedFile` 补文件状态；「没有工作区快照」这个拒绝理由只属于默认那一档，
`git` 那一档无条件可答。

### 决策 2：内联 `<think>` 剥离放哪一层 —— **已拍：provider adapter**（2026-09-23，用户）

自部署 GLM/Qwen 经没配 reasoning parser 的 vllm/sglang 出来时，`<think>…</think>` 会
**原样出现在正文里**。tuix 有 `think.rs`(418 行) 在前端剥；新栈全仓零对应
（kernel `agent/engine.rs:3210` 那处是"网关把答案错投进 reasoning 通道"的补救，是另一件事）。

**定在 adapter，不进 kernel、更不进屏幕**：它是这一路 provider 的输出形状问题，
不是内核语义，也不是"这块屏幕怎么画"——放屏幕里做一份，ACP / headless / daemon /
webui 四个入口还是裸的。

落点已勘好，见 P1-3。

### 决策 3：`Ctrl+R` 与跨会话历史 —— **已拍：两个一起做**（2026-09-23，用户）

原本的问法是「范围只剩本会话，还值不值得做」。用户选了**连跨会话历史一起重新
考虑**，而重新查过之后，挡住它的那个理由**不成立**：

> **0024 禁的不是「跨会话历史」，是 tuix 实现它的那个方式。**
>
> 0024 说的是**一个会话的对话**以它的事件日志为唯一权威，不许再有一份会漂移的
> 拷贝（快照、transcript、core 磁盘投影都是这么退役的）。
> tuix 的 `history-v2/project/entries.jsonl`（704 行，带文件锁、原子写、图片引用 GC）
> 正是那种拷贝：提交时另外追加一份，**撤销和 rewind 够不到它**——你撤掉一个回合，
> 那句话还留在历史文件里。
>
> 但「从每个会话的日志里把 `UserMessage` 折出来」是**读**，不是第二份存储。
> 这恰恰是 0024 要的形状。`Host::fold`(`tui/src/host.rs:1798-1804`) 已经在对
> **当前**会话这么做了；跨会话就是同一个折叠作用在更多份日志上。
> `/worklog` 也早就在扫会话日志算日报（`coding/src/host_rows.rs` 的
> `worklog_prompt_at(arg, today, sessions_root, english)`）——**同一种读法已有先例**。
>
> 2026-09-20 的 G3 把两条路都列对了（"要么去读别的会话的日志，要么另开一份文件"），
> 只是当时判"今天没人要"就没往下走，顺手把整件事记成了和 0024 冲突。
> **只有第二条路冲突。**

落到 P2-8，取代原来那条只谈键位的。

---

## 二、分批

规矩不变（0019）：**先写判据、摘掉被测代码证伪一次**，反证写进 commit；
`gates/tui-test-count.baseline` 随判据上抬。

### P0 — 先修红的和回退的（互不依赖，可并行）

> 判据基线上已经有红的时候，新判据是加在一片红上的。这一批最先做。

- [x] **P0-0 两条基线红判据**（2026-09-23）。复现属实,两条**都是判据自己的问题,
      产品没有坏** —— 而两条错法不同,都值得记:
      - `an_oversized_tool_result_…`:判据写着「16 KiB 是门槛,40000 字节越得过去」,
        而 `69fdc7cae` 把预算放宽到 50 KiB(理由正当:模型在普通 diff 上反复撞
        「输出被截断」)。**行没坏、放宽也没错,是判据在拿一个已经不存在的数字量**,
        于是它红着说功能坏了。改成**从常量本身取尺寸**(`THRESHOLD_BYTES + 8 KiB`),
        以后预算改成多少都越得过去。
      - `a_stopped_reply_is_kept_as_far_as_it_got`:它比的是**整份请求**,而
        `StatusReminderHook` 每一轮都在请求**末尾**追加一条带日期的
        `<system-reminder>`(它的模块说明写了为什么在末尾:日期放进系统前缀会
        每天把整个缓存前缀顶掉)。**末尾的注入永远不可能是后一次请求的前缀** ——
        这里它正好占着 `answer 3` 后来的位置,于是读起来像「resume 把回复弄丢了」。
        改成两侧都先把注入滤掉(`reminder::is_system_reminder`,现成的),比的才是
        对话本身。顺带加了一条**非空断言**:滤器要是把什么都滤掉,两个空表比下来
        照样绿。
      两条各证伪:关掉截断 → 第一条红(而它的阴性对照
      `a_small_tool_result_is_left_alone` 仍绿);只给 resume 那一侧多插一条 →
      第二条红,报的正是「a resume changed the conversation」。
      `cargo nextest run -p atomcode-coding`:859 全过。
      原文:交接文档记的，**本轮未复跑**，第一步是复现：
      `coding/tests/mechanism_rows.rs::an_oversized_tool_result_is_shown_head_and_tail_and_saved_whole`
      （疑似 09-21 放宽工具输出预算后，判据的输入不再算超长）、
      `coding/tests/runtime_criteria.rs::a_stopped_reply_is_kept_as_far_as_it_got`
      （中断回复 resume 后不一致）。红判据放着不管，最后训练出来的是所有人无视闸门。

- [x] **P0-1 `/view` 的三道防护 + `~` 展开**（2026-09-23,`491099567`）。
      照写的做了,另加两件当时没想到的:
      - **二进制按名字拒掉**,判定用内嵌 NUL 而不是「UTF-8 不合法」——字节上限会
        切在字符中间,而 latin-1 的 README 用替换字符读起来没问题,那两种都不该拒;
      - **路径解析与读取各提成一个函数**(`view_path` / `view_file_within`),
        三道上限于是能拿几个字节的文件去判,`~` 的展开也不必让判据去拥有这台机器
        的 `HOME`。第一版判据只钉了 `expand_home_with` 的语义、没钉接线,**破坏
        接线它照样绿**——提函数是为了把那一条也钉上。
      截断的话写在标题里而不是最后一行:需要知道的正是那个永远读不到末尾的人。
      四处破坏各判红一次,一一对应;1059 全过(基线 1054→1059)。
      原文:（`tui/src/commands.rs:314-336`）。
      今天是裸 `std::fs::read_to_string`：**没有行数、列宽、字节上限**，一个 500MB 的
      日志就整个读进内存；`~/x.md` 因为 `is_absolute()` 为假被拼到项目根下，必然读不到。
      对面三道上限写在 `tuix/src/modals/file_viewer.rs:39-49`（1000 行 / 2000 字符 / 8MB），
      `~` 展开在 `tuix/src/event_loop/commands.rs:5432-5449`。
      判据：超长文件被截断且**说出来被截了**（悄悄只给三分之二是一种沉默的谎）；
      `~/…` 能打开；二进制文件给一句话而不是一屏乱码。

- [x] **P0-2 文本字段编辑退化**（2026-09-23）。**只修了 `/config` 那一处,密码框
      不修** —— 清单把它们记成「同一种退化」是错的:密码框那样是**写下来的决定**,
      不是漏做。`modules/input.rs:160-167` 说得很清楚:光标停在末尾,「因为掩码里
      没有东西可供光标走过」;`:226-231` 还刻意拒掉了在掩码里点击定位。理由站得住
      ——看不见自己在哪,移动就是猜——而且这一版另给了 tuix 没有的 `Ctrl+U` 一键
      清空。要改那条决定是另一件事,不在一条「修回退」里顺手做。
      `/config` 那一处没有这种注释,它就是没接:改完之后左右 / Home / End /
      前向 Delete 都有了,而且 `Row::Editing` 本来就收一个 caret,渲染侧只是被写死
      成了末尾。五个光标函数从 `providers.rs` 提进 `text.rs`,三处共用一份。
      **`Delete` 一键两义**要分清:字段开着时它删字符,列表上它是「恢复默认」的
      第一下——单独一条判据钉住,否则编辑一个设置会把另一个设置扔掉。
      三处破坏各判红一次(去掉左移、去掉 Delete、渲染写死回末尾);1062 全过
      (基线 1059→1062)。原文:
      今天：`/config` 编辑态(`tui/src/settings.rs:586-608`)与密码框(`tui/src/secret.rs:158-200`)
      都只有 `Enter` / `Backspace`(pop 末尾) / `Char`(push 末尾)，**没有左右、Home/End、
      前向 Delete**。把 `8080` 改成 `9090` 要连按四次退格。
      对面两处都是全光标编辑（`tuix/src/modals/config_panel.rs:328-393`、`password.rs:60-98`），
      且 tuix 的编辑态还有**首次按键整体替换**（`replace_edit_value_on_input`）。
      **供体就在本 crate 里**：`tui/src/providers.rs:1140-1185` 已经有完整实现
      （`step_caret` / `insert_at` / `backspace_at` / `delete_at` + Home/End），把它抽成
      一个单行编辑器复用即可——三处从此是同一份。
      判据：在中间插入与删除、Home/End 生效；密码框那条**只断言掩码宽度与答案**，
      不让明文进任何可打印的地方（`secret.rs` 的整篇理由）。

- [x] **P0-3 `/logout` 要关掉共享**（2026-09-23）。**收的范围比原计划窄了一档**:
      原文写的是「收掉所有共享」,实际做的是**收掉这个会话的共享**——收配对的手机
      (中继子进程与这一次配对一一对应)+ 从 hub 上摘下来,**不关 webui server**。
      摘掉绑定之后浏览器那边就没有这个会话可看了,而那个 server 可能是人自己起来
      当服务用的、也可能正服务着别的东西;登出不该顺手关掉它。要关有 `/webui stop`。
      判据读源码而不是跑一次:要让 `sharing()` 为真得起 hub、拉中继、走配对,那是在
      判「daemon 挂不挂得上」,不是在判「登出记不记得收」。删掉那一行、以及把它挪到
      删凭据之后,各判红一次。
      原文:（`cli/src/host.rs:1450-1463`）。
      今天只删凭据 + deactivate provider；tuix 登出时先关 App 中继子进程与 app server
      （`tuix/src/event_loop/commands.rs:2617-2658`）。
      **这一条是 09-23 刚做完 `/webui`、`/sync`、`/app`、`/desktop` 之后才成立的**：
      共享做出来了，登出没跟着收口，于是"我登出了"和"手机上还能看这个会话"同时为真。
      **收口路径现成，只是没人调**：`cli/src/tui_share.rs:102` 的 `detach()`（解绑 hub）
      与 `:280` 的 `stop_for_phone()`（收掉中继子进程 + `stop_app_server`）——
      `/app stop`、`/webui stop`、`/sync off` 都在用它们，`SignOut` 一个都没调。
      判据：登出后 `sharing()` 为假、relay 子进程不在了；注意顺序——凭据先删的理由
      （失败时不能留下一个说"还登着"的身份文件）对共享同样成立，收共享应排在删凭据**之前**。

- [x] **P0-4 被信号杀掉时恢复终端**（2026-09-23）。两处与原计划不同:
      - **没有复用 `emergency_restore()`**:它锁 stdout、走 crossterm,信号在本进程
        已经持有 stdout 锁时到达,就会**死在处理器里**——把一次难看的退出变成一次
        挂住。处理器里只用 `write` / `tcgetattr` / `tcsetattr`(POSIX 的安全名单)
        和一个无锁原子,字节按常量原样发。
      - **终端恢复成「能用」而不是「一模一样」**:要还原原样得在进 raw 之前藏一份
        `termios` 再从处理器里读它;把那七个标志位翻回来不需要任何藏起来的状态。
        这条路上进程反正要死了,能用就是全部目标。
      处理器最后**恢复默认处置再重新 raise**:`kill` 过来的必须仍然看起来像被 kill,
      否则对 supervisor 和 `$?` 都是撒谎。
      两条判据:信号名单**在判据里重写一遍**而不是读那个常量(读常量的话,从名单里
      删掉一个信号只会让循环变短、照样绿);以及一条读源码的,钉住 `enter()` 两个
      hook 都装——`Terminal::enter` 要真 tty,跑不了,而处理器自己那条判据是自己
      arm 的,产品里没人 arm 它也照样绿。各证伪一次。1064 全过。`tui`/`cli` 全仓没有 SIGTERM/SIGINT/SIGHUP 处理，
      只有 panic hook 与 Drop（`tui/src/surface.rs:809-843`）——`kill` 一下或直接关掉
      终端窗口，终端留在 raw mode / Kitty 键盘协议没弹栈。
      对面是 `tuix/src/signal_restore.rs:44-99`（`libc::sigaction`，async-signal-safe）。
      判据：装上处理器后发 SIGTERM，恢复序列被写出（在无头 `Surface` 上可判）。

### P1 — 真缺口（P0 之后，彼此独立）

- [ ] **P1-1 `/mcp` 面板**。**设计已就绪，不用再设计**：未合分支 `feat/mcp-panel`
      （两个提交，`docs/mcp-panel-design.md`）已经定好边界、三条新命令
      （`McpManage` / `McpDetail` / `McpAct{Trust,Untrust,Login,Logout,Enable,Disable}`）、
      `McpServerState` 加两个变体、停用项读取路径、注释守卫的失败语义、11 条判据与
      5 条「不做」。**先把那份合进来再动手。**
      今天的缺口：`/mcp` 只有 `status` / `withdraw` / `tools`(`tui/src/commands.rs:1485-1552`)，
      屏幕能报 `Untrusted`(`host-api/src/lib.rs:772`) **却没有任何办法处理它**——
      一条断头路；MCP 的 OAuth 登录/登出同样没有入口（tuix 在
      `event_loop/commands.rs:3073-3164`、`:3234-3296`）。

- [ ] **P1-2 CodingPlan 的后台监视**——**一条缝，三个消费者**，所以一批做。
      今天用量只在打开 `/usage` 那一刻问一次(`tui/src/plugin.rs:2761-2810`)，于是：
      1. **用量阈值提示**没有：对面每回合结束后 30s 冷却轮询，≥80% 上状态行、≥95% 变红
         (`tuix/src/event_loop/usage_monitor.rs:45-89`)；
      2. **模型列表漂移**没有：对面每小时探一次 `/coding-plan/models`，模型没了或列表
         过期就提示(`tuix/src/event_loop/monitor.rs:65-188`)；
      3. **额度耗尽主动推 `/openrouter`** 没有：触发点挂在上面那条轮询上
         (`tuix/src/event_loop/mod.rs:10071-10083`)，轮询没了整条链就断了。
      落点建议：cli 侧一条行（`fetch_usage` / `fetch_plan` 端口已在），状态栏画。
      判据：阈值上下穿越各一次、冷却真的生效（虚拟时钟，见 AGENTS.md）、
      取不到答案时**不画**而不是画成 0。

- [x] **P1-3 内联 `<think>` 剥离**（2026-09-23,决策 2 已定:provider adapter）。
      按计划做的,落在 `provider/reasoning.rs` 的入站那半(`InlineThink`),
      两个适配器各接一处 + 各自的收尾点放掉 held 住的。
      **实现时被判据抓到一个真 bug**:第一版在**发出标签前面的正文之前**就决定了
      要不要剥,于是 `seen_visible` 还是假的 —— `models emit <think> tags…` 被当成
      块开头,整句后半截被吞掉。改成先发前面的正文、再决定,正是那条判据要防的事。
      判据 7(单元)+ 2(适配器各一条,钉接线——单元那 7 条在适配器不调它时照样绿)。
      两个适配器各摘掉一次,各自判红;`capabilities` 956 全过、`coding` 859 全过。

      **不是"剥掉丢弃"，是把错投进 content 的 reasoning 搬回 reasoning 通道**——
      和 kernel 对"整个答案掉进 reasoning_content"那一例做的是同一件事的镜像
      （`engine.rs:3210`）。搬回去之后，屏幕的可折叠推理块照常显示它，kernel 照常
      把它存在 `Message.reasoning` 上，下游一行都不用改。

      两个落点，都是 SSE chunk → `StreamEvent` 的那一行：
      - `capabilities/src/provider/openai_compat.rs:1753-1757`
        （`choice.delta.content` → `TextDelta`；隔壁 `:1758-1762` 就是
        `reasoning_content` → `Reasoning`，剥出来的送这里）
      - `capabilities/src/provider/ollama.rs:615-619`（同形；Ollama 另有
        `message.thinking` 字段走 `:621-623`，但没开 think 解析的本地模型
        照样把 `<think>` 写在 content 里）
      `anthropic.rs` / `responses.rs` 不需要——它们的推理有独立的块结构。

      **住哪**：`provider/reasoning.rs`（277 行）已经是"OpenAI 兼容路的
      reasoning wire 策略"这一职责的家（出站那半：`ReasoningPolicy` 管上一轮的
      reasoning 要不要回发）。入站这半是同一个关注点，放同一个模块。

      **这一层白拿的一个好处**：解码器本来就是**每条流一个**（`self` 上已经挂着
      `last_usage` / `seen_finish`），状态跟着流生灭，所以 tuix 那个
      "上一回合没闭合的 `<think>` 把下一回合整个吞掉、气泡全空"的 bug 类
      （它靠每回合手动 `reset()` 防）在这一层**结构上就不存在**。

      **要照抄的两个不变量**（tuix 踩出来的，别重新发现）：
      1. `carry` —— 标签会被切在两个 chunk 之间、甚至切在一个 UTF-8 字符中间，
         疑似标签开头的字节先攒着不发；
      2. `seen_visible` —— 已经吐过可见正文之后再出现的 `<think>` 按字面量处理。
         否则模型在**解释**"推理模型的输出长什么样"时写了个没闭合的 `<think>`，
         会把后面整个答案吞掉：屏幕上是回答中途截断，而 `/resume` 重渲原始消息
         时又是全的。
      3. 64KB 上限，超了放弃部分标签检测直接吐，保证内存有界。

      **一处要想清楚的联动**：搬回去的 reasoning 与原生解析出来的从此不可区分，
      于是 `ReasoningPolicy` 也会照样把它回发给模型（GLM/Qwen 是 `Preserve`）。
      这正是我们要的（"保住它的思路"），但**要在判据里钉住**，免得以后有人以为
      合成出来的那份不参与回发。

      **要不要按模型开关**：建议**不按**，无条件剥 + `seen_visible` 兜底。理由是
      不剥的失败态（标签糊在答案里）比误剥的失败态更常见，而误剥那一类正好是
      `seen_visible` 覆盖的。

      判据（`cargo nextest run -p atomcode-capabilities`）：
      `a_think_block_in_content_becomes_reasoning`、
      `a_tag_split_across_chunks_is_still_stripped`（含切在 UTF-8 中间那一刀）、
      `a_literal_think_tag_after_visible_text_is_left_alone`、
      `an_unclosed_think_block_does_not_swallow_the_next_stream`（新流新状态）、
      `the_carry_never_grows_past_the_cap`。每条摘掉被测代码证伪一次。

- [ ] **P1-4 键位补一批**（同一片区域，一次改完）：
      - `F2` / `Shift+F2` 循环切模型——对面挂在闲置按键路径(`tuix/src/modals/model_picker.rs:28-37`)，
        新前端 `keymap.rs` 一个 F 键都没绑，**无替代**；
      - `Ctrl+A` / `Ctrl+E` / `Ctrl+K`——行首、行尾、删到行尾，三个都没有
        （`keymap.rs:255-313` 通读确认）；对面 `tuix/src/input/key_action.rs:70,73-74`；
      - `^H` / `^?` 别名——SSH/PuTTY 把 Backspace 发成 `Ctrl+H`、Delete 发成 `Ctrl+?`，
        对面有 `normalize_edit_key`(`key_action.rs:43-52`)兜底，这边没有，于是这类
        终端上退格直接失灵。
      判据：`Keys::resolve` 是数据表 + 冲突检测，每条新绑定都能离线判。

### P2 — 深度补齐（按自用暴露的顺序做，不预补）

> 这一节刻意排在后面：自用会告诉我们哪几条是真缺的，预补出来的是「我以为要」的。
> 下面按"撞上的概率"粗排，不是承诺顺序。

- [ ] **P2-1 `@` 文件补全接 `file_index`**。今天是单层 `read_dir` + 纯前缀 + 不读
      `.gitignore`(`tui/src/plugin.rs:504-545`)；对面复用 `capabilities::file_index`
      的跨层子串 + gitignore + 异步索引（webui 的 `@` 也用同一套）。**能力现成**，
      是接线不是重写。
- [ ] **P2-2 粘贴健壮性**：字符级 paste-burst 聚合（无 bracketed paste 的终端、
      PowerShell 5-12ms 分片）与多条 `Event::Paste` 合并——对面
      `tuix/src/input/reader.rs:39-43,440-546`、`:694-936`（含 IME 假阳性排除）。
- [ ] **P2-3 `/resume`**：预览带上 `provider · model`（对面 `session_picker.rs:734-737`）、
      列表标出「这是你当前正在跑的那个」（`:715-718`）。另：G2 留的那个口子——
      「按两次 Delete 到端口」那段没有端到端判据，给 `tui/tests/e2e.rs` 的 `start_full`
      加第七个参数时一并补。
- [ ] **P2-4 塌缩掉的子命令**（一批，都是同一种做法）：
      `/team` 与 `/todo` 的子命令被塌成一个恒定的折叠开关（`tui/src/commands.rs:43-44,74-75`，
      `args` 根本不解析，对面各有 4 个 / 3 个子命令）；`/loop` 缺固定间隔
      （`coding/src/host_rows.rs:2676-2707` 只有 `<prompt>` 和 `stop`，对面 `/loop 5m <prompt>`）；
      `/goal` 缺 help 与 stop 的近义词、不支持带图；`/copy` 缺 `msg`（复制整条回复原文）；
      `/save` 缺「目标已存在且非 md 时拒绝覆盖」；`/paste` 语义从"贴剪贴板图片"
      变成了"贴文本"，Windows 下 `Ctrl+V` 被按键层吃掉时的兜底入口因此没了。
- [ ] **P2-5 `/cd`**：Tab 做 shell 式路径补全、Enter 直接去任意没列出的路径
      （对面 `dir_picker.rs:222-260`、`:128-150`）。这边换来了浏览与书签，**不是纯退**，
      但"我知道路径，让我直接去"这条快路没了。
- [ ] **P2-6 引导向导**：`Step::Setup` 的三选一里缺「手动配置 provider」那一支
      （对面 `onboarding_wizard.rs:784-797`），今天只有"登录 CodingPlan"或"跳过"，
      跳过的人落地即无 provider；以及会话中途重跑 `/welcome` 时的二次确认闸门
      （`onboarding_wizard.rs:297-315`），今天 `open()` 无条件直接开
      （`cli/src/tui_onboarding.rs:150-172`）。
- [ ] **P2-7 `/diff` 深度**（依赖决策 1）：文件状态字母、staged/unstaged 分段、
      详情页 Esc/Left **退回文件列表**而不是整个关掉（今天 `overlay.rs:270-280`
      除滚动键外一律 `Cancelled`，看下一个文件得重敲 `/diff`）。
- [ ] **P2-8 跨会话输入历史 + `Ctrl+R` 反向搜索**（决策 3 已定：两个一起做）。
      两件事绑在一起，因为**反向搜索的价值随语料规模走**：本会话里找一句话，
      Up/Down 翻几下就到了；几百上千条里挑，才需要搜。

      **历史怎么来——折叠，不是另存一份**（理由见决策 3）：
      - 会话在磁盘上**本来就按项目分好了**：`SessionManager::for_project(working_dir)`
        落在 `$ATOMCODE_HOME/sessions/<project_hash>`(`manager.rs:1034-1038`)，
        所以"这个项目里我打过什么"不用自己划范围；
      - `list_visible()`(`manager.rs:3025`) 按新到旧给会话，**且天然排除了成员与
        子 agent 会话**（它们的索引记 `parent`，不进目录）；
      - 每条日志是一行一个事实，只要 `UserMessage` 那些行——**加一个窄读法，
        不要用 `load_events()`**(`manager.rs:175`)把整份日志解析出来。

      **三条边界，都是"走日志"才拿得到的**：
      1. **撤回的回合不算**。`visible_turns()`(`manager.rs:524`) 已经是这个判据；
         这正是另存一份文件做不到的那件事，**要写成判据**
         （撤掉一个回合 → 那句话从历史里消失）；
      2. 连续重复丢掉，和当前 in-session 折叠同一条规则(`host.rs:1801`)；
      3. 上限：最近 N 个会话 / 最多 M 条（tuix 用 200，建议同）。

      **时机：懒加载**。不要在启动时扫。第一次有人越过本会话那几条往上翻、
      或第一次按 `Ctrl+R` 时才建，之后按屏幕的生命周期缓存。这样启动路径一行不动，
      代价只由真去翻历史的人付。

      **归属**：这是对项目会话库的一次读，是**宿主的问题不是屏幕的问题**
      （0022 §3：屏幕知道的产品信息都从缝过来）。契约加
      `HostCommand::History { session, limit }` → `HostReply::History { entries }`，
      cli 那侧实现扫描。屏幕不认识 `SessionManager`。

      **一个要顺手修的旧账**：日志里的 `UserMessage.text` 是**展开后的全文**
      （`expand_pastes` 在提交时就把 `[Pasted #N …]` 还原了，`moment.rs:236`），
      所以召回一条当初含大段粘贴的历史，会把整段灌进输入框。tuix 靠在历史文件里
      另存一份 pastes 注册表解决；这边**不存，改成召回时按同一条折叠规则重新折**
      （`PASTE_FOLD_LINES`/`PASTE_FOLD_CHARS` 现成）。
      **这个毛病今天的本会话历史就有**，不是跨会话带来的——一起修。

      **`Ctrl+R` 照抄 tuix 的五条规则**（`tuix/src/event_loop/mod.rs:4933-4981`，
      11 条判据在 `:7198-7476`）：可打印字符进 query 不进缓冲区并实时重搜；
      Backspace 删 query；再按 `Ctrl+R` 走更老的一条；**Enter 只是把命中接受进
      缓冲区并退出搜索态**（再按一次才提交——不会手一抖把旧命令发出去）；
      Esc 恢复进搜索前的草稿；**其他任何键先退出搜索态再照常执行**。
      匹配是大小写不敏感的**子串**，从新往旧；空 query 显示最近一条；
      没命中时缓冲区显示 query 本身，让人看见自己打了什么。
      搜索时隐藏历史位置指示（对面有判据钉着，`render/retained.rs:13690`）。

      **键位要先腾**：`Ctrl+R` 现在是「折叠推理块」(`keymap.rs:283`)。
      **显示槽现成**：输入框上沿 rule 的左肩已经在显示历史位置
      (`modules/input.rs:58-64`)，加第三种状态即可，和 tuix 放的是同一个地方。
      **分发有先例**：「搜索态先吃键、其余漏下去」与斜杠菜单的
      `Slash::owns`(`menu.rs:318-330`) 是同一种形状。
- [ ] **P2-9 零碎**：`/help commands` 子模式；`/skills` 一条命令链式调多个 skill；
      `/whoami` 补 auth 文件路径；Bash 命令展示行折叠 `$HOME`
      （对面 `tuix/src/platform.rs:112-158`）；Orca 终端的 OSC 11 乱序规避
      （对面 `terminal_bg.rs:14-21`，这边 `surface.rs` 探测范围更大但没有这条规避）；
      `/rewind` 面板第二步的「仅代码」档（对面有三档，这边面板主动砍到两档并写了理由，
      命令 `/rewind N code` 仍可达——**先确认这是不是要改的**）。

### P3 — 删与收口（P0+P1 落地并 soak 之后）

- [ ] **F1 删 tuix**。前置已补齐（`CLASSIC_ONLY` 空了）。**卡点重新盘过一遍，
      比 remaining-gaps 里记的轻**——那句「`main.rs` 的 26 处 `--classic` 分支
      （唯一真卡点）」是旧账：`screen_for` 把判定收成一处之后，`main.rs` 里
      带 `classic` 字样的只剩 4 行（`:865-869` 的 flag 定义、`:2248` 的调用），
      全 cli 13 处。真正要动的是 cli 里 27 处 `atomcode_tuix`，分两类：
      - **7 处是 `atomcode_tuix::i18n`**，而那个模块只有两行：
        `pub use atomcode_config::i18n::*`（`tuix/src/i18n/mod.rs`）。
        指回 `atomcode_config::i18n` 即可，**与删 tuix 无关，随时可先做**；
      - 其余是 `run` / `SpawnedRuntime` / `RuntimeControl` / `RuntimeEndpoint` /
        `RuntimeEventPayload` / `ProviderSelectionMode` / `session::Session` /
        `panic_restore_terminal`——**它们只为喂 `atomcode_tuix::run` 而存在**，
        经典屏幕一走，这套 spawn 胶水跟着走。
      `acp/commands.rs:5` 那处只是注释，不是卡点。
      另要动：workspace member(`Cargo.toml:17`)、`cli/Cargo.toml:21,38` 的
      `distro-pm` 转发与 path 依赖。顺带：`atomcode-i18n` 的 `product` 表里
      25 条 `Bg*` 文案随之成为死码。
- [ ] **F2 摘 `coding/src/team/`**（1466 行，决策 8）。半活半死：`stop_all` 还绑在
      `runtime.rs` 三处 `quiesce_current_agent`/`stop_current_agent` 上，单独砍会断。
- [ ] **F3 daemon 迁两份契约**。先读未合的 `feat/daemon-on-contracts`
      （worktree `.worktrees/daemon-on-contracts`，一个提交 `9ee49fcf`）：四条驱动路径
      里两条随 F1 自动死，线上格式已定 B，还有三条待定。决策 10（第一方各端走契约
      原样上线，ACP 只作第三方投影）要修订 ADR 0013，**文档也在那条分支上未合**。
- [ ] **F4 `daemon/src/legacy_convert.rs`（4485 行）不动**。唯一的历史 core JSON 单向
      importer，消费者归零前不碰；任何报告里**不得**称"格式已删除"。

---

## 三、每批的门

```sh
cargo nextest run -p <受影响的 crate>     # 不用 cargo test、不用 --workspace
cargo fmt --all -- --check                # 唯一阻塞门，别积债
bash gates/tui.sh                         # 碰 tui 时
gates/tui-test-count.baseline             # 随判据上抬（今天 1054）
grep -c 'name = "hkdf"\|name = "hmac"\|name = "zeroize_derive"' Cargo.lock   # 必须 0
```

新增 async 判据只要路径上有超时/退避/重试，一律
`#[tokio::test(flavor = "current_thread", start_paused = true)]`；但**先 grep 一遍
`elapsed`**——断言"必须真等过"的那种测试不要转（AGENTS.md「测试与构建命令」节）。

---

## 四、这份清单是怎么来的

五路测绘，每一路都打开两边实现读、不按名字对：

| 路 | 覆盖 |
|---|---|
| 命令逐名 | tuix 60 条 vs tui 自有 + cli 行 + 目录投影，逐条比子命令与参数形式 |
| 面板逐键 | tuix `modals/` 16 个文件 16824 行 vs tui 的五块面板 + overlay + wizard，逐键位、逐字段 |
| 输入键位 | `input/` + 粘贴 + 补全 + 历史 + 鼠标 + 中断排队 |
| 生命周期外围 | 后台槽、监视器、版本检查、标题、背景色探测、信号、平台、worktree、think、sanitize、trace、desktop/oauth |
| 渲染展示 | markdown、语法色、diff、位图、能力探测、保留模式、主题、思考与工具卡片 |

**纠错记在第零节**：五处报成缺口的其实都有，两处报成"新前端更浅"的方向反了
（语法色、逐条折叠是新前端更深）。这和 09-18 那次「有入口≠接上了」是同一把尺子的
两面——**名字对上不算有，名字对不上也不算没有**。
