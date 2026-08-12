# AtomCode MCP 集成说明

> AtomCode 实现了 **MCP（Model Context Protocol）客户端**：通过 `.mcp.json` / `~/.atomcode/mcp.json` 连接外部 MCP server，把它们的 **tools** 暴露成与内建工具一致的可调用工具（含审批链路）。
>
> 实现位于 **`crates/atomcode-capabilities/src/mcp/`**（L1 能力层，`mcp` Cargo feature，非 default），零 `atomcode-core` 依赖。

---

## 1. 用户如何添加一个 MCP server

### 1.1 `atomcode mcp add`（最快；仅 stdio）

```bash
# 写进项目根 .mcp.json（默认当前目录）
atomcode mcp add playwright npx @playwright/mcp@latest

# 写进用户级 ~/.atomcode/mcp.json
atomcode mcp add playwright npx -y @playwright/mcp@latest --global

# 指定项目目录
atomcode mcp add playwright npx @playwright/mcp@latest -C /path/to/repo
```

首参是 server 键名，其后整串是可执行文件 + 参数。**同名会整段覆盖**该键（只写 `command`/`args`，原有 `env` 等字段不保留）。HTTP 型 server 请手写 JSON。

### 1.2 手写配置（HTTP 只能走这条）

项目根 `.mcp.json` 或用户级 `~/.atomcode/mcp.json`，顶层键 `mcpServers`（兼容旧键 `servers`）：

```json
{
  "mcpServers": {
    "postgres": {
      "command": "npx",
      "args": ["-y", "@bytebase/dbhub", "--dsn", "${POSTGRES_DSN}"],
      "env": { "NODE_ENV": "production" },
      "timeout_ms": 10000
    },
    "notion": {
      "url": "https://mcp.notion.com/mcp",
      "headers": { "X-Workspace": "${NOTION_WS}" },
      "auth": { "type": "oauth", "issuer": "https://mcp.notion.com" },
      "trust": true,
      "autoApprove": ["search"]
    }
  }
}
```

**支持 JSONC 注释**：`//` 行注释与 `/* */` 块注释都会在解析前被剔除（`config.rs::strip_json_comments`），字符串内容不受影响——`"https://…"` 里的 `//` 不会被误当注释。仓库根的 `.mcp.json.example` 可以原样复制使用。

两条限制：

- **尾逗号仍然非法**，这是注释容忍不是 JSON5。
- `atomcode mcp add` 和运行时「Always 放行」是读-改-写，会重新序列化整个文件。为了不抹掉你的注释，它们检测到注释时**拒绝改写并报错**，请手动编辑（或先移除注释）。只读加载不受影响。

### 1.3 远程 OAuth server

```bash
atomcode mcp add-github-oauth github --global    # 只写配置，不登录
atomcode mcp login github --client-secret-env GITHUB_MCP_CLIENT_SECRET
atomcode mcp logout github                       # 删除已存凭证
```

TUI 里等价的是 `/mcp login <server>` / `/mcp logout <server>`。token 存 `~/.atomcode/mcp_auth.toml`（0600），后续 HTTP 请求自动加 `Authorization: Bearer`；过期且有 refresh token 会自动刷新，刷新失败需重新 login。**后台连接不会自动弹浏览器**，必须显式登录。

### 1.4 ⚠️ 项目信任门（容易踩的一步）

**项目级 `.mcp.json` 里的 server，在未信任的项目里根本不会连**，状态显示 `blocked: untrusted project`（`registry.rs::partition_by_trust`）。用户级 `~/.atomcode/mcp.json` 不受此限。

```
/mcp trust        # 信任当前项目
/mcp untrust      # 撤销
```

信任记录在 `~/.atomcode/mcp_trust.json`（可用 `ATOMCODE_MCP_TRUST_STORE` 覆盖路径，测试用）。daemon 侧对应 `POST /live/mcp/trust`。

### 1.5 生效

改完配置**不用重启**，`/mcp reload` 即可（重新读两份配置并后台重连）。

