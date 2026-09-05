# 一切皆插件：把 deepseek-harness 的 cordis 模式搬到 AtomCode

> 状态：**可运行的 spike**，分支 `feat/plexus-plugin-architecture`。
> 现有 crate 一行未改；两个新 crate 与旧装配并存，可直接对比。

---

## 一、30 秒看到它跑起来

```sh
# 不需要 API key：`llm` 那一行被换成脚本化的假 provider
cargo run -p atomcode-harness --bin harness -- --offline "看看这个目录"

# 打印实际挂载的插件树、fiber 状态、被填上的服务槽
cargo run -p atomcode-harness --bin harness -- --offline --dump-config

# 真实模型（真实 OpenAI 兼容适配器，就是 atomcode-capabilities 里那个）
ATOMCODE_API_KEY=… ATOMCODE_BASE_URL=… ATOMCODE_MODEL=… \
  cargo run -p atomcode-harness --bin harness -- "修一下构建"
```

随附 9 个 profile。每个都是「有序 bundle 列表 + 自己的 patch」，不是代码分支：

```sh
harness --list-profiles

profiles:
  embed      no front end; a library caller drives
  full       everything on: code graph, web access, delegation
  headless   one prompt, no rendering, no persistence — for evals and CI
  oneshot    one prompt, one turn, exit
  plan       read-only exploration: investigate and produce a plan
  repl       an interactive terminal session that can ask questions
  sdk        line-delimited JSON-RPC on stdio
  tui        a full-screen terminal UI
  web        an HTTP server with a live event stream

bundles: base, embed-app, oneshot-app, repl-app, sdk-app, tui-app, web-app

layer order: bundles -> the profile's patch -> ~/.atomcode/harness.patch.toml -> --patch overlays
```

四个前端，同一个 agent：

```sh
harness --profile repl                  # 行式终端会话，能问人、能 steering
harness --profile tui                   # 全屏终端 UI
harness --profile web                   # HTTP + SSE 事件流 + 一个页面
harness --profile sdk                   # stdio 上的 JSON-RPC，给程序用
```

`sdk` 那条真的跑起来长这样：

```
$ echo '{"jsonrpc":"2.0","id":2,"method":"agent/send","params":{"text":"look"}}' | harness --profile sdk
{"jsonrpc":"2.0","method":"session/event","params":{"event":{"kind":"user_message",…},"seq":3}}
{"jsonrpc":"2.0","method":"session/event","params":{"event":{"kind":"step_start","step":1,…},"seq":4}}
{"id":2,"jsonrpc":"2.0","result":{"steps":3,"stop":"Stopped","tool_calls":2,"turn":1,…}}
```

叠加层跟 profile 正交，任意组合：

```sh
harness --profile web --plan            # 浏览器里的只读探索
harness --profile sdk --read-only       # 只读世界里的 JSON-RPC
harness --profile tui --full            # 全屏 UI + 代码图 + 联网 + 委派
harness --profile repl --patch mine.toml
```

## 二、现在的规模

| | dsh | 这个 spike |
|---|---|---|
| 服务缝 | 68 | **20** |
| 插件 | 250 包 | **48** |
| 事件 | 三个事件域 | **11** |
| bundle / profile | 9 bundle + 5 profile 模板 | **7 bundle + 9 profile** |

20 个缝：

```
agents  agent-loop  ui                                   agent 注册表 / 循环 / 前端
llm  tools  system-prompt  approval                      模型 / 工具 / 提示词 / 审批
sessions  session-projections  session-persistence       会话事件日志 + 投影 + 持久化
session-title  compaction                                标题 / 压缩
fs  subprocess  shell                                    执行世界
skills  code-index  mcp  user-questions  subagents       能力 / 索引 / 外部服务器 / 人类问答 / 委派
```

48 个插件全部可 patch、可禁用、可替换。**包括前端**：launcher 不跑 turn，它解析 profile、挂树、拿 `ui`、交出控制权。

## 三、三角色约定：定义 / 提供方 / 消费方

dsh 的核心工程约定是：**一项能力不是一个包，是三个角色**——声明接口的 **定义**、填槽的 **提供方**、按 key 读它的 **消费方**。它用一张策展表 `SERVICE_ROLES` 记录，再用生成脚本从源码交叉校验，`--check` 保证文档不漂移。

Rust 能做得更彻底：**分类直接长在接口定义上**，不需要外部表。

