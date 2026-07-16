# AtomCode Hooks

The Hooks system allows you to insert custom logic at key execution points in AtomCode, enabling flexible extensibility.

## Quick Start

### Three-step setup: Directory → Script → TOML

**Step 1**: Create the hooks directory

```bash
# Global hooks (apply to all projects)
mkdir -p ~/.atomcode/hooks

# Project-level hooks (only apply to current project, override same-name global hook)
mkdir -p .atomcode/hooks
```

**Step 2**: Write a hook script

Create `~/.atomcode/hooks/my_hook.sh`:

```bash
#!/bin/bash
# Receive context JSON via stdin
INPUT=$(cat)

# Parse key info (install jq recommended: brew install jq / apt-get install jq)
if command -v jq &> /dev/null; then
    TOOL=$(echo "$INPUT" | jq -r '.tool_name // empty')
    echo "Hook saw tool: $TOOL" >&2
else
    # Without jq, use python instead:
    # TOOL=$(echo "$INPUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('tool_name',''))" 2>/dev/null)
    echo "Hook: raw input received" >&2
fi

# Return execution result
echo "ok"
```

Make it executable:

```bash
chmod +x ~/.atomcode/hooks/my_hook.sh
```

**Step 3**: Configure `hooks.toml`

Create `~/.atomcode/hooks/hooks.toml`:

```toml
[[hooks]]
name = "my-hook"
description = "My custom hook"
trigger = "post_tool"      # Trigger timing
script = "my_hook.sh"
script_type = "shell"       # shell | python
enabled = true
timeout_secs = 2
```

Done! Hooks are automatically loaded when AtomCode starts.

---

## Configuration Overview

AtomCode supports **three** hook implementations, managed via two config files:

| Method | Config file | Implementation | Use case |
|------|---------|------|---------|
| **TOML ScriptHook** | `hooks.toml` → `[[hooks]]` | Local script (shell/python) | Local customization, rapid prototyping |
| **TOML Webhook** | `hooks.toml` → `[[webhooks]]` / `[[async_webhooks]]` | HTTP remote call | Cloud services, external integrations |
| **JSON CC Compatible** | `.hooks.json` / `hooks.json` | Shell command (legacy protocol) | CC plugin compatibility |

> All three methods can coexist. They are loaded uniformly via `HookEngine::load_all()`:
> 1. JSON hooks（`hooks.json`）
> 2. TOML hooks (ScriptHook + WebhookHook, from `hooks.toml`)
> 3. Built-in Hooks (native Rust, auto-registered)
>
> Global hooks load first, project hooks load after. Same-name project hooks **override** global hooks (last-loaded wins).

---

## TOML ScriptHook (Recommended)

### Supported trigger values

| trigger value | Alias | When triggered | Can affect flow |
|-----------|------|---------|:--:|
| `pre_tool` | `pre_tool_execution` | Before tool execution | ✅ Can block/modify args |
| `post_tool` | `post_tool_execution` | After tool execution | ❌ fire-and-forget |
| `post_turn` | — | After turn completes | ❌ fire-and-forget |
| `system_prompt` | — | When building system prompt | ✅ Can append instructions |

### Script input (stdin JSON)

```json
{
  "tool_name": "edit_file",
  "tool_args": "{\"file_path\": \"...\", ...}",
  "working_dir": "/path/to/project",
  "session_id": "session-123",
  "turn_number": 5
}
```

`post_tool`'s stdin is a nested structure that additionally includes `result_context` (a sibling of `hook_context`):

```json
{
  "hook_context": {
    "tool_name": "edit_file",
    "tool_args": "{...}",
    "working_dir": "/path/to/project",
    "session_id": "session-123",
    "turn_number": 5
  },
  "result_context": {
    "tool_name": "edit_file",
    "tool_args": "{...}",
    "result": "File updated",
    "success": true,
    "duration_ms": 150
  }
}
```

`system_prompt` stdin input is the same as `post_turn` (includes base context, no `tool_args`/`result_context`). The script should output appended system prompt content to stdout (plain text or JSON `message` field).

### Script output format

```
ok                    # Continue (default)
deny: <reason>        # Block (only effective for pre_tool)
modify: <new_args>    # Replace args (only effective for pre_tool)
warning: <message>    # Continue but print warning
```

