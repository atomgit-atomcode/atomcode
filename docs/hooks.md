# AtomCode Hooks

Hooks 系统允许你在 AtomCode 的关键执行点插入自定义逻辑，实现灵活的扩展能力。

## 快速开始

### 1. 创建 Hooks 目录

全局 hooks：
```bash
mkdir -p ~/.atomcode/hooks
```

项目级 hooks：
```bash
mkdir -p .atomcode/hooks
```

### 2. 编写 Hook 脚本

创建 `~/.atomcode/hooks/my_hook.sh`：
```bash
#!/bin/bash
# 读取 JSON 输入
INPUT=$(cat)

# 解析工具名称
if command -v jq &> /dev/null; then
    TOOL_NAME=$(echo "$INPUT" | jq -r '.hook_context.tool_name // empty')
    echo "Hook saw tool: $TOOL_NAME" >&2
fi

# 返回执行结果
echo "ok"
```

赋予执行权限：
```bash
chmod +x ~/.atomcode/hooks/my_hook.sh
```

### 3. 配置 Hook

创建 `~/.atomcode/hooks/hooks.toml`：
```toml
[[hooks]]
name = "my-hook"
description = "My custom hook"
trigger = "post_tool"
script = "my_hook.sh"
script_type = "shell"
enabled = true
timeout_secs = 2
```

## Hook 类型

### pre_tool (工具执行前)

在工具实际执行前调用，可以：
- 修改工具参数
- 阻止工具执行
- 记录审计日志

**输入格式**：
```json
{
  "tool_name": "edit_file",
  "tool_args": "{...}",
  "working_dir": "/path/to/project",
  "session_id": "session-123",
  "turn_number": 5
}
```

**输出格式**：
- `ok` - 继续执行
- `modify: <new_args>` - 使用新参数
- `deny: <reason>` - 阻止执行
- `warning: <msg>` - 记录警告

### post_tool (工具执行后)

在工具执行完成后调用，可以：
- 处理执行结果
- 触发后续操作
- 收集统计信息

**输入格式**：
```json
{
  "hook_context": { ... },
  "result_context": {
    "tool_name": "edit_file",
    "tool_args": "{...}",
    "result": "File updated",
    "success": true,
    "duration_ms": 150
  }
}
```

### post_turn (Turn 完成后)

在一轮对话完成后调用，可以：
- 自动提交代码
- 运行测试
- 生成报告

**输入格式**：
```json
{
  "hook_context": { ... },
  "turn_result": "ToolUsed"
}
```

### system_prompt (系统 Prompt 扩展)

在构建系统提示时调用，返回内容会被追加到系统 prompt：
```bash
#!/bin/bash
echo "Additional rule: Always use tabs for indentation"
```

## 示例 Hooks

参见 `examples/hooks/` 目录：
- `log_tool_calls.sh` - 记录所有工具调用
- `auto_commit.sh` - 自动提交代码
- `code_review.sh` - 代码质量检查提示

## CLI 命令

列出已加载的 hooks：
```bash
atomcode hooks list
```

测试单个 hook：
```bash
atomcode hooks test my-hook
```

## 安全注意事项

1. **项目级 hooks 优先级低于全局 hooks** - 防止恶意项目覆盖你的全局设置
2. **Hooks 不能绕过权限系统** - pre_tool hook 的 deny 不会覆盖用户的 always_allow 设置
3. **脚本执行有超时限制** - 默认 2 秒，超时后会被终止
4. **脚本在用户权限下运行** - 注意脚本本身的安全性

## 开发 Rust 原生 Hooks

如果你需要更强大的能力，可以实现 Rust 原生的 Hook trait：

```rust
use atomcode_core::hook::{Hook, HookContext, HookResult, PreToolExecutionHook};
use async_trait::async_trait;

struct MyCustomHook;

#[async_trait]
impl Hook for MyCustomHook {
    fn name(&self) -> &str {
        "my-custom-hook"
    }
    
    fn description(&self) -> &str {
        "Does something special"
    }
}

#[async_trait]
impl PreToolExecutionHook for MyCustomHook {
    async fn on_pre_execute(&self, ctx: &HookContext) -> HookResult {
        // 自定义逻辑
        HookResult::Ok
    }
}

// 注册到 registry
registry.register_pre_tool_hook(Arc::new(MyCustomHook));
```
