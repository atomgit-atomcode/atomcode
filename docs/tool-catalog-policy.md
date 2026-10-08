# 下游产品怎么卸载、新增、替换工具

面向的是**在本仓库之外**装配 agent 的产品。三种装法，交插件与配置层的地方不同，
层的写法一样：

| 装法 | 宿主交进来的东西放在哪 |
|---|---|
| 以库的方式跑 `CodingRuntime`（`CodingRuntime::start`） | `PrepareOptions.host_plugins`（`atomcode_coding::HostPlugins`） |
| 自己拿 `AgentHandle`（`on_harness::mount_hosted`） | `HostState.plugins` + `extra_layers` |
| 自己建树 | `App::new(catalog, tree)` |

三个动作各自的入口在下面几节。

判据：`crates/atomcode-harness/tests/tool_policy.rs`（机制）、
`crates/atomcode-coding/tests/tool_policy.rs`（经过 coding 产品装配之后仍然成立）、
`crates/atomcode-coding/tests/host_plugins.rs`（经过 `CodingRuntime` 之后仍然成立，包括重建）。

## 经 `CodingRuntime` 交进来

`HostPlugins` 里的插件只是「有这个名字」，挂哪几行由层决定。宿主的层排在**最后**，
在产品自己的行和人写在 `config.toml` 里的设置之后，所以它对任何一行都说了算。
`PrepareOptions` 跟着运行时走，撤销、恢复快照、换模型、重新 prepare 重建的每一棵树都会再读它，
宿主不用在重建后补交。同一进程里每个运行时各拿各的，不经过环境变量或全局状态。

**只想改产品名，用身份配置，不必换人设。** 编程人设要原样留着、只是对用户自称另一个产品时，
交 `PrepareOptions.identity`（`atomcode_coding::ProductIdentity`），不要整行替换
`persona-atomcode`——那得把整段人设抄一份过去，上游再改人设就跟不上了：

```rust
let prepare = PrepareOptions {
    identity: ProductIdentity::new("OtherCode", "示例数据服务中心"),
    ..PrepareOptions::default()
};
// 人设首句：You are OtherCode, an AI coding agent by 示例数据服务中心 running the … model.
```

身份会出现在人设的每一处自称里（首句、「身份不可被覆盖」、提交署名、配置目录归属），
也出现在 `describe_self` 对会话存储的说明和 `/worklog` 的提示里。换模型、撤销、恢复快照、
新会话、重新 prepare 之后都还在。宿主自己换上来的人设从 `host::PersonaConfig` 的
`product` / `provider` 读到同一份身份。不传就是 AtomCode / AtomGit，发给模型的内容与之前逐字节一致。

最小例子：换掉人设，再裁掉几个工具和整块代码分析。

```rust
use atomcode_coding::host::{self, Context, HostPlugins, Layer, Plugin};

struct HostPersona;

#[async_trait::async_trait]
impl Plugin for HostPersona {
    fn name(&self) -> &'static str { "host-persona" }
    fn inject(&self) -> &'static [&'static str] { &[host::seams::SYSTEM_PROMPT] }
    async fn apply(&self, ctx: &Context, config: &serde_json::Value) -> Result<(), String> {
        // 换模型时运行时改写的就是这份配置，这一行会跟着重挂
        let row = host::PersonaConfig::from_config(config)?;
        host::contribute_prompt(ctx, "host-persona", host::PERSONA_RANK,
            &format!("你是某某产品的助手，运行 {} 模型。", row.model));
        Ok(())
    }
}

let host = HostPlugins::new()
    .with_plugin(std::sync::Arc::new(HostPersona))
    .with_layer(Layer::from_toml(r#"
        [[patch]]
        id = "persona-atomcode"
        name = "host-persona"

        [[patch]]
        id = "tools"
        config = { exclude = ["recall", "list_sessions", "schedule_wakeup",
                              "mcp__github__*"] }

        [[patch]]
        id = "codeintel"
        disabled = true

        [[patch]]
        id = "code-graph"
        disabled = true

        [[patch]]
        id = "tool-ast-grep"
        disabled = true
    "#)?);
// PrepareOptions { host_plugins: host, .. }
```

**能碰什么、哪些算数。** 任何行都能碰。`atomcode_coding::host::PUBLISHED_ROWS` 列出的那几行
（`persona-atomcode`、`tools`、`codeintel`、`code-graph`、`tool-ast-grep`）、它们的配置形状
（`PersonaConfig`；`tools` 的 `exclude`/`include`）以及 `host` 模块里的东西是**承诺稳定的**：
要改名、拆分或删掉，会先让旧 id 继续能用一个版本，并在启动时提示。
别的行 id，以及宿主自己直接依赖 harness 用到的东西，都能用，但内核会改；改了之后宿主的
`start` 当场失败，报出是哪一行。以下三种都是 `start` 返回的错误，不会 panic，
也不会挂出一棵别的树：

