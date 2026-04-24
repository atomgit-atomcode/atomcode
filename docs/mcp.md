# AtomCode MCP 功能设计与实施方案

> 目标：为 AtomCode 实现 MCP (Model Context Protocol) 客户端能力，接入外部工具生态。

## 1. 现状评估

### 1.1 当前能力

AtomCode 已具备接入 MCP 的基础设施：

- **工具系统**：`ToolRegistry` + `Tool` trait 支持动态注册
- **审批体系**：`PermissionStore` + `PermissionDecider` 已成熟
- **执行链路**：`AgentLoop` / `TurnRunner` 可消费运行时生成的工具
- **输出控制**：已有 `result_store` 和截断逻辑

### 1.2 明确缺失

- 没有 MCP server 配置模型
- 没有 `stdio` / HTTP transport
- 没有 initialize / capabilities 协商
- 没有 tools / resources / prompts 协议抽象
- 没有 `.mcp.json` 配置入口

> **结论**：AtomCode 具备接入 MCP 的基础设施，但尚未实现 MCP 客户端。

---

## 2. 设计原则

### 2.1 Native tools first，MCP as external layer

MCP 不应替换现有内建工具，而是作为外部能力接入层：
- 内建工具有更强的本地语义和稳定输出
- MCP server 适合补充能力：GitHub、Sentry、Notion、数据库等

### 2.2 复用现有基础设施

优先复用：
- `ToolRegistry` - 工具注册
- `PermissionStore` - 权限管理
- `TurnRunner` - 执行链路
- TUI 事件流 - 展示层

### 2.3 兼容事实标准

- 项目级 `.mcp.json`
- 用户级 `~/.atomcode/mcp.json`
- 环境变量展开 `${VAR}` 和 `${VAR:-default}`

### 2.4 分阶段落地

1. **Phase 1 (MVP)**：tools only
2. **Phase 2**：resources / prompts
3. **Phase 3**：OAuth / roots / elicitation

---

## 3. 目标架构

### 3.1 模块结构

```
crates/atomcode-core/src/mcp/
├── mod.rs              # 模块入口
├── config.rs           # 配置解析、scope 合并、env 展开
├── types.rs            # MCP 数据模型 (JSON-RPC, 能力等)
├── client.rs           # McpClient trait
├── registry.rs         # 多 server 连接管理
├── transport_stdio.rs  # stdio transport
├── transport_http.rs   # HTTP transport
└── tool_adapter.rs     # MCP tool -> Tool trait 适配
```

### 3.2 分层设计

| 层级 | 职责 |
|------|------|
| 配置层 | 读取 `.mcp.json`，env 展开，scope 合并 |
| 连接层 | 启动/连接 server，initialize，capabilities |
| 适配层 | MCP tools -> ToolRegistry |
| 展示层 | TUI / CLI / Daemon 状态展示 |

---

## 4. 能力映射

### 4.1 Tools (Phase 1)

**命名规则**：
```
mcp__{server_name}__{tool_name}
```

例如：
- `mcp__github__get_issue`
- `mcp__postgres__query`

**执行流程**：
1. AgentLoop 启动时连接所有 MCP servers
2. 拉取 `tools/list`
3. 为每个 tool 创建 `McpToolAdapter`，注册到 `ToolRegistry`
4. 模型调用时，`TurnRunner` 分发到 adapter
5. adapter 转发到对应 server 的 `tools/call`
6. 输出包装为 `ToolResult`

### 4.2 Resources (Phase 2)

不建议自动消费所有 resources，而是提供工具：
- `list_mcp_resources` - 列出可用资源
- `read_mcp_resource` - 读取指定资源

参数显式带 `server` 与 `uri`。

### 4.3 Prompts (Phase 2)

映射为 slash command：
```
/mcp__github__pr_review 123
/mcp__jira__create_issue "标题" high
```

---

## 5. 配置设计

### 5.1 项目级 `.mcp.json`

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

### 5.2 用户级配置

位置：`~/.atomcode/mcp.json`

与项目级同 schema，优先级低于项目级。

### 5.3 支持字段

| 字段 | 说明 |
|------|------|
| `command` | stdio 模式：可执行命令 |
| `args` | 命令参数 |
| `env` | 环境变量 |
| `url` | HTTP 模式：服务端点 |
| `headers` | HTTP 请求头 |
| `disabled` | 禁用标记 |
| `timeout_ms` | 超时时间 |

---

## 6. 运行时设计

### 6.1 启动顺序

1. 解析 MCP 配置
2. 建立 `McpRegistry`
3. 连接所有启用 servers
4. 获取 capability snapshot
5. 注册 MCP tools 到 `ToolRegistry`
6. 注入 `McpRegistry` 到 AgentLoop

### 6.2 连接策略

**stdio**：
- Agent 启动时拉起子进程
- 持有 stdin/stdout
- 失败标记 disconnected，不影响主程序

