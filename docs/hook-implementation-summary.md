# Hook 系统实现总结

## 概述

AtomCode Hook 系统基于 **HookEngine** 统一引擎架构，支持 13 个 trait 扩展点、3 种配置方式（JSON CC 兼容、TOML ScriptHook、TOML Webhook），以及 6 个内置工程化 Hook。

> 系统已从旧架构（`HookRegistry` + `HookExecutor`）完全迁移到 `HookEngine`。旧 `executor.rs` 仍存在但已不再使用，`HookRegistry` 已删除。

## 源文件清单

### 核心模块

| 文件 | 行数 | 说明 |
|------|:----:|------|
| `crates/atomcode-core/src/hook/mod.rs` | ~680 | 13 个 trait 定义、12 个 context 结构体、HookResult/HookEvent 枚举 |
| `crates/atomcode-core/src/hook/engine.rs` | ~1284 | **HookEngine** — 统一注册/触发引擎、ShellCommandHook 实现、12 个注册槽位 + 12 个触发方法 |
| `crates/atomcode-core/src/hook/script_runner.rs` | ~449 | **ScriptHook** — TOML 配置加载的外部脚本执行（stdin JSON 协议） |
| `crates/atomcode-core/src/hook/webhook.rs` | ~748 | **WebhookHook** — HTTP 远程调用，实现 12 个 trait |
| `crates/atomcode-core/src/hook/async_batcher.rs` | ~534 | **AsyncWebhookBatcher** — 异步批量发送（mpsc 通道 + tokio 后台任务） |
| `crates/atomcode-core/src/hook/built_in.rs` | ~572 | **6 个内置 Hook** — ToolAuditLogHook、TurnStatsHook、AutoCommitHook 等 |
| `crates/atomcode-core/src/hook/config_loader.rs` | ~501 | **HooksConfig** — TOML 配置文件加载、Webhook/AsyncWebhook 注册 |
| `crates/atomcode-core/src/hook/json_config.rs` | ~561 | **JSON 配置加载** — CC 兼容 `.hooks.json` 加载 |
| `crates/atomcode-core/src/hook/config.rs` | ~175 | 工具名匹配工具函数 |
| `crates/atomcode-core/src/hook/executor.rs` | ~1115 | ⚠️ **旧 HookExecutor**（已废弃，不再使用，待清理） |

### 调用侧集成

- `crates/atomcode-core/src/agent/mod.rs` — AgentLoop 初始化时调用 `HookEngine::load_all()`
- `crates/atomcode-core/src/turn/runner.rs` — TurnRunner 在各阶段触发 hook

## 架构概览

```
AgentLoop / TurnRunner
        │
        ▼
   HookEngine (engine.rs)
   统一注册/触发引擎
        │
        ├── ShellCommandHook  (JSON 配置 → shell 命令, 环境变量协议)
        ├── ScriptHook        (TOML 配置 → 外部脚本, stdin JSON 协议)
        ├── WebhookHook       (TOML 配置 → HTTP 远程调用)
        └── 6 BuiltInHook     (Rust 原生, 自动注册)
```

## 13 个 Trait 定义

| # | Trait | 关键方法签名 | 可影响流程 |
|---|-------|------------|:--:|
| 1 | `PreToolExecutionHook` | `on_pre_execute(ctx: &HookCtx) -> HookResult` | ✅ 修改/阻止 |
| 2 | `PostToolExecutionHook` | `on_post_execute(ctx: &HookCtx, result: &ToolResultContext) -> HookResult` | ❌ |
| 3 | `PostTurnHook` | `on_post_turn(ctx: &HookCtx, turn_result: &str) -> HookResult` | ❌ |
| 4 | `SystemPromptHook` | `extend_system_prompt() -> Option<String>` | ✅ 追加 |
| 5 | `OnUserPromptSubmitHook` | `on_user_prompt_submit(payload: &UserPromptSubmitPayload) -> UserPromptSubmitResult` | ✅ 注入/阻止 |
| 6 | `OnMessageReceivedHook` | `on_message_received(ctx: &UserMessageContext) -> HookResult` | ❌ |
| 7 | `OnTurnStartHook` | `on_turn_start(ctx: &TurnStartContext) -> HookResult` | ❌ |
| 8 | `OnToolCallStartHook` | `on_tool_call_start(ctx: &ToolCallStartContext) -> HookResult` | ❌ |
| 9 | `OnTurnCompleteHook` | `on_turn_complete(ctx: &TurnCompleteContext) -> HookResult` | ❌ |
| 10 | `OnSessionStartHook` | `on_session_start(ctx: &SessionContext) -> HookResult` | ❌ |
| 11 | `OnSessionEndHook` | `on_session_end(ctx: &SessionContext) -> HookResult` | ❌ |
| 12 | `OnErrorHook` | `on_error(ctx: &ErrorContext) -> HookResult` | ❌ |
| 13 | `OnModelResponseHook` | `on_model_response(response: &str, turn_ctx: &TurnStartContext) -> HookResult` | ❌ |

## 各实现覆盖的 Trait

