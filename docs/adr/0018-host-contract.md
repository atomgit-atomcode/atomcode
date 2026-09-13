# Host 契约:入口、薄组装、读取位置、接线形态

状态: 提议(2026-09-14)。是 [`0013`](./0013-agent-product-host-ui.md) 的细化:0013 定了
四层与依赖方向,这一页定 **Host 这一层具体做什么、边界在哪、越界的形态是什么**,以及
0013 没写明的一个机制问题——**接线是树的数据,不是 host 的代码**。

## 背景

用户口述了 8 条设计原则(下面逐条照录)。核对代码后发现:8 条里有 6 条与 0013 一致
(设计早已定),**两条今天是违反的**(host 代码的位置、配置读取的位置),还有一条
(**接线形态**)是 0013 没写、而实现已经给出答案的。

第 5 条用户未陈述。本页按 0013 补为 **Product**,并在该处标注这是推断、需人确认。

## 决策

### 0. 八条原则(逐条)

1. **程序的入口应该由 host 提供。** 用户进一步定:host **落到 CLI**
   (`crates/atomcode-cli`,包名 `atomcode`),因为那是程序入口。
2. **Host 是一层很薄的组装层。**
3. **所有的配置读取、环境变量读取应该在 host 上完成。**
4. **Host 应该组装 Agent 能力 + Product 能力 + 对外的协议或 UI。**
5. *(用户未陈述。按 0013 补:Product 是「每个产品一组 TOML overlay + 一个薄 crate」,
   只做选择和配置,不实现能力,判据是那 150 行 TOML。**这是推断,需确认。**)*
6. **Agent 能力 / Product 能力 / UI 都应该是低耦合的,可随意替换组装。**
7. **三者中间的交互由 host 连接。** 例:UI 向 Agent 提供工具(`adjust_layout`)供 Agent
   调用;UI 通过界面调用 Agent 的命令。
8. **Agent、Product、UI 之间存在接线,接线的逻辑应该在 host。**

### 1. Host 该做什么

| 职责 | 具体 |
|---|---|
| **入口** | 拥有进程:命令行解析、`main`、信号、退出码 |
| **读取** | 配置文件的读取与环境变量的读取(**全部**),`config.toml` / `$ATOMCODE_*` |
| **组装** | 选 profile、折配置树、挂 UI 行与协议行、`App::new` + `start` + `hand_over` |
| **拥有接线** | 决定**挂哪些行**(= 决定谁和谁连上),并把宿主自己提供的服务填进槽(`ControlSvc`、`ModelSourceSvc` 这类) |
| **资源** | 磁盘会话存储、OS 租约、调度、自更新、遥测发送、鉴权 |

### 2. Host 不该做什么 —— 以及越界的**形态**

原则不是靠记的,是靠**症状**认出来的。每条越界都有可观察的形态:

| 越界 | 症状(可观察) |
|---|---|
| Host 代码住进 Agent 层 | Agent 的 crate 里出现 `launch.rs` / `ui*.rs` / `*_APP` profile;改一个前端要动 Agent 层 |
| Agent 层读配置/环境 | 同一个事实两个真源(见 ADR 0017 的表格);`--offline`、测试、CI 要设环境变量才能跑;同一网关能被两套规则解析 |
| 行自己去够别的行 | `--dump-seams` 上出现没声明的消费者;`tests/seam_convention.rs` 判红(「声明消费的服务必须在 map 上有定义」) |
| UI 里有产品判断 | 换一个前端,行为跟着变 |
| UI 里有驱动逻辑 | 回合、steering、取消、压缩排队出现在前端——它们归 handle 泵 |

### 3. 接线由 **host** 建立(原则);今天不是(实况)

原则 8 直说:**接线的逻辑在 host**。理由是它与原则 2/6/7 同源——host 是唯一认识全部
四层的,**UI 不许知道 Agent 的细节,Agent 不许知道 UI 的细节**。所以「谁和谁连上」
是 host 的事,不是某一行的私事。

今天不是这样,原因三层,一层比一层深:

**(a) 机制缺口:`provides` 只有名字,没有值。** Spring 里 bean 不写
`ctx.registerBean(this)`——实现接口的 bean 由容器**收集**。这里没有「贡献值」这个概念
(`Plugin::provides() -> &'static [&'static str]`),于是行想提供什么,只剩「自己
`service::<>()` 拿缝、再 `register` 进去」一条路。这是**机制缺口**,不是设计优点。

**(b) crate 级依赖:UI 长在 Agent crate 里面。** `crates/atomcode-tui/Cargo.toml:15`
依赖 `atomcode-harness`,而且拿的不止缝:`seams::*`、`plugins::handle::{spawn,
Answers}`、`agent::{Agent, AgentStatus}`、`profile::Profiles`、`launch::{value,…}`、
`plugins`(整个 catalog)。这不是「UI 与 Agent 之间有一条线」,是 UI 在 Agent 内部。

