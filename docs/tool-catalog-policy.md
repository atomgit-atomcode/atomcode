# 下游产品怎么卸载、新增、替换工具

面向的是**在本仓库之外**装配 agent 的产品：它自己建树（`HostState.plugins` + `extra_layers`，
或直接 `App::new(catalog, tree)`）。三个动作各自的入口如下。

判据：`crates/atomcode-harness/tests/tool_policy.rs`（机制）与
`crates/atomcode-coding/tests/tool_policy.rs`（经过 coding 产品装配之后仍然成立）。

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

入口是一条命令（不是工具，见下）：

```
/tools                          列出能调的、本次会话关掉的、配置排除的
/tools off write_file           关掉一个
/tools off mcp__github__*       关掉一整台 server 的工具
/tools on  mcp__github__create_issue    放回其中一个
```

代码里是同一套东西：`ToolsSvc` 的 `turn_off(pattern)` / `turn_on(pattern)` /
`held_back()`，返回实际动了哪些名字。

### 五条语义

- **关掉不是卸载**：工具还在它自己那一行手里，只是不进 `defs()`（模型看到的 schema）
  也不被 `get()` 解析。放回来给的是同一个对象，行不用重挂。
- **下一次请求即生效**。行式引擎每次请求现读 `defs()`（`plugins/agent_loop.rs`），
  所以同一回合的下一轮就看不到它了，不用等到下一回合。
- **后到的工具也挡得住**。`off` 记的是模式本身：MCP server 连上是异步的，人先关、
  server 后连，工具一样进不来（判据 `a_tool_that_arrives_after_the_switch_is_born_hidden`）。
- **配置的答案不是建议**。`exclude` 掉的工具压根没进过目录，`/tools on` 放不回来——
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
静默失败路径。所以 `/tools` 只登记在命令目录里（`docs/adr/0021` §10），模型看不到它。

模型看得到的是**结果**：被关掉的工具会出现在 `describe_self` 的 operations 面里，写明
是人关的、可以用 `/tools on` 放回来——不然模型会继续说自己能干一件它已经干不了的事。

### 下游怎么接

自己建树的产品：`HostState.tool_switches = Some(ToolSwitches::new())`，运行时的 mount
会把 `tools` 行 swap 成 `tools-host`。手工建树用
`atomcode_coding::on_harness::tools_host_row(switches)` 拿到那一行，并把 `tools` 行
`[[patch]] name = "tools-host"`。判据见 `coding/tests/tool_policy.rs`。
