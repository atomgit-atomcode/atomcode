# 交接：coding 垫到 harness 上，2026-09-14

> 🟢 **接手先读这一页，再读代码。** 上一份交接（`handoff-plexus-line-2026-09-12.md`）
> 到 09-14 已经过期（它说 103 个 commit / 判据基线 352，实际是 118 / 423），别拿它的
> 数字校准。这一页只覆盖「coding 垫到 harness 上」这条线。

## 目标（用户 09-14 确认的两阶段）

1. **先让 harness 成为 coding 的 agent** —— 换掉 coding 自己那 5.3k 行手写装配
   （`parts.rs` + `assemble.rs`），`runtime.rs` 那 15.8k 与 tuix 一行不改。
2. **再开放 coding 的装配能力** —— 行清单从常量变成可配置数据，产品 A 与产品 B
   换 TOML 不换 Rust。

阶段一二做完，**Host 那一层仍然不存在**（ADR 0017/0018 是文档，用户给 atui 的
八条至今零条进代码）。这一点在动手前对齐过，不是遗漏。

## 为什么这条路走得通（最重要的一条事实）

`CodingRuntimeHandle` 与它的引擎之间**只有一对通道**：8 个 `AgentCommand` 进、
25 个 `AgentEvent` 出。而 harness 的 `ui-handle` 行交出来的，正是
`Agent::spawn()` 交出来的那个 `atomcode_kernel::agent::AgentHandle`。

**类型级对齐 —— 这是替换，不是适配。** 差分台用同一个 `drive_until(handle)` 同时
驱动两边，就是这条的现场证明。

（注意：ADR 0013 那句「coding 40 个 pub 方法 vs harness 8 个 AgentCommand，差 28
个」说的是 coding 对 **tuix** 的外向 API，不是它对引擎的接口。拿它估工作量会大幅
高估。）

## 现在在哪

```
分支 feat/plexus-plugin-architecture   本地未推 22 个
  c431fa6f  coding 装成行清单,链式与行式逐事件一致      ← 阶段一正题
  b084b184  三个要问人的闸门换壳
  54b662aa  第二个中间件换壳(artifact)
  290aa090  第一个闸门换壳(open_file workspace)
  006266cd  差分台补生产装配 + 修跨进程锁
  e4b75098  解开 harness → coding 依赖

分支 feat/coding-on-harness-approval   worktree: .claude/worktrees/approval-diff
  6504f4ca  审批差分上线,找出一处真分歧
  2f59c73b  Presence 规则,分歧 2 → 0
  f2f5be3e  本文档
  9124aed2  裁判铺宽:3 条 → 20 条
  9471d802  自纠错回路补成行(verify-cadence)
  4d63c0c6  催检查不能盖过人的话
  79988b53  修正本文档"下一步"(原来漏了三项)
  52e912d3  执行边界补成行 + 审批那两条根本没在测审批
  4c973541  交接同步
  3a6f7a6f  技能:目录 + skill-first
  e2fdfad4  datalog 补成行
  9c923012  用户的 hooks.json 补成行
```

**生产链上没有对应行的东西,现在只剩一个**:`GitPushLabelMiddleware`,
在 `atomgit` feature 后面(开它要拉 reqwest + auth,不值得)。

**两个分支都没 push。** 第二个分支尚未合回第一个。

## 已经证明了什么

`gates/differential.baseline` 是唯一的裁判，只准降。当前：

```
*_prod         coding 生产链 vs 通用 harness 树     6 条,全 0
*_rows         coding 生产链 vs coding 装在 harness 23 条
                 其中 20 条为 0
                 compact_rows=1 / truncated_rows=2 / truncated_redump_rows=2
                 —— 与最小路径的基线逐个相等
approval_*     审批,两侧都开着跑                     2 条
verify_cadence_* 自纠错回路,三态(headless / attended / 人禁了命令) 全 0
exec_policy_rows 人划的执行边界                        1(见下)
refused_call_started_rows                             1(见下,这条就是为它开的)
```

差分台 53/53，coding 全量 523/523。

`*_rows` 那三个非零值**不是新分歧**：最小路径本来就是 compact=1、truncated=2、
truncated_redump=2，一模一样。也就是说**生产那条中间件链没有引入任何新偏离**，
行清单对两个参考的偏差是同一处、同一大小。三处都在测试里写了为什么是良性的。

