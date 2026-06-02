# AtomCode Hook 文档全面梳理更新需求

> 供文档专家全面梳理各文档，确保与代码一致。请忽略现有文档内容，以如下 **代码真相** 为准重写/修订。

---

## 一、背景

AtomCode 的 Hook 系统经历了从 `HookRegistry + HookExecutor` 到 `HookEngine` 的重大重构。当前 `docs/` 下有 **10 份** hook 相关文档，其中多份是重构前留下的快照，与代码严重不符。

**目标**：以当前代码为唯一真相源，统一修订所有 10 份文档，消除错误、补齐缺失、去除冗余重复。

---

## 二、代码真相（以 `crates/atomcode-core/src/hook/` 为准）

### 2.1 架构概览

```
AgentLoop / TurnRunner
          │
          ▼
     HookEngine (engine.rs, ~1284 行)
     统一注册/触发引擎，ArcSwap 原子替换保证并发安全
          │
          ├── ShellCommandHook (engine.rs, JSON 配置加载)
          ├── ScriptHook      (script_runner.rs, TOML 配置加载)
          ├── WebhookHook     (webhook.rs, HTTP 远程)
          └── 6 个 BuiltInHook (built_in.rs, 自动注册)
```

**旧系统残留**：`executor.rs`（~1115 行）仍存在但已不再使用，`HookRegistry` 已删除。文档中不应再提及 `HookRegistry`。

### 2.2 13 个 Trait 定义（`mod.rs`）

| # | Trait | 关键方法 | 可否影响流程 |
|---|-------|---------|------------|
| 1 | `PreToolExecutionHook` | `on_pre_execute(ctx) -> HookResult` | ✅ 可修改/阻止 |
| 2 | `PostToolExecutionHook` | `on_post_execute(ctx, result_ctx) -> HookResult` | ❌ fire-and-forget |
| 3 | `PostTurnHook` | `on_post_turn(ctx, turn_result) -> HookResult` | ❌ fire-and-forget |
| 4 | `SystemPromptHook` | `extend_system_prompt() -> Option<String>` | ✅ 追加 prompt |
| 5 | `OnUserPromptSubmitHook` | `on_user_prompt_submit(payload) -> UserPromptSubmitResult` | ✅ 可注入/阻止 |
| 6 | `OnMessageReceivedHook` | `on_message_received(ctx) -> HookResult` | ❌ |
| 7 | `OnTurnStartHook` | `on_turn_start(ctx) -> HookResult` | ❌ |
| 8 | `OnToolCallStartHook` | `on_tool_call_start(ctx) -> HookResult` | ❌ (仅审计) |
| 9 | `OnTurnCompleteHook` | `on_turn_complete(ctx) -> HookResult` | ❌ |
| 10 | `OnSessionStartHook` | `on_session_start(ctx) -> HookResult` | ❌ |
| 11 | `OnSessionEndHook` | `on_session_end(ctx) -> HookResult` | ❌ |
| 12 | `OnErrorHook` | `on_error(ctx) -> HookResult` | ❌ |
| 13 | `OnModelResponseHook` | `on_model_response(response, turn_ctx) -> HookResult` | ❌ |

### 2.3 各实现覆盖的 Trait

| Hook 实现 | 实现的 Trait 数 | 具体 Trait |
|-----------|:---:|----------|
| **ShellCommandHook** | 6 | `PreToolExecution` + `PostToolExecution` + `OnSessionStart` + `OnSessionEnd` + `OnUserPromptSubmit` + `OnToolCallStart`（后一个是空操作占位）|
| **ScriptHook** | 4 | `PreToolExecution` + `PostToolExecution` + `PostTurn` + `SystemPrompt` |
| **WebhookHook** | 12 | 全部 13 个中除 `OnUserPromptSubmitHook` 外全部（含匹配过滤）|
| **ToolAuditLogHook** (built-in) | 1 | `OnToolCallStartHook` |
| **TurnStatsHook** (built-in) | 2 | `OnTurnStartHook` + `OnTurnCompleteHook` |
| **AutoCommitHook** (built-in) | 1 | `OnTurnCompleteHook` |
| **SessionSummaryHook** (built-in) | 2 | `OnSessionStartHook` + `OnSessionEndHook` |
| **ErrorReportHook** (built-in) | 1 | `OnErrorHook` |
| **ResponseValidationHook** (built-in) | 1 | `OnModelResponseHook` |

