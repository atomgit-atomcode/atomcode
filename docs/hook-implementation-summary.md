# Hook 系统实现总结

## 概述

为 AtomCode 实现了完整的 Hook（钩子）机制，允许开发者在关键执行点插入自定义逻辑，实现灵活的扩展能力。

## 实现的文件

### 核心模块

1. **`crates/atomcode-core/src/hook/mod.rs`** (280 行)
   - 定义了 Hook trait 和四种钩子类型
   - HookContext 和 ToolResultContext 数据结构
   - HookRegistry 注册表和触发器
   - HookResult 枚举（Ok/Warning/Denied/Modified）

2. **`crates/atomcode-core/src/hook/script_runner.rs`** (227 行)
   - ScriptHook 实现 - 支持外部 shell/python 脚本
   - 脚本执行和超时管理
   - JSON 输出解析和格式转换

3. **`crates/atomcode-core/src/hook/config_loader.rs`** (127 行)
   - hooks.toml 配置文件加载
   - 从全局和项目目录自动加载
   - 脚本路径解析和注册

### 集成点

4. **`crates/atomcode-core/src/turn/runner.rs`** (修改)
   - 添加 hook_registry 字段到 TurnRunner
   - 在 execute_single_tool 中注入 pre/post hooks
   - 在 run_with_filter 中注入 post-turn hooks

5. **`crates/atomcode-core/src/agent/mod.rs`** (修改)
   - 在 AgentLoop 初始化时加载 hooks
   - 导入 HookRegistry

6. **`crates/atomcode-core/src/agent/sub_agent.rs`** (修改)
   - 为 SubAgent 添加空 hook_registry

7. **`crates/atomcode-daemon/src/main.rs`** (修改)
   - 为 Daemon 模式添加空 hook_registry

### 测试

8. **`crates/atomcode-core/tests/hook_test.rs`** (248 行)
   - 5 个单元测试覆盖核心功能
   - 测试计数、拒绝、修改参数、优先级等

### 示例和文档

9. **`examples/hooks/`** 目录
   - `log_tool_calls.sh` - 工具调用日志记录
   - `auto_commit.sh` - 自动 Git 提交
   - `code_review.sh` - 代码质量检查
   - `hooks.toml` - 配置示例

10. **`docs/hooks.md`** (172 行)
    - 完整的使用文档
    - 快速开始指南
    - Hook 类型说明
    - 安全注意事项

## Hook 类型

### 1. PreToolExecutionHook (工具执行前)
- **触发时机**: 工具执行前，权限检查后
- **用途**: 参数修改、审计日志、阻止执行
- **返回值**: 
  - `Ok` - 继续执行
  - `Modified(new_args)` - 使用新参数
  - `Denied(reason)` - 阻止执行
  - `Warning(msg)` - 记录警告

### 2. PostToolExecutionHook (工具执行后)
- **触发时机**: 工具执行完成后
- **用途**: 结果处理、触发后续操作、统计收集
- **输入**: 工具名称、参数、结果、成功状态、执行时间

### 3. PostTurnHook (Turn 完成后)
- **触发时机**: 一轮对话完成后
- **用途**: 自动提交、代码审查、生成报告
- **输入**: Turn 结果（Responded/UsedTools/Failed）

### 4. SystemPromptHook (系统 Prompt 扩展)
- **触发时机**: 构建系统提示时
- **用途**: 注入额外规则、自定义指令
- **返回**: 要追加到系统 prompt 的文本

## 配置方式

### 全局 Hooks
```
~/.atomcode/hooks/
  ├── hooks.toml          # 配置文件
  ├── log_tool_calls.sh   # 脚本
  └── auto_commit.sh
```

### 项目级 Hooks
```
<project>/.atomcode/hooks/
  ├── hooks.toml
  └── custom_hook.py
```

### hooks.toml 格式
```toml
[[hooks]]
name = "my-hook"
description = "描述"
trigger = "post_tool"
script = "script.sh"
script_type = "shell"
enabled = true
timeout_secs = 2
```

## 加载优先级

1. 全局 hooks (~/.atomcode/hooks/) 先加载
2. 项目级 hooks (<cwd>/.atomcode/hooks/) 后加载
3. 可通过 CLI `--hooks-dir` 参数额外加载

## 安全机制

1. **不能绕过权限系统** - pre-tool hook 的 deny 不会覆盖用户的 always_allow 设置
2. **脚本执行超时** - 默认 2 秒，超时后终止进程
3. **项目级 hooks 优先级低** - 不能覆盖全局设置
4. **脚本在用户权限下运行** - 需要注意脚本安全性

## 使用示例

### Rust 原生 Hook
```rust
use atomcode_core::hook::*;

struct MyHook;

#[async_trait]
impl Hook for MyHook {
    fn name(&self) -> &str { "my-hook" }
}

#[async_trait]
impl PreToolExecutionHook for MyHook {
    async fn on_pre_execute(&self, ctx: &HookContext) -> HookResult {
        // 自定义逻辑
        HookResult::Ok
    }
}

registry.register_pre_tool_hook(Arc::new(MyHook));
```

### 外部脚本 Hook
```bash
#!/bin/bash
INPUT=$(cat)
TOOL_NAME=$(echo "$INPUT" | jq -r '.hook_context.tool_name')
echo "Saw tool: $TOOL_NAME" >&2
echo "ok"
```

配置：
```toml
[[hooks]]
name = "logger"
trigger = "post_tool"
script = "logger.sh"
script_type = "shell"
enabled = true
```

## 测试覆盖

运行测试：
```bash
cargo test -p atomcode-core --test hook_test
```

测试结果：
```
running 5 tests
test test_hook_registry_basic ... ok
test test_hook_deny_execution ... ok
test test_hook_modify_args ... ok
test test_system_prompt_hook ... ok
test test_hook_priority_order ... ok

test result: ok. 5 passed; 0 failed
```

## 未来扩展点

1. **Hook 链式组合** - 允许 hooks 之间传递数据
2. **异步脚本支持** - 支持 Node.js 等异步脚本
3. **Hook 热重载** - 修改配置后自动重新加载
4. **Hook 市场** - 社区共享和下载 hooks

## 关键技术决策

### 为什么使用 trait 而不是纯脚本？
- **类型安全** - Rust 编译时检查
- **性能** - 零开销抽象
- **灵活性** - 可以访问完整的 AtomCode API
- **可选性** - 脚本 hooks 仍支持快速原型

### 为什么 Hook 失败不中断流程？
- **容错性** - 非致命 hook 失败不应阻止用户工作
- **渐进式采用** - 用户可以逐步启用 hooks
- **Warning 机制** - 记录问题但不阻止

### 为什么项目级 hooks 优先级低于全局？
- **安全** - 防止恶意项目覆盖安全设置
- **一致性** - 用户的全局设置始终生效

## 总结

Hook 系统为 AtomCode 提供了强大的扩展能力：
- ✅ 4 种钩子类型覆盖关键执行点
- ✅ 支持 Rust 原生和外部脚本两种扩展方式
- ✅ 配置文件驱动，灵活启用/禁用
- ✅ 完善的测试覆盖（5/5 通过）
- ✅ 文档齐全，示例丰富
- ✅ 安全可控，不能绕过权限系统

这为实现 Issue #109 中描述的"灵活的扩展机制"奠定了坚实基础。
