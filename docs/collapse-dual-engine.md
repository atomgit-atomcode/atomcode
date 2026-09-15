# 收掉双引擎，删手写链

> 🟢 **接手先读这一页。** 分支 `feat/collapse-dual-engine`,worktree
> `.claude/worktrees/collapse-dual-engine`,自 `feat/plexus-plugin-architecture@7e1d7c98`。
> 前情见 `docs/handoff-coding-on-harness-2026-09-14.md`(引擎侧)。

## 目标

`atomcode-coding` 今天有两套装配引擎，由 `runtime.rs` 的 `Engine::from_env()`
(`ATOMCODE_ENGINE=harness`)选择：

- `Engine::Chain`:`parts::assemble` + `assemble.rs`,手写 kernel 中间件/钩子链(默认)。
- `Engine::Harness`:`on_harness::mount_swappable`,harness 行清单。

终态：只有 harness 引擎;`Engine` 枚举、`parts::assemble`、`assemble.rs` 与只为链式存在
的代码删掉;tuix / cli / daemon / acp / clix 经 `CodingRuntime` 的公开面照常工作。

## 已定决策(2026-09-16 用户拍板，不要重议)

| 决策 | 内容 |
|---|---|
| 会话持久化 | **原生快照为主、JSONL 为从。** coding runtime 的 resume / undo / rewind / restore / 会话目录 / 租约 / 落盘失败 fail-close 只认原生 `SessionManager`。harness `session-persistence-jsonl` 继续写，服务 `recall` 与 atui;coding 从不从它恢复。两份用同一个会话 id。undo 之后 JSONL 分叉先接受(判据钉住),日志加 rewind 事件属 `architecture-target.md` §11 未决项，不在本线。 |
| 节奏 | 本分支内翻默认 + 删链，分两个 commit;不留 `ATOMCODE_ENGINE` 开关。 |
| 差分台 | 删链前把链式输出录成 golden;场景改成 harness 对 golden 回归，棘轮照旧。 |
| 真模型 | 收尾允许少量真模型冒烟(临时 clone 私有签名 crate,用完还原，绝不提交)。 |
| clix | 不删，只保证能编译。 |
| 补齐 | 链式有而行清单缺的(telemetry、CodingPlan 窗口限流、GitPushLabel、MCP……)一律补齐。只本地 commit,不 push。 |

## 起点(实测)

```
CARGO_TARGET_DIR=../../../target cargo nextest run -p atomcode-coding
  链式                        549 passed, 8 skipped
  ATOMCODE_ENGINE=harness     543 passed, 6 failed(快照/会话一簇)
```

测试全绿不等于等价：差分台的链式参照只是 `prepare + assemble`,**不经过
`CodingRuntime`**,运行时层没有一项在差分覆盖里。

## 差距清单(2026-09-16 测绘，按性质分组)

### 会话(决定其余一切的形状)
- 树不认识 coding 的会话 id:`mount_swappable` 不 patch `session` 行，harness 自己 mint 一个。
- 原生快照在 harness 上从不写(链式靠 `SnapshotHook`,挂在 kernel 链上)。目录里出现 0 条消息的会话。
- resume:树从空对话开跑,`RuntimeSessionInfo.resumed` 却是 true。
- rewind 点只在 `SnapshotHook::turn_complete` 追加 → harness 回合不产生点。
- undo / restore / reprepare 家族(/cd、fresh、resume、reload)全部掉回链式。
- 落盘失败 fail-close(`take_snapshot_persistence_uncertain`)永远不触发。
- **残留树 bug**:无会话 undo 或 restore 之后 `harness_app` 未清，/model 去 patch 一棵死树，/logout 后凭据还在链式 agent 里。

### 运行时开关与控制器
- `SetMode` Plan / AcceptEdits:写 `parts` 原子量，树里无人读(plan-mode 行 disabled,accept_edits 是挂载期常量)。
- `resolve_policy_intervention`:harness 把 DenyTurn 降成普通拒绝，永远没有待决干预。
- `/loop`:`schedule_wakeup` 只注册进 `parts`,树里没有 → 跑完第一回合就 completed。
- MCP:`prepare` 真连了 server,工具却没进树;`wait_mcp_ready` 等满超时,`mcp_tools` 永远空。
- team:树里 team 行不喂 `TeamRunManager` → 永远不发 `CodingRuntimeEvent::Team`。
- `/compact`:focus 不进策略;`CompactionOutcome` 字段填的是事件条数;没有 CompactionCheckpoint。