JSON output is also supported (recommended):

```json
{"result": "ok", "message": "checked"}
{"result": "deny", "message": "unsafe path"}
{"result": "modify", "modified_content": "{\"file_path\": \"/safe\"}"}
{"result": "warning", "message": "file is large, review carefully"}
```

### Full configuration example

```toml
[[hooks]]
name = "pre-check"
description = "Block dangerous write operations"
trigger = "pre_tool"
script = "check_write.sh"
script_type = "shell"
enabled = true
timeout_secs = 3
```

---

## TOML Webhook

### Supported trigger values (comma-separated for multiple)

| trigger value (canonical) | Alias | When triggered |
|-----------|------|---------|
| `turn_start` | — | Before turn starts |
| `tool_call_start` | — | When tool call starts |
| `pre_tool` | `before_tool` | Before tool execution |
| `post_tool` | `after_tool` | After tool execution |
| `turn_complete` | `after_turn` | After turn completes (detailed stats) |
| `post_turn` | — | After turn completes (legacy compat) |
| `session_start` | — | On session start |
| `session_end` | — | On session end |
| `error` | — | On error |
| `model_response` | — | After model response |
| `system_prompt` | — | When building system prompt |
| `message`² | `message_received` | On user message received |

> Uses **contains matching** (comma-separated triggers). E.g. `trigger = "pre_tool,post_tool"` fires on both occasions.
>
> ² `message`: WebhookHook has implemented the corresponding trait, but the engine has not registered a trigger slot yet; currently not functional.

### Synchronous Webhook

```toml
[[webhooks]]
name = "slack-notify"
description = "Send tool call notifications to Slack"
trigger = "pre_tool,post_tool"
url = "https://hooks.slack.com/services/XXX"
method = "POST"
timeout_secs = 10
retries = 2
enabled = true

[webhooks.headers]
Authorization = "Bearer YOUR_TOKEN"
```

### Async Batch Webhook (recommended for high-frequency scenarios)

```toml
[[async_webhooks]]
name = "audit-log"
trigger = "post_tool"
url = "https://log.example.com/batch"
timeout_secs = 10
batch_size = 20            # default 10, send when reached
flush_interval_ms = 1000   # default 1000ms, periodic flush
retries = 2
enabled = true

[async_webhooks.headers]
Authorization = "Bearer AUDIT_TOKEN"
```

> Async webhooks do not block the main flow. See [Webhook Guide](./webhook-guide.md) and [Async Webhook Guide](./async-webhook-guide.md).

---

## JSON CC Compatible Configuration

Compatible with Claude Code plugin's `.hooks.json`. Load paths:

- `~/.atomcode/hooks.json` — Global
- `<project>/.hooks.json` — Project (overrides same-name global)

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

Supported `event` values: `pre_tool_use`, `post_tool_use`, `session_start`, `session_end`, `user_prompt_submit`.

> **Case/style-insensitive:** the loader accepts both CC PascalCase (`PreToolUse`, `PostToolUse`, `SessionStart`, `UserPromptSubmit`) and snake_case (`pre_tool_use`, `session_start`, …) — the two spellings are equivalent.

Hooks receive context via environment variables (`ATOMCODE_HOOK_EVENT`, `ATOMCODE_HOOK_CONTEXT`, `ATOMCODE_TOOL_NAME`, etc.). The stdout protocol varies by event:

- **`pre_tool_use`** — output `{"action":"allow"}` / `{"action":"block","reason":"..."}` / `{"action":"modify","args":{...}}` (`args` replaces the tool-call arguments)
- **`user_prompt_submit`** — output `{"decision":"block","reason":"..."}` to block submission, or `{"hookSpecificOutput":{"additionalContext":"..."}}` to inject extra context; plain-text stdout is treated as an additionalContext injection
- **`post_tool_use` / `session_start` / `session_end`** — fire-and-forget; stdout does not affect the flow

---

## Built-in Hooks (no configuration needed, auto-enabled)

