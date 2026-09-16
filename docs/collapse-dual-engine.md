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
| 1-4 会话与重建 | `b884f74c` `da2c0767` `03130ee8` | ✅ 日志带 meta/reasoning_blocks;`SessionDefaults.seed`;`turn/finishing`;`native_log` 往返;`session-native` + `kernel-hooks` 行;`mount_harness` / `build_agent` 统一建树(残留树随之消失)。判据 `tests/runtime_criteria.rs` 8 场景 |
| 5a 模式与授权 | `e51b7d16` | ✅ `modes` / `grants` 服务;`plan-mode-live`;`plan_verdict` 中立函数。判据 +3 场景 |
| 5b 策略干预、schedule_wakeup | `b07416c4` `67a332ec` | ✅ `SessionEvent::PolicyIntervention` + `StopReason::PolicyDenied`(会话格式 → 4);`host-tools`(schedule_wakeup) |
| 7 配置桥接 | `c840386c` `4f7ad139` | ✅ 工具清单两引擎一致(明单三条)、能力开关、`skills-host`、memory `inject`、`chat-options`、`swap_provider_for`、权限规则、回合上限/循环保护/压缩/重试/提问超时 |
| 8 装配缺口(一) | `85b392bb` | ✅ 上下文块、transcript、遥测、hooks.json + 插件钩子、datalog、todo 提醒;`kernel-middleware` 桥 |
| 6 MCP | `34f46781` | ✅ runtime 的注册表是唯一 owner,`mcp-host` 发布进树 |
| 8 装配缺口(二) | `8a582120` `e412c194` `2c38ab96` | ✅ 取消即撤销(`SessionEvent::Interrupted`)、CodingPlan 窗口限流(`rate-limit-coding`,`RateLimitPaused`)、手动 compact 落盘即时 + 只汇报一次 |
| 8 装配缺口(三) | `2ad275ed` `b857e241` `b89818ff` `631e33ca` `16914c71` `d5d5b277` `9d950b7e` | ✅ `tool-driver` 缝(工具进度 + 提问);树里挂产品自己的 `request_user_input` / `task` / `team` / `code_review` / `recall`(`parts::wire_side_providers` 两引擎共用：分层、会话 id、detached 用量记账、带 surface 计量;logout 清槽);Team 事件同源;`TurnProgress.continuing` + 轮次/截断检查点;`stream_idle_ms` → `Timeout`;`/compact <focus>` 由对话模型写摘要(`compaction-coding`);工具契约判据从名字升到描述+参数 |
| 9 team 事件 | `b857e241` | ✅ 随产品 `task`/`team` 进树，事件由 `parts.team_manager` 发 |
| 10 差分录 golden | `666adcf0` | ✅ `tests/golden/differential/*.json` 56 份，由链式录(`ATOMCODE_RECORD_GOLDEN=1`);回放下基线不变;篡改一份 → 棘轮红 |
| 11 翻默认 | `c81218a4` | ✅ 默认 harness,`ATOMCODE_ENGINE=chain` 只活到删链那一个 commit。翻之前先把 tuix/cli/daemon 在 harness 下跑了一遍，抓到一条真回归:provider 不报用量的回合 harness 一条 `Usage` 都不发，ACP 按它换 messageId,两轮并成一条消息(`6e3ff220` 修 + 判据) |
| 12 删链 | `f3a7f048` | ✅ `Engine`/`ATOMCODE_ENGINE`、`assemble.rs`、`parts::assemble`、`VerifyCadenceHook`/`SkillFirstHook`/`TodoHook` 的钩子外壳全部删除;`runtime::mount` 成为公开装配入口;12 个链式测试文件改挂树(`tests/support/mod.rs`),差分台只剩树对 golden |
| 13 真模型冒烟 | 不产生 commit | ✅ 纯文本 / 带工具 / resume / undo 四项在真网关上过(见下) |

### 删链时测试抓到的真问题(各自已修)