### 2.4 配置体系

#### A. JSON 配置（CC 兼容，`~/.atomcode/hooks.json` + `<project>/.hooks.json`）

```json
{
  "hooks": {
    "my-hook": {
      "event": "pre_tool_use",
      "matcher": "write*",
      "command": "echo ok",
      "timeout_ms": 10000,
      "disabled": false
    }
  }
}
```

支持的 event 值：`pre_tool_use`、`post_tool_use`、`session_start`、`session_end`、`user_prompt_submit`、`notification`（notification 被静默跳过）。

**重要**：JSON 配置是 CC (Claude Code) 兼容层，环境变量协议 (`ATOMCODE_HOOK_EVENT`、`ATOMCODE_HOOK_CONTEXT`、`ATOMCODE_TOOL_NAME` 等)，stdout 解析 `PreHookResult` JSON。

项目 hooks **覆盖**同名全局 hooks（而非追加）。

#### B. TOML 配置（新系统，`~/.atomcode/hooks/hooks.toml` + `<project>/.atomcode/hooks/hooks.toml`）

```toml
# === ScriptHook（TOML hooks 段） ===
[[hooks]]
name = "my-hook"
description = "描述"
trigger = "pre_tool"       # 仅支持: pre_tool | post_tool | post_turn | system_prompt
script = "myscript.sh"
script_type = "shell"      # shell | python
enabled = true
timeout_secs = 2           # 默认 2 秒

# === WebhookHook（同步） ===
[[webhooks]]
name = "notify"
trigger = "post_tool,post_turn,session_start"   # 逗号分隔，contains 匹配
url = "https://example.com/hook"
method = "POST"
timeout_secs = 10
retries = 2
enabled = true

# === 异步批量 Webhook ===
[[async_webhooks]]
name = "batch-logger"
trigger = "pre_tool,post_tool"
url = "https://example.com/batch"
batch_size = 10            # 默认 10
flush_interval_ms = 1000   # 默认 1000ms
timeout_secs = 10
retries = 2
enabled = true
```

**TOML ScriptHook trigger 仅支持 4 种**：
- `"pre_tool"` / `"pre_tool_execution"` → 注册为 `PreToolExecutionHook`
- `"post_tool"` / `"post_tool_execution"` → 注册为 `PostToolExecutionHook`
- `"post_turn"` → 注册为 `PostTurnHook`
- `"system_prompt"` → 注册为 `SystemPromptHook`

**其他值（如 `on_turn_complete`、`on_tool_call_start`、`session_start` 等）不会被 TOML ScriptHook 识别**，会打印 warning 并被跳过。

#### C. Webhook Trigger 支持的值

Webhook 通过 contains 匹配，支持以下 trigger 值（可逗号组合）：
`pre_tool`、`before_tool`、`post_tool`、`after_tool`、`post_turn`、`system_prompt`、
`session_start`、`session_end`、`error`、`turn_start`、`turn_complete`、`after_turn`、
`tool_call_start`、`model_response`

#### D. 内置 Hook（无需配置，自动注册）

所有 6 个内置 hook 在 `HookEngine::register_builtins()` 中自动注册，用户无需在配置文件中声明。要禁用，需通过后续 CLI 命令（TODO: 增加 enable/disable 开关）。

