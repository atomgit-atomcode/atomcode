# Hook 系统技术架构

> 重构后：`HookEngine` 统一调度，三种 Hook 实现共存在同一个引擎下。

## 宏观架构

```
                                AgentLoop
                          （会话生命周期管理者）
                                   │
         ┌──────────┬──────────┬────┴────┬──────────┬──────────┐
         │          │          │         │          │          │
    SessionStart  PreToolUse  PostTool  PostTurn  SessionEnd  UserPromptSubmit
         │          │          │         │          │          │
         └──────────┴──────────┴────┬────┴──────────┴──────────┘
                                    │
                         ┌──────────▼──────────┐
                         │    HookEngine       │
                         │  (Arc, 单例, 可替换) │
                         └─────────────────────┘
                                    │
                    ┌───────────────┼───────────────┐
                    ▼               ▼               ▼
            ShellCommandHook    ScriptHook    BuiltInHook
           (JSON 配置 → shell) (TOML → 脚本文件)  (Rust 原生)
                    │               │               │
                    ▼               ▼               ▼
               WebhookHook     (HTTP 远程调用)

        Legend:
        ━━━━━  = 调用流（async fn）
        ─ ─ ─  = 注册流（load_all 时建立）
        ┌──┐   = 数据/进程
```

## 分层拓扑

