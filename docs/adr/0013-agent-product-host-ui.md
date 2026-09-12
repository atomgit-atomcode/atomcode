# AtomCode 分成 Agent / Product / Host / UI 四层

状态: 已决定(2026-09-12)。承接 [`0012`](./0012-atomcode-tui-replaces-tuix.md)。

## 背景

plexus 线的目标已经改成替换现有栈(0012)。替换的对象是 `atomcode-coding`
加它上面的 cli / tuix / daemon。动手前先回答两个问题:coding 里到底装了什么,
新栈按什么切。

2026-09-12 对 `atomcode-coding` 和 `atomcode-review` 做了逐项测绘,对照
`atomcode-harness`。数字按生产行数计,`runtime.rs` 从第 7908 行起是 7,880 行
in-file 测试,已扣除。

| crate | src 总行 | 生产行 | harness 已有 | 部分 | 缺 |
|---|---|---|---|---|---|
| atomcode-coding | 33,602 | ≈18,000 | ≈14% | ≈25% | ≈61% |
| atomcode-review | 5,348 | ≈3,100 + 49 份 rules/*.md | 缝与隔离已有 | 策略层待搬成行 | 一个编排原语 |
| atomcode-harness | 12,788 | ≈12,700 | — | — | — |

循环层的等价性有硬证据:`gates/differential.baseline` 16 个场景 13 个零分歧,
`compact=1` 是 harness 多发一个 `CompactionStarted`,`truncated=2` 是两个
`Usage` 的先后顺序。差距不在循环,在循环之上。

coding 里确实混着产品:`rate_limit.rs` 拉 CodingPlan 配额窗口、
`provider_factory.rs` 的 AtomGit 网关签名和 tier provider、`skill_first.rs`
给 DeepSeek/Qwen 打补丁、`init_prompt.rs` 中英双语、`atomgit` feature 的
REST 工具。反面证据是 `6f9ec3e4` 从 harness 删掉的四个产品变体
(LongCode / LongCode Air / Code Security / Code Review):全部差异 150 行 TOML。

## 决策

**按四层组织,依赖方向只允许一种:Host 是唯一认识全部四层的。**

| 层 | 内容 | 今天对应 |
|---|---|---|
| **Agent 机制** | kernel、plexus、harness 的缝(`seams.rs`)、`agent-loop`、session 日志与投影 | 已有,不动 |
| **Agent 能力行** | 工具、codeintel、skills、mcp、review 行、team 行、编码纪律行 | capabilities、review 已是;coding 的 discipline / todo / plan_mode / 工作区 gate / team / controllers 要拆成行 |
| **Product** | 每个产品一组 TOML overlay,加一个薄 crate 放只有该产品需要的行 | coding 的 rate_limit、provider 签名、skill_first、init_prompt、persona 文案、atomgit 工具 |
| **Host** | 读配置、选产品 profile、挂 UI 行、起 `App`;distribution / endpoints、auth、updater、telemetry 发送、磁盘会话存储、租约、调度、信号 | cli、daemon、config、auth、updater、telemetry、codingplan client;coding `runtime.rs` 的持久化与重配部分 |
| **UI** | `ui` 缝后面的前端行 | tui、ui-web、ui-jsonrpc、以后的 ACP |

依赖规则:UI 只依赖 Agent 的缝;Product 只依赖 Agent;Agent 谁也不依赖;
Host 依赖全部。harness 今天对 review 是生产依赖、对 coding 只在
dev-dependencies 做差分,这个方向要守住。

Agent 分成「机制」和「能力行」两层是刻意的:review、team、VerifyCadence 既不是
核心机制,也不是某个产品独有,多个产品会复选。Product 只做选择和配置,不实现能力。

验收判据三句话:
- 换一个产品,只改 TOML 和一个小 crate。
- 换一个前端,只换 `ui` 行。
- 从 cli 换到 daemon,不动 Agent 和 Product 一行。

## 三条今天糊着的边界

1. **Product 不是大 crate。** 判据是那 150 行 TOML。Product crate 超过一两千行,
   就是 agent 机制漏进去了。
2. **Agent 与 Host 的界线在 coding 里最乱,harness 已经画对。** `runtime.rs` 的
   owner loop 把回合驱动(Agent)和租约、落盘事务、重配(Host)揉在一起。harness
   拆成 `agent-loop`(Agent)加 `session-persistence`、`control` 两个由 Host 填的缝。
3. **Host 与 UI 在 daemon 里是一体的。** daemon 27k 行同时是宿主(端口、鉴权、
   token 文件、调度)和前端适配(webui、live hub)。拆成「daemon 是一个 Host,挂
   `ui-web` 和 `ui-jsonrpc` 两个行」。

## 缺口清单

harness 相对 coding 完全缺失的三大块,按上表归层:

**① runtime 驱动层(主要归 Host,少数归 Agent 能力行)。**
`impl CodingRuntimeHandle`(`coding/src/runtime.rs:1127-1924`)40 个 pub 方法,
harness `handle.rs:649-707` 的 `AgentCommand` 只接 8 个分支,28 个无对应:

- Host:`undo_to_prompt` / `rewind` / `rewind_points` / `restore_snapshot`
  (`runtime.rs:1630-1806`,落盘事务在 `7085-7350`)、`resume_session_with_lease`、
  `change_directory`、`fresh_session`、`reload_capabilities`、`reprepare_config`、
  `wait_mcp_ready` / `mcp_status` / `mcp_tools` / `withdraw_mcp_tools`、
  `deactivate_provider`、`is_stopped` / `status` / `accepts`。
- Agent 能力行:`start_goal` / `stop_goal` / `start_loop` / `stop_loop` /
  `pause_goal`(控制器在 `controllers.rs:50-831`,含 `schedule_wakeup` 工具)、
  `resolve_policy_intervention`、`queue_local_context`。
- harness 的会话模型是「日志即快照」(`plugins/session.rs:171-172`),没有独立
  快照文件、undo sidecar、活动会话 OS 锁。补 undo 要先决定是在日志上加事件
  还是引入 sidecar。

**② 多 agent 编排(Agent 能力行)。**
team `manager.rs` / `runner.rs` / `tool.rs` 共 1,421 行、controllers 831 行、
`subagent_tiers.rs` 与外部 subagent profile 约 450 行。harness 零对应:
`grep -rci "team\|undo\|rewind"` 在 `harness/src` 为 0。子 agent 与父共用同一个
`llm` 行,没有快/强分级。review 的多 lens 扇出(`fanout.rs:13-298`)与 verify pass
(`fanout.rs:300-378`)也落在这里:缺的通用原语是「一个 agent 的结构化结果交给
下一个 agent 复核」,扇出、verify、team 都是它的特例。

**③ 审批梯度与生产级会话产物(Agent 能力行 + Host)。**
coding `parts.rs:1655-1800` 串了 10 道 middleware,顺序是契约(硬边界在前、便利
放行在后、审批最后);harness `bundle.rs:203-235` 对应 7 个策略行。缺:
CredentialBashGate(`parts.rs:1726`)、SensitivePathGate(`1735`)、
WriteApprovalGate(`1782`)、BashWorkspaceGate(`1790`)、OpenFileWorkspaceGate
(`1776`)、Claude Code 兼容 hooks 入口(`1758`)、artifact 溢出落盘 + `fetch_output`
(`1636-1648`)、datalog 落盘(`1685-1695`)。

**部分覆盖、需要补厚的(Agent 能力行或 Product):**
persona(harness `plugins/persona.rs:20-63` 是 5 句,coding `persona.rs` 693 行按
已挂能力分节拼装)、todo 尾部提醒与停机 nudge、plan mode 的 MCP `readOnlyHint`
与会话级 grant、compaction(harness `compaction-tail` 无模型摘要,`/compact <focus>`
的 focus 在 `handle.rs:731` 只进事件不进策略)、429(只认 `Retry-After`)、
telemetry 每轮 token 归因、provider 工厂(缺 anthropic / ollama / 网关签名 / tier)、
session title(不调模型)、vision 转写降级、MCP 生命周期(只在 apply 连一次)、
LSP diagnostics(harness `Cargo.toml` 未开 `lsp` feature)、`list_sessions`、
`request_user_input` 作为模型可调工具。

**review 侧要搬成行的(Agent 能力行):**
49 份 per-language rules 注入(`rules.rs:17-302`)、diff 行号标注(`diff.rs`)、
impact plan(`impact_plan.rs`)、git scope 取数与大 scope 确认令牌
(`review_tool.rs:353-423, 673-768`)、scope 路径白名单(`confine.rs:70-96`)、
round-budget 落地压力(`round_budget.rs`)、findings 排序渲染(`review_tool.rs:795-887`)。
harness 已有且直接复用的:`findings` 缝(`plugins/findings.rs`)、realm 隔离子 agent、
`ReviewPersonaPlugin` 调 `atomcode_review::review_persona`。

**harness 独有、替换路线上要保住的:**
`describe_self`、`report_finding`、`world` 执行世界缝、六种前端行、`control`
热重配缝、`--audit` / `--dump-config`。

**围栏覆盖不全:**
repo root 围栏只管走 fs 缝的工具,codeintel 与 ast_grep 在
`plugins/capabilities.rs:148-217` 直接落盘;只读语义是拒执行而非不挂载,写工具
仍对模型可见。

## 对私有化产品的含义

四个扩展方向的机制今天都开放:工具走进程内 `Plugin` 行 / MCP / skills;能力换
`Seam` 类型的缝提供方,`harness.patch.toml` 就能换;UI 的 `View` / `Producer` /
`CommandSet` 是 pub,面板与命令各是一行;特性走四层配置树(内置 bundle、
`$ATOMCODE_HOME/profiles/`、`harness.patch.toml`、命令行 overlay),改名改地址替换
`distribution.rs` 与 `endpoints.rs`。私有产品的骨架就是 `atui` 那个模式:自己的
binary 调 `plugins::catalog()` 再 `register` 自己的行,加一个 profiles 目录。

不满足的:插件编译期链接,装插件等于重编译;上面的能力缺口按需补;宿主层
没接上,cli / daemon 仍在老栈,harness 的 web 行是单 agent 单会话无鉴权;
auth / updater / telemetry 是 AtomGit 口味,私有化多半要换。

## 第一步

不新建四个 crate。把 coding 的 18k 生产行按上表分流:owner loop 已被差分证明
等价,不搬;持久化和租约归 Host、纪律和 gate 归能力行、CodingPlan 和签名归
Product。切完 `atomcode-coding` 这个 crate 不应存在,它本来就是「一个产品在老
引擎上的装配」。

私有产品与主线并行:先把产品的行和 profile 写出来跑在 `atui` 上验证,宿主层
当独立工程排期。

## 失效条件

- 若 Product 层长成大 crate,回头检查是哪块机制漏进去了,而不是接受它。
- 若某能力发现无法作为行落地、必须改 kernel 或 plexus,那是机制的缺口,单独立 ADR。
- 若替换路线叫停,本 ADR 的缺口清单仍作为 coding 与 harness 的对照表保留。