| 内置 Hook | 注册 Slot | 功能 |
|-----------|----------|------|
| `ToolAuditLogHook` | `OnToolCallStartHook` | 记录工具调用到 tracing 日志 |
| `TurnStatsHook` | `OnTurnStartHook` + `OnTurnCompleteHook` | 统计 Turn 耗时和操作 |
| `AutoCommitHook` | `OnTurnCompleteHook` | 每 N 个 Turn 自动 git commit |
| `SessionSummaryHook` | `OnSessionStartHook` + `OnSessionEndHook` | 会话开始/结束摘要 |
| `ErrorReportHook` | `OnErrorHook` | 错误详情记录 |
| `ResponseValidationHook` | `OnModelResponseHook` | 检测响应中的敏感信息 |

### 2.5 加载顺序

`HookEngine::load_all()` 按以下顺序：
1. JSON 配置（`load_json_hooks`）→ ShellCommandHook
2. TOML 配置（`load_toml_hooks`）→ ScriptHook + WebhookHook
3. 内置 Hook（`register_builtins`）
4. Webhook（`load_webhook_hooks`）

全局 hooks 先加载，项目 hooks 后加载（同名可覆盖）。

### 2.6 关键数据流

**PreToolExecution**（可阻断）：
```
HookResult::Ok        → 继续
HookResult::Modified  → 替换参数
HookResult::Denied    → 阻止工具执行
HookResult::Warning   → 继续但打印警告
```

**OnUserPromptSubmit**（可注入/阻断）：
```
UserPromptSubmitResult::Continue  → 继续
UserPromptSubmitResult::Inject(s) → 注入上下文到用户消息
UserPromptSubmitResult::Block(s)  → 阻止消息
```

**其他 hook**（fire-and-forget）：结果仅用于日志，不改变执行流程。

### 2.7 Container 中的 HookCtx vs HookContext

- `HookCtx`（PR 系统，用于 ScriptHook/WebhookHook）：`{tool_name, tool_args, working_dir, session_id, turn_number, metadata}`
- `HookContext`（CC 兼容系统，用于 ShellCommandHook）：`{event, tool_name, tool_args, tool_result, tool_success, session_id, working_dir}`

两者不同，不可混用。

---

## 三、各文档差异清单与修订要求

### 3.1 hooks.md — 🔴 整体重写

**当前问题**：
- 只列了 3 种 hook 类型 (pre_tool/post_tool/post_turn)，实际有 13 个 trait + 3 种配置方式
- 配置示例是 TOML 格式但用了不存在的 trigger 值
- 输入/输出格式描述是旧协议
- 缺少 JSON 配置 (.hooks.json) 说明
- 缺少 Webhook 配置说明
- 缺少内置 Hook 说明
- "项目级 hooks 优先级低于全局 hooks" 与代码不符（代码中是 project 后加载可覆盖同名）

**修订要求**：
- 重写为面向用户的"入门指南"定位
- 包含 3 种配置方式的总览：JSON（CC 兼容）、TOML（ScriptHook）、TOML（Webhook）
- 列出所有可用的 trigger 值及其含义
- 区分"用户可配置的 trigger"（TOML 4 种 + Webhook 11 种）和"内部 trait"（仅内置可用）
- 输入/输出格式以当前代码为准
- 安全注意事项更新：项目 hooks 覆盖全局 hooks
- 添加内置 Hook 的开关说明

### 3.2 hook-cli-guide.md — 🟡 中度修订

**当前问题**：
- trigger 值用了 trait 名称（`on_turn_complete`、`on_tool_call_start` 等），这些值在 TOML ScriptHook 中无效
- 默认超时写 "2 秒"，JSON 配置实际默认 10000ms
- 配置示例中的 trigger 值不可用
- 输出格式（`ok` / `deny:` / `warning:`）是 ScriptHook 的旧格式，ShellCommandHook 走 CC JSON 协议