**(c) 服务定位器:递给每行的是整个 `Context`。** `apply(ctx, config)` 里那个 `ctx`
能 `service::<任何东西>()`。这才是「UI 能知道 Agent 细节」的根源,比 `register` 那一下
严重得多。(它同时也是 `--dump-seams` 画得出全图的原因——同一枚硬币的两面。)

#### 3.1 我先前写错了,更正在此

本页第一版写的是「接线的**声明**在 host、接线的**建立**在该行自己」,并给了三条证据。
那些证据(`--dump-config` / `--dump-seams` 能打印、`App::patch` 能运行中换、
`ctx.effect` 卸载自动断线)支持的是**注册表这个模式**——多个插件往一个中立目录里投
贡献,而不是点对点互相持引用。**它们一条都不支持「由行来调 `register`」。** 谁调
`register`,与这个模式好不好,是两件事;我把后者当成了前者的理由。用户的反对是对的。

一处自证的例子:本轮我给 tui 加了 `REASONING_EFFORT_LEVELS` / `REASONING_EFFORT_ROW`,
放在 `harness/src/lib.rs` 让 tui 读——**加深了这条依赖**。共享词汇该放中立位置,
不该借 harness 转手。

#### 3.2 必须一起解决的机械约束:重挂之后重连

`ctx.effect` 的撤销绑在**注册它的那个 fiber**(`context.rs` 的 `effect` → `record`;
`fiber.rs` 卸载时 `dispose_all`)。于是安装者换了,寿命跟着换:

| 谁安装 | 行被 `App::patch` 掉之后 |
|---|---|
| 行自己(今天) | 注册跟着行走——这是它唯一的优点,代价见 §3(a)-(c) |
| host 在 `hand_over` 安装 | undo 落在**根 fiber**(`launch.rs:371` 用的是 `self.app.context()`),那条线**不会消失** |

所以 host 装配必须配一个**「重挂后重连」**的时机,否则它会引入自己本想消灭的那类
缺陷(悬空钩子)。这不是反对 host 建立接线,是说**不能只把 `register` 挪个位置**。

#### 3.3 分两步,别当一件事做

**L1 协议中立化**(先做,也最要紧):UI 依赖的不该是 `harness`。今天 `ToolsSvc` /
`UiSvc` / `ControlSvc` 在 `harness/src/seams.rs`(**Agent 层**),而 `Tool`、
`AgentCommand`(`kernel/event.rs:126`,8 个变体)、`Message` 在 **kernel(中立)**。
缝要挪到中立的协议位置,UI 只认协议。判据可写成:`atomcode-tui` 的 `Cargo.toml`
除测试外不依赖任何 Agent **实现** crate。

**L2 贡献声明化**:让「贡献」可声明、由 host 收集安装,并给 host 那个重连时机。
要动 plexus(`provides` 加值,或新增「贡献」概念)或 host(宿主侧的接线行)。

### 4. 与今天的差距

| 原则 | 今天 | 差距 |
|---|---|---|
| ① 入口 | **四个入口**:`atomcode`(cli,旧 tuix 栈的宿主)、`atomcode-daemon`、`atui`、`harness`。后两个是 plexus 栈的**第二个和第三个 host**(见 §5) | 流程是 host 的(`Launch::parse` → `mount` → `hand_over`),**入口不是**。按用户决定收进 CLI |
| ② 薄组装层 | `launch.rs` / `bundle.rs` / `plugins/ui*.rs` **住在 `atomcode-harness`** | `architecture-target.md:58` 已列「要移出」,`:236` 自认「harness 含 UI 行与 launch」。待办 10 |
| ③ 读取在 host | `model_source.rs` 在 harness;`capabilities` 读 `$ATOMCODE_HOME` 三次 | 待办 10 + ADR 0017 |
| ④ 组装四层 | `Profiles::resolve`(`profile.rs:134`)+ `bundle::base()` + profile overlays 已在做 | **机制对,位置错** |
| ⑥ 低耦合可替换 | 7 个 `ui-*` 行;`--ui` 一行换前端;协议面(web / jsonrpc)也是行;`AgentClient` 只发 `AgentCommand`(`kernel/event.rs:126`,8 个变体) | **最成立的一条** |
| ⑦⑧ 接线 | 机制是**注册表**(对):`adjust_layout` 由 UI 行投进中立的 `ToolsSvc`,UI 调 agent 也只走 `AgentCommand`(`kernel/event.rs:126`)——两边都不持对方的引用 | **模式对,位置与调用方不对**:注册是行自己调的,UI 又整块依赖 harness。见 §3 |

### 5. 入口:host 落到 CLI,`atui`/`harness` 不再是 host

