# Hook 系统完整时机列表

## 概述

AtomCode Hook 系统提供了 **13 个关键时机** 的扩展点，覆盖用户消息接收、Turn 执行、工具调用、会话管理等全生命周期。

> **重要**：不同时机通过不同配置方式可用。TOML ScriptHook 仅支持 4 种 trigger，完整列表见下表。

## 完整 Hook 时机列表

### 一、消息级别（1 个）

| # | Hook 名称 | 触发时机 | 主要用途 | 可否修改 |
|---|----------|---------|---------|---------|
| 1 | `OnMessageReceived` | 收到用户消息时 | 消息过滤、审计、自动回复、上下文增强 | ✅ 可修改消息内容 |

**典型应用**：
- 敏感词过滤
- 自动添加上下文前缀
- 消息审计日志

> **当前状态**：`OnMessageReceivedHook` trait 已定义，暂未在 HookEngine 中注册触发。仅 WebhookHook 实现了该 trait（需后续 PR 激活）。

---

### 二、Turn 级别（4 个）

| # | Hook 名称 | 触发时机 | 主要用途 | 可否修改 |
|---|----------|---------|---------|---------|
| 2 | `OnTurnStart` | Turn 开始前 | 注入自定义上下文、设置环境变量 | ❌ |
| 3 | `OnTurnComplete` | Turn 完成后（含详细信息） | 统计分析、自动操作、报告生成 | ❌ |
| 4 | `PostTurn` | Turn 完成后（向后兼容） | 向后兼容旧版 hook | ❌ |
| 5 | `OnModelResponse` | 模型响应完成后 | 响应验证、自动修正、日志记录 | ❌ |

**典型应用**：
- Turn 开始：设置临时环境变量、加载项目特定配置
- Turn 完成：收集统计信息、触发自动提交
- 模型响应：验证输出格式、检测幻觉

---

### 三、工具调用级别（3 个）

| # | Hook 名称 | 触发时机 | 主要用途 | 可否修改 |
|---|----------|---------|---------|---------|
| 6 | `OnToolCallStart` | 工具调用开始时（权限检查前） | 审计、限流、拦截 | ❌ 可拒绝 |
| 7 | `PreToolExecution` | 工具执行前（权限检查后） | 参数验证/修改、额外检查 | ✅ 可修改参数 |
| 8 | `PostToolExecution` | 工具执行完成后 | 结果处理、日志记录、触发后续操作 | ❌ |

**典型应用**：
- 工具调用开始：记录审计日志、限流控制
- 工具执行前：添加默认参数、阻止危险操作
- 工具执行后：处理结果、触发通知

---

### 四、会话级别（2 个）

| # | Hook 名称 | 触发时机 | 主要用途 | 可否修改 |
|---|----------|---------|---------|---------|
| 9 | `OnSessionStart` | 会话启动时 | 初始化、加载自定义上下文、环境检查 | ❌ |
| 10 | `OnSessionEnd` | 会话结束时 | 清理、生成报告、保存状态 | ❌ |

**典型应用**：
- 会话开始：加载项目特定规则、检查依赖
- 会话结束：生成总结报告、清理临时文件

---

### 五、系统级别（2 个）

| # | Hook 名称 | 触发时机 | 主要用途 | 可否修改 |
|---|----------|---------|---------|---------|
| 11 | `OnError` | 错误发生时 | 错误报告、自动恢复、通知 | ❌ |
| 12 | `SystemPrompt` | 构建系统提示时 | 注入额外规则、添加自定义指令 | ✅ 可追加内容 |

**典型应用**：
- 错误：发送到错误追踪系统、尝试自动恢复
- 系统 Prompt：添加团队编码规范、项目特定规则

---

### 六、用户交互级别（1 个）

| # | Hook 名称 | 触发时机 | 主要用途 | 可否修改 |
|---|----------|---------|---------|---------|
| 13 | `OnUserPromptSubmit` | 用户提交 prompt 时 | 注入上下文、阻止敏感消息 | ✅ 可注入/阻止 |

---

## Hook 执行顺序

> **注意**：下表中编号对应上文的 Hook # 序号，不表示执行先后次序。
> 执行流按时间线从上到下排列。

```
用户发送消息
  ↓
[13] OnUserPromptSubmit (可注入/阻止)
  ↓
开始 Turn
  ↓
[2] OnTurnStart
  ↓
模型响应
  ↓
[5] OnModelResponse
  ↓
工具调用循环 (多次)
  ↓
  [6] OnToolCallStart (可拒绝)
  ↓
  [7] PreToolExecution (可修改参数/拒绝)
  ↓
  工具实际执行
  ↓
  [8] PostToolExecution
  ↓
Turn 完成
  ↓
[3] OnTurnComplete (含详细统计)
[4] PostTurn (向后兼容)
  ↓
(重复上述过程，直到 Turn 结束)
  ↓
错误发生（如果有）
  ↓
[11] OnError
  ↓
会话结束
  ↓
[9] OnSessionStart (已在开始时调用)
[10] OnSessionEnd
```

---

## 可用配置方式矩阵

