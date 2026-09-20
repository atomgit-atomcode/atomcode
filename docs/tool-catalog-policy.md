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