用户定:host 落到 `crates/atomcode-cli`(包名 `atomcode`),因为入口属于 host。核对后
发现这条不只是搬家——**`atui` 现在是一个 host,而且证据很硬**:

| 证据 | 位置 |
|---|---|
| `ui-tui2` 这个「UI 行」**定义在 atui 的 main 里** | `atui.rs:40` 的 `TUI2` overlay,`:47` 就是 `name = "ui-tui2"` |
| `atui` 的 main 做 host 的四件事 | `Launch::new`(`:96`)、自己解 flag 并 `Flag::Exit`(`:114`/`:129`/`:152`)、`launch.mount`(`:167`)、`hand_over`(`:221`) |
| 因此宿主显式拒绝 `--ui tui` | `launch.rs:120-123`「the full-screen front end is its own crate with its own launcher; **this catalog has no row for it**」 |

第三条是闭环证据:拒绝的理由就是"它有自己的 launcher"。也就是说 **`--ui tui` 不能被选,
不是设计选择,而是 atui 兼任 host 的后果**。

#### 5.1 收进 CLI 的连锁后果

1. `TUI2` overlay(surface 行、trace 静音、独立 asker 让位)是**宿主侧的树组装**,
   随 host 搬走;它现在住在 UI 二进制里,正是"UI 知道 Agent 细节"的一例。
2. **`--tui` 从"被拒绝"变成"普通 flag"**(§5.2):今天 `launch.rs:120` 与
   `atui.rs:150` 各有一份拒绝,都要删;`UI_NAMES`(`bundle.rs:822`)不必补 `"tui"`——
   它是 flag,不是 `--ui` 的取值。
3. **`atui` 这个二进制不再是入口。** 它**不能留在 `atomcode-tui`**——留在一个 UI
   crate 里就等于 UI 仍拥有入口,原则 1 没满足;它也不能改成宿主侧的壳,因为那必须
   **依赖 `atomcode-tui`** 去挂 `ui-tui2` 行,而 UI 认 Host 就违反 0013 的反向。
   所以:入口收进 `atomcode --tui`,UI crate 只提供**行**(`ui-tui2` 与屏幕面板)。
4. **依赖方向翻成该有的样子:Host → UI 的行;UI → 中立协议。** 今天靠
   `atomcode-tui/Cargo.toml:15` 依赖 `atomcode-harness`,而那正是因为 `Launch` 住在
   harness(§3(b))。host 搬走之后,这条反向依赖同时消失——**这一搬同时满足原则 1 与 2**。

#### 5.2 入口形态由用户定:`atomcode --tui`(不是保留 `atui`)

用户定:**以后的入口是 `atomcode --tui`**,`atui` 这个二进制不再作为入口。理由与原则 1
同源——入口属于 host,而 `atui` 现在「成为 host」这件事本身就是**不对的**(§5 的那三
条证据是症状,不是设计)。所以:

- **`atui` 二进制消失**(或退化成内部测试用的壳,不再是面向人的入口);
- `--tui` 在宿主里变成**换个 UI 行**的普通 flag,与 `--repl`/`--web`/`--sdk` 同级——
  今天它在 `launch.rs:120` 是被**拒绝**的(`exit 2`),`atui.rs:150` 那份拒绝也随之删;
- `--ui tui` 不再需要单独存在(它就是 `--tui`),`UI_NAMES` 里也不必露 `"tui"`——
  它是 flag,不是 `--ui` 的取值;
- 一条测试要**反向**:今天 `launch.rs` 的
  `the_full_screen_front_end_is_not_a_row_here` 把 `--tui` 与 `--ui tui` 钉成 `exit 2`,
  它守的是"UI 不能被宿主选"这个错状态。改造后它应该变成「`--tui` 挂上 `ui-tui2` 行」
  的判据。

我先前把这一条记成"保留二进制名的薄壳,标注为产品选择"——那是**回避决定**,不是记决定。
用户的原话就是 `atomcode --tui`,照此记。

#### 5.3 一个风险,记在案

`atomcode-cli` 今天是**旧 tuix 栈的宿主**(`atomcode-cli/Cargo.toml:34` 依赖
`atomcode-tuix`),且**不依赖** `atomcode-harness`/`atomcode-tui`。host 落进去意味着
一个 crate 在迁移期同时托两个栈。备选是**新建 `atomcode-host` 库 crate**,让
`atomcode`、`atomcode-daemon` 共用——daemon 确实需要同一个宿主库。本页记用户的选择
(CLI),把这个备选留在"权衡过"里。

注意 §5.2 的选择**加重了**这里:入口统一到 `atomcode` 之后,这个 crate 同时是
「旧栈宿主 + 新栈宿主 + 唯一入口」,迁移期责任更集中。搬迁时这一条要重新评估一次。

## 权衡过、没做的

