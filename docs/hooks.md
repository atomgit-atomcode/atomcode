# AtomCode Hooks

Hooks 系统允许你在 AtomCode 的关键执行点插入自定义逻辑，实现灵活的扩展能力。

## 快速开始

### 三步配置：目录 → 脚本 → TOML

**步骤 1**：创建 hooks 目录

```bash
# 全局 hooks（对所有项目生效）
mkdir -p ~/.atomcode/hooks

# 项目级 hooks（仅当前项目生效，可覆盖同名全局 hook）
mkdir -p .atomcode/hooks
```

**步骤 2**：编写 hook 脚本

创建 `~/.atomcode/hooks/my_hook.sh`：

```bash
#!/bin/bash
# 通 过 stdin 接收上下文 JSON
INPUT=$(cat)

# 解析关键信息（可选安装 jq）
if command -v jq &> /dev/null; then
    TOOL=$(echo "$INPUT" | jq -r '.tool_name // empty')
    echo "Hook saw tool: $TOOL" >&2
fi

# 返回执行结果
echo "ok"
```

赋予执行权限：

```bash
chmod +x ~/.atomcode/hooks/my_hook.sh
```

**步骤 3**：配置 `hooks.toml`

创建 `~/.atomcode/hooks/hooks.toml`：

```toml
[[hooks]]
name = "my-hook"
description = "My custom hook"
trigger = "post_tool"      # 触发时机
script = "my_hook.sh"
script_type = "shell"       # shell | python
enabled = true
timeout_secs = 2
```

完成！启动 AtomCode 后 hook 会自动加载。

---

## 配置方式总览

AtomCode 支持 **三种** hook 配置方式：

| 方式 | 配置文件 | 实现 | 适用场景 |
|------|---------|------|---------|
| **TOML ScriptHook** | `hooks.toml` → `[[hooks]]` | 本地脚本（shell/python） | 本地定制、快速原型 |
| **TOML Webhook** | `hooks.toml` → `[[webhooks]]` / `[[async_webhooks]]` | HTTP 远程调用 | 云端服务、外部集成 |
| **JSON CC 兼容** | `.hooks.json` / `hooks.json` | Shell 命令（旧协议） | 兼容 CC 插件 |

> 三种方式可以共存。加载顺序：JSON → TOML → 内置 → Webhook。项目 hooks 覆盖同名全局 hooks。

---

## TOML ScriptHook（推荐）

### 支持的 trigger 值

| trigger 值 | 别名 | 触发时机 | 可影响流程 |
|-----------|------|---------|:--:|
| `pre_tool` | `pre_tool_execution` | 工具执行前 | ✅ 可阻止/修改参数 |
| `post_tool` | `post_tool_execution` | 工具执行后 | ❌ fire-and-forget |
| `post_turn` | — | Turn 完成后 | ❌ fire-and-forget |
| `system_prompt` | — | 构建系统 prompt 时 | ✅ 追加指令 |

### 脚本输入（stdin JSON）

```json
{
  "tool_name": "edit_file",
  "tool_args": "{\"file_path\": \"...\", ...}",
  "working_dir": "/path/to/project",
  "session_id": "session-123",
  "turn_number": 5
}
```

`post_tool` 会额外包含 `result_context`：

```json
{
  "result_context": {
    "tool_name": "edit_file",
    "tool_args": "{...}",
    "result": "File updated",
    "success": true,
    "duration_ms": 150
  }
}
```

### 脚本输出格式

```
ok                    # 继续（默认）
deny: <reason>        # 阻止（仅 pre_tool 有效）
modify: <new_args>    # 替换参数（仅 pre_tool 有效）
warning: <message>    # 继续但打印警告
```

也可以输出 JSON（推荐）：

```json
{"result": "ok", "message": "checked"}
{"result": "deny", "message": "unsafe path"}
{"result": "modify", "modified_content": "{\"file_path\": \"/safe\"}"}
```

### 完整配置示例

```toml
[[hooks]]
name = "pre-check"
description = "阻止危险的 write 操作"
trigger = "pre_tool"
script = "check_write.sh"
script_type = "shell"
enabled = true
timeout_secs = 3
```

---

## TOML Webhook

### 支持的 trigger 值（逗号分隔多个）

