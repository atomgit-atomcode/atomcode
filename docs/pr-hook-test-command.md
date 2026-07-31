# PR: 实现 `hook test <name>` 命令

## 改动概述

实现了 `atomcode hook test <name>` 命令，目前该命令只是一个打印 TODO 的空壳，现改为实际执行指定 hook 并展示详细结果。

## 涉及文件

| 文件 | 改动 |
|------|------|
| `crates/atomcode-core/src/hook/json_config.rs` | 新增 `load_hooks_config_with_names()` 公共函数，保留 hook 名称信息 |
| `crates/atomcode-core/src/hook/config_loader.rs` | 新增 `load_script_hooks_with_names()` 函数，支持按名称查找 TOML 配置的 hook |
| `crates/atomcode-core/src/hook/engine.rs` | 新增 `list_hook_names()` 方法，为测试命令提供可用 hook 列表 |
| `crates/atomcode-cli/src/main.rs` | 替换 `HookCommands::Test` 存根为完整实现 |

## 改动的价值

### 1. `atomcode hook test <name>` 命令

**之前**：执行 `atomcode hook test my-hook` 只会打印：
```
Testing hook: my-hook
(TODO: Implement hook testing)
```
完全不做事。

**之后**：该命令会：

- 从 `hooks.json`（全局）和 `.hooks.json`（项目）加载所有已配置的 JSON hook，同时从 `hooks.toml`（全局 + 项目）加载 TOML 格式的 script hook
- 按名称查找目标 hook
- 显示 hook 的完整元信息（事件类型、命令、超时时间、matcher、plugin 路径）
- 构建模拟的 `HookContext` 环境（含测试用的 session_id、tool_name、tool_args）
- 以 hook 自身配置的 timeout 执行命令，环境变量与真实运行时完全一致
  - `ATOMCODE_HOOK_EVENT` — 事件名
  - `ATOMCODE_HOOK_CONTEXT` — JSON 序列化的完整上下文
  - `ATOMCODE_TOOL_NAME` — 当前工具名
  - `CLAUDE_PLUGIN_ROOT` / `ATOMCODE_PLUGIN_ROOT` — 插件根目录
- 展示详细的执行结果：**stdout / stderr / 退出码 / 耗时 / 超时状态**
- 若指定名称未找到，列出所有可用 hook 供参考

### 2. `load_hooks_config_with_names()` 函数

在 `json_config.rs` 中新增了一个公共函数，与内部的 `load_hooks_config()` 行为完全一致，但保留 hook 的名称信息（返回 `Vec<(String, HookConfig)>` 而非 `Vec<HookConfig>`）。这为后续需要按名称操作 hook 的功能提供了基础。

## 新命令使用示例

```bash
# 测试一个名为 "check-bash" 的 hook
$ atomcode hook test check-bash

🔧 Testing Hook: check-bash
  Event:     pre_tool_use
  Command:   ./scripts/check_bash.sh
  Timeout:   10000 ms
  Matcher:   bash

📋 Result:
  Duration:  12.345ms
  Status:    ✅ SUCCESS (exit code 0)
  ── stdout ──
  │ Tool check passed: bash

  ✅ Hook 'check-bash' executed successfully.
```

```bash
# 查找不存在的 hook 时
$ atomcode hook test nonexistent

❌ Hook 'nonexistent' not found.

Available hooks:
  🔹 check-bash                  (event: pre_tool_use, command: ./scripts/check_bash.sh)
  🔹 notify-slack                (event: post_tool_use, command: ./scripts/notify.sh)
```

```bash
# 超时场景
$ atomcode hook test slow-hook

🔧 Testing Hook: slow-hook
  Event:     pre_tool_use
  Command:   sleep 30
  Timeout:   5000 ms

📋 Result:
  ⏱ TIMEOUT after 5000 ms
  The hook command was killed because it exceeded the configured timeout.
```

## 改动行数

- `crates/atomcode-core/src/hook/json_config.rs`: +48 行
- `crates/atomcode-core/src/hook/config_loader.rs`: +34 行
- `crates/atomcode-core/src/hook/engine.rs`: +29 行
- `crates/atomcode-cli/src/main.rs`: -5 行 / +133 行（后续修复 +219 行 / -44 行）

## 兼容性

- 完全向后兼容：没有修改任何现有函数的签名或行为
- `load_hooks_config()` 保持不变，只在旁边新增一个 name-preserving 版本
- 所有测试无需修改，新增代码不影响现有 test suite


