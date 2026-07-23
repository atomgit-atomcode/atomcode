# atomcode-daemon

AtomCode HTTP API 服务，提供对话历史查询、流式聊天、配置管理、Provider 管理等 RESTful 接口。

基于 [Axum](https://github.com/tokio-rs/axum) 框架构建，支持 SSE（Server-Sent Events）流式响应。

## 快速开始

### 编译

```bash
cargo build -p atomcode-daemon
```

### 启动服务

```bash
# 默认启动，绑定 127.0.0.1:13456
cargo run -p atomcode-daemon

# 指定端口
cargo run -p atomcode-daemon -- --port 8080

# 指定绑定 IP（允许外部访问）
cargo run -p atomcode-daemon -- --host 0.0.0.0

# 同时指定 host 和 port
cargo run -p atomcode-daemon -- --host 0.0.0.0 --port 8080
```

### 启动参数

| 参数 | 格式 | 默认值 | 说明 |
|------|------|--------|------|
| `--host` | `--host <ip>` | `127.0.0.1` | 绑定 IP 地址 |
| `--port` | `--port <port>` | `13456` | 监听端口号 |

也支持 `--host=<ip>` 和 `--port=<port>` 的等号格式。

> **安全提示**：当绑定非 loopback 地址（非 `127.0.0.1`、`localhost`、`::1`）时，服务会打印安全警告。daemon 暴露了聊天、文件编辑、工具执行等敏感端点，请确保网络可信或在前方部署反向代理并配置认证。

### 环境变量

| 环境变量 | 说明 |
|----------|------|
| `ATOMCODE_DAEMON_ENABLE_DANGEROUS_TOOLS` | 设为 `1` 启用 bash 和写文件的 daemon 工具 |

## API 接口

所有接口基础路径为 `http://<host>:<port>`，请求和响应均为 JSON 格式。

### 健康检查

#### `GET /health`

健康检查端点。

**响应示例：**

```json
{
  "status": "ok",
  "version": "5.0.1",
  "service": "atomcode-daemon"
}
```

---

### 项目管理

#### `GET /project`

获取当前项目状态（工作目录）。

**响应示例：**

```json
{
  "working_dir": "/home/user/my-project",
  "previous_dir": "/home/user/other-project",
  "recent_dirs": ["/home/user/my-project", "/home/user/other-project"],
  "name": "my-project"
}
```

#### `POST /cd`

切换工作目录（类似终端 `cd` 命令）。

**请求体：**

```json
{
  "path": "/path/to/project"
}
```

- `path`：目标目录路径，支持 `~` 展开；传 `"-"` 返回上一次目录

**响应示例：**

```json
{
  "success": true,
  "message": "Changed to /path/to/project",
  "current_dir": "/path/to/project",
  "project_hash": "a1b2c3d4e5f6a1b2"
}
```

#### `GET /projects`

列出所有历史项目（从 sessions 目录扫描）。

**响应示例：**

```json
[
  {
    "hash": "a1b2c3d4e5f6a1b2",
    "name": "my-project",
    "working_dir": "/home/user/my-project",
    "description": null,
    "session_count": 5,
    "created_at": 1715000000,
    "last_updated": 1715100000
  }
]
```

---

### 会话管理

#### `GET /projects/:hash/sessions`

列出指定项目下的所有会话。

**路径参数：**
- `hash`：项目哈希值（从 `/projects` 接口获取）

#### `GET /projects/:hash/sessions/:id`

获取会话详情，包含完整消息列表。

**路径参数：**
- `hash`：项目哈希值
- `id`：会话 ID

**响应示例：**

```json
{
  "id": "abc123",
  "name": "Bug fix session",
  "working_dir": "/home/user/my-project",
  "created_at": 1715000000,
  "updated_at": 1715100000,
  "message_count": 10,
  "messages": [
    {
      "role": "user",
      "content": "Fix the bug in main.rs"
    },
    {
      "role": "assistant",
      "content": "I'll fix the bug...",
      "tool_calls": null,
      "tool_result": null,
      "artifacts": null
    }
  ]
}
```

#### `DELETE /projects/:hash/sessions/:id`

删除指定会话。

#### `PATCH /projects/:hash/sessions/:id/rename`

重命名会话。

**请求体：**

```json
{
  "name": "New session name"
}
```

#### `GET /sessions`

列出所有项目下的所有会话（跨项目），按更新时间倒序，最多返回 50 条。

#### `POST /sessions`

创建新会话。

**请求体：**

```json
{
  "working_dir": "/path/to/project",
  "title": "Optional session title"
}
```

- `working_dir`：可选，默认使用当前项目工作目录
- `title`：可选，会话标题

**响应示例：**

```json
{
  "id": "new-session-id",
  "name": "Optional session title",
  "working_dir": "/path/to/project",
  "project_hash": "a1b2c3d4e5f6a1b2",
  "created_at": 1715100000
}
```

#### `GET /sessions/search?q=<keyword>`

按名称搜索会话（不区分大小写）。

**查询参数：**
- `q`：搜索关键词

---

### 模型

#### `GET /models`

列出所有已配置 Provider 提供的可用模型。

**响应示例：**

```json
[
  {
    "provider": "openai",
    "model": "gpt-4o",
    "provider_type": "openai",
    "is_default": true
  },
  {
    "provider": "claude",
    "model": "claude-sonnet-4-20250514",
    "provider_type": "claude",
    "is_default": false
  }
]
```

---

### 聊天（SSE 流式）

#### `POST /chat`

发送聊天消息，以 SSE（Server-Sent Events）流式返回响应。

**请求体：**

```json
{
  "message": "你的问题",
  "provider": "openai",
  "working_dir": "/path/to/project",
  "session_id": "existing-session-id"
}
```

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `message` | string | 是 | 用户消息内容 |
| `provider` | string | 否 | Provider 名称，默认使用配置中的默认 Provider |
| `working_dir` | string | 否 | 工作目录，默认使用当前项目目录 |
| `session_id` | string | 否 | 会话 ID，传入则继续已有会话，不传则创建新会话 |

**SSE 事件类型：**

| 事件类型 (`type`) | 说明 |
|-------------------|------|
| `text` | LLM 文本增量输出 |
| `reasoning` | LLM 推理/思考内容 |
| `tool_start` | 工具调用开始（含 `name`、`arguments`） |
| `tool_output` | 工具实时输出片段 |
| `tool_progress` | 长时间工具的临时进度（含 `id`、`progress`，客户端应原位替换） |
| `tool_result` | 工具调用完成（含 `name`、`output`、`success`、`duration_ms`） |
| `tokens` | Token 用量更新（含 `prompt`、`completion`、`total`） |
| `artifact_start` | 检测到代码/HTML/SVG 等制品开始 |
| `artifact_content` | 制品内容片段 |
| `artifact_end` | 制品输出结束 |
| `done` | 聊天完成（含 `tokens`、`tool_calls`、`session_id`） |
| `stopped` | 聊天被用户停止 |
| `error` | 发生错误（含 `message`） |

**示例（curl）：**

```bash
curl -N -X POST http://127.0.0.1:13456/chat \
  -H "Content-Type: application/json" \
  -d '{"message": "Hello, how are you?"}'
```

#### `POST /chat/stop`

停止正在进行的聊天。

**请求体：**

```json
{
  "session_id": "session-id-to-stop"
}
```

**响应示例：**

```json
{
  "success": true,
  "message": "Chat session abc123 stopped"
}
```

---

### 配置

#### `GET /config`

获取脱敏后的配置信息（不暴露 api_key）。

**响应示例：**

```json
{
  "path": "/home/user/.atomcode/config.toml",
  "default_provider": "openai",
  "default_workdir": "/home/user/my-project",
  "providers": [
    {
      "name": "openai",
      "type": "openai",
      "model": "gpt-4o",
      "base_url": null,
      "has_api_key": true,
      "is_default": true,
      "context_window": 128000,
      "max_tokens": null,
      "thinking_enabled": null,
      "thinking_budget": null,
      "thinking_type": null,
      "thinking_keep": null,
      "reasoning_history": null,
      "skip_tls_verify": false,
      "ephemeral": false
    }
  ]
}
```

#### `POST /config/reload`

从磁盘重新加载配置并返回。

---

### Provider 管理

#### `GET /providers`

列出所有已配置的 Provider（脱敏，不暴露 api_key）。

**响应示例：**

```json
{
  "default_provider": "openai",
  "providers": [
    {
      "name": "openai",
      "type": "openai",
      "model": "gpt-4o",
      "has_api_key": true,
      "is_default": true,
      "..."
    }
  ]
}
```

#### `POST /providers`

创建或替换一个 Provider。

**请求体：**

```json
{
  "name": "my-provider",
  "type": "openai",
  "model": "gpt-4o",
  "api_key": "sk-xxx",
  "base_url": "https://api.openai.com/v1",
  "context_window": 128000,
  "max_tokens": 4096,
  "thinking_enabled": true,
  "thinking_budget": 10000,
  "thinking_type": "enabled",
  "thinking_keep": "last",
  "reasoning_history": "full",
  "skip_tls_verify": false,
  "set_default": true
}
```

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `name` | string | 是 | Provider 名称 |
| `type` | string | 是 | Provider 类型（如 `openai`、`claude`、`ollama`） |
| `model` | string | 是 | 模型标识 |
| `api_key` | string | 否 | API 密钥 |
| `base_url` | string | 否 | 自定义 API 地址 |
| `context_window` | number | 否 | 上下文窗口大小（默认根据类型自动推断） |
| `max_tokens` | number | 否 | 最大输出 token 数 |
| `thinking_enabled` | boolean | 否 | 是否启用思考模式 |
| `thinking_budget` | number | 否 | 思考预算（最低 1024） |
| `thinking_type` | string | 否 | 思考类型 |
| `thinking_keep` | string | 否 | 思考保留策略 |
| `reasoning_history` | string | 否 | 推理历史策略 |
| `skip_tls_verify` | boolean | 否 | 跳过 TLS 验证（默认 `false`） |
| `set_default` | boolean | 否 | 设为默认 Provider（默认 `false`） |

#### `PATCH /providers/:name`

部分更新指定 Provider 的配置。

**请求体（所有字段可选）：**

```json
{
  "type": "openai",
  "model": "gpt-4o-mini",
  "api_key": "sk-new-key",
  "clear_api_key": false,
  "base_url": "https://new-url.com/v1",
  "clear_base_url": false,
  "context_window": 192000,
  "max_tokens": 8192,
  "clear_max_tokens": false,
  "thinking_enabled": true,
  "thinking_budget": 10000,
  "thinking_type": "enabled",
  "thinking_keep": "last",
  "reasoning_history": "full",
  "skip_tls_verify": false
}
```

> 对于可清空的字段（如 `api_key`、`base_url`、`max_tokens`），使用 `clear_*` 布尔字段设为 `true` 来清除，传 `null` 值到对应字段也可清除。

#### `DELETE /providers/:name`

删除指定 Provider。如果删除的是默认 Provider，会自动选择剩余中的第一个作为新默认。

#### `POST /providers/:name/default`

将指定 Provider 设为默认。

#### `PATCH /providers/:name/thinking`

更新指定 Provider 的思考模式设置。

**请求体（所有字段可选）：**

```json
{
  "enabled": true,
  "budget": 10000,
  "type": "enabled",
  "keep": "last",
  "reasoning_history": "full"
}
```

---

### 认证

#### `GET /auth/status`

获取当前认证状态。

**响应示例：**

```json
{
  "logged_in": true,
  "expired": false,
  "auth_path": "/home/user/.atomcode/auth.toml",
  "user": {
    "username": "example_user",
    "email": "user@example.com"
  },
  "token": {
    "token_type": "Bearer",
    "expires_in": 3600,
    "created_at": 1715000000,
    "has_refresh_token": true
  }
}
```

#### `POST /auth/login/start`

启动 OAuth 登录流程。

**请求体：**

```json
{
  "open_browser": true
}
```

- `open_browser`：是否自动打开浏览器（默认 `true`）

**响应示例：**

```json
{
  "login_id": "uuid-of-login-session",
  "url": "https://auth.example.com/login?code=xxx",
  "expires_in_seconds": 600,
  "daemon_instance_id": "uuid-of-daemon-process"
}
```

#### `POST /auth/login/:login_id/poll`

轮询登录会话状态。

**响应示例（等待中）：**

```json
{
  "status": "pending",
  "user": null,
  "retry_after_ms": 2000
}
```

`pending`、`authorized` 使用 HTTP 200。`expired`、`cancelled` 使用 HTTP 410，
`failed` 使用 HTTP 500；这些终态附带稳定的 `code`/`error` 和 `retryable: false`。
可重试的临时错误使用 HTTP 503 和 `retryable: true`。登录会话只属于创建它的 daemon
实例，实例重启后客户端必须重新发起登录，不能重放旧 `login_id`。

**响应示例（已授权）：**

```json
{
  "status": "authorized",
  "user": {
    "username": "example_user"
  }
}
```

#### `DELETE /auth/login/:login_id`

幂等取消登录会话。终态会短暂保留，保证并发请求和响应重试能读取一致结果。

#### `POST /auth/logout`

登出（删除本地存储的认证信息）。

---

### MCP（Model Context Protocol）

#### `GET /mcp/status`

获取所有 MCP 服务器的连接状态。

**响应示例：**

```json
{
  "servers": [
    {
      "name": "filesystem",
      "status": "connected",
      "tool_count": 5,
      "error": null
    },
    {
      "name": "github",
      "status": "error",
      "tool_count": null,
      "error": "Connection refused"
    }
  ]
}
```

#### `POST /mcp/reload`

重新加载 MCP 配置（从 `~/.atomcode/mcp.json`）。

**响应示例：**

```json
{
  "status": "reloading"
}
```

---

### CodingPlan

#### `POST /codingplan/setup`

运行 CodingPlan 初始化设置（登录、领取模型、配置 Provider）。

**请求体：**

```json
{
  "login_id": "optional-login-session-id"
}
```

- `login_id`：可选，如果未登录可传入登录会话 ID

**响应示例：**

```json
{
  "success": true,
  "report_text": "Setup complete!",
  "default_provider": "codingplan",
  "providers": ["..."],
  "steps": {
    "login": { "status": "skipped", "message": "Already logged in" },
    "claim": { "status": "ok", "message": "" },
    "models": { "status": "ok", "message": "" },
    "status": { "status": "ok", "message": "" }
  }
}
```

---

## CORS 策略

daemon 默认仅允许来自 loopback 地址的跨域请求：

- `http://localhost:*`
- `http://127.0.0.1:*`
- `http://[::1]:*`
- `https://localhost:*`

来自局域网 IP（如 `192.168.x.x`）或其他非 loopback 地址的请求将被 CORS 策略拒绝。

## 可用工具

daemon 聊天模式下可用的工具：

| 工具名 | 说明 |
|--------|------|
| `read_file` | 读取文件内容 |
| `write_file` | 创建/写入文件 |
| `edit_file` | 编辑已有文件 |
| `bash` | 执行 Shell 命令 |
| `grep` | 搜索文件内容 |
| `glob` | 按模式匹配文件 |
| `list_directory` | 列出目录内容 |
| `web_search` | 网页搜索 |
| `web_fetch` | 获取网页内容 |
| `search_replace` | 搜索替换文件内容 |
| `diagnostics` | LSP 诊断信息（需配置 LSP） |
| `use_skill` | 调用已注册的 Skill（需有可用 Skill） |

## 项目结构

```
atomcode-daemon/
├── Cargo.toml
├── README.md
└── src/
    ├── main.rs            # 主入口、路由定义、聊天流处理
    ├── api_auth.rs        # 认证相关接口（登录/登出/状态）
    ├── api_config.rs      # 配置相关接口及共享工具函数
    ├── api_provider.rs    # Provider 管理接口
    └── api_codingplan.rs  # CodingPlan 初始化接口
```

## 配置文件

daemon 使用以下配置文件（位于 `~/.atomcode/` 目录）：

| 文件 | 说明 |
|------|------|
| `config.toml` | 主配置（Provider、默认工作目录等） |
| `mcp.json` | MCP 服务器配置 |
| `auth.toml` | 认证信息（OAuth token） |
| `sessions/` | 会话数据存储目录 |