---

## 2. 配置 schema

顶层 `{ "mcpServers": { "<name>": { … } } }`。每个 server 只写一种 transport；同时写 `command` 和 `url` 时**按 stdio 处理**，请勿双写。

| 字段 | 适用 | 说明 |
|---|---|---|
| `command` | stdio | 必填。可执行文件 |
| `args` | stdio | 参数数组 |
| `env` | stdio | 传给子进程的环境变量 |
| `url` | HTTP | 必填。Streamable HTTP 端点 |
| `headers` | HTTP | 自定义请求头；用户在此钉的头不会被客户端覆盖 |
| `auth` | HTTP | `{"type":"oauth", …}` 或 bearer/header 形式 |
| `timeout_ms` | 两者 | 单请求超时，默认 30000 |
| `disabled` | 两者 | `true` 时该 server 完全跳过 |
| `trust` | 两者 | `true` ⇒ 该 server 所有工具免审批 |
| `autoApprove` | 两者 | 按工具名白名单免审批（别名 `auto_approve`） |

`auth` 子字段：`type`（`"oauth"`）、`provider`、`issuer`、`resource`、`client_id`、`client_secret_env`、`scopes`、`bearer`、`header`。省略 `issuer` 时客户端先请求 MCP server，从 `WWW-Authenticate` 发现 resource metadata；省略 `client_id` 时尝试动态客户端注册（RFC 7591），授权服务器不支持则报错要求预注册 ID。

所有字符串支持 `${VAR}` 与 `${VAR:-默认值}` 展开，`command`/`args` 还支持 `~` 展开。

**同名 server 项目级覆盖用户级**。

---

## 3. 运行时行为

**单一装配路径**：TUI / 无头 / clix 都走 `McpRegistry::from_config_background_with_events`（`atomcode-coding/src/parts.rs:490`）——后台并行连接，不阻塞启动。区别只在要不要等：

| 模式 | 是否等待 |
|---|---|
| TUI | 不等。每个 server `initialize` 成功后，`mount()` 原子发布该 server 的工具供下一轮使用 |
| 无头 / clix | `runtime.wait_mcp_ready(CONNECT_TIMEOUT)`（30s，`atomcode-cli/src/main.rs:2251`）等到初次连接尝试全部落定 |

单个 server 失败不拖垮进程；失败通过 `McpConnectEvent::Failed` 进入会话区，并保留在 `/mcp` 列表里显示为 `failed: <error>`。

MCP 总开关：`CodingRuntimeConfig.mcp` 默认 `true`；`atomcode-clix` 提供 `--no-mcp`。主 CLI 没有全局关闭开关，按 server 用 `"disabled": true`。

> **缓存红线**：MCP 工具定义属于 provider 请求的缓存前缀，所以连接在首轮之前发起、工具集不在会话中途原地变更；`/mcp reload` 是重建（新前缀世代），不是原地改。

---

## 4. 工具命名、审批与放行

- **工具名**：`mcp__{server键}__{远端工具名}`。例：`"mcpServers": {"github": …}` 且远端有 `get_issue` → `mcp__github__get_issue`。
- **名字会被 sanitize**：模型看到的 `function.name` 必须匹配 `^[a-zA-Z0-9_-]+$`，而真实 server 常声明带空格、点、冒号甚至中文的工具名（litellm 会直接 400 打断整个请求，#1289）。因此两段名字里的非法字符都会被替换成 `-`（`tool.rs::sanitize_name_segment`），例如远端 `search.docs` → `mcp__srv__search-docs`。**实际调用仍按真实的 server / tool 名路由**，sanitize 只影响对外可见的名字；反查、autoApprove 匹配、按 server 列工具都按 sanitize 后的形式对齐。
- **默认审批**：MCP 是外部不可信代码，适配器声明 `risk() = Risky`，每次调用都过审批。
- **免审批的三条路**：配置 `trust: true`（整个 server）、配置 `autoApprove: [...]`（按工具）、运行时选 "Always"（写回配置，`config.rs::add_auto_approved_tool`）。
- **权限键**：session grant / override 按**完整工具名** `mcp__server__tool` 记录，与内建工具一致，**不是** `mcp:server:tool`。
- **Plan mode**：服务器显式标注 `readOnlyHint: true` 且未标 `destructiveHint: true` 的工具，在计划模式下可执行；矛盾标注（两者皆 true）按不可读只处理，走审批（`types.rs::is_read_only`，与 codex 行为对齐）。

