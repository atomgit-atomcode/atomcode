# AtomCode MCP 集成说明

> AtomCode 已实现 **MCP（Model Context Protocol）客户端**：通过 `.mcp.json` / `~/.atomcode/mcp.json` 连接外部 MCP server，将其 **tools** 暴露为与普通内建工具一致的可调用工具（含审批链路）。

---

## 1. 当前能力概览

### 1.1 已实现

- **配置**：项目根 `.mcp.json` 与用户目录 `~/.atomcode/mcp.json`；顶层键 **`mcpServers`**（与 Cursor 等一致）；旧键 `servers` 仍可解析；同名 server **项目覆盖用户**；支持 `disabled`；字符串中 `${VAR}`、`${VAR:-default}` 展开。
- **传输**：`stdio`（`command` + `args` + `env`）、`HTTP`（`url` + `headers`）；默认超时 30s，可用 `timeout_ms` 覆盖。HTTP 请求在未自定义 `Accept` 时默认带 `Accept: application/json, text/event-stream`（与 Playwright 等要求 Streamable HTTP / SSE 协商的端点一致）。HTTP OAuth 支持 `auth.type = "oauth"`、Bearer 注入、401 metadata discovery、PKCE 登录、动态客户端注册与 refresh token。**stdio 消息格式**与 MCP 规范一致：每条 JSON-RPC 为 **一行 NDJSON**（末尾换行）；仍兼容读取旧式 **`Content-Length:` + 正文** 的服务端响应。
- **协议**：JSON-RPC；`initialize`、`tools/list`、`tools/call`；`initialize` 结果里解析 `capabilities`（当前仅用到 tools 能力标记）。
- **兼容性细节**：当某个请求不需要参数时，客户端会**省略** `params` 字段（不会发送 `"params": null`），避免部分 JS SDK 服务端在收到 `null` 时卡住（例如 `tools/list`）。
- **工具注册**：每个远端 tool 映射为 `mcp__{server_key}__{tool_name}`，经 `McpToolAdapter` 注册到 `ToolRegistry`。
- **权限**：MCP 适配器对每次调用返回 `RequireApproval`（说明里带 server / tool / 参数摘要），走现有交互式审批；`PermissionStore` 的 session grant / override 按键为 **完整工具名** `mcp__...`（与内建工具相同），**不是** `mcp:server:tool` 形式。
- **无头模式**（`-p` / `--prompt-file` / `fixissue`）：启动时 **`McpRegistry::from_config` 同步连接**启用中的 server，连接成功后再 `list_tools` 并一次性注册 MCP 工具。
- **TUI 模式**：**后台并行连接**（`from_config_background_with_events`），不阻塞进入界面；连接成功或失败通过 `McpConnectEvent` 写入会话区；**每连上一个 server 就 `register_mcp_tools_async` 动态追加**该 server 的工具。
- **`/mcp`**：列出当前 registry 中 **已成功 `initialize` 并入表** 的 server 及其 `ServerStatus`（见下节限制）；`/mcp reload` 重新加载 `.mcp.json` / `~/.atomcode/mcp.json` 并后台重连启用中的 server；`/mcp tools <server>` 异步列出该 server 的远端 tools（若超时/失败会提示）。TUI 动态注册和 `/mcp tools` 的外层 `tools/list` 等待时间按该 server 的 `timeout_ms + 5s` 计算（默认 35s），避免早于 transport 自身超时取消。

### 1.2 未实现 / 限制（与代码一致）

- **Resources / Prompts**：无 `list_mcp_resources`、`read_mcp_resource`，无 MCP prompt → slash command。
- **HTTP 自动重连**：无指数退避；stdio 子进程无自动重启。
- **`tools/list` 变更**：无 `list_changed` 动态刷新。
- **`/mcp` 展示**：registry 里**只保存连接成功**（`initialize` 成功）的 server，因此 `/mcp` 只会展示**已连接**的 server；失败/未连上/正在连接的 server **不会**出现在列表中（失败仅通过会话行 `✗ MCP server '…' failed: …` 可见）。
- **工具结果内容**：`call_tool` 仅将 **text** 类型 content 块拼接为字符串；image/resource 块当前不参与输出。
- **roots / elicitation**、daemon 侧 MCP API、插件捆绑 server：未实现。
- **OAuth 限制**：通用 OAuth 只覆盖 HTTP MCP；登录需显式执行 `atomcode mcp login <server>` 或 `/mcp login <server>`，后台连接不会自动弹浏览器。若授权服务器不提供 dynamic client registration，需要在配置或命令行提供 `client_id`；confidential client 需要通过环境变量提供 secret。

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

- **对外工具名**：`mcp__{mcpServers 映射中的 key}__{远端 tool name}`  
  例：配置里 `"mcpServers": { "github": { ... } }` 且远端有 `get_issue` → `mcp__github__get_issue`。
- **执行**：`TurnRunner` 分发到适配器 → `McpRegistry::call_tool` → 对应 transport 的 `tools/call`。
- **禁用工具**：与其它工具相同，可使用 `--disable-tools` 或环境变量 `ATOMCODE_DISABLE_TOOLS`（逗号分隔），传入完整名如 `mcp__github__get_issue`。

---

## 5. 配置说明

### 5.1 Schema 要点

顶层为 `{ "mcpServers": { "<name>": { ... } } }`（亦兼容旧键 `"servers"`）。每个 server 建议只写一种 transport：

