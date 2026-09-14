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

## 09-15 增量（本页其余小节按此校准）

本页写于 09-14。09-15 又落了 12 个 commit，其中三件改变了这条线的形状，
后面小节里跟它们冲突的说法都以这里为准。

```
78b4d33e..9c5e72c1   ToolStarted 缺口、会话日志顺序、五个闸门进 catalog
8c08749d  /model:探针被证伪,代码丢弃(见下面那一节,结论已被推翻)
7646cfa9  /model 在 harness 上是一次 patch,不是一次重建
fb35fbc8  /logout 之后只能重建——patch 换不回一个被拆掉的 agent
d4079e66  /logout 也留在 harness 上——凭据被拿走,agent 留下
a5a08c35  工具清单成为判据——第一次跑就报出 11 个缺口
86213ec5  fmt
d4023f8f  coding 不再继承 harness 的产品决定,自己写这份清单   ← 形状改变
7284a3d3  harness 上也有子 agent 了——task 和 team 两行都挂上
850d9e40  tool-web 变成一个能被设置的行——后端可配,离线不挂
a7875da9  本文档
c298ea08  /logout 之后凭据真的走了——provider 表只留当前那一个
8cec1282  code_review 成为一行——判据最后一个已知盲点关掉了
```

### 一、`/model` 和 `/logout` 已经在 harness 上（推翻了下面那一节的结论）

下面「`/model` 换到 harness：先别投」那一节写于探针被证伪之后，用户当时说
「丢掉」。**随后用户说「不，要做完 /model」，再后来说「不行，要做完整。
不能自动切回去」**，于是两条都做完了：

- `/model` 是一次 `App::patch`（`llm` 行换 `provider_id` + `persona-atomcode`
  换 `model`），agent 不重建，handle 不换。
- `/logout` 把 `NoProvider` 换进 `llm` 缝——**凭据离开进程，agent 留下**。
  它不会悄悄掉回链式；一个自动掉回去的逃生口等于这条线没做完。

那一节里"缺的每一样都跟换 agent 无关,是跟调用方/UI 的契约"仍然成立，
现在那些契约都由 harness 分支自己履行（`quiesce_current_agent` 取消 + 存快照，
generation 自增、`Reconfiguring` 事件、相位都照旧走）。

### 二、工具清单成为判据

`Script` 现在记下每次请求被提供了哪些工具（`tools()`）。**模型被提供了什么，
和模型被告知了什么一样是产品的一部分**，而事件流里没有任何东西会说这件事——
65 条场景全绿也发现不了，它们只调用自己碰巧要用的那几个。

第一次跑就报出 11 个差异，没有一个是"harness 做不到"，全是行没挂或行忘了
带工具。剩下 4 个是决定不是遗漏，写成带理由的明单
`KNOWN_TOOL_DIFFERENCES`，判据还反过来盯着它：任何一条悄悄不再是差异，
就要求删掉它。

**起因值得记住**：用户问 harness 上有没有子 agent，我去读代码推理，推错了。
用户说「还是你不仔细，测一下原来的引擎装配了什么东西就能判断出来」、
「要写成判据」。判据会一直说话，探针只说一次，读代码连一次都不算。

### 三、base 拆成 INFRA / DEFAULTS，coding 自己写清单

用户：「不要用 harness base，coding 完全定义呢？」→「拆出来吧」。

    bundle::INFRA     30 行,机器本身:注册表、会话日志、循环、恢复策略
    bundle::DEFAULTS  29 行,产品决定:挂哪些工具、哪个人格、审批姿态、谁渲染
    bundle::base()    仍然是两者相加,九个出厂 profile 与部署方自写的 profile
                      文件一个字都不用改

`on_harness::mount_swappable` 现在取 `bundle::infra()` + 自己的
`CODING_DEFAULTS` + `CODING_ROWS`。**这不是一份对 base 的 diff**——往
`bundle::DEFAULTS` 加一行不会悄悄到 coding 这里来。