---

## 5. `/mcp` 与命令行

**TUI 斜杠命令**（`atomcode-tuix/src/event_loop/commands.rs::parse_mcp_subcommand`）：

| 命令 | 作用 |
|---|---|
| `/mcp` | 列出所有 server 及状态（含 `failed` / `blocked: untrusted project`） |
| `/mcp reload` | 重新加载两份配置并后台重连 |
| `/mcp tools <server>` | 列出该 server 的远端工具（等待上限 = 该 server `timeout_ms + 5s`，默认 35s） |
| `/mcp login <server>` / `/mcp logout <server>` | OAuth 登录 / 清除凭证 |
| `/mcp trust` / `/mcp untrust` | 信任 / 取消信任当前项目 |

**CLI 子命令**：`atomcode mcp add`、`add-github-oauth`、`login`、`logout`。

**daemon HTTP 端点**：`GET /mcp/status`、`POST /mcp/reload`、`POST /live/mcp/trust`。

---

## 6. 协议细节

- **协议修订**：`initialize` 请求 `2025-11-25`（`types.rs::MCP_PROTOCOL_VERSION`），并接受服务器协商降级到更早修订。
- **`MCP-Protocol-Version`**：HTTP 传输在握手之后的每个请求（含 `notifications/initialized` 与会话 DELETE）都回显**服务器同意的**修订；握手本身不带、空值不带、用户自钉该头时不覆盖。
- **客户端能力**：`initialize` 声明**空** `capabilities: {}`——我们不实现 roots / sampling / elicitation。（历史上曾错误地声明 `{"tools": {}}`，`tools` 是服务器能力。）
- **方法**：`initialize`、`tools/list`、`tools/call`。
- **Server instructions**：读取 `initialize` 响应中的可选 `instructions`，并在模型请求前通过独立的 `<mcp-server-instructions>` 不可信边界临时注入（不得复用权威 `<system-reminder>`）。只有该 server 至少一个工具已挂载到当前 runtime 时才会注入；`/mcp reload`、禁用或撤销工具后立即停止。该内容不写入 session 快照，并被明确限制为该 server 的工具使用指引，不能覆盖 system、用户、项目、安全、权限或审批规则。每个 server 最多 4000 字符，单次请求合计最多 16000 字符；当前没有独立开关。
- **stdio 帧**：标准 NDJSON（一行一条 JSON-RPC）；额外兼容读取旧式 `Content-Length:` + 正文；对启动期打到 stdout 的非协议日志行有容忍（上限 100 行）。
- **stdio 断线重连**：进程退出 / EPIPE 等可恢复错误触发一次自动重连（generation 计数避免并发重复重启）。**已发出的 `tools/call` 不会自动重放**——副作用不明时宁可报错，不重复执行。
- **HTTP**：默认 `Accept: application/json, text/event-stream`（用户未自定义时），响应支持单 JSON 或 SSE 帧；捕获并回送 `Mcp-Session-Id`（Figma Dev Mode 等有状态服务器要求），析构时尽力发 DELETE 释放会话。
- **`params` 省略**：不需要参数的方法不会发 `"params": null`（部分 JS SDK 服务端收到 `null` 会卡住，如 `tools/list`）。

---

## 7. 未实现 / 已知限制