`pre_tool` | `before_tool` | `post_tool` | `after_tool` | `post_turn` |
`session_start` | `session_end` | `error` | `turn_start` | `turn_complete` |
`after_turn` | `tool_call_start` | `model_response` | `system_prompt`

### 同步 Webhook

```toml
[[webhooks]]
name = "slack-notify"
description = "发送工具调用通知到 Slack"
trigger = "pre_tool,post_tool"
url = "https://hooks.slack.com/services/XXX"
method = "POST"
timeout_secs = 10
retries = 2
enabled = true

[webhooks.headers]
Authorization = "Bearer YOUR_TOKEN"
```

### 异步批量 Webhook（高频场景推荐）

```toml
[[async_webhooks]]
name = "audit-log"
trigger = "post_tool"
url = "https://log.example.com/batch"
timeout_secs = 10
batch_size = 20            # 默认 10，达到后发送
flush_interval_ms = 1000   # 默认 1000ms，定时刷新
retries = 2
enabled = true

[async_webhooks.headers]
Authorization = "Bearer AUDIT_TOKEN"
```

> 异步 Webhook 不阻塞主流程。详细用法见 [Webhook 指南](./webhook-guide.md) 和 [异步 Webhook 指南](./async-webhook-guide.md)。

---

## JSON CC 兼容配置

兼容 Claude Code 插件的 `.hooks.json`。加载路径：

- `~/.atomcode/hooks.json` — 全局
- `<project>/.hooks.json` — 项目（覆盖同名全局）

```json
{
  "hooks": {
    "my-hook": {
      "event": "pre_tool_use",
      "matcher": "write*",
      "command": "echo '{\"action\": \"allow\"}'",
      "timeout_ms": 10000,
      "disabled": false
    }
  }
}
```

支持的 `event` 值：`pre_tool_use`、`post_tool_use`、`session_start`、`session_end`、`user_prompt_submit`。

Hook 通过环境变量接收上下文（`ATOMCODE_HOOK_EVENT`、`ATOMCODE_HOOK_CONTEXT`、`ATOMCODE_TOOL_NAME` 等），stdout 需输出 `{"action": "allow" | "block" | "modify"}` JSON。

---

## 内置 Hook（无需配置，自动启用）

| Hook | 触发时机 | 功能 |
|------|---------|------|
| `ToolAuditLogHook` | 工具调用时 | 记录调用到审计日志（tracing） |
| `TurnStatsHook` | Turn 开始+完成 | 统计 Turn 耗时和操作 |
| `AutoCommitHook` | Turn 完成 | 每 N 个 Turn 自动 `git commit` |
| `SessionSummaryHook` | 会话开始+结束 | 打印会话摘要 |
| `ErrorReportHook` | 错误发生时 | 记录错误详情 |
| `ResponseValidationHook` | 模型响应后 | 检测敏感信息 |

内置 Hook 自动注册，暂不支持通过配置禁用（后续 CLI 会提供 enable/disable 开关）。

---

## CLI 命令

```bash
# 列出已加载的 hooks
atomcode hooks list

# 查看配置路径
atomcode hooks paths

# 测试单个 hook
atomcode hooks test my-hook
```

---

## 安全注意事项

1. **项目 hooks 可覆盖全局 hooks 同名项**（项目 hooks 后加载）
2. **Hooks 不能绕过权限系统** — `pre_tool` deny 不覆盖用户的 `always_allow` 设置
3. **脚本执行有超时** — TOML ScriptHook 默认 2 秒，JSON 默认 10 秒，Webhook 默认 10 秒
4. **脚本在用户权限下运行** — 注意脚本本身的安全性
5. **超时/崩溃 fail-open** — 脚本超时或崩溃时视为 `ok`，不阻塞流程

---

## 相关文档

- [CLI 使用指南](./hook-cli-guide.md) — `atomcode hooks` 命令详解
- [完整时机列表](./hook-timing-complete.md) — 所有 hook 时机、可用配置方式
- [Webhook 指南](./webhook-guide.md) — HTTP 远程调用
- [异步 Webhook 指南](./async-webhook-guide.md) — 批量异步发送
- [技术架构](./hook-architecture.md) — 面向开发者的架构参考