### 装配缺口(链式有、树里无或关着)
- 遥测:`TelemetryHook`、`ToolTelemetryMiddleware`、`MeteredProvider`(含 code_review / subagent surface)。
- 用量记账:`UsageRecordingProvider`、子 agent tier 的 session id 与 recorder。
- CodingPlan 窗口限流:`RateLimitHook` 接 `RateLimitWindowSource`(树里只有通用 429 等待)。
- `SessionContextHook` 的环境信息(工作目录/平台/shell)与 git 快照。
- `TodoEagerHook`、`keep_interrupted_context`、`round_cap_checkpoint`。
- `PluginHookSource`(插件内联钩子);`cc-hooks` / `datalog` 行已注册但不在清单里。
- 工具:lsp、外部 `subagent_<name>`、AtomGit 工具、`GitPushLabelMiddleware`、`list_sessions`。
- read_file 的 vision 写死 false。
- 配置未桥接：preferred_language、credential_shell_policy、permission_rules、compact_threshold、重试次数、审批超时、todo / request_user_input / subagents / web / review / memory / mcp 开关、skill_dirs、plugin_skill_dirs。
- 多数行目录默认进程 cwd 而非 working_dir(skills、memory、mcp、permissions、project-instructions、session-persistence-jsonl、team)。

## 做法

### 会话：原生为主的三个零件
1. **id 与种子进树。** `session` 行由 coding 给 id;resume 时把原生快照转成日志事件作为种子。
   转换是纯函数，判据是往返：原生 → 事件 → 原生，逐字段相等(不带的字段列明)。
2. **一个可 await 的回合末时刻。** harness 今天只有同步 emit 的 `turn/end`,在
   `TurnEnd` commit 之后——驱动已经收到回合结束了。原生提交必须在驱动看到之前完成，
   否则 fail-close 检查与紧随其后的 undo 都读到旧快照。
3. **原生写入行。** 复用 `SnapshotHook` 本体(inflight、计数、rewind 点、meta、租约、
   持久化状态、模型归因),由行在对应的 harness 时刻调用它的 `LifecycleHooks` 方法
   ——与 `cc-hooks` 行同一个手法。

### 运行时：重建走树
undo / restore / reprepare 家族今天是「停 agent → 写原生 → 重装链式 agent」。改成
「停 agent → 写原生 → 用新种子重挂树」,runtime 的 generation / 相位 / 事件契约不动。
跨重建必须存活的状态(模式开关、授权表、team 运行)由 coding 提供的共享句柄注入行。

### 装配缺口：能复用本体就复用
凡是 kernel 钩子/中间件本体已在 capabilities/coding 里的，行只做时刻映射，不重写判断。

## 步骤与判据

每步：先写判据(链式绿、harness 红)→ 实现 → harness 绿 → 提交。

| # | 内容 | 判据 |
|---|---|---|
| 1 | 残留树 bug;`Engine` 只读一次 | 无会话 undo 后 /logout,旧 provider 无人持有 |
| 2 | 会话 id 与种子进树;原生⇄事件转换 | 往返判据;harness 下 resume 看到旧对话 |
| 3 | 回合末可 await 时刻 + 原生写入行 | 6 条快照/会话测试转绿;回合结束前原生已提交 |
| 4 | 重建走树(undo/restore/reprepare/resume/cd/fresh) | 这些路径不再打印「continues on the hand-written chain」 |
| 5 | 模式开关、策略干预、schedule_wakeup | plan 模式下写工具被拒;/loop 第二轮会醒 |
| 6 | MCP 单一 owner | 对 `mcp-test-server` 真连，工具进树且只连一次 |
| 7 | 配置桥接 + working_dir | 每项一条「config 写了、树里生效」 |
| 8 | 遥测/用量/限流/上下文/插件钩子/缺失工具/compact 保真 | 各一条 |
| 9 | team 事件 | harness 下发 `CodingRuntimeEvent::Team` |
| 10 | 差分录 golden | golden 由链式生成;harness 对 golden 分歧不涨 |
| 11 | **翻默认**(单独 commit) | coding / harness / tuix / cli / daemon / tui 全绿，clix 编译 |
| 12 | **删链**(单独 commit) | `Engine`、`assemble.rs`、`parts::assemble` 不存在;同上全绿 |
| 13 | 真模型冒烟 | 纯文本 + 带工具 + resume + undo |

## 构建约定(本 worktree)

- 共用主仓 target:`CARGO_TARGET_DIR=../../../target`,并 `CARGO_INCREMENTAL=0`。
  磁盘 98%,主仓另有会话在编译;剩余低于 10G 就停。
- 按 crate 跑 nextest,不跑 `--workspace`(见 AGENTS.md)。

## 进度

(每步完成后在这里记 commit 与判据数字)

