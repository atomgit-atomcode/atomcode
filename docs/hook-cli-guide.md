# Hook CLI 命令使用指南

## 概述

AtomCode 提供了 `atomcode hooks` 系列 CLI 命令来管理、测试和验证 Hook 配置。

---

## 命令列表

### 1. `atomcode hooks list` - 查看已加载的 Hook

显示所有已加载的 Hook 及其数量统计：

```bash
$ atomcode hooks list

Loaded Hooks:
─────────────────────────────────────────────
  Type                           Count
  ────────────────────────────── ─────
  OnToolCallStart                    1
  PostToolExecution                  2
  OnTurnComplete                     1
  OnSessionEnd                       1
  ────────────────────────────── ─────
  Total                              5

Hook Directories:
─────────────────────────────────────────────
  ✓ Global:   ~/.atomcode/hooks
  ✓ Project:  /path/to/project/.atomcode/hooks
```

**输出说明**：
- **Type** - Hook 注册的 trait 槽位名称
- **Count** - 该槽位注册的 Hook 数量（含内置）
- **Total** - 已加载的 Hook 总数
- **Hook Directories** - Hooks 目录状态（✓ 表示存在，✗ 表示不存在）

`Total` 会始终 ≥ 8（6 个内置 Hook 产生 8 次 slot 注册）。

### 2. `atomcode hooks paths` - 查看配置路径

```bash
$ atomcode hooks paths

Global config:
  JSON:  ~/.atomcode/hooks.json
  TOML:  ~/.atomcode/hooks/hooks.toml

Project config:
  JSON:  /path/to/project/.hooks.json
  TOML:  /path/to/project/.atomcode/hooks/hooks.toml
```

### 3. `atomcode hooks test <name>` - 测试单个 Hook

```bash
$ atomcode hooks test my-hook

Testing hook: my-hook
  Config:  ~/.atomcode/hooks/hooks.toml
  Trigger: pre_tool
  Script:  ~/.atomcode/hooks/my_hook.sh
  Status:  ✓ enabled
```

---

## 快速开始

### 步骤 1：创建 Hooks 目录

```bash
# 全局 Hooks（对所有项目生效）
mkdir -p ~/.atomcode/hooks

# 项目级 Hooks（仅当前项目生效）
mkdir -p .atomcode/hooks
```

### 步骤 2：编写 Hook 脚本

创建 `~/.atomcode/hooks/audit.sh`：

```bash
#!/bin/bash
# 读取 JSON 输入
INPUT=$(cat)

# 解析关键信息
if command -v jq &> /dev/null; then
  TOOL=$(echo "$INPUT" | jq -r '.tool_name // empty')
  TURN=$(echo "$INPUT" | jq -r '.turn_number // 0')

  # 记录到日志文件
  echo "[$(date)] Turn #$TURN: $TOOL" >> ~/.atomcode_audit.log
fi

# 返回 ok 表示 hook 执行成功
echo "ok"
```

赋予执行权限：

```bash
chmod +x ~/.atomcode/hooks/audit.sh
```

### 步骤 3：配置 Hook

创建 `~/.atomcode/hooks/hooks.toml`：

```toml
[[hooks]]
name = "audit"
description = "记录所有工具调用到审计日志"
trigger = "post_tool"
script = "audit.sh"
script_type = "shell"
enabled = true
timeout_secs = 2
```

> **重要**：TOML ScriptHook 的 `trigger` 字段只支持 4 个值：`pre_tool`、`post_tool`、`post_turn`、`system_prompt`。其他值（如 `on_turn_complete`）不会被识别。

### 步骤 4：验证 Hook 已加载

```bash
$ atomcode hooks list
```

---

## Hook 触发类型

### 用户可在 TOML ScriptHook 中配置的 trigger（4 种）

| trigger 值 | 别名 | 触发时机 | 可影响流程 |
|-----------|------|---------|:--:|
| `pre_tool` | `pre_tool_execution` | 工具执行前 | ✅ 可阻止/修改参数 |
| `post_tool` | `post_tool_execution` | 工具执行后 | ❌ fire-and-forget |
| `post_turn` | — | Turn 完成后 | ❌ fire-and-forget |
| `system_prompt` | — | 构建系统 prompt 时 | ✅ 追加指令 |