为什么非拆不可：通用底座拿不准的每一行都出厂即关，而**产品不会把继承来的
弃权体验成一个问句，只会体验成一个本来能用的能力没了**，并且是一次一个支持
问题地发现。`ast_grep`、code graph、`open_file`、子 agent、web 五样都是人
发现的，没有一个是测试发现的。

随后按这份清单补上的：`task` / `team`（`subagent-in-process` +
`team-in-process`，出厂三个 driver 全都传 `SubagentPolicy::Enabled`）、
`tool-web`（链式 `PrepareOptions::default()` 的 `web` 从来是 true）。

### 四、rig 的盲点（已关完，只剩两个两边同关的）

差分台在链式那边关了 `mcp` / `web` / `review` / `memory`，注释写的是
"它们门住的东西对链式都是增量"。这话**对行为成立，对工具清单不成立**：
关着的那几行正好是 coding 真正打开的那几行，判据于是在两个子集之间比较。

- `web`、`subagents`、`review`：**都已在两边打开**。挂工具不产生 I/O，
  调用才会，而没有场景调用它们。`review` 是最后一个，连带补了
  `tool-code-review` 行（`8cec1282`）。
- `mcp`、`memory`：两边都关，那是避开副作用（连别人的进程 / 读开发者家目录），
  不是藏起差异。**切默认路径前 `mcp` 要么真跑一次，要么明说这个引擎没有。**

### 五、捕获了 `llm` 的行，必须跟 `llm` 一起 patch（一条会反复踩的规则）

`App::patch` 只重挂**自己那一条**配置变了的行，`Fibers::unload` 又只向
孩子级联、不向消费者级联。所以一个在挂载时从 `llm` 缝取走 provider 并
自己持有的行，`/model` 之后还在跟旧模型说话，`/logout` 之后还攥着凭据。

`swap_provider` 因此一次 patch 三行：`llm` + `persona-atomcode` +
`tool-code-review`。**新增这类行时要记得加进去**，判据是
`a_logout_drops_the_provider_object_and_not_only_the_seam`——它拿 `Weak`
问"还有没有人握着它"，摘掉任何一行都会报红。

顺带：`ProviderSlots` 原来把每个交给它的 provider 都留着（`c298ea08` 修），
于是"注销把凭据带走"这句话当时只是接近为真。**写下一句安全断言之后，
先写一条能证伪它的判据。**

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

**分支状态（2026-09-14 复查，前一版记的「远端无此分支」是错的）：**

- `origin/feat/plexus-plugin-architecture` **远端是有的**，但落后本地 25 个 commit。
- `feat/coding-on-harness-approval`（本线）**远端没有**。
- **合并方向：本线 → `feat/plexus-plugin-architecture`。**
  2026-09-14 用户明确：「这个要合回 **feat/plexus-plugin-architecture**」。

  > 上一版这里把这句话记反了（写成「合回 feat/coding-on-harness-approval」，
  > 并据此把「合回 plexus」当成错误待办删掉）。那是错的，已更正。本线是一条
  > **特性分支**，plexus 才是这条线的主干。

  09-14 已做的是**反方向的追平**（`390ea135`：plexus → 本线），目的是提前
  验证冲突面，不是交付。真正的交付还没做。

  **怎么做（快进，因为本线已是 plexus 的超集，`HEAD..plexus` = 0）：**

  ```sh
  cd /Users/lichao/project/gitcode/ai/atomcode   # plexus 在主 checkout
  git merge --ff-only feat/coding-on-harness-approval
  ```

  **不能从本 worktree 做**：`git worktree list` 显示 plexus 被主 checkout 占着，
  同一分支不能两处 check out。而且主 checkout 里另一个会话可能有未提交的工作，
  快进会在它脚下换文件——**合之前先确认那边干净**。
- 合进来的 3 个 commit 全是 tui（tip 行、右键菜单、面板浮层），**与本线零重叠**
  （本线动的是 harness + coding + `gates/differential.baseline`，他们动的是
  `atomcode-tui/*` + `gates/tui-test-count.baseline`）。合完两边一起跑：
  harness+coding 825/825、tui 466/466。