链式测试改挂树的过程本身是判据 —— 12 个文件里有 6 个第一次跑就红，每一条都是真差异:

- **transcript 两个写者**:runtime 的 transcript 与树的 `session-persistence-jsonl`
  都写 `<bucket>/<id>.jsonl`,两种格式混在一个文件里(`recall`/会话目录读它)。从此
  follower 写自己的 root;判据 `the_session_transcript_has_one_writer`。
- **不完整聚合没 fail-close**:mount 把任何 NotFound 都当「还没存过」,少了 presentation
  文件的会话会静默从空开始。改成只有 staged fresh 才容忍。
- **溢出无法恢复**:树的压缩按回合折叠，第一回合自己的工具输出撑爆窗口时没有可折的东西。
  给日志加 `ToolResultsStubbed` 事实 + 溢出阶梯第一级把长工具结果显示成摘要。
- **内部提醒的回话外露**:链式把 verify 提醒的「纯说话」回复藏起来;树直接播了出去。
  加 `MessageOrigin::Internal` / `InjectionOrigin::InternalNudge`,投影不播它的文字(照样入日志),
  空回复也不当 provider 失败重试。
- **人设/datalog 的模型名**:mount 用 provider 自报的名字，不是人选的那个。`HostState.model`。
- **resume 后重复提醒**:「这条编辑已经提醒过」原来在钩子的内存状态里;挪进 `unverified_edit`
  判断本身(连同「用户禁止跑命令就别催」),这样 resume 也认。

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

### 与本线无关的既有红(翻默认前后都红)
- `atomcode-daemon webui::tests::serves_embedded_index` / `unknown_path_falls_back_to_index`
  (内嵌前端资源没构建)
- `atomcode-tuix modals::config_panel::tests::retry_setting_is_searchable_in_both_languages`
- **`atomcode-review assemble::tests` 三条超时(各 120s 被 nextest 掐断)**:
  `max_rounds_stops_runaway_review`、`exact_no_progress_loop_is_stopped_when_rounds_are_unbounded`、
  `exact_guard_can_be_disabled_for_an_intentional_repetition_policy`。
  2026-09-16 才发现——之前几轮验证根本没跑 `-p atomcode-review`。
  **确认与本线无关**:`atomcode-review` 只依赖 `atomcode-kernel`(不依赖 harness/coding),
  把本分支改过的三个 kernel 文件(`agent.rs`/`hook.rs`/`request.rs`)退回分叉点 `617a93fe`
  再跑，三条照样超时。三条都用假 provider(`LoopingProvider`),所以不是网络;
  看名字都在「循环该被拦住」这一族，像是 review 自己的熔断不生效，值得单独查。

  > 教训:**「全绿」只对跑过的 crate 成立。** 这个 worktree 前面十几轮验证的 crate 清单
  > 里一直没有 review,于是这三条躲了很久。收尾时按 workspace 成员逐个点名跑一遍。

## 真模型冒烟(步骤 13)

跑的是 `target/debug/atomcode`(默认装配 = 树),隔离 home,真网关 `AtomGit-glm5.3-flash-pro`。
四项都是端到端经二进制，不碰测试替身:

| 项 | 怎么跑 | 结果 |
|---|---|---|
| 纯文本 | `atomcode -p "Reply with exactly the word: pineapple"` | 回 `pineapple`，退出 0，打印 resume 提示 |
| 带工具 | `-y -v -p "用 write 工具建 note.txt…"` | `[tool→ write_file]` → `[tool← ok]`,文件内容对;`turns=2 tool_calls=1`;两轮各报一次用量(`6e3ff220` 修的那条),第二轮 `cached=18752` 说明前缀缓存命中 |
| resume | `--resume <id> -p "不用工具，回答你刚建的文件名和那一行"` | 答 `note.txt` / `hello-from-harness`,即原生快照种子进树成功 |
| undo | ACP stdio(`atomcode -y acp`),两轮各建一个文件后发 `/undo`,再问「你这段对话里建过哪些文件」 | 答 `alpha.txt`;**反证**:同样三轮不发 `/undo`,答 `alpha.txt, beta.txt` |

