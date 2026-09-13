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

1. **程序的入口应该由 host 提供。**
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

### 3. 接线形态:**声明在 host,建立在行**(本条 0013 未写,实现已给出答案)

原则 8 说「接线的逻辑应该在 host」。这个仓库的落地形态比字面更具体:

> **接线的*声明*在 host(它决定挂哪些行),接线的*建立*在该行自己
> (`ctx.effect` 注册 + 卸载时自动撤销)。**

以原则 7 自己举的例子为准,代码是:

```rust
// crates/atomcode-tui/src/plugin.rs:1449 —— 在 tui 那一行里
if let Some(tools) = ctx.service::<ToolsSvc>() {
    tools.register(Arc::new(crate::layout_tool::AdjustLayout { layout, modules }))?;
    let _ = ctx.effect(move || t.unregister("adjust_layout"));  // 行卸载,线自断
}
```

不是 host 里写 `agent.add_tool(ui.layout_tool())`,而是「这一行声明它填 `tools`
(`provides`),那一行声明它读 `tools`(`uses`/`inject`)」,由缝的有向可见性接起来;
host 拥有的是「挂哪一行」这个决定。

**为什么这样比 host 里的连接代码更强,三条证据都在本仓库里:**

1. **看得见。** `--dump-config` 打得出整个系统,`--dump-seams` 打得出「谁填 `tools` /
   谁读 `tools`」。命令式接线的系统画不出这张图——接线是代码,不是数据。
2. **能运行中换。** `App::patch`(`plexus/src/app.rs:176`)能替换任何一个接线点:
   `/effort` 换档位、`/rows` 开关一行、`--ui`(`launch.rs:104`)换整个前端。
   命令式接线要重启进程。
3. **卸载自动断线。** 上面那个 `ctx.effect` 就是保证(`fiber.rs` 的 `dispose_all`)。
   命令式接线的典型缺陷是漏掉撤销,留下悬空钩子。

**代价,如实说:** 要回答「谁把 `adjust_layout` 给了 agent」,得 grep,读不到一个
host 装配函数。补法是工具化的——`--dump-seams` 的能力地图,加上
`tests/seam_convention.rs` 那六条约定闸门(声明了消费的服务就必须在 map 上、每个缝
至少一个 provider、宿主填的缝要标出来、每个缝至少一个消费者、指出哪些缝只是名义上
可替换、分类写在定义上)。也就是说这个代价被**换成了可执行的检查**,而不是留给下一读者。

### 4. 与今天的差距

| 原则 | 今天 | 差距 |
|---|---|---|
| ① 入口 | `atui.rs:93`、`harness.rs:24`、`cli`、`clix`、`daemon` **各有 `main`** | 流程是 host 的(`Launch::parse` → `mount` → `hand_over`),**入口不是**。搬 `Launch` 去宿主 crate 时一并收 |
| ② 薄组装层 | `launch.rs` / `bundle.rs` / `plugins/ui*.rs` **住在 `atomcode-harness`** | `architecture-target.md:58` 已列「要移出」,`:236` 自认「harness 含 UI 行与 launch」。待办 10 |
| ③ 读取在 host | `model_source.rs` 在 harness;`capabilities` 读 `$ATOMCODE_HOME` 三次 | 待办 10 + ADR 0017 |
| ④ 组装四层 | `Profiles::resolve`(`profile.rs:134`)+ `bundle::base()` + profile overlays 已在做 | **机制对,位置错** |
| ⑥ 低耦合可替换 | 7 个 `ui-*` 行;`--ui` 一行换前端;协议面(web / jsonrpc)也是行;`AgentClient` 只发 `AgentCommand`(`kernel/event.rs:126`,8 个变体) | **最成立的一条** |
| ⑦⑧ 接线 | `adjust_layout` 由 UI 行注册进 `ToolsSvc`(`tui/plugin.rs:1449`);UI 调 agent 只走 `AgentCommand` | **成立**(形态见 §3) |

## 权衡过、没做的

- **让 host 拥有一段显式的接线代码**(`a.connect(b)`):放弃 `--dump-config` /
  `--dump-seams` / `App::patch` 三条能力,换「读一个函数就懂全图」。不取。
  (若将来真需要那种可读性,正确的补法是**生成**一张图,而不是把接线改成命令式。)
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

## 未做

- 本页是契约,不是实现计划。§4 的 ①②③ 是三次独立的搬迁/收口,顺序上 ①③ 可以一起
  (都在 `launch.rs` 那一趟),② 是目录搬迁,碰 `bundle.rs` 的 `*_APP` profile。
- 第 5 条(Product)我按 0013 补的,未与人确认。
- 「协议面」(web / jsonrpc / 将来的 acp)算 Host 还是 UI,0013 与 `architecture-target.md`
  的口径不一致(`architecture-target.md:99` 把 `ui-web`、`ui-acp` 列在 UI「装什么」下,
  而 §2.4 Host 的「装什么」里没有协议面)。本页按 UI 记,但不改那份文档——口径统一
  是另一件事。