### 用户可在 JSON CC 配置中使用的 event（5 种）

| event 值 | 触发时机 |
|----------|---------|
| `pre_tool_use` | 工具执行前（环境变量协议） |
| `post_tool_use` | 工具执行后 |
| `session_start` | 会话启动 |
| `session_end` | 会话结束 |
| `user_prompt_submit` | 用户提交 prompt |

### 用户可在 Webhook 中配置的 trigger（12 种，逗号分隔）

| trigger 值 | 别名 | 触发时机 |
|-----------|------|---------|
| `pre_tool` | `before_tool` | 工具执行前 |
| `post_tool` | `after_tool` | 工具执行后 |
| `post_turn` | — | Turn 完成后（旧版） |
| `turn_start` | — | Turn 开始前 |
| `turn_complete` | `after_turn` | Turn 完成后（详细） |
| `tool_call_start` | — | 工具调用开始时 |
| `model_response` | — | 模型响应完成后 |
| `session_start` | — | 会话启动 |
| `session_end` | — | 会话结束 |
| `error` | — | 错误发生时 |
| `system_prompt` | — | 构建系统 prompt 时 |
| `message`² | `message_received` | 用户消息接收时 |

> ² `message`：WebhookHook 已实现对应 trait，但引擎尚未注册触发槽位，当前实际不可用。

### 只能通过内置 Hook 或 Webhook 触发的时机

以下时机由于内置 Hook 已覆盖，用户无需手动配置（但可通过 Webhook 接收事件）：

| Hook 类型 | 内置实现 | 功能 |
|----------|---------|------|
| `OnToolCallStart` | `ToolAuditLogHook` | 工具调用审计日志 |
| `OnTurnStart` | `TurnStatsHook` | Turn 开始统计 |
| `OnTurnComplete` | `TurnStatsHook` + `AutoCommitHook` | Turn 完成统计 & 自动提交 |
| `OnSessionStart` | `SessionSummaryHook` | 会话开始摘要 |
| `OnSessionEnd` | `SessionSummaryHook` | 会话结束摘要 |
| `OnError` | `ErrorReportHook` | 错误详情记录 |
| `OnModelResponse` | `ResponseValidationHook` | 模型响应验证 |

---

## 配置示例

### TOML ScriptHook（正确写法）

```toml
# 工具执行前检查
[[hooks]]
name = "pre-check"
description = "阻止危险操作"
trigger = "pre_tool"
script = "check_write.sh"
script_type = "shell"
enabled = true
timeout_secs = 3

# 工具执行后通知
[[hooks]]
name = "notify"
description = "工具执行后记录日志"
trigger = "post_tool"
script = "notify.sh"
script_type = "shell"
enabled = true
timeout_secs = 2

# Turn 完成后处理
[[hooks]]
name = "post-processing"
description = "Turn 完成后执行清理"
trigger = "post_turn"
script = "cleanup.sh"
script_type = "shell"
enabled = true
timeout_secs = 5

# 系统 Prompt 扩展
[[hooks]]
name = "rules"
description = "注入项目编码规范"
trigger = "system_prompt"
script = "inject_rules.py"
script_type = "python"
enabled = true
timeout_secs = 2
```

### 混合配置（脚本 + Webhook + JSON）

```toml
# hooks.toml

# 本地脚本 Hook
[[hooks]]
name = "audit"
trigger = "post_tool"
script = "audit.sh"

# 同步 Webhook
[[webhooks]]
name = "slack-notify"
trigger = "pre_tool,post_tool"
url = "https://hooks.slack.com/services/XXX"
timeout_secs = 10

# 异步批量 Webhook
[[async_webhooks]]
name = "audit-log"
trigger = "post_tool"
url = "https://log.example.com/batch"
batch_size = 20
flush_interval_ms = 1000
```

---

## 故障排查

### 问题 1：Hook 未加载

**症状**：`atomcode hooks list` 显示自定义 Hook 数量为 0