```
┌─────────────────────────────────────────────────────────────────┐
│                    第 4 层 · 调用侧                              │
│                                                                 │
│  agent/mod.rs          turn/runner.rs                           │
│  ├─ SessionStart       ├─ PreToolUse (单次触发, Result<Option>) │
│  ├─ SessionEnd         ├─ PostToolUse (fire-and-forget)         │
│  ├─ UserPromptSubmit   └─ PostTurn                              │
│  └─ ReloadConfig                                                │
│                                                                 │
│  特征：只调用 HookEngine 的 pub trigger_*，                        │
│        不引用任何具体的 Hook 实现类                                │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    第 3 层 · 统一引擎                            │
│                                                                 │
│  engine.rs · HookEngine                                         │
│  ├─ load_all(working_dir)    ← 唯一的配置入口                    │
│  ├─ 12 个 register_* 方法                                       │
│  └─ 12 个 trigger_*/collect_* 方法 (11 trigger_* + 1 collect_*)  │
│                                                                 │
│  关于 Arc:                                                       │
│  TurnRunner 持有 Arc<HookEngine>，AgentLoop 持有同引用。           │
│  ReloadConfig 时:                                                │
│    let engine = Arc::new(HookEngine::new());                     │
│    engine.load_all(&wd);                                        │
│    self.hook_engine.store(engine);   // ArcSwap 原子替换          │
│                                                                 │
│  旧引用继续存活直到所有正在执行的 trigger 返回。                    │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    第 2 层 · 适配实现                            │
│                                                                 │
│  ShellCommandHook              ScriptHook          WebhookHook   │
│  (engine.rs)                  (script_runner.rs)   (webhook.rs)  │
│  ┌─────────────────────┐     ┌──────────────┐    ┌───────────┐  │
│  │ command: String     │     │ script: Path │    │ url       │  │
│  │ matcher: regex      │     │ timeout      │    │ method    │  │
│  │ timeout_ms          │     │ script_type  │    │ headers   │  │
│  │ plugin_root         │     └──────────────┘    └───────────┘  │
│  │                     │                                         │
│  │ 实现 6 个 trait:    │  实现 4 个 trait:      实现 12 个 trait:   │
│  │ PreToolExecution    │  PreToolExecution     (除 OnUserPrompt     │
│  │ PostToolExecution   │  PostToolExecution     Submit 外全部)    │
│  │ OnSessionStart      │  PostTurn                               │
│  │ OnSessionEnd        │  SystemPrompt                           │
│  │ OnUserPromptSubmit  │                                         │
│  │ OnToolCallStart     │                                         │
│  └─────────────────────┘                                         │
│                                                                 │
│  BuiltInHook (built_in.rs)                                      │
│  ┌────────────────────────────────────────────────────────────┐ │
│  │ ToolAuditLogHook     → OnToolCallStartHook                 │ │
│  │ TurnStatsHook        → OnTurnStart + OnTurnComplete        │ │
│  │ AutoCommitHook       → OnTurnComplete                      │ │
│  │ SessionSummaryHook   → OnSessionStart + OnSessionEnd       │ │
│  │ ErrorReportHook      → OnError                             │ │
│  │ ResponseValidation   → OnModelResponse                     │ │
│  └────────────────────────────────────────────────────────────┘ │
│                                                                 │
│  注意：OnMessageReceivedHook trait 已定义但暂未在 HookEngine 中    │
│  注册触发，仅 WebhookHook 实现了该 trait（待后续 PR 激活）          │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                    第 1 层 · 协议适配                            │
│                                                                 │
│  ShellCommandHook 的两套子进程协议：                             │
│                                                                 │
│             CC 兼容协议                 新 stdin JSON 协议        │
│            (JSON 配置旧 hook)          (TOML 配置 ScriptHook)     │
│  ┌─────────────────────────┐    ┌──────────────────────────┐    │
│  │ Env:                    │    │ Env:                     │    │
│  │  ATOMCODE_HOOK_EVENT    │    │  ATOMCODE_HOOK_TYPE      │    │
│  │  ATOMCODE_HOOK_CONTEXT  │    │  ATOMCODE_TOOL_NAME      │    │
│  │  ATOMCODE_TOOL_NAME     │    │  ATOMCODE_WORKSPACE      │    │
│  │  CLAUDE_PLUGIN_ROOT     │    │                          │    │
│  │                         │    │ stdin:                   │    │
│  │ stdin:                  │    │  完整的上下文 JSON         │    │
│  │  PreToolUse/PostToolUse │    │  stdout:                 │    │
│  │  → HookContext JSON     │    │  HookResult JSON         │    │
│  │  UserPromptSubmit       │    │                          │    │
│  │  → {prompt, session_id, │    │ 超时:                    │    │
│  │     hook_event_name,    │    │  tokio::time::timeout    │    │
│  │     cwd}                │    │  + kill_on_drop(true)    │    │
│  │                         │    │  fail-open               │    │
│  │ stdout (last-line JSON):│    │                          │    │
│  │  decision: allow│block  │    │                          │    │
│  │  hookSpecificOutput:    │    │                          │    │
│  │   additionalContext     │    │                          │    │
│  │                         │    │                          │    │
│  │ 非 JSON stdout →        │    │                          │    │
│  │  纯文本注入              │    │                          │    │
│  │                         │    │                          │    │
│  │ 超时: fail-open         │    │                          │    │
│  │ crash: fail-open        │    │                          │    │
│  └─────────────────────────┘    └──────────────────────────┘    │
└─────────────────────────────────────────────────────────────────┘
```

## 配置加载路径（合并后唯一入口）

```
HookEngine::load_all(&working_dir)
│
├─ 1. JSON 配置 (老系统, CC 兼容)
│   ├─ ~/.hooks.json                    ─┐
│   ├─ <working_dir>/.hooks.json         ├─▶ json_config::load_hooks_config()
│   └─ plugin hooks.json / plugin.json  ─┘   ─▶ Vec<HookConfig>
│                                             ─▶ ShellCommandHook::from_hook_config()
│                                             ─▶ engine.register_*()
│
├─ 2. TOML 配置 (新系统)
│   ├─ ~/.atomcode/hooks/hooks.toml     ─┐
│   └─ .atomcode/hooks/hooks.toml        └─▶ HooksConfig::from_dir()
│                                             ─▶ register_hooks_to_engine()
│                                             ─▶ ScriptHook / WebhookHook
│
├─ 3. 内置 Hook
│   ├─ ToolAuditLogHook      → OnToolCallStart
│   ├─ TurnStatsHook         → OnTurnStart + OnTurnComplete
│   ├─ AutoCommitHook        → OnTurnComplete
│   ├─ SessionSummaryHook    → OnSessionStart + OnSessionEnd
│   ├─ ErrorReportHook       → OnError
│   └─ ResponseValidationHook → OnModelResponse
│
└─ 4. Webhook
    └─ HooksConfig::register_webhooks_to_engine()
```