| 步 | commit | 状态 |
|---|---|---|
| 1-4 会话与重建 | `b884f74c` `da2c0767` `03130ee8` | ✅ 日志带 meta/reasoning_blocks;`SessionDefaults.seed`;`turn/finishing`;`native_log` 往返;`session-native` + `kernel-hooks` 行;`mount_harness` / `build_agent` 统一建树(残留树随之消失)。判据 `tests/engine_parity.rs` 8 场景 |
| 5a 模式与授权 | `e51b7d16` | ✅ `modes` / `grants` 服务;`plan-mode-live`;`plan_verdict` 中立函数。判据 +3 场景 |
| 5b 策略干预、schedule_wakeup | `b07416c4` `67a332ec` | ✅ `SessionEvent::PolicyIntervention` + `StopReason::PolicyDenied`(会话格式 → 4);`host-tools`(schedule_wakeup) |
| 7 配置桥接 | `c840386c` `4f7ad139` | ✅ 工具清单两引擎一致(明单三条)、能力开关、`skills-host`、memory `inject`、`chat-options`、`swap_provider_for`、权限规则、回合上限/循环保护/压缩/重试/提问超时 |
| 8 装配缺口(一) | `85b392bb` | ✅ 上下文块、transcript、遥测、hooks.json + 插件钩子、datalog、todo 提醒;`kernel-middleware` 桥 |
| 6 MCP | `34f46781` | ✅ runtime 的注册表是唯一 owner,`mcp-host` 发布进树 |
| 8 装配缺口(二) | `8a582120` `e412c194` `2c38ab96` | ✅ 取消即撤销(`SessionEvent::Interrupted`)、CodingPlan 窗口限流(`rate-limit-coding`,`RateLimitPaused`)、手动 compact 落盘即时 + 只汇报一次 |
| 8 装配缺口(三) | `2ad275ed` `b857e241` `b89818ff` `631e33ca` `16914c71` `d5d5b277` `9d950b7e` | ✅ `tool-driver` 缝(工具进度 + 提问);树里挂产品自己的 `request_user_input` / `task` / `team` / `code_review` / `recall`(`parts::wire_side_providers` 两引擎共用：分层、会话 id、detached 用量记账、带 surface 计量;logout 清槽);Team 事件同源;`TurnProgress.continuing` + 轮次/截断检查点;`stream_idle_ms` → `Timeout`;`/compact <focus>` 由对话模型写摘要(`compaction-coding`);工具契约判据从名字升到描述+参数 |
| 9 team 事件 | `b857e241` | ✅ 随产品 `task`/`team` 进树，事件由 `parts.team_manager` 发 |
| 10 差分录 golden | 本 commit | ✅ `tests/golden/differential/*.json` 56 份，由链式录(`ATOMCODE_RECORD_GOLDEN=1`);回放下基线不变;篡改一份 → 棘轮红 |

### 装配缺口的做法(8)

链式 `assemble` 里还没对上的，按「能复用本体就复用」分三类:
- **走 `kernel-hooks` 桥**(LifecycleHooks 本体照用):TelemetryHook(on_request/on_model_response/on_error/turn_complete)、
  TodoEagerHook 与 StatusReminderHook(只往请求尾巴追加 → 桥要支持 pre_request「只许追加」)。
- **已有行、要按配置条件插入**:`datalog`(cfg.datalog)、`cc-hooks`(hooks.json + PluginHookSource)。
- **要单独写行**:SessionContextHook(环境与 git 快照进 system prompt,resume 冻结 git 段)、
  RateLimitHook(on_rate_limit 属重试策略)、ToolTelemetryMiddleware / GitPushLabelMiddleware(工具中间件)。

### 已知的偶发
`atomcode-harness::harness the_log_reaches_the_disk_in_the_order_it_was_committed` 全量并行下偶发红，
单跑与重复 10 次均绿;测试注释自认此事。

## 判据(`tests/engine_parity.rs`)

两引擎各跑一遍的运行时判据，只经 `CodingRuntime` 公开面，删链后原样留作 harness 判据。
每条先摘掉被测代码证伪一次(commit message 里记了反证)。截至步骤 10 共 44 个场景 + 3 个
清单/凭据判据。

## 已知差异(决定保留，删链后照此为准)

- **压力触发的自动压缩**:链式是「超阈值把旧工具结果原地改成桩」(StubCompaction),溢出时
  桩 → 截断 → 摘要;树是 `compaction-coding` 的免模型清单(保留最近 2 回合)。原地改写旧消息
  在追加式日志里没有对应事实，要做得先给日志加事件，属日志模型的决定，不在本线。人要的
  `/compact <focus>` 已对齐(对话模型写摘要，计费)。
- **摘要的锚点**:链式下一次压缩认得上一次摘要(哨兵行)并在其上更新;树里上一次摘要投影成
  合成 system 消息，下一次会把它当普通内容一起摘。
- **边界**:链式按 token 预算留最近内容，树按回合数(2)。
- **工具指引的归属**:树里 fs / shell / 搜索 / codeintel / web / todo / describe_self 的提示词
  由各自的行写，措辞与链式人设里的段落不同;产品自有工具(`request_user_input` / `task` /
  `team` / `code_review`)的段落由 host-tools 带入，与链式同文。
- **`max_continuations`(offer_continuation 熔断，默认 50)**:树里的续写提醒(verify-cadence、
  todo-reminder)各自一次性，没有需要熔断的循环，不桥接。
- **流静默重连次数**:链式内核自带 5 次无内容重连;树里静默是可重试错误，次数归 `llm-retry`。
- **链式 logout 不清审查/子 agent 槽位**:树里清(判据 `a_logout_leaves_no_signed_in_provider_alive`)。