```rust
plexus_service!(LlmSvc => dyn LlmProvider, "llm", Seam, "Model adapter");
plexus_service!(ToolsSvc => ToolBox,       "tools", Core, "The live tool catalog");
```

三种 `SeamMode` 和 dsh 一致：`Core`（主干，换它不是用例）/ `Seam`（可替换）/ `Bundle`（组合点）。插件侧声明自己的角色：

```rust
fn inject(&self)   -> &[&str]   // 硬依赖：等它就绪才激活
fn uses(&self)     -> &[&str]   // 软依赖：有就读，没有就降级
fn provides(&self) -> &[&str]   // 填哪些槽
```

`uses` 是 `inject` 之外必需的一格：agent-loop 会读 `system-prompt` 和 `session-projections`，但没有它们照样跑。只认 `inject` 的能力图会对半个树都在调用的服务写「无人消费」。

### 能力图从代码生成

```sh
harness --dump-seams            # 表格
harness --dump-seams-mermaid    # 和 dsh 同形状的 mermaid 图
```

```
Seams — replaceable capabilities
  llm  — Model adapter
    provided by: llm-openai-compat, llm-replay
    consumed by: agent-loop, compaction-tail (optional)
  fs  — One execution world's view of files
    provided by: fs-local, fs-readonly
    consumed by: tool-fs-world
  compaction  — History compaction strategy
    provided by: compaction-tail
    consumed by: compaction-tail (optional)
    note: declared a seam but has fewer than two providers
```

最后那行提示是刻意的：**一个只有单一提供方的「缝」只是名义上的缝**，图会说出来，而不是让人在真去换的时候才发现。

### 约定是被强制的，不是被记录的

```sh
harness --audit    # 把每一行的声明和运行中的树对照
```

它抓四类不一致：声明了 `provides` 却没填槽（半挂载——缝看起来配好了其实没有）；填了槽却没声明（能力图看不见它）；`inject`/`uses` 一个没人能提供的服务（拼写错误或漏了行）；提供了却没人读（死重量）。

**这一层立刻发现了两个真实缺陷**：默认树里挂着一个没人消费的 `user-questions` 行；`--native-tools` profile 禁用了 fs/shell 的唯一消费者却留着三个提供方空转。两个都已修。11 个测试钉住这套约定，包括两个故意撒谎的插件（声明不填 / 填了不声明），验证 audit 真的抓得到。

## 四、两个可组合性轴

**时间可组合性**——同一个系统在不同时刻可以是不同的东西：

| 能力 | 实现 |
|---|---|
| 副作用可逆 | fiber 账本逆序回放，卸载级联到子 fiber |
| 运行中热替换提供方 | `App::patch`，消费者不感知（服务每次调用重新解析，不缓存） |
| 依赖驱动激活 | fixed-point，行顺序无关 |
| **未满足的行持续等待** | `App` 保留 pending 队列，后续 patch 供上依赖就自动补挂 |
| 同一段计算可重入 | `Next` 是 `Copy`，中间件能重试下游 |

**空间可组合性**——同一时刻多个组件并存互不干扰：

| 能力 | 实现 |
|---|---|
| realm 树 | `realm.rs`，服务表和事件总线**共用同一棵树** |
| 服务槽按 realm 隔离 | 沿父链查找，子覆盖不扰动父 |
| **监听器按 realm 隔离** | `EventBus` 每个注册带 realm，分发按可见性过滤 |
| 子插件可挂进独立 realm | `Context::plugin_isolated` |

### 可见性只朝一个方向

这是整个设计的关键。查找**向上**走：子 realm 看得见自己定义的 + 所有祖先定义的；父看不见子添加的任何东西。

- 根上装的策略对每个 agent 都生效——**凭据门不能靠 spawn 一个 subagent 绕过**；
- agent 装的任何东西留在那个 agent 里——subagent 的受限工具集不会漏回父的下一轮。

服务和事件走的是同一棵树，所以「限定到这个 agent」对服务和对监听器是同一个意思。**只做对一半比两个都不做更糟**：隔离看起来是真的，其实不是。（这正是上一版的状态：服务隔离了，事件没隔离。）

### subagent 行使了这两个轴

`subagent-in-process` 是这套机器存在的理由，不是额外功能：

```rust
let child = self.ctx.isolate();           // 一个自己的 realm
child.provide::<ToolsSvc>(restricted)?;   // 自己的工具目录（父的不受影响）
child.provide::<SessionSvc>(own_log)?;    // 自己的会话
child.plugin("agent-loop", cfg).await?;   // 循环挂进这个 realm
// ... 跑完
child.unload(fiber);                      // 一次卸载，全部footprint消失
```