**HTTP**：
- 启动即初始化
- 便于尽早发现认证/网络问题

### 6.3 重连策略

- HTTP server：指数退避重连
- stdio server：不自动重启，需手动 reload

---

## 7. 安全设计

### 7.1 权限粒度

权限键格式：
```
mcp:{server}:{tool}
```

例如：`mcp:github:get_issue`

### 7.2 审批策略

- 所有 MCP tools 默认需要审批
- 项目级 server 首次连接需额外确认
- 认证失败 / 配置变更需重新确认

### 7.3 输出控制

- 单次输出 token 上限
- 超时限制
- 并发调用限制
- 复用现有 `result_store` 和截断逻辑

### 7.4 Prompt Injection 防护

MCP 返回内容视为不可信工具输出，不得升级为 system instruction。

---

## 8. 实施任务清单

### Phase 1: MVP

**目标**：AtomCode 能使用标准 MCP tool

#### Task A: 配置模型与加载器

- [x] 设计 `.mcp.json` 反序列化结构
- [x] 实现项目级与用户级配置读取
- [x] 实现环境变量展开 `${VAR}` 和 `${VAR:-default}`
- [x] 实现按 server name 合并与优先级覆盖

**产出**：
- `mcp/config.rs`
- `mcp/types.rs`

#### Task B: Runtime 基础类型与客户端接口

- [x] 定义 `McpClient` trait
- [x] 定义 `McpRegistry`
- [x] 设计 server 状态结构

**产出**：
- `mcp/mod.rs`
- `mcp/client.rs`
- `mcp/registry.rs`

#### Task C: stdio transport

- [x] 启动子进程
- [x] 建立 stdin/stdout 通信
- [x] 实现 initialize
- [x] 实现 `tools/list`
- [x] 实现 `tools/call`
- [x] 加入超时和错误包装
- [x] 处理启动消息排水

**产出**：`mcp/transport_stdio.rs`

#### Task D: HTTP transport

- [x] 建立 HTTP client
- [x] 实现 initialize
- [x] 实现 `tools/list`
- [x] 实现 `tools/call`
- [x] 请求超时和错误处理

**产出**：`mcp/transport_http.rs`

#### Task E: Tool Adapter

- [x] 为每个 MCP tool 生成 adapter
- [x] 实现 `Tool` trait
- [x] 实现 `execute`
- [x] 命名规则 `mcp__{server}__{tool}`

**产出**：`mcp/tool_adapter.rs`

#### Task F: 权限接入

- [x] MCP tool 默认需审批
- [x] 权限键扩展为 `mcp:{server}:{tool}`

#### Task G: 输出控制

- [ ] MCP tool 输出纳入截断体系
- [ ] 大输出外部化

#### Task H: TUI 状态展示

- [ ] `/mcp` 基础命令
- [ ] server 列表与状态

### Phase 2: 增强

- [ ] `list_mcp_resources` / `read_mcp_resource`
- [ ] MCP prompts -> slash command
- [ ] `list_changed` 动态刷新
- [ ] HTTP 重连
- [ ] 完整 `/mcp` 管理面板

### Phase 3: 完整能力

- [ ] OAuth
- [ ] roots 协商
- [ ] elicitation
- [ ] daemon MCP API
- [ ] 插件携带 MCP server

---

## 9. MVP 验收标准

1. 在项目根目录放入 `.mcp.json` 后，AtomCode 启动时能发现并连接 server
2. MCP server 暴露的 tools 能出现在模型可用工具集中
3. 模型可以正常调用 MCP tool，结果回流到对话
4. 需要审批时，MCP tool 走现有确认链路
5. MCP tool 大输出能被截断或外部化
6. `/mcp` 能看到 server 名、连接状态、错误信息
7. 某个 server 连接失败时，不影响 AtomCode 整体可用

---

## 10. 测试方法

### 10.1 内置测试服务器

AtomCode 内置 `mcp-test-server` 提供测试工具：

```bash
cargo build --release --bin mcp-test-server
```

### 10.2 配置示例

在项目根目录创建 `.mcp.json`：

```json
{
  "servers": {
    "test-server": {
      "command": "./target/release/mcp-test-server",
      "timeout_ms": 5000
    }
  }
}
```

### 10.3 测试命令

启动 AtomCode：
```bash
cargo run --release
```

测试工具调用：
```
请使用 mcp__test-server__echo 工具，传入 message 参数 "Hello MCP!"
```

### 10.4 真实 MCP 服务器

安装官方 filesystem server：

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

| 产品 | 配置格式 | Transport | 特性 |
|------|----------|-----------|------|
| Claude Code | `.mcp.json` | stdio/HTTP/SSE | OAuth, resources, prompts |
| Cursor | `.mcp.json` | stdio/HTTP | roots, elicitation |
| Codex | CLI 添加 | stdio/HTTP | OpenAI 生态 |

AtomCode 兼容 `.mcp.json` 格式，可复用现有 MCP server 生态。
