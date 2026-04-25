# AtomCode MCP 集成说明

> AtomCode 已实现 **MCP（Model Context Protocol）客户端**：通过 `.mcp.json` / `~/.atomcode/mcp.json` 连接外部 MCP server，将其 **tools** 暴露为与普通内建工具一致的可调用工具（含审批链路）。

---

## 1. 当前能力概览

### 1.1 已实现

- **配置**：项目根 `.mcp.json` 与用户目录 `~/.atomcode/mcp.json`；同名 server **项目覆盖用户**；支持 `disabled`；字符串中 `${VAR}`、`${VAR:-default}` 展开。
- **传输**：`stdio`（`command` + `args` + `env`）、`HTTP`（`url` + `headers`）；默认超时 30s，可用 `timeout_ms` 覆盖。
- **协议**：JSON-RPC；`initialize`、`tools/list`、`tools/call`；`initialize` 结果里解析 `capabilities`（当前仅用到 tools 能力标记）。
- **工具注册**：每个远端 tool 映射为 `mcp__{server_key}__{tool_name}`，经 `McpToolAdapter` 注册到 `ToolRegistry`。
- **权限**：MCP 适配器对每次调用返回 `RequireApproval`（说明里带 server / tool / 参数摘要），走现有交互式审批；`PermissionStore` 的 session grant / override 按键为 **完整工具名** `mcp__...`（与内建工具相同），**不是** `mcp:server:tool` 形式。
- **无头模式**（`-p` / `--prompt-file` / `fixissue`）：启动时 **`McpRegistry::from_config` 同步连接**所有 server，连接成功后再 `list_tools` 并一次性注册 MCP 工具。
- **TUI 模式**：**后台并行连接**（`from_config_background_with_events`），不阻塞进入界面；连接成功或失败通过 `McpConnectEvent` 写入会话区；**每连上一个 server 就 `register_mcp_tools_async` 动态追加**该 server 的工具。
- **`/mcp`**：列出当前 registry 中 **已成功 `initialize` 并入表** 的 server 及其 `ServerStatus`（见下节限制）。

### 1.2 未实现 / 限制（与代码一致）

- **Resources / Prompts**：无 `list_mcp_resources`、`read_mcp_resource`，无 MCP prompt → slash command。
- **HTTP 自动重连**：无指数退避；stdio 子进程无自动重启。
- **`tools/list` 变更**：无 `list_changed` 动态刷新。
- **`/mcp` 展示**：失败或未连上的 server **不会**出现在列表中（失败仅通过启动时的会话行 `✗ MCP server '…' failed: …` 可见）；正在连接中的 server 在连上之前也不会出现在 `/mcp` 列表中。
- **工具结果内容**：`call_tool` 仅将 **text** 类型 content 块拼接为字符串；image/resource 块当前不参与输出。
- **OAuth / roots / elicitation**、daemon 侧 MCP API、插件捆绑 server：未实现。

---

## 2. 设计原则（仍适用）

- **内建工具优先，MCP 作外延**：内建工具语义稳定；MCP 用于 GitHub、数据库等外部能力。
- **复用现有栈**：`ToolRegistry`、`PermissionStore` / `InteractivePermissionDecider`、`TurnRunner`、`AgentLoop`；工具输出与其它工具一样会经过统一的 `post_process_tool_results` 等后处理路径（大输出截断策略由全局 truncate 逻辑决定，**无**单独的「MCP-only」外部化存储）。

---

## 3. 代码模块布局

实现位于 `crates/atomcode-core/src/mcp/`：

| 文件 | 职责 |
|------|------|
| `mod.rs` | 模块导出 |
| `config.rs` | 配置反序列化、`load_mcp_config`、环境变量展开 |
| `types.rs` | JSON-RPC、initialize/list/call 相关类型、`ServerStatus` |
| `client.rs` | `McpClient` trait、`McpToolInfo` |
| `registry.rs` | `McpRegistry`、后台/阻塞加载、`McpConnectEvent`、`call_tool` |
| `transport_stdio.rs` | stdio 子进程与读写循环 |
| `transport_http.rs` | HTTP 客户端封装 |
| `tool_adapter.rs` | `McpToolAdapter`、`register_mcp_tools` / `register_mcp_tools_async` |

CLI 入口（`crates/atomcode-cli/src/main.rs`）根据是否无头选择阻塞或后台 MCP 初始化；TUI（`crates/atomcode-tuix`）消费 `mcp_connect_rx` 并动态注册工具；斜杠命令 **`/mcp`** 在 `crates/atomcode-tuix/src/event_loop/commands.rs` 中实现。

---

## 4. 工具命名与执行路径

- **对外工具名**：`mcp__{servers 映射中的 key}__{远端 tool name}`  
  例：配置里 `"servers": { "github": { ... } }` 且远端有 `get_issue` → `mcp__github__get_issue`。
- **执行**：`TurnRunner` 分发到适配器 → `McpRegistry::call_tool` → 对应 transport 的 `tools/call`。
- **禁用工具**：与其它工具相同，可使用 `--disable-tools` 或环境变量 `ATOMCODE_DISABLE_TOOLS`（逗号分隔），传入完整名如 `mcp__github__get_issue`。