`llm` 和每一条根策略仍然解析到父的。**escape-proof 不是这个模块里的任何一处检查**——它是 realm 可见性单向性的自然结果。8 个测试钉住：根策略看得见子的每次工具调用；根拒绝无法靠委派绕过；子的 transcript 不进父的日志；子结束后父的目录一字不差。

## 五、Agent 是一等实体，前端也是一行

### turn 不是一次函数调用

**step** = 一次模型请求 + 它调用的工具。**turn** 含零到多个 step：领取首条输入前打开，不再欠工作时关闭（没有待回的工具结果，inbox 里也没有能唤醒它的东西）。

这个区分带来的是 **steering**：turn 进行中到达的消息**折叠进正在跑的 turn**，而不是排在它后面。

```rust
agent.send("do this");
let turn = driver.drive(&agent);      // 跑起来
// …turn 进行中，另一个任务：
agent.send("actually, also do this"); // 加入当前 turn，成为下一个 step
```

测试 `a_message_arriving_mid_turn_joins_the_turn_already_running` 断言的就是：一个 turn，两个 step。

### inbox 区分「消息」和「注入」

**消息**唤醒 agent（有人要工作）；**注入**是上下文（memory、reminder、hook 的备注），它**永远不能自己开一个 turn**——否则一个后台提醒会让闲置的 agent 一直烧钱。它在 inbox 里等，直到一条真消息把它带进去。

### agent/pre-step 决定模型看到什么

waterfall。监听器可以改写领取到的输入、追加、或直接拒绝。**首次领取被拒绝 → 关闭一个不含 step 的 turn，但这次尝试仍然进日志**——被拒绝的 turn 也是关于这个会话的事实。

### 每个 agent 一个 realm

`Agents::create()` 无条件 isolate 一个 realm。所以「限定到这个 agent」对服务和监听器都成立——这正好是上一节那套机器。subagent 因此不再需要在子 realm 里挂第二个 loop：同一个 driver，`drive(&child)` 用子的 context 解析服务。子的轮次预算是注册在子 realm 里的一个 `turn-stopping` 监听器，父的预算不受影响。

### profile：命名装配，用户有最后一票

一个 **bundle** 贡献行；一个 **profile** 是有序的 bundle 列表 + 自己的 patch。解析时按固定顺序叠：

```
profile 列出的 bundle，按序
  → profile 自己的 patch
    → $ATOMCODE_HOME/harness.patch.toml
      → --patch 叠加层，按给的顺序
```

**用户那层在最后**，这是刻意的：profile 决定的一切——哪个前端、哪种审批、哪个执行世界——运行它的人都能不 fork 任何东西就推翻。

profile 是数据。往 `$ATOMCODE_HOME/profiles/<name>.toml` 扔一个文件和在代码里加一个是同一件事，**而且文件赢**：

```toml
# ~/.atomcode/profiles/audit-only.toml
bundles = ["base", "sdk-app"]
description = "read-only JSON-RPC for an auditor"
patch = """
[[patch]]
id = "fs"
name = "fs-readonly"

[[patch]]
id = "approval"
config = { mode = "read-only" }
"""
```

`harness --profile audit-only` 就能跑。一个文件甚至能重定义 `repl` 的含义——`a_file_can_redefine_a_shipped_profile` 测的就是这个。

### 前端是 `ui` 那一行

launcher 不跑 turn：

```rust
let front_end = app.context().require::<UiSvc>()?;
front_end.run(&app.context(), prompt).await
```

五个提供方，覆盖四种传输：

| 行 | 是什么 |
|---|---|
| `ui-oneshot` | 跑一个 prompt 就退出 |
| `ui-repl` | 行式终端会话 |
| `ui-tui` | 全屏终端 UI（crossterm），从会话日志渲染 |
| `ui-web` | axum HTTP：一个页面 + SSE 事件流 + 一个 send 端点 |
| `ui-jsonrpc` | stdio 上的行分隔 JSON-RPC，给程序用 |
| `ui-quiet` | 什么都不做，嵌入时用 |

**下面的一切不变**：`the_agent_underneath_is_identical_across_front_ends` 断言 oneshot / web / sdk / embed 四个 profile 的工具目录和 system prompt 逐字相同。换前端不改变模型能做什么，也不改变它被告知什么。

