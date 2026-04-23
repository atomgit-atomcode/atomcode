# Hook CLI 命令使用指南

## 概述

AtomCode 提供了 `hooks` 子命令来管理和查看已加载的 Hook。

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
  ✓ Global:   C:\Users\DonkeyLee\.atomcode\hooks
  ✓ Project:  E:\work_space\atomcode\.atomcode\hooks
```

**输出说明**：
- **Type** - Hook 触发类型
- **Count** - 该类型的 Hook 数量
- **Total** - 已加载的 Hook 总数
- **Hook Directories** - Hooks 目录状态（✓ 表示存在，✗ 表示不存在）

### 2. `atomcode hooks paths` - 查看配置路径

显示 Hook 配置文件的位置和相关文档：

```bash
$ atomcode hooks paths

Hook Configuration Paths:
─────────────────────────────────────────────
  ✓ Global config:   C:\Users\DonKeyLee\.atomcode\hooks\hooks.toml
  ✓ Project config:  E:\work_space\atomcode\.atomcode\hooks\hooks.toml

Documentation:
─────────────────────────────────────────────
  docs/hooks.md - Hook usage guide
  docs/hook-timing-complete.md - Complete timing list
  docs/hook-expansion-summary.md - Expansion summary
```

### 3. `atomcode hooks test <name>` - 测试单个 Hook

测试指定的 Hook（开发中）：

```bash
$ atomcode hooks test my-custom-hook

Testing hook: my-custom-hook
(TODO: Implement hook testing)
```

## 快速开始

### 步骤 1：创建 Hooks 目录

```bash
# 全局 Hooks
mkdir -p ~/.atomcode/hooks

# 项目级 Hooks
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
    TOOL_NAME=$(echo "$INPUT" | jq -r '.tool_name // empty')
    TURN=$(echo "$INPUT" | jq -r '.turn_number // 0')
    
    # 记录到日志文件
    LOG_DIR="$HOME/.atomcode/hooks-logs"
    mkdir -p "$LOG_DIR"
    LOG_FILE="$LOG_DIR/audit.log"
    
    TIMESTAMP=$(date '+%Y-%m-%d %H:%M:%S')
    echo "[$TIMESTAMP] Turn #$TURN: $TOOL_NAME" >> "$LOG_FILE"
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
trigger = "on_tool_call_start"
script = "audit.sh"
script_type = "shell"
enabled = true
timeout_secs = 2
```

### 步骤 4：验证 Hook 已加载

```bash
$ atomcode hooks list

Loaded Hooks:
─────────────────────────────────────────────
  Type                           Count
  ────────────────────────────── ─────
  OnToolCallStart                    1
  ────────────────────────────── ─────
  Total                              1

Hook Directories:
─────────────────────────────────────────────
  ✓ Global:   C:\Users\DonkeyLee\.atomcode\hooks
  ✗ Project:  E:\work_space\atomcode\.atomcode\hooks
```

## Hook 触发类型

### 消息级别

| 触发类型 | 说明 | 可否修改 |
|---------|------|---------|
| `on_message_received` | 用户消息接收时 | ✅ 可修改消息 |

### Turn 级别

| 触发类型 | 说明 | 可否修改 |
|---------|------|---------|
| `on_turn_start` | Turn 开始前 | ❌ |
| `on_turn_complete` | Turn 完成后（含详细统计） | ❌ |
| `on_model_response` | 模型响应完成后 | ❌ |

### 工具调用级别

| 触发类型 | 说明 | 可否修改 |
|---------|------|---------|
| `on_tool_call_start` | 工具调用开始时（可拒绝） | ❌ 可拒绝 |
| `pre_tool` | 工具执行前（权限检查后） | ✅ 可修改参数 |
| `post_tool` | 工具执行后 | ❌ |

### 会话级别

| 触发类型 | 说明 | 可否修改 |
|---------|------|---------|
| `on_session_start` | 会话启动时 | ❌ |
| `on_session_end` | 会话结束时 | ❌ |

### 系统级别

| 触发类型 | 说明 | 可否修改 |
|---------|------|---------|
| `on_error` | 错误发生时 | ❌ |
| `system_prompt` | 系统 Prompt 构建时 | ✅ 可追加内容 |

## 配置示例

### 完整 hooks.toml

```toml
# 工具调用审计
[[hooks]]
name = "audit"
description = "记录所有工具调用"
trigger = "on_tool_call_start"
script = "audit.sh"
script_type = "shell"
enabled = true
timeout_secs = 2

# Turn 统计
[[hooks]]
name = "stats"
description = "收集 Turn 统计信息"
trigger = "on_turn_complete"
script = "stats.sh"
script_type = "shell"
enabled = true
timeout_secs = 2

# 自动 Git 提交
[[hooks]]
name = "auto-commit"
description = "每 5 个 Turn 自动提交"
trigger = "on_turn_complete"
script = "auto_commit.sh"
script_type = "shell"
enabled = false  # 默认禁用
timeout_secs = 5

# 会话总结
[[hooks]]
name = "summary"
description = "会话结束时生成报告"
trigger = "on_session_end"
script = "summary.sh"
script_type = "shell"
enabled = true
timeout_secs = 3

# 错误上报
[[hooks]]
name = "error-report"
description = "错误详细信息记录"
trigger = "on_error"
script = "error_report.sh"
script_type = "shell"
enabled = true
timeout_secs = 2

# 模型响应验证
[[hooks]]
name = "validate"
description = "检测敏感信息泄露"
trigger = "on_model_response"
script = "validate.sh"
script_type = "shell"
enabled = true
timeout_secs = 2
```

## 故障排查

### 问题 1：Hook 未加载

**症状**：`atomcode hooks list` 显示 `(No hooks loaded)`

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

3. 检查脚本是否有执行权限：
   ```bash
   ls -la ~/.atomcode/hooks/*.sh
   chmod +x ~/.atomcode/hooks/*.sh
   ```

4. 查看 stderr 输出（Hook 加载日志）：
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
   - 默认超时 2 秒
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

## 最佳实践

### 1. 使用全局 Hooks 做审计

```toml
# ~/.atomcode/hooks/hooks.toml
[[hooks]]
name = "audit"
trigger = "on_tool_call_start"
script = "audit.sh"
enabled = true
```

### 2. 使用项目级 Hooks 做定制

```toml
# <project>/.atomcode/hooks/hooks.toml
[[hooks]]
name = "project-specific"
trigger = "on_turn_complete"
script = "project_hook.sh"
enabled = true
```

### 3. 脚本输出使用 JSON

```bash
#!/bin/bash
INPUT=$(cat)

# 处理...

# 输出 JSON（推荐）
echo '{"result": "ok", "message": "Hook executed successfully"}'
```

### 4. 错误处理

```bash
#!/bin/bash
if ! command -v jq &> /dev/null; then
    echo "warning: jq not installed" >&2
    echo "ok"
    exit 0
fi

# 正常处理...
echo "ok"
```

## 相关文档

- [Hook 使用指南](./hooks.md)
- [完整 Hook 时机列表](./hook-timing-complete.md)
- [Hook 扩展总结](./hook-expansion-summary.md)