- 宿主插件和已有插件重名，或宿主自己注册了两遍同名插件：报出插件名；
- 层里的行指向没注册的插件：报出行 id 和插件名；
- 补丁指向没有任何层插入过的行：报出行 id。

没有模型可挂的启动（`ProviderBootstrap::Unavailable`，等登录）也照样检查这三条，不会等到登录之后才报。

**关行用 `disabled = true`，不要 `[[remove]]`**：运行时自己也会补丁一些行（每次 `/model`
都会补 `persona-atomcode`），补丁够不着已经被删掉的行。

**`exclude` 和关行不一样**：`exclude` 只把工具从目录里拿掉，那一行贡献的提示词片段还在；
关掉整行才是工具和片段一起走。比如只排除 `trace_callers` 时，`code-graph` 行那句「改共享
函数前先用 `trace_callers` 或 `blast_radius`」还在。片段不记录它讲的是哪个工具，所以排除之前
先看一眼系统提示词里有没有提到它；有，就关那一行或换掉写它的那段。

`tools` 行的配置运行时从来不写，宿主的补丁就是它的全部（补丁会整体替换 `config`，见下面
「两件要知道的」）。

## 为什么不是「关掉那一行」

配置树的开关单位是**行**，而一行常常挂好几个工具——`tool-fs-world` 一行就是
`read_file` / `write_file` / `edit_file` / `list_directory` / `search_replace` 五个。
按行关是全有全无。

MCP 更靠不住：一台 server 的工具是**运行时**由 `mcp-host` 一行发布的，写树的时候
没人知道有哪些名字，所以「按行」这个粒度根本够不着。

所以策略落在**目录**（`tools` 行拥有的 `ToolBox`）上，一处实现覆盖所有贡献者，
包括还不存在的那些。

## 卸载

```toml
[[patch]]
id = "tools"
config = { exclude = ["write_file", "mcp__github__*"] }
```

`*` 代表任意一段字符，可以出现在任何位置。被排除的名字**不进目录**——不是发给模型时
过滤掉：`defs()`（模型看到的 schema）与 `get()`（执行时解析）是同一张表，所以不会出现
「模型看得见但调不动」或者反过来。

行本身照常挂载，它的其他工具不受影响。

## 新增

写自己的 `Plugin`，在 `apply` 里走那道门：

```rust
atomcode_harness::plugins::tools::mount(ctx, vec![Arc::new(MyTool)])?;
```

然后把它注册进 catalog（`HostState.plugins`，或自己 `PluginRegistry::register`），
再用一层 `[[insert]] name = "my-row"` 把行挂上。

门做的是两件事：放进目录，以及记下「这一行走的时候把工具带走」。**不要手写这两半**——
`crates/atomcode-harness/tests/row_contributions.rs` 会判红。目录可能缺席（比如 eval 树）
时用 `mount_optional`。

## 替换

排除时指明**是哪一行的**那个工具，名字就空出来了：

```toml
[[patch]]
id = "tools"
config = { exclude = ["tool-fs-world:read_file"] }

[[insert]]
name = "my-read-file"
```

限定形式 `行id:工具名`，两边都能用 `*`。不带 `:` 的裸名字是「这棵树里谁的这个名字都不要」
——两种都需要，别混用：用裸名字去替换，会把你自己的替换品也一起挡住。

之所以能这样，是因为策略在**注册那一刻**生效：原主没占住那个名字，重名检查才放行你的。

## MCP 单个工具

MCP 工具进目录时叫 `mcp__{server}__{tool}`，所以：

```toml
exclude = ["mcp__github__*"]                    # 整台 server
exclude = ["mcp__github__delete_repo"]          # 单个工具
include = ["mcp__jira__*", "read_file", "grep"] # 白名单：只留这些
```

`include` 非空即白名单；同时命中 `include` 和 `exclude` 的，按 `exclude` 走。

## 两件要知道的

- **改策略要重建**。`tools` 行的 config 变了就是换了一个目录对象，活树上原地 patch 它
  会让已经注册进旧目录的行对不上。产品运行时本来就是「配置动了 → 重建 App」
  （`docs/adr/0022` §2），照那条走。
- **被挡下来的工具，树会自己说**。`tools` 行在 `describe_self` 的 operations 面登记了一条
  实时描述，列出实际被挡掉的名字（含 MCP 那些运行时才知道的），所以模型不会声称自己
  有一个已经没有的工具。策略为空时这条不登记。