`ui-tui` 值得单说：它**从会话日志渲染**，不是累积 print 调用。所以 resize、重画、重连看到的是真实的对话，而不是副作用的流水账——这正是事件日志那一节的直接兑现。

`ui-repl` 里 inbox 是支点：读取任务**直接把输入投递到 inbox**，只用 channel 发信号。如果输入先在 channel 里等主循环回来，它就只能开下一个 turn——steering 就没了。

它同时填 `user-questions`：**有终端的前端本来就该问人**。所以 `--repl` 也把审批切成 asking 策略——「直接拒绝」只在没人可问时才是对的默认。这个改动是 `--audit` 逼出来的：它报告 `--repl` 下 `user-questions` 没人消费。

## 六、核心主张

deepseek-harness 的架构文档只讲一件事：

> **不存在需要打补丁的特权内核。**模型适配器、工具注册表、审批策略、乃至 agent loop 本身，都是挂在别人旁边的插件；每一项注册都是可撤销的副作用。

这个 spike 把这句话在 Rust 里落成了可执行的代码：

| cordis（TS） | 这里（Rust） |
|---|---|
| 插件 = 带 `inject` + `apply(ctx)` 的对象 | `trait Plugin`（`plexus/src/plugin.rs`） |
| 上下文是服务容器（`ctx.tools`） | `Context::provide::<K>()` / `service::<K>()`，槽位用**标记类型**做 key |
| 依赖靠声明，不靠顺序 | `Plugin::inject()`，`App::start()` 跑到不动点 |
| 五种分发模式的类型化事件 | `trait Event` + `Mode`（`plexus/src/event.rs`） |
| 注册是可逆副作用 | `Disposable` 记账到 fiber（`plexus/src/fiber.rs`） |
| profile / bundle / patch 配置树 | `ConfigTree` + `Layer`（`plexus/src/loader.rs`） |

### 与 cordis 的两处实质差异

**1. 服务槽用类型做 key，不用字符串。** TypeScript 靠 declaration merging 让 `ctx.fs` 有类型；Rust 没有这个东西，所以一个槽由一个标记类型声明，它同时携带消费者拿到的「面」：

```rust
plexus_service!(LlmSvc => dyn LlmProvider, "llm");

ctx.provide::<LlmSvc>(Arc::new(provider))?;      // 提供方
let llm = ctx.require::<LlmSvc>()?;              // 消费方拿到 Arc<dyn LlmProvider>
```

比 TS 更强：编译期类型安全，没有运行时 cast。字符串名字保留给它真正擅长的地方——配置行、`inject` 声明、诊断输出。

**2. 插件在编译期链接。** Rust 不能运行时加载 crate，所以**有哪些插件**在编译时定：`plugins::catalog()` 就是这个 build 的插件目录。但价值不在那里——真正动态的是**哪些插件跑、以什么配置跑、谁填哪个缝**，全部由用户可 patch 的配置树决定，而且进程运行中就能换（`App::patch`）。进程外扩展（今天的 MCP、以后的 wasm）本身也只是一个桥接到同样缝上的插件。

---

## 七、和现在的 AtomCode 比，具体差在哪

| 现在（`atomcode-coding`） | 这个 spike |
|---|---|
| `assemble.rs:118` 起的 `Agent::builder().provider(..).tools(..).middleware(..)` 链 | 配置树里的行 |
| 轮次循环是 `atomcode-kernel/src/agent.rs`（6040 行，不可替换） | 循环是 `agent-loop` 那一行，实现 308 行，换一行就换掉整个循环 |
| 审批是编译进装配的 `ToolMiddleware` | 挂在 `tools/execute` 上的监听器，由一行挂载 |
| persona 是装配传进去的 `String` | 贡献给 `system-prompt` 注册表的一个片段 |
| 换 provider 要改 `assemble.rs` / `provider_factory.rs` | `[[patch]] id = "llm"` |
| 加一个横切关注点要找到"缝"或改内核 | 加一行；缝已经在事件上 |
| 工具集在装配时快照 | 工具目录是活的：插件卸载，工具立刻从模型 schema 里消失 |

一个具体对照。现在 `assemble.rs` 里中间件顺序是这样硬编码的：

```rust
.middleware(Arc::new(RepairToolArgsMiddleware))
.middleware(turn_execution_policy.clone())
.middleware(Arc::new(CredentialBashGate::new(..)))
```

同样的顺序契约（修参数必须早于任何审批，因为人批的必须是真正会执行的字节），在这里是：