- **Resources / Prompts**：无 `resources/*`、`prompts/*`，MCP prompt 不映射为斜杠命令。
- **通知 / 订阅**：无 `list_changed` 动态刷新、无 `notifications/*` 消费、无 sampling、无 elicitation、无 roots。
- **工具结果**：`call_tool` 只把 **text** 内容块拼成字符串，image / resource 块被丢弃（`registry.rs::call_tool`），`structuredContent` / `outputSchema` 未支持。
- **HTTP 重连**：无指数退避（stdio 有一次性重连，HTTP 没有）。
- **OAuth**：仅覆盖 HTTP MCP；无 `iss` 校验（RFC 9207）、DCR 不带 `application_type`、凭证未按 issuer 绑定——均为 `2026-07-28` 修订新增的 SHOULD/MUST。
- **`2026-07-28` 修订整体未支持**：该修订删除 `initialize` 握手改用 `server/discover`，本客户端不会说；同时新增的 Tasks / MRTR / `subscriptions/listen` / 缓存提示均未实现。
- **插件捆绑 server**：未实现。

替换为官方 Rust SDK（`rmcp`）以一次性解决上述协议层落后的可行性评估见 **[mcp-rmcp-feasibility.md](./mcp-rmcp-feasibility.md)**。

---

## 8. 代码布局

`crates/atomcode-capabilities/src/mcp/`（feature `mcp`）：

| 文件 | 职责 |
|---|---|
| `mod.rs` | 导出 + `register_mcp_tools` + `CONNECT_TIMEOUT` |
| `config.rs` | `.mcp.json` 解析、两级合并、env/`~` 展开、`add_auto_approved_tool` |
| `types.rs` | JSON-RPC / initialize / list / call 类型、`MCP_PROTOCOL_VERSION`、`initialize_params()`、`ServerStatus`、工具注解判定 |
| `client.rs` | `McpClient` trait + `McpToolInfo` |
| `registry.rs` | `McpRegistry`：后台并行连接、trust 分区、`tools/list`、`call_tool`、状态、`McpConnectEvent` |
| `transport_stdio.rs` | stdio 子进程、NDJSON 读写、重连、Windows `.cmd` 包装 |
| `transport_http.rs` | Streamable HTTP、SSE 帧解析、会话 id、协议头、OAuth token 注入 |
| `oauth.rs` | OAuth 登录/刷新、token store、GitHub 特例、metadata discovery |
| `trust.rs` | 项目信任存储与判定 |
| `tool.rs` | `McpToolAdapter`：远端工具 → kernel `Tool`，风险等级与审批 |
| `util.rs` | 本地 home/config-dir 与控制台辅助 |

消费侧：装配在 `atomcode-coding/src/parts.rs`；`/mcp` 斜杠命令在 `atomcode-tuix/src/event_loop/commands.rs`；CLI 子命令在 `atomcode-cli/src/main.rs`；daemon 端点在 `atomcode-daemon/src/lib.rs`。

---

## 9. 测试

### 9.1 内置 `mcp-test-server`

源码 `crates/atomcode-capabilities/src/bin/mcp-test-server.rs`，提供 `echo` 工具（参数 `message`）。**需要 `mcp` feature**（`required-features = ["mcp"]`）：

```bash
cargo build --release -p atomcode-capabilities --features mcp --bin mcp-test-server
```

产物：`target/release/mcp-test-server`。最小配置：

```json
{
  "mcpServers": {
    "test-server": {
      "command": "target/release/mcp-test-server",
      "timeout_ms": 5000
    }
  }
}
```

对话中调用 `mcp__test-server__echo`，参数含 `"message": "Hello MCP!"`。

### 9.2 自动化测试

```bash
cargo test -p atomcode-capabilities --features mcp
```

`tests/mcp.rs` 用上面这个真实子进程覆盖连接/发现/调用、状态检测、重连与并发失败路径；各模块另有内联单测。

### 9.3 真实生态 server

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

（放用户级可绕开项目信任门；放项目级记得先 `/mcp trust`。）

---

## 10. 服务专项文档

- [GitHub MCP OAuth 使用说明](./mcp/github.md)