**仍然没有覆盖到的**：多 agent 编排、真 provider、session 持久化落盘
（`session-persistence-jsonl` 在差分里是关掉的）。

### 一条属于 harness 的分歧（`refused_call_started_rows=1`）

**每一个被拒的调用，在行式这边都会闪一下「开始了」。**

```
链式  Request approval → ToolResult error=true
行式  ToolStarted bash → Request approval → ToolResult error=true
```

`ui-handle` 在 assistant 消息落盘那一刻就为每个**已挂载**的工具合成
`ToolStarted`，比 `tools/execute-batch`、比 `tools/execute`、比任何能拒绝的东西
都早；coding 的链式是中间件放行之后才宣告，所以被拒的调用根本不会被宣告。

这不是某个产品行的问题（`execution-policy` 行第一版就是这么误判自己的，白花一轮）。
`handle.rs` 已经守了隔壁那个 case——「没挂载的工具不宣告」，注释里说那也是差分
发现的——所以**修法的形状是现成的，且属于 `ui-handle`**。两条基线携带它
（`approval_refusal_rows`、`exec_policy_rows`），`refused_call_started_rows`
这条场景专门认领它，修好时把三条一起降。

### 上一版交接把两条审批场景说成「全 0」，那是因为它们没在测审批

- `approval_refusal_rows` 复用「往工作区外写」的脚本再答 `deny`，但两个引擎对
  那次写**都不问**，`deny` 从来没被消费过——它一边断言「拒绝结束的是调用」，
  一边看着一次成功的写。换成两边真会停下来问的递归 `rm` 之后，基线是诚实的 1。
  **0 不是好成绩，是没在测。**
- `approval_write_outside_rows` 的注释描述的分歧，正是 `Presence` 规则关掉的那个，
  已经过期了（名字保留，因为那个名字就是这段历史）。
- 好消息：审批缝**本身**是通的，两个引擎都发 `Request approval`、都按答案拒。

## 已定的决策（不要重议）

| 决策 | 理由 / 出处 |
|---|---|
| 依赖方向 `coding → harness`，harness 对 coding 零引用 | 反向边会成环；差分台已搬到 coding 侧 |
| 闸门换壳 = **一个判断、两个外壳** | L1 出中立裁决函数，kernel middleware 与 harness 行各调一次；两个"今天一致的判断"比一个判断更坏 |
| `Decision` 保持两态，不加 `AllowAlways` | "always" 已由审批行消化；暴露给调用方等于让每个闸门重建一个授权存储，正是要消掉的形态 |
| 不可授权的表达在**提问侧** | `AsRisky { scope, grantable }`；给了 always 按钮再让它失效，是在告诉人一件关于自己权限的假话 |
| **有人在就问，没人在就拒** | `on_harness::Presence::{Attended, Headless}`。Attended 不设 fs root（不围栏）→ 走审批；Headless 设 root → 围栏拒绝 |
| `Headless` 只拥有围栏，"从不问"归前端 | 挂了 `ui-handle` 就意味着有驱动在，**按定义就是 attended** |
| 行清单放在 coding 不放在 harness | harness `PROFILES` 注释：产品特化不进那张表，否则那个 crate 会变成四个产品悄悄分叉的地方 |

## 踩过的坑（每条都花了一轮）

**装配缝的三次碰撞**（审批差分时连撞三下，每一下都是树在说话）：
1. `bundle::INTERACTIVE` 会连带开 `user-questions-unattended`，它抢走
   `user-questions` 缝，`ui-handle` 的 `Asker` 挂不上。
2. 改成只开 `approval-interactive` —— 又和 `ui-handle` 抢 `approval` 缝本身。
3. 正解：把 base 那条 `approval`（`deny-risky`，只拒不问）**关掉**。
   **「关掉审批行」恰恰是「开始问人」**，因为被关掉的是那条从不问的策略。

**空 grant scope 早有含义。** `tools/edit.rs:102`、`tools/parallel_edit.rs:157`
返回空表示*工具级*授权（本会话所有编辑都允许）。第一版拿它当"不可授权"信号，
撞车判红了一个既有测试。现在用 `seams::NEVER_GRANT`（含 NUL）。