`/undo` 只回滚对话不回滚文件(两次跑完 `beta.txt` 都还在),这是 `undo_to_prompt` 的契约 ——
文件回滚是 `rewind(RewindScope)` 的事，不是缺陷。

顺带在真跑里看到了会话的两个写者各写各的:

```
sessions/<bucket>/<id>.jsonl          {"v":N,...}          ← 原生 transcript(主)
sessions/harness/<bucket>/<id>.jsonl  {"header":{...}}/seq ← harness 日志(从)
```

两种格式、两个 root,任何一方都没写进对方的文件 —— 判据
`the_session_transcript_has_one_writer` 在真跑上的样子。

> 网关要签名，签名 crate(`atomcode-codingplan-crypto`)是闭源 overlay,不在公共仓。
> 冒烟时从主仓工作区临时拷进来编译，跑完 `git checkout HEAD -- crates/atomcode-codingplan-crypto/ Cargo.lock`
> 还原 + 删掉 overlay 文件 + `cargo clean -p atomcode-codingplan-crypto` 重编，
> 本分支不含它的任何一行，Cargo.lock 也没被它污染。

## 删链之后的 review 与修复(2026-09-16)

两个对抗性 review agent 各查一面(「链有树无的静默丢失」「重建路径的状态搬运」),
报了 6 条,每条我都独立复核过:**6 条发现全部成立**。

但**它们的修法建议不能照单全收**——A 那条 agent 给的方向是「把 `permissions` 行摆到
正确位置」,而在 plexus 里位置根本不决定顺序(见下),照做会同时打破 allow 规则的另一半。
发现靠证据,修法靠自己验。

| | 缺陷 | commit | 判据 |
|---|---|---|---|
| A | `[permissions] allow` 规则能解开凭据 shell 硬边界 | `f6e0fbc1` | 2 条，互为反向 |
| B | `withdraw_mcp_tools` 只销账不 unregister | `c4010968` | 1 条 |
| C | 登出 patch 失败 → 被取消的回合永不收尾 | `00875b4d` | **测不到** |
| D | `/model` 不重发 `llm-retry`，重试预算错配 | `d6c474c3` | 1 条 |
| E | patch 失败 → 槽表与树不一致，子 agent 跑错 provider | `00875b4d` | **测不到** |
| F | `TodoHook` 整个掉了(sidecar / 续写 / 每轮锚点) | `3ab4d947` | 1 条 |

另加 `67fee7e8`:装配审计(`the_product_mounts_and_audits_clean`),精简与全功能各审一次。
`atomcode-tui` 一直有这条,coding 直到行清单成为唯一装配才补上。

### A 那条值得单独读:修的是字段语义，不是位置

`ToolExec.pre_approved: bool` 换成了 `authorization: Authorization`
(`No`/`Presumed`/`ByPerson`)。一个 bool 同时承载了「人对这一次调用点了头」和「有条配置
说别问」,凭据闸门分不出来只好都认,于是一条图省事的 `allow = ["Bash(curl *)"]` 让
`credential_shell = "strict"` 形同虚设。现在便利闸门问 `settled()`,安全边界问
`by_person()`。

**一开始的修法是错的**:想把 `permissions` 行摆到「硬边界之后、便利闸门之前」。
做不到——`App::start` 的文档写着 **"Order in the file is irrelevant"**,行在清单里的
位置不决定执行顺序(顺序 = 注册时机 = 挂载顺序),`prepend` 是唯一的排序杠杆而且只有
两档。实测把行摆到凭据闸门之后,执行上它仍在最内层。

**结论:安全性不该压在一个作者控制不了的性质上。** 改完之后权限闸门至今仍 prepend 在
最前、行清单一个字没动,两条判据同时绿——这就是「与顺序无关」的实证。顺带,harness
自带的那份 `PermissionGate` 有同样的洞(也 prepend),改字段一次把两份壳都修了。