- **用户 09-14 决定：先不 push。** 所以 48 个 commit 仍然只在这台机器上。

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

### ~~一条属于 harness 的分歧~~ —— 已修（`78b4d33e`）

**曾经：每一个被拒的调用，在行式这边都会闪一下「开始了」。**

```
链式  Request approval → ToolResult error=true
行式  ToolStarted bash → Request approval → ToolResult error=true
```

`ui-handle` 在 assistant 消息落盘那一刻就为每个**已挂载**的工具合成
`ToolStarted`，比 `tools/execute-batch`、比 `tools/execute`、比任何能拒绝的东西
都早；coding 的链式是中间件放行之后才宣告，所以被拒的调用根本不会被宣告。

这不是某个产品行的问题（`execution-policy` 行第一版就是这么误判自己的，白花一轮）。

**修法不是「更聪明的猜」，是补一条事实。** 日志里本来只有「模型要求调用」
（`AssistantMessage`，闸门之前）和「调完了」（`ToolResultLogged`），中间
**「它到底跑没跑」哪儿都没有**——想区分「被拒」和「跑挂了」只能解析 content
的散文。新增 `SessionEvent::ToolStarted`，提交在 `exec.rs` 的瀑布终点（每个
监听器都放行之后），投影改从它发。这跟内核是同一个位置：`agent.rs` 那句
`events.send` 的注释写着 *"as THIS tool actually starts"*，被取消的工具
*"does not start, emits no ToolStarted"*——**回到内核已立的规矩**。

事实带整个 `ToolCall`，顺带补上另一个缺口：`tool-args-repair`（`policy.rs:36`）
和 cc-hooks 的 `updatedInput` 都会在执行前改写参数，而日志里只有模型要求的
那份。现在「真正跑的是什么」有记录了。

**代价：`SESSION_FORMAT_VERSION` 1 → 2。** 枚举是 `#[non_exhaustive]`，Rust 侧
加变体是加性的；磁盘上不是——`serde(tag="kind")` 遇到不认识的 kind 会报错，
`JsonlStore::parse` 又用 `?` 传播，旧读端会拒掉整个文件。**这个改动之后写出的
会话，旧构建打不开。** 把读端改成宽容是另一笔取舍（「猜」而不是「拒」），
留给拥有那个决定的人。

**七处分歧一起清零**：`exec_policy` 1→0、`approval_refusal` 1→0、
`cc_hooks_deny` 1→0、`refused_call_started` 1→0、`approval_write_outside` 2→0、
`grant_scope` 2→0。

### 上一版交接把两条审批场景说成「全 0」，那是因为它们没在测审批

- `approval_refusal_rows` 复用「往工作区外写」的脚本再答 `deny`，但两个引擎对
  那次写**都不问**，`deny` 从来没被消费过——它一边断言「拒绝结束的是调用」，
  一边看着一次成功的写。换成两边真会停下来问的递归 `rm` 之后，基线是诚实的 1。
  **0 不是好成绩，是没在测。**
- `approval_write_outside_rows` 的注释描述的分歧，正是 `Presence` 规则关掉的那个，
  已经过期了（名字保留，因为那个名字就是这段历史）。
- 好消息：审批缝**本身**是通的，两个引擎都发 `Request approval`、都按答案拒。

## 运行时那一半：逐条测绘（2026-09-14）

前面几节讲的是**引擎**。引擎基本到位了。真正的距离在**运行时**，而这一节是它的账。

```
runtime.rs        15,811 行,68 个 pub 方法
CodingParts       ~45 个字段
runtime 伸手进去   20 个不同的字段/方法
on_harness::mount 交出 (AgentHandle, App) —— 就这两样
```

### 三个 `assemble()` 调用点，性质完全不同

| | 做什么 | 换到 harness |
|---|---|---|
| ① spawn（`runtime.rs:1995`） | 建 agent 然后 `.spawn()` | **今天就能换** —— 两边交出同一个 `AgentHandle`，差分证的正是这个 |
| ② `/model` 切换（`:4441`） | 拿新 config **重新装配整个 agent** | `ControlSvc::patch(toml)` —— 换机制，不是换实现 |
| ③ reprepare（`:7084`） | 同上 | 同上 |