**修订要求**：
- 所有配置示例中的 `trigger` 值改为 `pre_tool` / `post_tool` / `post_turn` / `system_prompt`
- 区分 JSON 配置和 TOML 配置的 CLI 命令
- 补充 `atomcode hooks test` 命令说明
- 更新脚本输出格式：TOML ScriptHook 走 `ok`/`deny:`/`modify:` 文本协议；JSON ShellCommandHook 走 CC JSON 协议

### 3.3 hook-implementation-summary.md — 🔴 整体重写

**当前问题**：
- 整个文档基于旧 `HookRegistry` 架构编写
- 引用 `HookRegistry` 和 `HookExecutor`，当前已改为 `HookEngine`
- 文件行数和测试数量与实际不符
- 引用不存在的测试文件 `hook_test.rs`
- 称项目级 hooks 优先级低于全局

**修订要求**：
- 完全重写为当前 `HookEngine` 架构的实现总结
- 正确列出所有源文件和模块
- 更新测试覆盖统计
- 正确描述加载优先级（global 先→project 后，同名覆盖）

### 3.4 hook-timing-complete.md — 🟡 中度修订

**当前问题**：
- 列出的 12 个 hook 时机正确（与 trait 一一对应），但配置示例使用了无效的 trigger 值
- `[[hooks]]` 段中的 `trigger` 值不正确（用户无法在 TOML 中配置 `on_turn_complete` 等）
- 缺少说明：大部分"时机"只能通过 Webhook 或内置 Hook 触发，不能通过 TOML ScriptHook 触发

**修订要求**：
- 保留完整的 13 个时机列表（加 `OnUserPromptSubmit` = 13），这是好的参考文档
- **关键**：添加清晰说明——每个时机通过什么配置方式可用
  - TOML ScriptHook：仅 pre_tool / post_tool / post_turn / system_prompt
  - JSON ShellCommandHook：pre_tool_use / post_tool_use / session_start / session_end / user_prompt_submit
  - Webhook：11 种（全部除 OnUserPromptSubmit/OnMessageReceived）
  - BuiltIn：6 种（OnToolCallStart / OnTurnStart / OnTurnComplete / OnSessionStart / OnSessionEnd / OnError / OnModelResponse）
- 配置示例使用正确的 trigger 值，或分别标注 TOML/JSON/Webhook 的写法

### 3.5 hook-expansion-summary.md — 🟡 中度修订

**当前问题**：
- 与 `hook-timing-complete.md` 大量重复
- 配置示例 trigger 值无效
- 缺少 JSON 配置的说明

**修订要求**：
- 可考虑与 `hook-timing-complete.md` 合并，或减少重复内容
- 如果不合并，确保配置示例正确、新增时机说明准确

### 3.6 hook-architecture.md — 🟢 小幅修订（最准确的文档）

**当前问题**（仅 2 处小差异）：
- 第 83 行注释称 ShellCommandHook "实现 5 个 trait"，实际实现 6 个（多了 `OnToolCallStartHook`，为空操作占位）
- 没有提及 `OnMessageReceivedHook` trait 存在但无实现注册

**修订要求**：
- 更新 `ShellCommandHook` trait 计数：5 → 6
- 可补充说明 `OnMessageReceivedHook` 定义但暂未激活
- 其余内容基本正确，保持不变

### 3.7 webhook-guide.md — 🟢 小幅修订

**当前问题**：
- 基本准确，trigger 值匹配代码逻辑
- 缺少数值默认值（timeout 10s、retries 2）与代码一致
- trigger 值列表中 `on_message_received` 实际不会生效（代码中 Webhook 没有 `message` contains 匹配分支）

**修订要求**：
- 从 trigger 支持列表中移除 `on_message_received`
- 确认 trigger 值与 `config_loader.rs` 中的 contains 逻辑一致
- 补充 `before_tool` / `after_tool` / `after_turn` 别名说明
- 可保留，整体准确

### 3.8 webhook-implementation-summary.md — 🟢 小幅修订