### C 和 E:一条测试够不到的错误分支，等于从来没人跑过它

两条都在同一个形状上:**可失败的操作已经改了共享状态,中间没有回滚**。也都写不出判据
——唯一能让 patch 失败的途径是某一行拒绝重挂,而测试够不到那个条件(layer 是固定字符串,
碰的只有 `llm` 一行)。它们靠结构和注释守着,缺口记在 `finish_stopped_native_turn`
的文档里,连同「一旦有了注入 patch 失败的办法,该写的判据是什么」。

### 这一轮反复踩的坑：顺序靠读代码推，连错四次

waterfall 的执行顺序我先后猜过「挂载轮次」「底座有重复行」「patch 会移位」「声明位置
生效」,四次全错。打一次真实顺序就全清楚了——`tests/gate_order.rs` 留了这个探针
(`dump_row_order`,`#[ignore]`)。**顺序是能打印的东西,不该靠推。**

同一类错误还有一次:把 `todo-reminder` 当成 `TodoHook` 的替身,是看名字判断的;它其实是
`on_emit::<SessionEventCommitted>` 观察者,既不唤醒已结束的回合也不写 sidecar。
**「某某行顶替了它」要看那行挂在哪个事件上。**

### 6 条修完之后的第二轮真模型冒烟

同一条网关，隔离 home，**验的是修复本身而不只是「还能跑」**：

| 项 | 结果 |
|---|---|
| 纯文本 | 回 `pineapple` |
| 带工具 + todo | 8 轮 7 次工具调用;`note.txt` 内容对;**会话目录里出现 `<id>.todos.json`**,内容是两条已完成的计划 —— F 的修复在真跑上生效 |
| **凭据边界(A)** | 配 `[permissions] allow = ["Bash(curl *)"]` + `shell_guard_policy = "strict"`,再加 `-y` 跳过所有审批,让模型跑一条带 `$SECRET_TOKEN` 的 curl → **`stopped=PolicyDenied`**,命令没跑 |
| 同上，阴性对照 | 只把 `shell_guard_policy` 改成 `off`,其余一字不动 → 命令正常执行 |

A 那条对照特别值得留意:**`-y`（自动批准一切）也没能让它通过**。这正是
`Authorization` 想表达的——`-y` 是「别再问我」,不是「我同意泄露凭据」。

> 配置键是 `[coding] shell_guard_policy`,不是 `[tools] credential_shell`;而且它本来
> 就有值,是替换不是新增。我在这上面连错两次，记在这里省下一次。

## 判据(`tests/runtime_criteria.rs`)

运行时判据，只经 `CodingRuntime` 公开面。每条都在两个引擎上写过、并摘掉被测代码证伪过
一次(反证记在各自 commit 里),删链后原样留下来当 harness 判据。48 个场景 + 能力开关、transcript 单一写者、logout 不留凭据三条，共 51 条;
另有 `tests/gate_order.rs` 的装配审计与「权限闸门只挂一次」两条。

## 已知差异(决定保留，删链后照此为准)