**差分台只证明了 ①。** 它没有、也不可能用现在这套场景证明 ②③：那不是「同一个引擎跑同一个脚本」，是「跑着的 agent 怎么被改」。

### 20 个句柄的去向

| 组 | 访问次数 | harness 对应物 | 判定 |
|---|---|---|---|
| **session / resume** | `session`×17、`set_runtime_resume`×5、`runtime_resume_snapshot`、`publish_staged_session`、`inherit_runtime_continuity` | `SessionSvc`(`SessionLog`)、`SessionPersistenceSvc`、`SessionDefaults` | **形状不同**。coding 传一个 `SessionSnapshot` + OS 锁 lease + `staged_fresh`（装配成功前不进目录）；harness 的 `SessionDefaults` 是 `{ id, resume: bool }` |
| **team** | `team_manager`×15 | `team` / `team-in-process` 行**存在但不在 base bundle** | 要显式插行，再把 `TeamRunManager` 的 generation / event_sender 映射过去 |
| **快照持久化状态** | `snapshot_persistence_status`×11、`snapshot_hook`×3、`take/report_*`×4、`take_cost_persistence_warning` | **没有** | 是「你这次会话可能没存住」的告警信箱。类型在 L1（`capabilities::session::snapshot::SnapshotPersistenceStatus`）可直接复用，但 harness 没有这条告警通道 |
| **模式开关** | `bypass_mode`×4、`plan_mode`×3、`accept_edits`×3 | `plan-mode` 行（base 里 `disabled = true`）；`accept_edits` 是 **mount-time 配置**；`bypass` **没有** | 见下 |
| **MCP** | `withdraw_mcp_tools`×2、`mcp_statuses`×2、`mcp_tools_for_server`、`mcp_readiness_receiver` | `McpSvc => McpRegistry` —— **同一个类型** | **机械**：改成从树里 resolve，四个都是 `McpRegistry` 的薄包装 |
| **杂项** | `register_extra_tool`、`rate_limit_source` | `ToolBox::register/unregister`、`llm-rate-limit` 行 | 机械 |

### 最关键的一条结构差异

`CodingParts` 自己的文档注释说得最准：

> Everything `assemble` composes — **and everything a respawn must REUSE so state survives**（approval grants, hook state, session identity）。

`inherit_runtime_continuity` 逐字实现了它：

```rust
self.plan_mode    = Arc::clone(&previous.plan_mode);
self.bypass_mode  = Arc::clone(&previous.bypass_mode);
self.accept_edits = Arc::clone(&previous.accept_edits);
self.approval     = Arc::clone(&previous.approval);
```

**coding 的模型是：重新装配一个新 agent，但把旧状态 `Arc::clone` 交给它。**

harness 的 `App::patch`（`plexus/src/app.rs:200`）是另一回事：

```rust
Some(new) if new.name != old.name || new.config != old.config || old.disabled => {
    self.unload_row(id);          // 状态没了
    to_mount.push(new.clone());
}
```

**config 变了的行会被卸载重挂，它持有的状态归零；config 没变的行原地不动。**

对 `/model` 切换这**比 coding 现在的做法好**：只有 `llm` 行重挂，授权、模式、会话全不受影响，而 coding 要重新装配整条链再把状态搬回去。

但反过来：任何「必须跨重配存活」的状态，harness 没有对应机制。coding 那四个 `Arc::clone` 就是这份清单。切默认路径前必须逐个回答：它是重挂就没了（可以接受），还是必须活下来（需要新机制）。

### 顺带验出来的一个洞（已在代码里确认，尚未被测量）

`ui-handle` 提供 `approval` 时用的 `Asker::decide`（`plugins/handle.rs:462`）用**参数原字节**做授权键：

```rust
let key = (call.name.clone(), call.arguments.clone());
```