**关键**：不是所有时机都能通过 TOML ScriptHook 触发。下表列出每个时机的可用配置方式：

| # | Hook 时机 | TOML ScriptHook | JSON CC | Webhook | 内置 Hook |
|---|----------|:---:|:---:|:---:|:---:|
| 1 | `OnMessageReceived` | — | — | —¹ | — |
| 2 | `OnTurnStart` | — | — | `turn_start` | `TurnStatsHook` |
| 3 | `OnTurnComplete` (Turn 完成，含 token 等详细统计) | — | — | `turn_complete` / `after_turn` | `TurnStatsHook` + `AutoCommitHook` |
| 4 | `PostTurn` (Turn 完成，旧版兼容，上下文较少) | `post_turn` | — | `post_turn` | — |
| 5 | `OnModelResponse` | — | — | `model_response` | `ResponseValidationHook` |
| 6 | `OnToolCallStart` | — | — | `tool_call_start` | `ToolAuditLogHook` |
| 7 | `PreToolExecution` | `pre_tool` | `pre_tool_use` | `pre_tool` / `before_tool` | — |
| 8 | `PostToolExecution` | `post_tool` | `post_tool_use` | `post_tool` / `after_tool` | — |
| 9 | `OnSessionStart` | — | `session_start` | `session_start` | `SessionSummaryHook` |
| 10 | `OnSessionEnd` | — | `session_end` | `session_end` | `SessionSummaryHook` |
| 11 | `OnError` | — | — | `error` | `ErrorReportHook` |
| 12 | `SystemPrompt` | `system_prompt` | — | `system_prompt` | — |
| 13 | `OnUserPromptSubmit` | — | `user_prompt_submit` | — | — |

> ¹ OnMessageReceived：WebhookHook 实现了对应 trait，但引擎尚未注册触发槽位，当前实际不可用。

---

## 内置工程化 Hook（6 个已实现）

| # | Hook 名称 | 类型 | 功能 |
|---|----------|------|------|
| 1 | `ToolAuditLogHook` | `OnToolCallStart` | 记录所有工具调用到审计日志 |
| 2 | `TurnStatsHook` | `OnTurnStart` + `OnTurnComplete` | 收集 Turn 级别统计信息 |
| 3 | `AutoCommitHook` | `OnTurnComplete` | 每 N 个 Turn 自动提交 Git |
| 4 | `SessionSummaryHook` | `OnSessionStart` + `OnSessionEnd` | 会话结束时生成总结报告 |
| 5 | `ErrorReportHook` | `OnError` | 记录错误详细信息到日志 |
| 6 | `ResponseValidationHook` | `OnModelResponse` | 验证模型响应，检测敏感信息 |

---

## 对比原实现

### 原有 Hook 时机（4 个）

1. ✅ `PreToolExecution` - 工具执行前
2. ✅ `PostToolExecution` - 工具执行后
3. ✅ `PostTurn` - Turn 完成后
4. ✅ `SystemPrompt` - 系统 Prompt 扩展

### 新增 Hook 时机（9 个）

5. ✨ `OnMessageReceived` - 用户消息接收时（trait 已定义，待激活）
6. ✨ `OnTurnStart` - Turn 开始前
7. ✨ `OnToolCallStart` - 工具调用开始时
8. ✨ `OnTurnComplete` - Turn 完成后（详细信息）
9. ✨ `OnSessionStart` - 会话启动时
10. ✨ `OnSessionEnd` - 会话结束时
11. ✨ `OnError` - 错误发生时
12. ✨ `OnModelResponse` - 模型响应完成后
13. ✨ `OnUserPromptSubmit` - 用户提交 prompt 时

---

## 使用建议

### 必备 Hook（推荐启用）

- ✅ `ToolAuditLogHook` - 审计日志（安全合规）
- ✅ `TurnStatsHook` - 统计分析（了解模型行为）
- ✅ `ErrorReportHook` - 错误记录（问题排查）

### 可选 Hook（按需启用）

- ⚙️ `AutoCommitHook` - 自动提交（适合个人项目）
- ⚙️ `SessionSummaryHook` - 会话总结（适合长会话）
- ⚙️ `ResponseValidationHook` - 响应验证（企业环境）

### 开发 Hook（调试用）

- 🔧 自定义脚本 Hook - 快速原型验证

---

## 性能影响

| Hook 类型 | 性能影响 | 建议超时 |
|----------|---------|---------|
| Rust 原生 Hook | 零开销（直接调用） | N/A |
| 脚本 Hook | 进程启动开销（~10ms） | 2-5 秒 |
| 网络 Hook | 网络延迟（~100ms） | 10 秒 |

**注意**：所有脚本 Hook 都有超时保护，超时后会被强制终止。

---

## 安全机制

1. **项目 hooks 覆盖全局 hooks**（项目 hooks 后加载，同名覆盖）
2. **不能绕过权限系统** - Hook 的 deny 不会覆盖用户的 always_allow
3. **脚本在用户权限下运行** - 注意脚本本身的安全性
4. **超时保护** - 默认 2 秒（TOML ScriptHook）/ 10 秒（JSON/Webhook），防止 Hook 阻塞主流程
