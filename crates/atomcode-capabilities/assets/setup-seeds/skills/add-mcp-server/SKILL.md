---
name: add-mcp-server
description: 给这个 AtomCode 接一个 MCP 服务器（读写 mcp.json），并说明手机上哪些命令真能跑。用户提到「MCP」「接外部工具」「连数据库/搜索」时使用。
allowed-tools: [read_file, write_file, edit_file, bash]
---

# 接一个 MCP 服务器

## 配置写在哪

| 位置 | 作用 |
|---|---|
| `$ATOMCODE_HOME/mcp.json` | 用户级，所有项目共享。手机上是应用私有目录里的 `mcp.json`（和 `config.toml` 同一层） |
| `<项目>/.mcp.json` | 项目级，覆盖同名的用户级服务器 |

两个文件都允许 `//` 注释（JSONC）。

## 形状

```json
{
  "mcpServers": {
    "example-http": {
      "url": "http://127.0.0.1:8090/mcp"
    },
    "example-stdio": {
      "command": "python3",
      "args": ["/storage/emulated/0/AtomCode/mcp_server.py"],
      "trust": true
    }
  }
}
```

- `url` → HTTP（远程或本机端口）；`command` → stdio，由 AtomCode 拉起这个进程。
- `trust: true` 表示调用该服务器的工具时不再逐次询问；不给就保持逐次确认。
- `autoApprove: ["query", "search"]` 可以按工具名放行其中一部分。

## Android 上的硬限制（关键）

`command` 必须是**本机允许执行的程序**。Android 10+ 禁止从应用数据目录 `execve`，
所以「把脚本写成可执行文件直接当命令跑」这条路是死的。能用的是：

- `PATH` 里已有的：`git`、`jq`、`rg`、`curl`、`python3`（RustPython，随 APK 打包）、
  `make` / `tcc` / `zig`（装了 helper APK 之后）；
- 已安装运行时插件提供的别名：`lua`、`sqlite3`、`cpython`（`command -v` 确认）。

所以 stdio 服务器只有两种写法：**Python 脚本交给 `python3`**，或者**走 `url`**。
需要 Node 才能跑的 `npx` 类服务器在这台设备上跑不起来，别硬配。

注意：脚本本身是**数据**，可以被 `python3` 读，不需要（也不能）`chmod +x`。

## 项目级配置要先信任

别人仓库里提交的 `.mcp.json` 默认**不连接**——查 `/mcp/status` 能看到该项目
`trusted: false`、服务器列在 `blocked` 里。这是防止打开一个仓库就自动拉起它的进程。
信任之后才会连接；本机自己的项目放心确认即可。

## 验证

1. 改完配置让运行时重连（设置页的「重新加载」，或 `POST /config/reload`）。
2. `GET /mcp/status` 里应看到该服务器名，状态从 `connecting` 变成 `ok`，
   工具数为它实际提供的数量。
3. MCP 工具在模型侧的全名是 `mcp__<server>__<tool>`，不在 `list_skills` 里列出
   （那只是 skill）。

## 常见失败

- `ok` 但工具数 0：服务器连上了但没声明工具，先确认它自己的 `tools/list` 返回什么。
- 一直 `connecting`：stdio 的 `command` 找不到（Android 上多半是 exec 被拒或路径不在
  `PATH`），`url` 的端口没起。
- 服务器出现在 `blocked`：项目没被信任，见上一节。