## 事件 → Trait → 实现 映射表

| 引擎方法 | 触发时机 | Trait | ShellCommandHook | ScriptHook | BuiltIn |
|---|---|---|---|---|---|
| `trigger_pre_tool_use` | 工具执行前 | `PreToolExecutionHook` | ✅ | ✅ | — |
| `trigger_post_tool_use` | 工具执行后 | `PostToolExecutionHook` | ✅ | ✅ | — |
| `trigger_user_prompt_submit` | 用户发消息 | `OnUserPromptSubmitHook` | ✅ | — | — |
| `trigger_session_start` | 会话开始 | `OnSessionStartHook` | ✅ | — | ✅ (Summary) |
| `trigger_session_end` | 会话结束 | `OnSessionEndHook` | ✅ | — | ✅ (Summary) |
| `trigger_post_turn` | Turn 完成 | `PostTurnHook` | — | ✅ | ✅ (AutoCommit) |
| `trigger_on_turn_start` | Turn 开始 | `OnTurnStartHook` | — | — | ✅ (Stats) |
| `trigger_on_turn_complete` | Turn 完成（详细） | `OnTurnCompleteHook` | — | — | ✅ (Stats) |
| `trigger_on_tool_call_start` | 工具调用开始 | `OnToolCallStartHook` | — | — | ✅ (Audit) |
| `trigger_on_error` | 错误发生 | `OnErrorHook` | — | — | ✅ (Report) |
| `trigger_on_model_response` | 模型响应后 | `OnModelResponseHook` | — | — | ✅ (Validation) |
| `collect_system_prompt_extensions` | 构建 prompt | `SystemPromptHook` | — | ✅ | — |

## 结果类型流转

```
OnUserPromptSubmitHook::on_user_prompt_submit()
  └─▶ UserPromptSubmitResult ─▶ HookEngine::trigger_user_prompt_submit()
        │                        │
        │                        ├─▶ Continue  → UserPromptHookResult::Continue
        │                        ├─▶ Inject(s) → UserPromptHookResult::Inject(s)
        │                        └─▶ Block(s)  → UserPromptHookResult::Block(s)
        │
        └── 可转换为 HookResult（用于日志/统计）:
            Continue → Ok
            Inject   → Modified
            Block    → Denied

PreToolExecutionHook::on_pre_tool_execution()
  └─▶ HookResult ─▶ HookEngine::trigger_pre_tool_use()
        │
        ├─▶ Ok              → 继续, 参数不变
        ├─▶ Modified(json)  → 继续, 参数替换为 json
        ├─▶ Denied(reason)  → 阻塞, 返回 reason (≈ 老系统 Block)
        └─▶ Warning(msg)    → 继续, 仅打印警告

PostToolExecutionHook::on_post_tool_execution()
  └─▶ HookResult → fire-and-forget, 仅 Warning/Denied 会打印日志
```

## 并发安全模型

```
ReloadConfig 触发时:
  ┌─────────────────────┐
  │  创建新 HookEngine   │
  │  engine.load_all()   │
  └─────────┬───────────┘
            │
            ▼
  ┌─────────────────────────┐
  │ ArcSwap 原子替换         │
  │ hook_engine.store(arc)  │
  └─────────┬───────────────┘
            │
            ├──▶ 旧 Arc 引用计数 > 0? 等待正在执行的 trigger 返回
            │     (Rust Arc 自动管理, 不需要显式锁)
            │
            └──▶ 新请求看到新 Arc, 使用新引擎

  优点:
  - 读路径 (trigger_*) 零锁开销
  - 写路径 (reload) 只建新引擎 + 原子替换一次
  - 不阻塞任何正在执行的 hook
```