它**从不调用 `Tool::always_grant_scope`**。而那五个闸门行专门算了 scope（写按**目录**、bash 按**命令**），就是为了让一次「总是允许」覆盖和链式相同的范围。在 coding-on-harness 树里（`approval` 由 `ui-handle` 提供，这是 `Presence` 设计决定的），**那个 scope 被忽略**：「总是允许这个目录」退化成「总是允许这一次调用」，下一个文件还会再问。

`NEVER_GRANT` 同理会被忽略 —— 包括 `cc-hooks` 那条「钩子强制的询问不该被记住」。

没被差分抓到，因为写那条路径在此之前**根本没问过**（见下一节）。

## 真跑起来了（2026-09-14）

`ATOMCODE_ENGINE=harness` 起的会话，**真网关认证 + 真模型 + 真工具调用**，
和链式逐字一致：

```
              链式                                harness
纯文本        pong                                同
带工具        It prints `hello from answer_42`.   同
```

在此之前 61 条差分场景**全部**是脚本 provider。

### 怎么构建（这一步卡了两次）

```sh
# 1. 私有签名 crate:clone,不要 cp。项目自己的做法在
#    ~/project/gitcode/ai/atomcode-build-official/build-official.sh
rm -rf crates/atomcode-codingplan-crypto
git clone --depth 1 <私有库> crates/atomcode-codingplan-crypto

# 2. feature 必须显式带上
cargo build -p atomcode --bin atomcode --features atomcode/codingplan-crypto
```

**新建的 worktree 一定拿到的是仓库里那份占位符**（`lib.rs` 是 tracked 的），
所以一定会 panic `request signing requires the official build`。
clone 之后 `Cargo.toml` 和 `src/lib.rs` 会显示为修改——**绝不能提交**，
用完 `git checkout HEAD -- crates/atomcode-codingplan-crypto/` 还原。

### 试用要点

- 用 `--provider <名字>` 在**启动前**定 provider。`/provider`、`/model`、`/cd`
  走的是重配置那条路，在 harness 引擎上**会掉回链式**（会打印一行说明；看到
  那行之后的读数作废）。
- `/undo`、rewind 已知是坏的（快照机器在 `parts` 里，而 harness 引擎下
  `parts` 准备了但它的 agent 从不跑）。

### 两个只有真跑才会暴露的缺陷

**1. 树和驱动各打印一遍（`96b8789c`）。** 无头模式答 "pongpong"。`trace` 行在
`base` 里默认 `stream = true`，自己 `print!` 到 stdout，而驱动方也在打印同一份。
挂了 `ui-handle` 就意味着驱动方在渲染，树不能再往终端写——overlay 里关掉。

**差分台正好盖住了它**：`quiet_rows` 为了让测试输出干净，自己把 `trace` patch 掉了。

**2. 会话日志不按顺序落盘（`a645cd87`，既有问题，非本线引入）。**
持久化对每条事件 `tokio::spawn` 一个任务，它们互相赛跑；seq 在提交时按序分配，
落盘顺序由调度器决定。而回读的三步都不排序（`parse` 按文件序 → `restore`
原样存 → `derive_messages` 原样折叠），**resume 会重放出错乱的历史**。

磁盘上量过 274 个 jsonl：**132 个的模型可见事件乱了序**，绝大多数是 v1
（本分支之前的构建写的，主要是 atui 那条线）。改成一个写入器 + mpsc 队列。

> **这两条合起来是一条教训：测试为了干净而关掉的东西，可能正是要测的东西。**
> 本会话四次「绿着的场景其实没在测」，只有这两次是靠真跑才发现的。

### 写并发测试的人注意

持久化那条测试第一版**没有牙**——把 bug 放回去它照样绿。因为 `#[tokio::test]`
默认是**单线程** runtime，spawn 出来的任务按 FIFO 跑完，竞态根本不出现。
改成 `#[tokio::test(flavor = "multi_thread")]` 立刻红。真实二进制是多线程的，
测试的 runtime 也必须是。

## `/model` 换到 harness：探过了，结论是先别投（2026-09-15，**已被推翻**）