- **让 host 拥有一段显式的接线代码**(`a.connect(b)`):本页第一版把它列为「不取」,
  理由是「会放弃 `--dump-config`/`--dump-seams`/`App::patch` 三条能力」。**那个理由
  是错的,而且是个假二分**——那三条能力来自**注册表这个模式**(贡献投进中立目录),
  不是来自「谁调 `register`」。host 收集贡献并注册,同样能打印、能 patch、能撤销。
  正确的问题不是「要不要 host 接线」,而是「怎么让 plexus 支持『声明贡献』」
  (见 §3.3 的 L2)。这一条保留在案,是为了记住这个错。
- **新建 `atomcode-host` 库 crate 而不是落进 CLI**(§5.3):`atomcode` 今天同时是旧 tuix
  栈的宿主,一个 crate 托两个栈是迁移期风险;而 daemon 也需要同一个宿主库。不取的理由
  是用户的选择(入口属于 CLI),不是因为这条更好或更差——它值得在 §5.3 的搬迁里
  重新评估一次(§5.2 把入口也统一过去之后,这条备选的吸引力比先前更大)。
- **让 plexus 知道 host 的概念**(配置来源、进程环境):通用容器里塞宿主关注点,
  0013 的依赖方向反过来破。ADR 0017 已定「容器提供遍历、宿主注入展开器」。
- **把入口收到单一二进制**(`atomcode --ui tui`):今天每个前端一个 `main`,是因为
  `atui` 有自己的启动链(终端能力探测、HEADLESS overlay)。收口是可能的,但它属于
  §4 的搬迁,不单独做。

## 闸门

原则要能被检查,否则会再违反一次。可执行的:

- **层级**:`cargo tree` 上 Agent 层(harness / capabilities / kernel / plexus)对
  host 依赖为零;host 代码不在 Agent 层 crate 里(§4 ① ② 完成后由目录保证)。
- **读取位置**:沿用本轮立下的两条读源码守卫(`tests/reasoning_effort.rs` 的
  `nothing_outside_the_source_module_reaches_the_environment` /
  `only_one_module_loads_the_model_config`),范围扩到 host 之外的 crate——ADR 0017
  已把它列进闸门。
- **接线**:`tests/seam_convention.rs`(6 条)今天已经在跑,且会判红——本轮我给一个行
  加服务声明时撞上的就是其中第一条(`every_service_a_plugin_touches_is_on_the_capability_map`:
  「声明了消费的服务必须在 map 上有定义」)。新行的接线必须过它。
- **可替换性**:换一行实现不改消费方——`ControlSvc`(宿主填)与 `UiSvc`(前端填)两条
  已是活证据,各自的替换各有一条测试。
- **依赖方向(L1 做完后)**:`atomcode-tui` 的 `Cargo.toml` 除测试外不依赖 Agent
  **实现** crate——今天它依赖 `atomcode-harness`(§3(b))。这条可以用读 `Cargo.toml`
  的守卫钉住,形状同上面那条读源码的。
- **入口唯一(§5 做完后)**:面向人的入口只剩 `atomcode`——`atui.rs` 与 `harness.rs`
  的 `main` 不再是入口。可执行的判定:`atomcode --tui` 与 `atomcode --repl` 都由
  同一个二进制接受,且 `--tui` 挂的是 `ui-tui2` 行(今天两处都报 `exit 2`:
  `launch.rs:120`、`atui.rs:150`)。

## 未做

- 本页是契约,不是实现计划。§4 的 ①②③ 是三次独立的搬迁/收口,顺序上 ①③ 可以一起
  (都在 `launch.rs` 那一趟),② 是目录搬迁,碰 `bundle.rs` 的 `*_APP` profile。
- §3.3 的 L1(协议中立化)与 L2(贡献声明化)**不要合成一趟做**:L1 是挪缝与断依赖,
  判据是 `Cargo.toml`;L2 要动 plexus 的 `provides` 语义和 host 的重连时机,改动面在
  底座。L1 不依赖 L2,**先做 L1**——它把「UI 知道 Agent 细节」这个根源先切掉,
  L2 之后要接的东西就只剩中立的协议。
- 本轮我把 `REASONING_EFFORT_LEVELS` / `REASONING_EFFORT_ROW` 放在 harness 让 tui 读
  (§3.1),这是**新加的一处**同类依赖;L1 做的时候连它一起挪去中立位置。
- 第 5 条(Product)我按 0013 补的,未与人确认。
- 「协议面」(web / jsonrpc / 将来的 acp)算 Host 还是 UI,0013 与 `architecture-target.md`
  的口径不一致(`architecture-target.md:99` 把 `ui-web`、`ui-acp` 列在 UI「装什么」下,
  而 §2.4 Host 的「装什么」里没有协议面)。本页按 UI 记,但不改那份文档——口径统一
  是另一件事。