**行放行后必须标 `pre_approved`**，否则它后面的普通审批会把同一个问题再问一遍
（测试里表现为一次写入两张卡片）。kernel 那侧靠 `BeforeOutcome::Allow` 短路。

**`Reply::call` 要 `&'static str`。** 差分台里造工具调用参数不能用 `format!`，
用字面量；相对路径（`../x`）反而更贴近模型真实产出。

**棘轮的锁必须跨进程。** `ratchet()` 原来用 `static Mutex` 护着
`gates/differential.baseline` 的读-改-写 —— 那只在 `cargo test`（同进程多线程）
下成立。本会话把 runner 换成 nextest（每测试独立进程）之后那把锁失效了，已改成
fs2 文件锁，锁基线文件本身。

## 阻塞与外部状况

- **五个新行都还没进 `plugins::catalog()`。** 那个文件（`harness/src/plugins/mod.rs`）
  正被另一条线重写（把 UI 行移出 catalog，ADR 0018 §5）。`git commit -- <path>`
  取工作区内容，提交它会把别人没写完的改动卷走。目前由 `on_harness::mount` 与
  测试显式注册。**那边落地后，通用的几个应移进 catalog。**
- **`cargo nextest run -p atomcode-harness` 全量是红的**，7 个测试文件
  （composition / concurrency / front_ends / opener / profile / recovery /
  seam_convention），报错都是 `entry 'ui' names plugin 'ui-quiet', which is not in
  the registry` —— 对应上面那条未完成的改动，**与本线无关**。
- `gates/differential.baseline` **没有任何地方在读它**（gates/、CI、Makefile 全仓
  grep 不到消费者）。棘轮跑在测试里，门口没人拦。接上是一行的事。

## 下一步（建议顺序）

> **这一节 09-14 重写过一次。** 原来的第 2 步写「`DatalogHook` / `CCExternalHooks`
> —— 生产链上仅剩的两个」，**这句是错的，别照着它估工作量**。照单核对
> `parts::assemble` 的 15 个 `ToolMiddleware` + 13 个 `LifecycleHooks` 之后，
> 真正没有对应行的是**五个**，其中最要紧的一个当时根本没被点到。

### 生产链还缺的行（逐条核对过，2026-09-14）

| 缺的东西 | 是什么 | 状态 |
|---|---|---|
| `VerifyCadenceHook` | 改了代码不检查就走 → 补一轮追问 | **已补**(`9471d802`) |
| `TurnExecutionPolicy` | 每回合的用户执行边界(「不要跑任何命令」) | **已补**(`52e912d3`) |
| `SkillFirstHook` | 先用 skill 的推动 | **已补**(`3a6f7a6f`) |
| `SkillCatalogHook` | 整份技能目录进系统提示 | **已补**(`3a6f7a6f`,原表漏了这条) |
| `DatalogHook` | 落盘的 transcript(hook + middleware 各一半) | **已补**(`e2fdfad4`) |
| `CCExternalHooks` | 用户的 `hooks.json` 外部钩子 | **已补**(`9c923012`) |
| `GitPushLabelMiddleware` | — | 够不着,在 `atomgit` feature 后面,开它要拉 reqwest+auth |
| `PermissionRuleGate` | — | 不用搬,harness `permissions` 行已自实现 |

1. **`ui-handle` 的 `ToolStarted` 缺口**（上面那节）。属于 harness，不属于这条线，
   但切默认路径前该有人看一眼：它影响每一条拒绝路径，现在有三条基线带着它
   （`approval_refusal_rows`、`exec_policy_rows`、`cc_hooks_deny_rows`）。
2. **五个闸门行进 `plugins::catalog()`**，等另一条线放开 `plugins/mod.rs`。
   现在由 `on_harness::mount` 显式注册，能跑，但不该长期这样。
3. **然后才切 `build_coding_agent` 的默认路径**，并留一个逃生开关（参照当初
   `--engine v1`）。切之前要先答的两个问题，都不是技术问题：
   - **技能目录 vs 指针**。coding 内联整份目录，通用 harness 只报数量 + 让你
     `list_skills`。`skill-catalog-inline` 保持了 coding 今天的行为（内联），
     因为这条线的前提是「上面什么都不变」——但那是每次请求都要付的 token，
     值得有人明确拍一次板。
   - **`datalog` / `cc-hooks` 两行都刻意不在 `CODING_ROWS` 里**：前者往用户磁盘
     写每一次请求的完整记录，后者跑用户自己的外部命令。对它们来说挂载即启用。
     切默认路径时别顺手把它们加进去。