> ⚠️ **这一节的结论不再成立，保留是因为过程有用。** 用户随后说「不，要做完
> /model」，`/model` 与 `/logout` 都已经落在 harness 上（`7646cfa9`、`fb35fbc8`、
> `d4079e66`）。见上面「09-15 增量」第一条。下面记的是**为什么第一次估错**，
> 那部分仍然值得读。


**判断（我的）**：`runtime.rs` 那 279 行重装配（停 agent、验终结、fail-close 未决
请求、重建、generation 自增、恢复快照）之所以是 279 行，是因为**链式必须把一个
`AgentHandle` 换成另一个**；harness 上 handle 不换，`agent-loop` 每回合现取
`LlmSvc`，所以换掉缝后面那个 provider 就是 `/model` 的全部，~15 行。

**探针**（`SwappableProvider` + `mount_swappable` + 一个分支，已丢弃）**证伪了它**：

```
基线              433/439
探针 v1           430/439
探针 v2(补契约)    432/439     ← 61 行代码,仍不如基线
```

**修好 0 个，弄坏 1 个**（`provider_reassemble_preserves_the_latest_sessionless_snapshot`）。

机制那半是对的：分支确实跑了，provider 确实换了，`reassemble_provider` 返回了对的
generation，`context_stats` 报出新模型。**错的是「~15 行」**——缺的每一样都跟换
agent 无关，是**跟调用方/UI 的契约**：

| 漏掉的 | 谁在依赖 |
|---|---|
| `generation` 自增 | 不只是过滤陈旧事件,**是返回给调用方的回执** |
| `controls.state` 存相位 | `handle.status().generation` |
| `Reconfiguring` 事件 | UI 依赖四个事件的顺序 |
| `preserve_sessionless_snapshot` | 无会话运行的快照 |

**两条更正，都是我之前分簇错了：**

1. 那唯一失败的 reassemble 测试（`..._updates_cost_attribution_...`）**根本不是关于
   重装配的**——它读 coding 的 `SessionManager` 算成本，而 harness 引擎从不喂那套。
   它属于快照/会话那一簇。真实分簇是 **6 个快照/会话 + 0 个 reassemble**。
2. **`/model` 在 harness 引擎上本来就能用**——它落到链式分支重建了一个链式 agent，
   测试全绿。那行「静默掉回链式」的警告描述的是一个**功能正确、只是没在 harness 上
   跑**的路径，不是一个坏掉的功能。

**所以：`/model` 的收益比看上去小得多，不是下一步。** 真正卡住的是快照/会话那 6 个。

（探针里那个 `Box::leak` 是为绕开 `LlmProvider::model_name(&self) -> &str` 编的。
真要做原生 `/model`，正路是让 `llm` 行从**配置**构造 provider，于是 `/model` 变成一次
普通的 `patch`——代价是把 coding 的 `provider_factory`（认证、CodingPlan、子 agent
分层、视觉探测）搬进树里。那笔投资等快照那一簇做完再谈。）

## 已定的决策（不要重议）

**技能目录内联，不建开关（2026-09-14 用户拍板）。** 通用 harness 的 `skills` 行
只给一句指针（113 字节），coding 内联整份目录（1044 字节固定引导语 + 列表体，
预算上限 `CATALOG_BYTE_BUDGET = 8000`，最坏约 9 KB）。`skill-catalog-inline` 行
保持内联，并**故意用通用行同一个 fragment id 贡献**，让目录顶掉指针而不是并排。

三条理由，第二条是量的时候撞见的：

1. 它是**系统提示片段**，属缓存前缀——有 prompt cache 时 9 KB 是一次性代价，
   不是每次请求都付。
2. **L1 已经站在内联这边了。** `capabilities` 里有四段面向模型的文字写着
   「看系统提示里的 `=== AVAILABLE SKILLS ===`」——`skills/use_skill.rs:37,46`
   和 `tools/web_fetch.rs:79,264`。而通用 harness 只给指针，**那个 section
   根本不存在**。这是既有的不一致，与本线无关，但它说明落单的是那句指针。