| 实现 | Trait 数 | 具体覆盖 |
|------|:---:|------|
| **ShellCommandHook** | 6 | PreTool + PostTool + OnSessionStart + OnSessionEnd + OnUserPromptSubmit + OnToolCallStart（空操作占位） |
| **ScriptHook** | 4 | PreTool + PostTool + PostTurn + SystemPrompt |
| **WebhookHook** | 12 | 除 OnUserPromptSubmitHook 外的全部（含 trigger 字段过滤） |
| **ToolAuditLogHook** | 1 | OnToolCallStartHook |
| **TurnStatsHook** | 2 | OnTurnStartHook + OnTurnCompleteHook |
| **AutoCommitHook** | 1 | OnTurnCompleteHook |
| **SessionSummaryHook** | 2 | OnSessionStartHook + OnSessionEndHook |
| **ErrorReportHook** | 1 | OnErrorHook |
| **ResponseValidationHook** | 1 | OnModelResponseHook |

## 配置体系

### JSON CC 兼容配置（`.hooks.json`）

- 加载路径：`~/.atomcode/hooks.json`（全局）+ `<project>/.hooks.json`（项目）
- 支持 event：`pre_tool_use`、`post_tool_use`、`session_start`、`session_end`、`user_prompt_submit`
- 协议：环境变量（`ATOMCODE_HOOK_EVENT`、`ATOMCODE_HOOK_CONTEXT` 等），stdout 输出 CC JSON
- 项目 hooks **覆盖**同名全局 hooks

### TOML 配置（`hooks.toml`）

- 加载路径：`~/.atomcode/hooks/hooks.toml` + `<project>/.atomcode/hooks/hooks.toml`
- 三段式结构：
  - `[[hooks]]` → ScriptHook（4 种 trigger: `pre_tool`/`post_tool`/`post_turn`/`system_prompt`）
  - `[[webhooks]]` → WebhookHook（11 种 trigger, contains 匹配, 逗号分隔）
  - `[[async_webhooks]]` → AsyncWebhookBatcher（批量异步, 同名关联 WebhookHook）
- 默认超时：ScriptHook 2s, Webhook 10s, AsyncWebhook 10s

### 内置 Hook（无配置，自动注册）

6 个内置 Hook 在 `HookEngine::register_builtins()` 中自动注册。

## 加载顺序与优先级

`HookEngine::load_all()` 按以下顺序加载：
1. **JSON hooks** (`load_json_hooks`) → ShellCommandHook（CC 兼容）
2. **TOML hooks** (`load_toml_hooks`) → ScriptHook + WebhookHook
3. **内置 Hook** (`register_builtins`) → 6 个内置 Hook
4. **Webhook 异步关联** (`load_webhook_hooks`) → AsyncWebhookBatcher 关联

**全局 hooks 先加载，项目 hooks 后加载**。后加载的同名 hook **覆盖**先前注册的同名 hook（后加载优先）。

## 关键设计决策

### 为什么使用 trait 而不是纯脚本？

- **类型安全** — Rust 编译时检查
- **性能** — 零开销抽象
- **灵活性** — 可以访问完整的 AtomCode API
- **可选性** — 脚本 hooks 仍支持快速原型

### 为什么 Hook 失败不中断流程？

- **容错性** — 非致命 hook 失败不应阻止用户工作
- **渐进式采用** — 用户可以逐步启用 hooks
- **Warning 机制** — 记录问题但不阻止

### 并发安全模型

- `HookEngine` 通过 `ArcSwap` 原子替换实现无锁热重载
- 读路径（trigger_*）零锁开销
- 写路径（reload）只建新引擎 + 原子替换一次
- 旧 Arc 引用计数 > 0 时等待正在执行的 trigger 返回

### 为什么项目 hooks 优先级高于全局？

- 项目 hooks 后加载，同名覆盖全局
- 允许项目定制覆盖用户全局设置
- 安全性靠 hook 不能绕过权限系统保证（`pre_tool` deny 不覆盖 `always_allow`）

## 测试覆盖

核心测试分布在：
- `mod.rs` — HookEvent/HookConfig/PreHookResult/HookContext 序列化测试
- `engine.rs` — ShellCommandHook 全场景测试（allow/block/modify/matcher/timeout/crash 等 20+ 测试）
- `config_loader.rs` — TOML 解析/注册/trigger 测试（10+ 测试）
- `json_config.rs` — JSON 加载/合并/覆盖/CC 转换测试（15+ 测试）
- `script_runner.rs` — ScriptHook 输出解析/协议测试（10+ 测试）
- `webhook.rs` — WebhookHook 配置/响应解析测试（7+ 测试）
- `built_in.rs` — 内置 Hook 行为测试（11+ 测试）

## 未来扩展点

1. **OnMessageReceivedHook 激活** — trait 已定义，需在 HookEngine 增加注册槽位和触发调用
2. **内置 Hook 开关** — CLI 命令启用/禁用内置 Hook
3. **Hook 热重载** — 修改配置后自动重新加载（架构已支持 ArcSwap）
4. **AsyncWebhookBatcher flush 调度** — 当前 batcher 已创建但未定期 flush（TODO #914）