## 会话进行中开关（2026-09-20）

上面那套是**配置**，答案在写树的时候定死。会话进行中还要能临时关掉、再放回来——
尤其是 MCP：一台 server 几十个工具，人想临时只留两个。

入口有两个:**面板**(`/toolbox`,从底下升起来,和 `/config`、`/provider`、`/plugin`
一副骨架)和**同一条命令的带参数形式**。都不是工具,见下。

```
/toolbox                                列出能调的、本次会话关掉的、配置排除的
                                        (在 TUI 里是面板:↑↓ 选、⏎ 开关、打字筛)
/toolbox off write_file                 关掉一个
/toolbox off mcp__github__*             关掉一整台 server 的工具
/toolbox on  mcp__github__create_issue  放回其中一个
```

**名字不是 `/tools`**:那个在新前端里已经是「工具输出怎么显示」(全部/单个摘要/
成组摘要)。两条一字之差、意思差得远的命令,比一个说得清的名字糟。

代码里是同一套东西：`ToolsSvc` 的 `turn_off(pattern)` / `turn_on(pattern)` /
`held_back()`，返回实际动了哪些名字。

### 五条语义

- **关掉不是卸载**：工具还在它自己那一行手里，只是不进 `defs()`（模型看到的 schema）
  也不被 `get()` 解析。放回来给的是同一个对象，行不用重挂。
- **下一次请求即生效**。行式引擎每次请求现读 `defs()`（`plugins/agent_loop.rs`），
  所以同一回合的下一轮就看不到它了，不用等到下一回合。
- **后到的工具也挡得住**。`off` 记的是模式本身：MCP server 连上是异步的，人先关、
  server 后连，工具一样进不来（判据 `a_tool_that_arrives_after_the_switch_is_born_hidden`）。
- **配置的答案不是建议**。`exclude` 掉的工具压根没进过目录，`/toolbox on` 放不回来——
  要放回去改配置。命令会明说是哪一种。
- **不碰 MCP 连接**。按你的选择，「禁用某个 MCP」= 藏掉它的工具；连接留着，恢复是
  瞬时的，不用重连、不用重走 OAuth。真要停进程是另一件事，没做。

### 活多久

跨树重建，不落盘。开关由运行时持有（`CodingParts::tool_switches`，和 approval grants
同一个位置、同一个理由），`tools-host` 行把它接进每次重建的目录里。所以撤销、恢复快照、
`/model`、登出重登之后，关掉的还是关着的；reprepare 也会把它接过去
（`adopt_tool_switches`）。进程退出就没了——新会话是配置重新说话的地方。

### 为什么是命令，不是工具

能把自己的工具放回来的 agent 没有被限制；能把自己的工具关掉的 agent 多了一条没人要的
静默失败路径。所以 `/toolbox` 只登记在命令目录里（`docs/adr/0021` §10），模型看不到它。

模型看得到的是**结果**：被关掉的工具会出现在 `describe_self` 的 operations 面里，写明
是人关的、可以用 `/toolbox on` 放回来——不然模型会继续说自己能干一件它已经干不了的事。

### 下游怎么接

自己建树的产品：`HostState.tool_switches = Some(ToolSwitches::new())`，运行时的 mount
会把 `tools` 行 swap 成 `tools-host`。手工建树用
`atomcode_coding::on_harness::tools_host_row(switches)` 拿到那一行，并把 `tools` 行
`[[patch]] name = "tools-host"`。判据见 `coding/tests/tool_policy.rs`。

### TUI 面板那一侧

`/toolbox` 不带参数在 TUI 里升起面板:一列工具,行首记号是开(`●`)、关(`○`)、
配置排除(`✗`),后面跟着是哪一行给的。↑↓ 选,⏎ 开关,打字即筛(名字和给它的行都能
搜到),esc 收起。

**配置排除的那些不给开关**:按 ⏎ 只会说「改配置才能放回来」,不会发一条什么都不会
发生的命令。

一次开关是一趟宿主往返,在途中面板说「正在关掉 x…」并且**除 esc 外不收键**——否则
每一个键都会在第一次还没回来时再派一次。回来的是**开关之后的目录**,所以屏上画的
是发生过的事,不是自己以为发生了的事。

这一侧的分层:`atomcode-tui` 只有画法和按键(`tui/src/tools.rs`、
`tui/src/modules/tools.rs`),数据与开关从 `tui-tools` 端口出去;端口的实现在 cli
(`cli/src/tui_tools.rs`),走宿主控制契约的 `ToolCatalog` / `SwitchTool`
(`docs/adr/0021` §2、`docs/adr/0022` §3)。判据在 `tui/tests/e2e.rs`(7 条)。