- **stdio**：`command`（必填）+ 可选 `args`、`env`、`timeout_ms`、`disabled`
- **HTTP**：`url`（必填）+ 可选 `headers`、`auth`、`timeout_ms`、`disabled`

如果同一个 server 同时写了 `command` 和 `url`，当前实现会优先按 stdio（`command`）处理；为避免歧义，对外配置请不要双写。

### 5.2 项目级 `.mcp.json` 示例

```json
{
  "mcpServers": {
    "github": {
      "url": "https://api.githubcopilot.com/mcp/",
      "auth": {
        "type": "oauth",
        "provider": "github",
        "client_secret_env": "GITHUB_MCP_CLIENT_SECRET"
      },
      "timeout_ms": 60000
    },
    "notion": {
      "url": "https://mcp.notion.com/mcp",
      "auth": {
        "type": "oauth",
        "issuer": "https://mcp.notion.com",
        "resource": "https://mcp.notion.com/mcp"
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

OAuth 字段：

- `type = "oauth"`：启用 HTTP OAuth。
- `issuer`：可选授权服务器 issuer；省略时客户端会先请求 MCP server 并从 `WWW-Authenticate` 发现 resource metadata。
- `resource`：可选 resource identifier 或 metadata URL；token 请求会带上 resource。
- `client_id`：可选预注册客户端 ID；若省略，客户端会尝试 dynamic client registration。
- `client_secret_env`：可选环境变量名，用于 confidential client 的 secret。
- `scopes`：可选 scope 列表；省略时不主动请求 scope，由授权服务器使用默认授权范围。

GitHub OAuth App 使用 authorization-code flow 时需要 client secret；可在配置中写 `"client_secret_env": "GITHUB_MCP_CLIENT_SECRET"`，或登录时传 `--client-secret-env GITHUB_MCP_CLIENT_SECRET`。

GitHub MCP 的完整配置和排错流程见 [mcp/github.md](./mcp/github.md)。

### 5.3 用户级配置

路径：`~/.atomcode/mcp.json`，字段相同。**同名 server 以项目级为准**（后写入的 project 配置覆盖 user）。

### 5.4 CLI：`atomcode mcp add`（类 Claude `mcp add`）

向 **stdio** 配置写入 `mcpServers.<name>`（`command` + `args`），无需手改 JSON：

```bash
# 项目根 .mcp.json（默认当前目录）
atomcode mcp add playwright npx @playwright/mcp@latest

# 用户级 ~/.atomcode/mcp.json
atomcode mcp add playwright npx @playwright/mcp@latest --global

# 指定项目目录
atomcode mcp add playwright npx @playwright/mcp@latest -C /path/to/repo
```

说明：

- 与 `claude mcp add <name> <command> [args…]` 同一思路：首参为 server 键名，其后为可执行文件及参数。
- **同名会整段覆盖**该键（仅写入 `command` / `args`，不保留该键下原 `env` 等字段）；HTTP 型 `url` 条目请仍用手写 JSON 或编辑器。
- 合并时会读入已有 `servers` + `mcpServers`，写回时只保留 **`mcpServers`**（去掉顶层 `servers`）。

### 5.5 CLI：HTTP OAuth 登录

```bash
# GitHub remote MCP（client id 也可由 ATOMCODE_GITHUB_MCP_CLIENT_ID 提供）
atomcode mcp add-github-oauth github --global
atomcode mcp login github \
  --client-id "$ATOMCODE_GITHUB_MCP_CLIENT_ID" \
  --client-secret-env GITHUB_MCP_CLIENT_SECRET

# 通用 HTTP OAuth MCP
atomcode mcp login notion

# 需要 confidential client 的服务
atomcode mcp login slack --client-id "$SLACK_CLIENT_ID" --client-secret-env SLACK_CLIENT_SECRET
```

登录成功后 token 存在 `~/.atomcode/mcp_auth.toml`，后续 HTTP 请求自动加 `Authorization: Bearer ...`。token 过期且有 refresh token 时会自动刷新；刷新失败时重新执行 login。

---

## 6. 运行时行为摘要

| 模式 | MCP 加载 | 工具出现时机 |
|------|------------|----------------|
| TUI | 后台 `tokio::spawn` 并行连接 | 各 server `initialize` 成功后陆续注册 |
| 无头（`-p` / `--prompt-file` / `fixissue`） | `from_config().await` 阻塞至各连接尝试结束 | 仅在至少拿到一批工具时挂载 `McpRegistry`；若全部失败则无 MCP 工具 |

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
| Phase 3 | roots、elicitation、daemon MCP API、插件携带 server | 未做 |

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
  "mcpServers": {
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
  "mcpServers": {
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

## 11. 服务文档

特定 MCP 服务的配置、OAuth 登录和排错步骤放在 `docs/mcp/` 目录：

- [GitHub MCP OAuth 使用说明](./mcp/github.md)

---

## 12. 同类产品参考

| 产品 | 配置 | Transport | 备注 |
|------|------|-------------|------|
| Claude Code | `.mcp.json` | stdio/HTTP/SSE | OAuth、resources、prompts 等更全 |
| Cursor | `.mcp.json` | stdio/HTTP | roots、elicitation 等 |
| Codex | CLI 添加 | stdio/HTTP | OpenAI 生态 |

AtomCode 使用 **`.mcp.json` 的 `mcpServers` 块**（与 Cursor 等一致，并兼容旧 `servers` 键），常见 `command` / `url` 类型的 MCP server 配置通常可直接复用；超出当前字段或能力范围的配置需按本仓库代码与上表「未实现」一节确认。