**当前问题**：
- WebhookHook 实现描述准确
- 配置文件路径和格式描述基本正确

**修订要求**：
- 更新以反映当前代码状态（已从旧 HookRegistry 迁移到 HookEngine）
- 确保与 `webhook-guide.md` 不重复（一个用户指南，一个实现总结）

### 3.9 async-webhook-guide.md — 🟢 小幅修订

**当前问题**：
- 异步批量配置参数与 `async_batcher.rs` 一致
- 批量请求格式描述准确

**修订要求**：
- 确认默认值（batch_size: 10, flush_interval_ms: 1000）与代码一致
- 可保留，基本准确

### 3.10 async-webhook-summary.md — 🟢 小幅修订

**当前问题**：
- 与 `async-webhook-guide.md` 内容高度重复

**修订要求**：
- 减少与 guide 的重复，聚焦于实现总结而非用户指南
- 或考虑合并到 guide 中

---

## 四、整体编排建议

### 4.1 推荐文档结构（按用户旅程）

| 优先级 | 文档 | 定位 | 修订程度 |
|--------|------|------|----------|
| P0 | **hooks.md** | 用户入门指南（快速开始 + 配置总览） | 🔴 重写 |
| P1 | **hook-cli-guide.md** | CLI 使用指南（`atomcode hooks` 命令） | 🟡 中度 |
| P1 | **hook-timing-complete.md** | 完整时机参考（含可用配置方式矩阵） | 🟡 中度 |
| P2 | **webhook-guide.md** | Webhook 用户指南 | 🟢 小幅 |
| P2 | **async-webhook-guide.md** | 异步批量 Webhook 用户指南 | 🟢 小幅 |
| P3 | **hook-architecture.md** | 技术架构参考（面向开发者） | 🟢 小幅 |
| P3 | **hook-implementation-summary.md** | 实现总结（面向贡献者） | 🔴 重写 |
| P4 | **webhook-implementation-summary.md** | Webhook 实现总结 | 🟢 小幅（可合并） |
| P4 | **async-webhook-summary.md** | 异步 Webhook 实现总结 | 🟢 小幅（可合并） |
| P4 | **hook-expansion-summary.md** | 扩展总结（快照型，可归档） | 🟡 可考虑删除或归档 |

### 4.2 合并建议

- **hook-timing-complete.md + hook-expansion-summary.md**：两篇大量重复，建议合并到 `hook-timing-complete.md`
- **webhook-implementation-summary.md → webhook-guide.md**：考虑作为 guide 的附录
- **async-webhook-summary.md → async-webhook-guide.md**：同上

### 4.3 全局规则

1. **trigger 值是敏感信息**：必须在每个配置示例中使用代码实际支持的值，TOML ScriptHook 只能用 4 种
2. **不再提 `HookRegistry`**：当前统一引擎是 `HookEngine`
3. **区分"用户可配置"与"内部 trait"**：13 个 trait 中只有少数是用户可以直接配置的
4. **配置优先级**：全局先加载，项目后加载（同名覆盖），而非"全局优先"
5. **默认超时**：JSON ShellCommandHook 默认 10000ms，TOML ScriptHook 默认 2s，Webhook 默认 10s
6. **代码行数、文件列表、测试数据**：以实际代码为准，不要从文档抄

---

## 五、验证方式

修订完成后，请逐项确认：
- [ ] hooks.md 中的配置示例可以复制粘贴后正常工作
- [ ] hook-cli-guide.md 中的 trigger 值被代码正确识别
- [ ] 不再出现 `HookRegistry`、`HookExecutor`（除非专门说明旧系统）
- [ ] 不再出现 `on_turn_complete` / `on_tool_call_start` 等 TOML ScriptHook 不支持的 trigger
- [ ] 输入输出格式与代码实际序列化一致
- [ ] 默认值（timeout、retries、batch_size）与代码一致
