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
# 通过 stdin 接收上下文 JSON
INPUT=$(cat)

# 解析关键信息（推荐安装 jq：brew install jq / apt-get install jq）
if command -v jq &> /dev/null; then
    TOOL=$(echo "$INPUT" | jq -r '.tool_name // empty')
    echo "Hook saw tool: $TOOL" >&2
else
    # 无 jq 时可用 python 替代：
    # TOOL=$(echo "$INPUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('tool_name',''))" 2>/dev/null)
    echo "Hook: raw input received" >&2
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

AtomCode 支持 **三种** hook 实现，通过两个配置文件管理：

| 方式 | 配置文件 | 实现 | 适用场景 |
|------|---------|------|---------|
| **TOML ScriptHook** | `hooks.toml` → `[[hooks]]` | 本地脚本（shell/python） | 本地定制、快速原型 |
| **TOML Webhook** | `hooks.toml` → `[[webhooks]]` / `[[async_webhooks]]` | HTTP 远程调用 | 云端服务、外部集成 |
| **JSON CC 兼容** | `.hooks.json` / `hooks.json` | Shell 命令（旧协议） | 兼容 CC 插件 |

> 三种方式可以共存。实际上都是通过 `HookEngine::load_all()` 统一加载：
> 1. JSON hooks（`hooks.json`）
> 2. TOML hooks（ScriptHook + WebhookHook，来自 `hooks.toml`）
> 3. 内置 Hook（Rust 原生，自动注册）
>
> 全局 hooks 先加载，项目 hooks 后加载。项目级同名 hook **覆盖**全局同名 hook（后加载优先）。

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

`system_prompt` 的 stdin 输入与 `post_turn` 一致（包含基础上下文，无 `tool_args`/`result_context`）。脚本应将追加的系统 Prompt 内容输出到 stdout（纯文本或 JSON 的 `message` 字段）。

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
{"result": "warning", "message": "file is large, review carefully"}
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

| trigger 值（规范） | 别名 | 触发时机 |
|-----------|------|---------|
| `turn_start` | — | Turn 开始前 |
| `tool_call_start` | — | 工具调用开始时 |
| `pre_tool` | `before_tool` | 工具执行前 |
| `post_tool` | `after_tool` | 工具执行后 |
| `turn_complete` | `after_turn` | Turn 完成后（详细统计） |
| `post_turn` | — | Turn 完成后（旧版兼容） |
| `session_start` | — | 会话启动时 |
| `session_end` | — | 会话结束时 |
| `error` | — | 错误发生时 |
| `model_response` | — | 模型响应完成后 |
| `system_prompt` | — | 系统 Prompt 构建时 |
| `message`² | `message_received` | 用户消息接收时 |

> 使用 **contains 匹配**（逗号分隔多个 trigger）。例如 `trigger = "pre_tool,post_tool"` 会在两个时机都触发。
>
> ² `message`：WebhookHook 已实现对应 trait，但引擎尚未注册触发槽位，当前实际不可用。

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

Hook 通过环境变量接收上下文（`ATOMCODE_HOOK_EVENT`、`ATOMCODE_HOOK_CONTEXT`、`ATOMCODE_TOOL_NAME` 等），stdout 协议按事件区分：

- **`pre_tool_use`** — 输出 `{"action":"allow"}` / `{"action":"block","reason":"..."}` / `{"action":"modify","args":{...}}`（`args` 替换工具调用参数）
- **`user_prompt_submit`** — 输出 `{"decision":"block","reason":"..."}` 阻止提交，或 `{"hookSpecificOutput":{"additionalContext":"..."}}` 注入额外上下文；纯文本 stdout 视为 additionalContext 注入
- **`post_tool_use` / `session_start` / `session_end`** — fire-and-forget，stdout 不影响流程

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

内置 Hook 自动注册，暂不支持通过配置禁用（后续 CLI 会提供 enable/disable 开关）。同名项目级 hook 无法覆盖内置 Hook（内置 Hook 是 Rust 原生，不在 TOML 配置体系内）。

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

## 调试技巧

### 手动测试 hook 脚本

TOML ScriptHook 通过 stdin 接收上下文 JSON：

```bash
# 构造测试上下文
echo '{"tool_name":"read_file","event":"post_tool_use"}' | bash path/to/hook.sh
```

JSON CC 兼容 Hook 通过环境变量接收：

```bash
# 导出环境变量模拟运行环境
export ATOMCODE_HOOK_EVENT="post_tool_use"
export ATOMCODE_TOOL_NAME="read_file"
export ATOMCODE_HOOK_CONTEXT='{"tool_name":"read_file"}'
python path/to/hook.py
```

### 配置文件语法校验

```bash
# TOML 格式校验（需要 Python ≥ 3.11；旧版请 pip install tomli 并将 tomllib 替换为 tomli）
python -c "import tomllib; tomllib.load(open('path/to/hooks.toml','rb'))"

# JSON 格式校验
python -c "import json; json.load(open('path/to/hooks.json'))"
```

### CLI 排查命令

```bash
# 查看当前加载的所有 hook
atomcode hooks list

# 查看 hook 配置路径
atomcode hooks paths

# 测试单个 hook 是否正常触发
atomcode hooks test <hook-name>
```

### Hook 不触发的 6 步排查清单

| 步骤 | 检查项 | 常见问题 |
|------|--------|---------|
| 1 | 文件路径是否存在 | `~` 不会自动展开，需用绝对路径 |
| 2 | `enabled = true` | 默认 `true`，检查是否意外设为 `false` |
| 3 | `trigger` / `event` 拼写正确 | 参考上方事件表，大小写敏感 |
| 4 | 脚本有执行权限 | Linux/macOS 需 `chmod +x` |
| 5 | 脚本没有超时 | TOML 默认 2s，JSON 默认 10s |
| 6 | 项目级 hook 覆盖了全局 hook | 项目 hook 优先级更高 |

---

## 安全注意事项

1. **项目 hooks 可覆盖全局 hooks 同名项**（项目 hooks 后加载）
2. **Hooks 不能绕过权限系统** — `pre_tool` deny 不覆盖用户的 `always_allow` 设置
3. **脚本执行有超时** — TOML ScriptHook 默认 2 秒，JSON 默认 10 秒，Webhook 默认 10 秒
4. **脚本在用户权限下运行** — 注意脚本本身的安全性
5. **超时/崩溃 fail-open** — 脚本超时或崩溃时视为 `ok`，不阻塞流程
6. **Windows 兼容性** — `~` 不会自动展开为家目录，需使用完整路径（如 `C:\Users\...`）；路径分隔符使用 `\\` 或 `/`；建议为 Python 脚本显式指定解释器路径

---

## 相关文档

- [CLI 使用指南](./hook-cli-guide.md) — `atomcode hooks` 命令详解
- [完整时机列表](./hook-timing-complete.md) — 所有 hook 时机、可用配置方式
- [Webhook 指南](./webhook-guide.md) — HTTP 远程调用
- [异步 Webhook 指南](./async-webhook-guide.md) — 批量异步发送
- [技术架构](./hook-architecture.md) — 面向开发者的架构参考