4. 阶段二：把 `on_harness.rs` 里的 `CODING_ROWS` 常量变成可配置数据。

### 抽行时反复用上的几条

- **harness 没有 `offer_continuation`，但续问是做得到的**，而且
  `truncation-recovery` 行早就在这么干：`agent/request` 看刚跑完那一轮，
  要续就往 agent 的 inbox 里 `send_from(text, MessageOrigin::Harness)`。
  循环判「回合结束了没」时重读的正是 `inbox().has_waking_input()`。
- 用 `MessageOrigin::Harness` 入队，会记成 `InjectionOrigin::Continuation`，
  渲染成一条 **synthetic user 消息** —— 跟 `offer_continuation` 产出的完全一样，
  所以 `verify_reminder_already_present` / `current_real_user_start` 原样可用。
- **瀑布注册用 `prepend = true`（最外层）**，这样看到的是*结算后*的那一轮：
  限流等待、重试、溢出裁剪、截断续写都发生在它里面。
- 判断抽成中立函数（`&Conversation` → `&[Message]`），跟五个闸门同一个手法。
  两种形状问同一个问题、发同一句话，纪律不会在两套装配之间漂。
- **负对照是必须的**：只测「headless 两边都追问」等于没测，一个从不追问的行清单
  也能通过 attended 那一半。三态都要写:headless 追、attended 不追、人禁了命令
  也不追。第三条上线时先验过「把判断摘掉测试会红」才算数。
- **一条 0 分歧的场景，先确认它真的触发了它要测的东西。** 上面那两条审批场景
  就是反例:绿了一整条线，因为两边都没走到审批。看一眼 render，确认该出现的事件
  （这里是 `Request`）真的在，比多写三条场景管用。这个错本会话又犯了一次:
  cc-hooks 的第一版夹具写成 `.claude/settings.json` 配 CC 的嵌套数组格式，
  两个引擎都没找到文件，分歧 0，全绿。真正的路径是 `<project>/.hooks.json`。
- **`cargo nextest run -p <crate>` 不等于编译了整个 crate。** 先前报的
  「capabilities 909 全绿」根本没编译 `datalog.rs` —— 它在
  `#[cfg(feature = "session")]` 后面。带 feature 跑是 1124 条。
  验证命令那一节已按 feature 分开写。
- **差分台能判的东西比事件流多，但要先给它眼睛。** 临时请求尾巴（skill-first
  这类）不落日志、不发事件、快照也照不出来 —— `transcript` 那条辅助函数会对
  两个发给模型完全不同内容的引擎报「一致」。现在 `Script` 记下每次请求
  （`seen()`），也可以自称是别的模型（`as_model`），因为真实行为按模型名分支。
  datalog / cc-hooks 两条则直接按**文件**和**子进程副作用**判。

## 怎么验证

```sh
cargo nextest run -p atomcode-coding --test differential   # 60/60,裁判
cargo nextest run -p atomcode-coding                       # 531/531
cargo nextest run -p atomcode-capabilities --features session   # 1124/1124
cargo nextest run -p atomcode-capabilities --features cc-hooks  # 939/939
cargo nextest run -p atomcode-harness --test policy_rows   # 24/24(全量会红,见上)
git status --short gates/                                  # 基线只准降,不准手改
```

**用 `cargo nextest`，不要 `cargo test`** —— 约定与实测数字见 `AGENTS.md` 的
「测试与构建命令」节。同一份 kernel 测试 `cargo test` 570s，nextest + 虚拟时钟
0.64s。

## 本会话的另一条线（已推，供参考）

开发循环提速，五个提交已在 `origin/feat/plexus-plugin-architecture`：nextest 替换
`cargo test`、11 处 tokio 虚拟时钟、`[profile.dev.package."*"] debug = 0`、闸门与
CI 换 runner、约定写进 `AGENTS.md`。kernel 570s → 0.64s，harness 14.8s → 4.0s。
