# 交接:plexus 线,2026-09-12

分支 `feat/plexus-plugin-architecture`,HEAD `361b3e71`,本地 13 个 commit 未 push
(`e1cfa439..361b3e71`,60 文件,+5007/−648)。工作区干净。

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

## 怎么验证

```sh
cargo test -p atomcode-harness -p atomcode-tui --no-fail-fast   # 489 全绿
cargo clippy --no-deps -p atomcode-harness -p atomcode-plexus --all-targets -- -D warnings
bash gates/tui-layers.sh && bash gates/tui-negative.sh && bash gates/tui-test-count.sh
git status --short gates/     # 差分基线 gates/differential.baseline 只准降,不准动
```

`gates/tui.sh` 的 clippy 步骤会红:tui 的 host.rs / input.rs / theme.rs 三条,kernel 与
telemetry 各几条,都是 rust 1.94 新 lint 的既有问题,不在本次范围。

## 未决(要人拍板)

1. undo 的会话模型:日志上加事件,还是 sidecar。
2. 配置树改写方法(`harness/patch` 等)是否进公开 SDK。当前进程内开放、进程外不开。
3. 生产 profile 要不要默认开模型起名。
4. steering vs 定时:定时消息在回合中到达会折进当前回合,「每小时跑一遍」需要「等回合结束」语义。
5. 成员到成员直接通话不开;ACP 与 tui 里成员输出怎么呈现。

## 下一步(建议顺序)

1. **真模型验证 team**:挂 Claude 和 GLM 各跑一遍 delegate → 收报告 → tell 的完整流程。
   只在 replay 上测过。
2. **`ui-acp` 行**:移植 `crates/atomcode-cli/src/acp/`(7.7k 行)到句柄协议之上;多会话已就绪
   (`CreateAgent` + `by_session`),差 fs/terminal 客户端世界行、RejectAlways、config option 白名单。
3. **`compaction-summary` 行**:把 capabilities 的 `OverflowCompaction`(stub + 模型摘要)接到
   `compaction` 缝,摘要走 `llm-utility`;顺便让 `/compact <focus>` 的 focus 进策略签名。
4. **ToolMiddleware 适配行**:CredentialBashGate / WriteApprovalGate / BashWorkspaceGate 原样
   挂上(4.6k 行实现已在 capabilities)。
5. **定时与 loop 行**:往 inbox 放 `Harness` 消息即可,机制已备;coding 的 `controllers.rs` 是策略参考。
6. team 剩余:lead 取消级联、`report_finding` 进成员工具集、tui 成员面板行、fork 切片器。
7. 会话剩余:`delete`、OS 租约、`Titled` 的 `/title` 之外的提交点。

## 踩过的坑

- 树级监听器用自己的 ctx 找日志会永远找到根:必须 `crate::agent::scoped(&self.ctx)`。
- 泵在 spawn 里注册监听器,消息可能先到:注册后先看 inbox。
- 旁路模型调用不能借主 `llm`:会吃掉 replay 脚本的下一行,而且并行触发时吃哪行不确定。
- 测试断言别假设报告在回合外到达:steering 会把它折进当前回合,用 `heard_by` 那种两态断言。
- patch 的目标 id 是行名(`session-title-first-prompt`),不是缝名(`session-title`)。
- `cargo test --keep-going` 不存在;`cargo build --tests --keep-going` 可以。
- 事件可见性:监听器只看到自己 realm 及子孙 realm 的 emit;根发的事件子 realm 听不到。
- 用 python 脚本批量改文件时,先打印原文再写模式:注释与空行会让模式失配,脚本半途失败会留下半写的文件。