| Hook | When triggered | Function |
|------|---------|------|
| `ToolAuditLogHook` | On tool call | Log calls to audit log (tracing) |
| `TurnStatsHook` | Turn start + complete | Track turn duration and operations |
| `AutoCommitHook` | Turn complete | Auto `git commit` every N turns |
| `SessionSummaryHook` | Session start + end | Print session summary |
| `ErrorReportHook` | On error | Log error details |
| `ResponseValidationHook` | After model response | Detect sensitive information |

Built-in hooks auto-register and cannot be disabled via configuration yet (future CLI will provide enable/disable switches). Same-name project-level hooks cannot override built-in hooks (built-in hooks are native Rust, outside the TOML configuration system).

---

## CLI Commands

```bash
# List loaded hooks
atomcode hooks list

# View config paths
atomcode hooks paths

# Test a single hook
atomcode hooks test my-hook
```
---

## 调试技巧

### 手动测试 hook 脚本

TOML ScriptHook 通过 stdin 接收上下文 JSON（字段对应 `HookCtx`：`tool_name` / `tool_args` / `working_dir` / `session_id` / `turn_number`，无 `event` 字段）：

```bash
# pre_tool 测试上下文（扁平结构）
echo '{"tool_name":"read_file","tool_args":"{}","working_dir":"/tmp","session_id":"s1","turn_number":1}' | bash path/to/hook.sh

# post_tool 测试上下文（嵌套结构，含 result_context）
echo '{"hook_context":{"tool_name":"read_file","tool_args":"{}","working_dir":"/tmp","session_id":"s1","turn_number":1},"result_context":{"tool_name":"read_file","tool_args":"{}","result":"File content here","success":true,"duration_ms":12}}' | bash path/to/hook.sh
```

JSON CC 兼容 Hook 通过环境变量接收（TOML ScriptHook 不适用，TOML 用 stdin）：

```bash
# 导出环境变量模拟运行环境（仅 JSON CC 格式）
export ATOMCODE_HOOK_EVENT="post_tool_use"
export ATOMCODE_TOOL_NAME="read_file"
export ATOMCODE_HOOK_CONTEXT='{"tool_name":"read_file"}'
python path/to/hook.py
```

### 配置文件语法校验

```bash
# TOML 格式校验（需要 Python ≥ 3.11；旧版请 pip install tomli 并将 tomllib 替换为 tomli）
python -c "from pathlib import Path; import tomllib; tomllib.load(Path('path/to/hooks.toml').open('rb'))"

# JSON 格式校验
python -c "from pathlib import Path; import json; json.load(Path('path/to/hooks.json').open('rb'))"
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
| 1 | 文件路径是否存在 | `~` 不会自动展开，需用绝对路径（如 `C:\Users\you\...` 或 `/home/you/...`） |
| 2 | `enabled = true` | 默认 `true`，检查是否意外设为 `false` |
| 3 | `trigger` / `event` 拼写正确 | 参考上方事件表，大小写敏感 |
| 4 | 脚本有执行权限 | Linux/macOS 需 `chmod +x` |
| 5 | 脚本没有超时 | TOML 默认 2s，JSON 默认 10s |
| 6 | 项目级 hook 覆盖了全局 hook | 项目 hook 优先级更高 |

---

## Security Notes

1. **Project hooks override same-name global hooks** (project hooks load after global hooks)
2. **Hooks cannot bypass the permission system** — `pre_tool` deny does not override the user's `always_allow` settings
3. **Script execution has timeouts** — TOML ScriptHook default 2s, JSON default 10s, Webhook default 10s
4. **Scripts run under user permissions** — be mindful of script security itself
5. **Timeout/crash is fail-open** — a script timeout or crash is treated as `ok`, not blocking the flow
6. **Windows compatibility** — `~` is not auto-expanded; use absolute paths (e.g. `C:\Users\you\...` or `/home/you/...`); use `\\` or `/` as the path separator; for Python scripts, specify the interpreter path explicitly

---

## Related Docs

- [CLI Guide](./hook-cli-guide.md) — `atomcode hooks` command reference
- [Complete Timing List](./hook-timing-complete.md) — all hook timings and available configurations
- [Webhook Guide](./webhook-guide.md) — HTTP remote calls
- [Async Webhook Guide](./async-webhook-guide.md) — batch async delivery
- [Architecture](./hook-architecture.md) — developer-oriented architecture reference