```
repair-args  →  approval  →  result-cap  →  [ 工具执行 ]
   改写           把关          出栈时截断
```

由 `prepend: true` 和配置行顺序表达（`harness/src/plugins/policy.rs`）。而且 `tools/execute` 是一个 waterfall 事件，进去改写、出来包装是同一个监听器的两半——不再需要 `before`/`after` 两个注册点。

---

## 八、已经被测试钉住的主张

`cargo test -p atomcode-plexus -p atomcode-harness` — **133 个测试，全绿**，零 clippy 警告，fmt clean。

**内核**（`plexus/tests/runtime.rs`，22 个）：含 9 个组合性测试——子 realm 的监听器对父不可见；根策略仍governs每个 realm；兄弟 realm 互不可见；根策略包在 agent 策略外层；**服务和监听器对 realm 的理解一致**；等不到依赖的行被记住，后续 patch 供上依赖就自动补挂。以及：依赖驱动激活（消费者写在提供方上面照样先等到服务）；patch 在进程运行中换掉提供方而消费者毫不知情；卸载精确回收服务与监听器；挂载停滞时报出「哪一行等哪个服务」；同槽两个提供方是错误而非静默覆盖；realm 隔离；五种分发模式各自的语义。

**会话事件模型**（`session.rs`，11 个）：投影是从事实到 prompt 的唯一路径；原始 chunk 留在日志里所以回放是保真的；**压缩改变投影但不擦除日志**；注入带 provenance；**不变量能抓到没进日志就到达模型的内容**；持久化是监听器且能往返重建。

**执行世界**（`world.rs`，8 个）：世界围栏拒绝 root 之外的路径；**只读世界在 approval 全放行时仍然拒绝写**（策略可以被覆盖，世界不能被说服）；**换掉 subprocess 提供方，bash 自动搬家**（`bash-local` 从没被 patch 过）；两套工具实现互斥且模型可见行为一致。

**能力行**（`capabilities.rs`，8 个）：符号层真的解析代码；图层是 opt-in 且自带 `code-index` 服务；skills 只在真有技能时才进 prompt；**memory 作为带 provenance 的日志事实注入，且只注一次**；删掉任何一行 agent 照跑。

**循环策略**（`loop_policy.rs`，11 个）：**轮次预算是一行，loop 只剩保险丝**（删掉 round-cap，停止原因变成 `RunawayFuse` 而不是伪装成正常结束）；retry 真的重发请求（`Next` 是 `Copy`）；401 不重试；压缩剪掉投影但日志完整；工具循环守卫先警告再终止，结果变化时不误伤。

**三角色约定**（`seam_convention.rs`，11 个）：每个插件碰的服务都在能力图上；每个 seam 至少一个提供方、至少一个消费方；分类长在定义上；**六个随附 profile 全部 audit 干净**；两个故意撒谎的插件被抓到。

**profile 系统**（`profile.rs`，12 个）：所有随附 profile 都能解析且都装配出前端；profile = bundle + 自己的 patch；**home patch 压过 profile，overlay 压过 home patch**；扔一个文件就能加 profile，且能重定义随附的同名 profile；坏 profile 被跳过而不是拖垮其它的；错误消息列出真实存在的名字。

**前端**（`front_ends.rs`，9 个）：五个前端填同一个槽；**换前端后底下的工具目录和 system prompt 逐字相同**；有终端的能问人、headless 的不能；前端能在运行中被 patch 换掉；每个 profile 都能叠 offline。

**组合性**（`seam_convention.rs` 里 3 个）：**每个随附 profile 都能挂起来且 audit 干净**；每个都装配出前端；**9 个 profile × 4 个 overlay = 36 种组合全部干净**。

**Agent 实体**（`agent.rs`，12 个）：注册表可被任何人查到；每个 agent 一个 realm；注入独自不开 turn；注入随下一条消息进场且排在它前面；一条消息一个 step 保证顺序；**turn 进行中的消息加入当前 turn**；日志记录 step 边界；pre-step 改写/拒绝；cancel 让在飞的 step 收尾。

**subagent**（`subagent.rs`，8 个）：子 agent 在自己的 realm 里跑并只回报结论；子的工具目录受限而父的完好；**根策略看得见子的每次调用**；**根拒绝无法靠委派绕过**；子的 transcript 不进父的日志；子结束后父的目录一字不差。