- ~~**压力触发的自动压缩**~~ / ~~**摘要的锚点**~~ / ~~**边界**~~ —— **2026-09-16 已对齐，不再是差异**
  (分支 `fix/auto-compaction`)。原来这三条写的是：树只按回合数(2)留最近内容、不调模型地把
  其余折成提问清单、下一次压缩认不出上一次摘要，并且说「原地改写旧消息在追加式日志里没有
  对应事实，要先给日志加事件」。对齐之后：
  - `compaction-coding` 直接跑链式那套 `OverflowCompaction` over `StubCompaction`,由
    `harness::plugins::compaction::decide_with_strategy` 把内核计划翻译成日志事实，内核原来
    守的不变量(保护首个请求、切点不留孤儿工具结果、净缩减守卫)在翻译层照守。
  - 日志加了 `MessagesRewritten`(逐字记下替换文本，不是规则)和 `Compacted.from`(本次压缩
    不折叠的头部;多次压缩各自隐藏 `(from, through]`,取并集，旧日志 `from=0` 投影不变)。
    格式版本 4 → 5。
  - 压力下：利用率 ≥ 阈值(0.7)折旧工具输出(`read_file` 豁免、活动回合不动);≥ 0.78 且
    判断有用时由对话模型写锚定摘要，保留首个请求和约 1/4 窗口的最近回合，下一次在上一份
    摘要上更新。每回合每档(快/调模型)最多试一次。
  - 溢出：发送前估算超过可用上限、或 provider 拒绝为过长，都走 桩 → 截断 → 摘要(拆开超长
    回合)三级;没事可做的一级不发请求。
  - 调模型的压缩开始时驱动会收到 `CompactionStarted`,即使最后什么都没提交也会收到终态
    (`compactions.is_active()` 会让 rewind 返回 Busy,所以开始必须有终态)。
  - 判据：harness `tests/compaction_strategy.rs`(6)、`tests/session.rs` 投影 4 条、
    `loop_policy.rs` 2 条 + 触发器 1 条、`recovery.rs` 发送前 1 条;coding
    `runtime_criteria.rs` 端到端 3 条。每条都摘掉被测代码证伪过。
- **工具指引的归属**:树里 fs / shell / 搜索 / codeintel / web / todo / describe_self 的提示词
  由各自的行写，措辞与链式人设里的段落不同;产品自有工具(`request_user_input` / `task` /
  `team` / `code_review`)的段落由 host-tools 带入，与链式同文。
- **`max_continuations`(offer_continuation 熔断，默认 50)**:树里会续写的两处
  (verify-cadence 行、`kernel-hooks` 上的 `TodoHook::offer_continuation`)各自一次性,
  没有需要熔断的循环，不桥接。
  > 2026-09-16 更正:这条原先写的是「verify-cadence、todo-reminder 各自一次性」。
  > `todo-reminder` **根本不是续写** —— 它是 `on_emit::<SessionEventCommitted>` 观察者,
  > 只 commit 一条随下次请求带出去的提醒，不唤醒已结束的回合。当时拿它当 `TodoHook`
  > 的替身，于是没发现 `TodoHook` 整个掉了(见 `515a70c9`)。**「某某行顶替了它」这种
  > 判断得去看那行挂在哪个事件上，不能看名字。**
- **默认配置下回合数其实被硬顶在 100，而且不问人**:`agent-loop` 行在 `bundle::INFRA`
  里带 `max_rounds = 100`,而运行时用 `{ working_dir, undo_cancelled, stream_idle_ms }`
  patch 它 —— `Op::Patch` 整块替换 config,`max_rounds` 掉回 serde 默认(也是 100)。
  链式在 `cfg.max_rounds == 0`(默认)时**根本不接内核熔断**,回合数不封顶,只靠重复守卫。
  `CODING_DEFAULTS` 里有整段注释论证过这件事、也点名了「宿主 patch 会把它打回去」,
  所以是决定不是疏漏;判据 `no_row_silently_loses_a_configured_field` 的 INTENDED
  清单里记着它。
  > **但注释没权衡的一面(2026-09-16 补)**:这道熔断在 `TurnStopping` 之后直接 break,
  > 不问任何人;而「切断前问一句要不要继续」只长在 `round-cap` 行上,默认 `max_rounds = 0`
  > 时那条检查点被跳过。于是**显式设了回合上限的人会被问，没设的人在第 100 轮被闷头砍**,
  > 理由还报成「回合用完」——正好是反的。要不要改属产品决定,记在这里而不是默默放着。
- **流静默重连次数**:链式内核自带 5 次无内容重连;树里静默是可重试错误，次数归 `llm-retry`。
- **链式 logout 不清审查/子 agent 槽位**:树里清(判据 `a_logout_leaves_no_signed_in_provider_alive`)。