**排查步骤**：

1. 检查 hooks 目录是否存在：
   ```bash
   atomcode hooks paths
   ```

2. 检查 `hooks.toml` 文件格式：
   ```bash
   # 验证 TOML 格式
   cat ~/.atomcode/hooks/hooks.toml
   ```

3. 检查 `trigger` 值是否有效：
   - TOML ScriptHook 只支持：`pre_tool`、`post_tool`、`post_turn`、`system_prompt`
   - 如果使用了 `on_turn_complete`、`on_tool_call_start` 等值，会被跳过并打印 warning

4. 检查脚本是否有执行权限：
   ```bash
   ls -la ~/.atomcode/hooks/*.sh
   chmod +x ~/.atomcode/hooks/*.sh
   ```

5. 查看 stderr 输出（Hook 加载日志）：
   ```bash
   atomcode -p "test" 2>&1 | grep -i hook
   ```

### 问题 2：Hook 执行失败

**症状**：Hook 脚本执行后显示警告

**排查步骤**：

1. 手动测试脚本：
   ```bash
   echo '{"tool_name":"test","tool_args":"{}"}' | ~/.atomcode/hooks/audit.sh
   ```

2. 检查脚本输出格式：
   - 必须输出 `ok` 表示成功
   - 可输出 `warning: <msg>` 记录警告
   - 可输出 `deny: <reason>` 拒绝执行

3. 检查超时设置：
   - TOML ScriptHook 默认超时 2 秒
   - 复杂脚本可增加 `timeout_secs`

### 问题 3：Windows 路径问题

**症状**：脚本路径解析失败

**解决方案**：

使用正斜杠或双反斜杠：

```toml
# 正确
script = "C:/Users/DonkeyLee/.atomcode/hooks/audit.sh"
script = "C:\\Users\\DonkeyLee\\.atomcode\\hooks\\audit.sh"

# 错误
script = "C:\Users\DonkeyLee\.atomcode\hooks\audit.sh"
```

或使用相对路径：

```toml
script = "audit.sh"  # 相对于 hooks.toml 所在目录
```

---

## 最佳实践

### 1. 使用正确 trigger 值

```toml
# ✅ 正确 — TOML ScriptHook 支持的 4 个 trigger
trigger = "pre_tool"
trigger = "post_tool"
trigger = "post_turn"
trigger = "system_prompt"

# ❌ 错误 — 这些值不会被 TOML ScriptHook 识别
trigger = "tool_call_start"   # 仅内置/Webhook 可用
trigger = "turn_complete"     # 仅内置/Webhook 可用
trigger = "session_end"       # 仅 Webhook / JSON CC 可用
```

### 2. 使用全局 Hooks 做审计

```toml
# ~/.atomcode/hooks/hooks.toml
[[hooks]]
name = "audit"
trigger = "post_tool"
script = "audit.sh"
enabled = true
```

### 3. 使用项目级 Hooks 做定制

```toml
# <project>/.atomcode/hooks/hooks.toml
[[hooks]]
name = "project-specific"
trigger = "pre_tool"
script = "project_hook.sh"
enabled = true
```

> 项目级 hooks 后加载，同名会覆盖全局 hooks。

### 4. 脚本输出使用 JSON（推荐）

```bash
#!/bin/bash
INPUT=$(cat)

# 处理...

# 输出 JSON（推荐）
echo '{"result": "ok", "message": "Hook executed successfully"}'
```

### 5. 错误处理

```bash
#!/bin/bash
set -e  # 遇到错误立即退出

INPUT=$(cat)

# 正常处理...
# 如果失败，输出 deny
if ! do_something; then
  echo "deny: operation failed"
  exit 1
fi

echo "ok"
```

---

## 相关文档

- [Hook 系统总览](./hooks.md) — 配置方式和 trigger 值汇总
- [完整时机列表](./hook-timing-complete.md) — 所有 hook 时机及可用配置方式
- [Webhook 指南](./webhook-guide.md) — HTTP 远程调用
- [异步 Webhook 指南](./async-webhook-guide.md) — 批量异步发送