**产品行**（`policy_rows.rs`，10 个）：allow 规则预授权后下游 approval 不再问；deny 规则压过 yolo；**规则解析失败让整行失败，而不是留一个用户以为关着的口子**；plan mode 在 approval 全放行时仍拒绝写；**同一个 approval 行，换 `user-questions` 提供方就从「无人值守拒绝」变成「问人」**；telemetry 读共享投影而不是自己再数一遍。

## 九、代价与规模

| | 行数 |
|---|---|
| `atomcode-plexus`（内核 + 测试） | 2842 |
| `atomcode-harness`（20 个缝 + 48 个插件 + 9 个 profile + launcher + 13 个测试文件） | 13100 |
| 合计 | **15942** |

内核本身约 1900 行（不含测试），对照 vendor 的 cordis 是 2693 行 TS。

**没有动任何现有代码**：`git diff` 只有两个新 crate、`Cargo.lock`，以及一处 `cargo fmt` 补上的 HEAD 遗留格式化。

## 十、如果要真的换过去

推荐绞杀者路线，和 collapse-bridge 那次一样，每一阶段都可停可回退：

**阶段 1 — 缝对齐（低风险）。**
把 harness 的服务定义扩成现有能力的全集：`fs`、`shell`、`sandbox`、`skills`、`mcp`、`subagent`、`compaction`、`session-persistence`、`telemetry`。每个缝三件套（定义 / 提供方 / 消费方）都已经有现成实现可以包，`atomcode-capabilities` 基本可以原样搬进插件的 `apply` 里。

**阶段 2 — 循环对等（这是真正的工作量）。**
`atomcode-kernel/src/agent.rs` 6040 行里装着大量硬赚来的东西：溢出压缩阶梯、provider 重试分层、429 处理、工具循环检测、取消语义、快照/恢复、并行工具执行。spike 的 308 行循环**没有**这些。两条路：
- (a) 把 kernel 的 `Agent` 包成一个 `agent-loop` 提供方（保住全部行为，先只换装配层）；
- (b) 逐块把这些能力从循环里搬到插件（round-cap 是一个监听器、压缩是一个缝、重试是 `agent/request` 上的一个 waterfall）。
建议先 (a) 再 (b)：先让配置树成为唯一装配入口，再把循环掏空。

**阶段 3 — 驱动层。**
`tuix`（118k 行）从 `AgentEvent` 消费改成从事件总线消费。这一层最大，但改动是机械的：今天的 `AgentEvent` 枚举基本可以一对一映射成事件。

**阶段 4 — 用户可见的配置树。**
把 `~/.atomcode/config.toml` 的 `[providers]` / `[models]` / `[permissions]` 投影成配置行，用户的 `cordis.patch.toml` 成为最后一层。这一步才真正把「一切皆插件」的收益交到用户手里。

**诚实的风险清单：**
- 阶段 2 是唯一有真实回归风险的一步，那 6040 行是 SWE-bench 分数的一部分；
- 编译期插件目录意味着「装个第三方插件」仍然要重新编译，除非走 wasm/子进程；
- 配置树 patch 的表达力越强，「为什么我的 agent 是这样」就越难回答——`--dump-config` 是必须的，不是加分项；
- 服务槽冲突从"最后一个赢"变成硬错误，迁移期会暴露出一批今天靠覆盖顺序凑合的地方（这是好事，但会疼）。

---

## 十一、这个 spike 没做的

- **subagent 只有 in-process 一种提供方**：`subagent-fork`（从当前会话 fork 出去）和外部 agent 驱动（Claude Code / Codex）没做；
- **没有 ACP 前端**：`agent-client-protocol` 在 workspace 里但没接；JSON-RPC 前端是自定义方法名，不是 ACP；
- **web 前端是单 agent 单会话**：没有多会话路由、没有鉴权、没有静态资源打包；SSE 落后的客户端靠 `/api/events?after=` 自己重同步；
- **TUI 没有滚动/选区/鼠标**：底部锚定的重绘，够证明「从日志渲染」，不够当日常终端用；
- **没有热重载配置文件**：`App::patch` 有了，profile 也有了，但没有文件监听；
- **没有 wasm / 动态库插件**：进程外扩展只有 MCP 一条路；
- **卸载没有 quiescence**：in-flight 调用靠 `Arc` 不悬垂，但没有「等它做完」的语义；
- **循环仍然比生产版薄**：没有并行工具执行、没有取消传播到工具内部、没有快照恢复、没有 429 的 `Retry-After` 处理、只有一个 tail 压缩器；
- **没接 tuix / daemon / cli**：主线驱动层还是老架构，这个 spike 始终是并存的第二套。