---

## 5. 配置说明

### 5.1 Schema 要点

顶层为 `{ "servers": { "<name>": { ... } } }`。每个 server **必须**二选一：

- **stdio**：`command`（必填）+ 可选 `args`、`env`、`timeout_ms`、`disabled`
- **HTTP**：`url`（必填）+ 可选 `headers`、`timeout_ms`、`disabled`

### 5.2 项目级 `.mcp.json` 示例

```json
{
  "servers": {
    "github": {
      "url": "https://api.github.com/mcp/",
      "headers": {
        "Authorization": "Bearer ${GITHUB_TOKEN}"
      },
      "timeout_ms": 30000
    },
    "postgres": {
      "command": "npx",
      "args": ["-y", "@bytebase/dbhub", "--dsn", "${POSTGRES_DSN}"],
      "env": {
        "NODE_ENV": "production"
      },
      "timeout_ms": 10000
    }
  }
}
```

### 5.3 用户级配置

路径：`~/.atomcode/mcp.json`，字段相同。**同名 server 以项目级为准**（后写入的 project 配置覆盖 user）。

---

## 6. 运行时行为摘要

| 模式 | MCP 加载 | 工具出现时机 |
|------|------------|----------------|
| TUI | 后台 `tokio::spawn` 并行连接 | 各 server `initialize` 成功后陆续注册 |
| 无头 | `from_config().await` 阻塞至各连接尝试结束 | 仅在至少拿到一批工具时挂载 `McpRegistry`；若全部失败则无 MCP 工具 |

单个 server 连接失败 **不会**拖垮进程；错误打到 stderr 或 TUI 会话中的 `McpConnectEvent::Failed`。

---

## 7. 安全与审批

- MCP 工具默认 **每次** 走 `RequireApproval`（外部不可信代码）。
- 持久/会话放行在 `PermissionStore` 中按 **`mcp__server__tool` 全名** 记录。
- 将 MCP 返回内容视为不可信工具输出，不得当作 system 指令升级。

---

## 8. 路线图（文档层跟踪，非代码承诺）

| 阶段 | 内容 | 状态 |
|------|------|------|
| Phase 1 MVP | stdio/HTTP、tools、配置、审批、TUI 连接提示与 `/mcp`（已连 server） | **已落地** |
| Phase 2 | resources/prompts 工具、动态 list_changed、HTTP 重连、更完整的 MCP 面板 | 未做 |
| Phase 3 | OAuth、roots、elicitation、daemon MCP API、插件携带 server | 未做 |

---

## 9. 验收参考（手工）

1. 项目根放置 `.mcp.json` 后，启动 AtomCode（TUI）可在会话区看到 MCP 连接成功/失败行。
2. 连接成功后，对应 MCP tools 以 `mcp__...` 形式进入模型可用工具集（TUI 下可能略晚于首屏）。
3. 模型调用 MCP tool 时走现有确认 UI；无头模式下需审批的工具策略与现有一致（如 bash 自动允许等，其它仍受审批逻辑约束）。
4. 某一 server 失败时，其余 server 与主程序仍可用。

---

## 10. 测试方法

### 10.1 内置 `mcp-test-server`

源码：`crates/atomcode-core/src/bin/mcp-test-server.rs`，提供 `echo` tool（参数 `message`）。

在工作区根目录构建：

```bash
cargo build --release -p atomcode-core --bin mcp-test-server
```

可执行文件：`target/release/mcp-test-server`（相对仓库根）。

### 10.2 `.mcp.json` 最小示例

```json
{
  "servers": {
    "test-server": {
      "command": "target/release/mcp-test-server",
      "args": [],
      "timeout_ms": 5000
    }
  }
}
```

（路径请按本机 `target/release/...` 或绝对路径调整。）

### 10.3 运行与调用

```bash
cargo run --release -p atomcode
```

在对话中可让模型使用：`mcp__test-server__echo`，参数 JSON 含 `"message": "Hello MCP!"`。

仓库内另有 **`.mcp.json.example`**，演示用 `cargo run ... mcp-test-server` 的方式（默认 `disabled: true`，启用前请改为 `false` 并确认 `manifest-path` 指向 **`crates/atomcode-core/Cargo.toml`** 或等价路径，否则从仓库根执行会找不到包）。

### 10.4 真实生态 Server 示例

```bash
cat > ~/.atomcode/mcp.json << 'EOF'
{
  "servers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"],
      "timeout_ms": 10000
    }
  }
}
EOF
```

---

## 11. 同类产品参考

| 产品 | 配置 | Transport | 备注 |
|------|------|-------------|------|
| Claude Code | `.mcp.json` | stdio/HTTP/SSE | OAuth、resources、prompts 等更全 |
| Cursor | `.mcp.json` | stdio/HTTP | roots、elicitation 等 |
| Codex | CLI 添加 | stdio/HTTP | OpenAI 生态 |

AtomCode 使用常见 **`.mcp.json` 的 `servers` 块**，便于复用现有 MCP server 配置思路；具体能力与上表「未实现」一节以本仓库代码为准。
