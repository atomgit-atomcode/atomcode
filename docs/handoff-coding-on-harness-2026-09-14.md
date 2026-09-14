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
```

**两个分支都没 push。** 第二个分支尚未合回第一个。

## 已经证明了什么

`gates/differential.baseline` 是唯一的裁判，只准降。当前：

```
*_prod         coding 生产链 vs 通用 harness 树     6 条,全 0
*_rows         coding 生产链 vs coding 装在 harness 23 条
                 其中 20 条为 0
                 compact_rows=1 / truncated_rows=2 / truncated_redump_rows=2
                 —— 与最小路径的基线逐个相等
approval_*     审批,两侧都开着跑                     2 条,全 0(曾是 2)
verify_cadence_* 自纠错回路,三态(headless / attended / 人禁了命令) 全 0
```

差分台 53/53，coding 全量 523/523。

`*_rows` 那三个非零值**不是新分歧**：最小路径本来就是 compact=1、truncated=2、
truncated_redump=2，一模一样。也就是说**生产那条中间件链没有引入任何新偏离**，
行清单对两个参考的偏差是同一处、同一大小。三处都在测试里写了为什么是良性的。

**仍然没有覆盖到的**：多 agent 编排、真 provider、session 持久化落盘
（`session-persistence-jsonl` 在差分里是关掉的）。

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
| `TurnExecutionPolicy` | 每回合的用户执行边界(「不要跑任何命令」) | 未补 |
| `SkillFirstHook` | 先用 skill 的推动 | 未补 |
| `DatalogHook` | 落盘的 transcript(hook + middleware 各一半) | 未补 |
| `CCExternalHooks` | 用户的 `hooks.json` 外部钩子 | 未补 |
| `GitPushLabelMiddleware` | — | 够不着,在 `atomgit` feature 后面,开它要拉 reqwest+auth |
| `PermissionRuleGate` | — | 不用搬,harness `permissions` 行已自实现 |

1. **`TurnExecutionPolicy` 换壳** —— 现在最该做的一个，因为它是**安全边界**：
   生产链把它注册在「每一个可能 `Allow` 的中间件之前」。形状很顺：
   `pre_request` → `AgentRequest` 瀑布的 `next.run` 之前读 `req.messages`；
   `before` → `Waterfall<ToolsExecute>`，挡掉策略禁止的 bash。一行注册两处，
   共享一个 `Arc`。判断函数 `execution_policy_for_messages` 已经收 `&[Message]`。
   **写场景的办法**：让脚本在被禁的前提下去跑 bash，比较两边是否都拒。
2. **`SkillFirstHook` / `DatalogHook` / `CCExternalHooks`** —— 都不是审批闸门。
   `cc-hooks` 要给 harness 开一个 feature（只需 `tools`+`dirs`+`tokio/process`，
   代价接近零）。`DatalogHook` 的观测价值是实打实的（memory 里那条
   「TUI 看不到 ≠ 没生效，先 grep datalog」说的就是它）。
3. **然后才切 `build_coding_agent` 的默认路径**，并留一个逃生开关（参照当初
   `--engine v1`）。
4. 阶段二：把 `on_harness.rs` 里的 `CODING_ROWS` 常量变成可配置数据。

### 补 VerifyCadence 时学到的、下一条也用得上的

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

## 怎么验证

```sh
cargo nextest run -p atomcode-coding --test differential   # 53/53,裁判
cargo nextest run -p atomcode-coding                       # 523/523
cargo nextest run -p atomcode-capabilities                 # 909/909
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