3. 反悔是**对称**的，且没有结构性锁定：两个方向都只是改 7 段散文
   （上面四段 + `coding/skill_first.rs:59` + `persona.rs:386,486`），
   `grep "AVAILABLE SKILLS"` 全找得到。加 `mode` 开关只能切「行」，切不动那
   7 段文字——维护两套措辞是长期成本，而切换需求没人提出过
   （参见 [[feedback-over-engineering]] 那次「为了以后好换造脚手架」的失败）。

真实反悔成本 = 半天改字 + **一轮跨模型验证**（提示词改动不能只在一个模型上
dogfood，见 `feedback_cross_model_verify`）。

**一个没人验过的角落（内联路线自己的洞）：** 装了几十个技能的用户会顶到 8000
字节预算，那时按 `source_rank` 截断。截断之后 `skill-first` 那句「如果匹配目录里
某条描述，你必须调 `use_skill`」还成不成立？目录里已经没有那条了，而催促还在催。
没测过。


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
- ~~`cargo nextest run -p atomcode-harness` 全量是红的~~ —— **09-14 复查已全绿
  （292/292）**。那批 `entry 'ui' names plugin 'ui-quiet'` 的报错来自另一条线
  当时未提交的工作，从未进过本 worktree。
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

1. ~~`ui-handle` 的 `ToolStarted` 缺口~~ —— **已修（`78b4d33e`）**，见上。
2. ~~五个闸门行进 `plugins::catalog()`~~ —— **已做（`9c5e72c1`）**。
3. **快照 / 会话那 6 个**，这是现在唯一挡在「切默认路径」前面的东西：

   ```
   failed_sessionless_restore_rolls_back_to_the_original_snapshot
   native_undo_rollback_persistence_failure_is_sticky
   provider_reassemble_updates_cost_attribution_and_failed_reload_keeps_current_model
   rewind_catalog_and_conversation_scope_are_runtime_owned
   runtime_replays_a_safe_recovered_prompt_through_a_normal_turn
   undo_preserves_snapshot_identity_and_reassembles_sessionless_runtime
   ```

   共同的形状：它们读 coding 自己的 `SessionManager` / 快照，而 harness 引擎
   从不喂那一套（harness 有自己的 JSONL 日志）。**先判清每一条是「行式缺能力」
   还是「测试测的是链式的内部表示」**，别一上来就补代码——`list_sessions`
   那条已记在 `KNOWN_TOOL_DIFFERENCES` 里，同名不同物。
4. **`/cd`（`Reprepare` 分支）仍然是链式独有。** `/model`、`/logout` 都已落地，
   这是最后一条还会掉回链式的运行时命令。
5. ~~`tool-code-review` 行~~ —— **已做（`8cec1282`）**，rig 两边的 `review`
   都打开了。
6. **切 `build_coding_agent` 的默认路径**，留一个逃生开关（`ATOMCODE_ENGINE=chain`，
   现在的默认值反过来）。切之前仍然要注意：
   - `datalog` / `cc-hooks` 两行**刻意不在 `CODING_DEFAULTS` 里**：前者往用户
     磁盘写每一次请求的完整记录，后者跑用户自己的外部命令。对它们来说挂载即启用。
   - `mcp` 行**在清单里但是关着**（链式出厂 `mcp: true`）：连别人的进程是
     副作用，且这一行还没在这个引擎上对着真 MCP server 跑过。切默认路径前
     要么跑一次把它打开，要么明说这个引擎暂时没有 MCP。
7. 阶段二：把 `CODING_DEFAULTS` / `CODING_ROWS` 两个常量变成可配置数据。
   **拆完之后这一步小了很多**——`CODING_DEFAULTS` 本身已经是一份清单而不是
   一串补丁，剩下的是让它从文件读。

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
cargo nextest run -p atomcode-coding --test differential   # 67/67,裁判
cargo nextest run -p atomcode-coding                       # 539/539(链式)
ATOMCODE_ENGINE=harness cargo nextest run -p atomcode-coding  # 533/539
cargo nextest run -p atomcode-harness                      # 296/296
cargo nextest run -p atomcode-tui                          # 466/466
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
