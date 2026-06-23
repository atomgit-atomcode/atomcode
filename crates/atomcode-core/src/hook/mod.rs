// ============================================================================
// Submodule declarations
// ============================================================================

// Upstream (HEAD) submodules
pub mod config;
pub mod executor;
pub mod json_config;

// PR submodules
pub mod async_batcher;
pub mod built_in;
pub mod config_loader;
pub mod engine;
pub mod script_runner;
pub mod webhook;

// ============================================================================
// Imports
// ============================================================================

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

// ============================================================================
// Upstream (HEAD) types — hook event system
// ============================================================================

/// Events in the agent lifecycle that can trigger hooks.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    SessionStart,
    SessionEnd,
    Notification,
    /// Fires when a user submits a new message but before it reaches the
    /// LLM. Hooks bound to this event can inject additional context or
    /// block the submission entirely (CC parity — used by workflow router
    /// plugins like the ascend `workflow_planner_hook.py`).
    UserPromptSubmit,
}

/// A single hook definition from the user's configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookConfig {
    pub event: HookEvent,
    /// Optional glob/regex pattern to match against tool names.
    /// Only meaningful for `PreToolUse` and `PostToolUse` events.
    pub matcher: Option<String>,
    /// Shell command to execute when the hook fires.
    pub command: String,
    /// Maximum time (in milliseconds) the hook command may run before being killed.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// Plugin install dir — set when this hook came from an installed
    /// plugin. The executor exports it as `CLAUDE_PLUGIN_ROOT` and
    /// `ATOMCODE_PLUGIN_ROOT` so plugin authors can reference resources
    /// alongside their manifest. This is the safe channel: doing string
    /// substitution into the command line would break on paths containing
    /// spaces / quotes / `$` and could open command injection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_root: Option<std::path::PathBuf>,
}

fn default_timeout_ms() -> u64 {
    10_000
}

/// CC-compatible payload sent to a `UserPromptSubmit` hook over stdin
/// (serialized as JSON). Field names match Claude Code's spec verbatim so
/// existing CC plugin scripts work unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPromptSubmitPayload {
    pub session_id: String,
    pub hook_event_name: String,
    pub prompt: String,
    pub cwd: String,
}

/// Result of running all `UserPromptSubmit` hooks for a single user message.
#[derive(Debug, Clone, PartialEq)]
pub enum UserPromptHookResult {
    /// All hooks passed without modifying or blocking the prompt.
    Continue,
    /// At least one hook contributed extra context (concatenated by the
    /// agent into the user message before LLM processing).
    Inject(String),
    /// A hook explicitly blocked the prompt; `reason` is shown in the UI.
    Block(String),
    /// A hook failed due to an environment issue (e.g. missing dependency)
    /// rather than an explicit block. The turn continues but the user is
    /// warned via the status-bar hint.
    Warning(String),
}

/// Internal: parsed shape of a single hook's stdout payload (CC spec).
/// We only deserialize the fields we act on; CC adds more keys in newer
/// versions and we ignore them by default.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub(crate) struct UserPromptSubmitOutput {
    /// Top-level decision. `"block"` blocks the prompt; absent / other
    /// values are treated as continue.
    pub decision: Option<String>,
    /// Reason shown to the user when `decision == "block"`.
    pub reason: Option<String>,
    /// Newer CC layout: hook-specific output bag.
    #[serde(rename = "hookSpecificOutput")]
    pub hook_specific_output: Option<UserPromptHookSpecific>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub(crate) struct UserPromptHookSpecific {
    /// Plain text appended to the user prompt as additional context.
    #[serde(rename = "additionalContext")]
    pub additional_context: Option<String>,
}

/// Result returned by a pre-tool-use hook to control tool execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PreHookResult {
    /// Allow the tool call to proceed unchanged.
    Allow,
    /// Block the tool call with a reason shown to the user/agent.
    Block { reason: String },
    /// Allow the tool call but replace its arguments.
    Modify { args: Value },
}

/// Context passed to a hook command via stdin as JSON (upstream hook system).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookContext {
    /// The event name that triggered this hook (e.g. `"pre_tool_use"`).
    pub event: String,
    /// Tool name, present for tool-related events.
    pub tool_name: Option<String>,
    /// Tool arguments, present for `pre_tool_use`.
    pub tool_args: Option<Value>,
    /// Tool output/result, present for `post_tool_use`.
    pub tool_result: Option<String>,
    /// Whether the tool succeeded, present for `post_tool_use`.
    pub tool_success: Option<bool>,
    /// Unique session identifier.
    pub session_id: String,
    /// Current working directory of the agent.
    pub working_dir: String,
}

// ============================================================================
// PR types — Hook 执行结果与上下文
// ============================================================================

/// Hook 执行结果
#[derive(Debug, Clone)]
pub enum HookResult {
    /// Hook 成功执行
    Ok,
    /// Hook 明确允许操作（跳过后续审批）
    ExplicitAllow,
    /// Hook 失败（非致命，仅记录警告）
    Warning(String),
    /// Hook 拒绝继续操作（致命）
    Denied(String),
    /// Hook 修改了参数（返回新的参数）
    Modified(String),
}

/// 用户消息上下文
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessageContext {
    /// 用户消息内容
    pub content: String,
    /// Session ID
    pub session_id: Option<String>,
    /// 附加的文件路径
    pub attached_files: Vec<String>,
    /// 时间戳
    pub timestamp: String,
}

/// Turn 开始上下文
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnStartContext {
    /// Turn 编号
    pub turn_number: u32,
    /// Session ID
    pub session_id: Option<String>,
    /// 工作目录
    pub working_dir: String,
    /// 当前阶段（planning/diagnosis/execution）
    pub phase: String,
    /// 是否有文件上下文
    pub has_file_context: bool,
}

/// 工具调用开始上下文
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallStartContext {
    /// 工具名称
    pub tool_name: String,
    /// 工具参数
    pub tool_args: String,
    /// 调用 ID
    pub call_id: String,
    /// Turn 编号
    pub turn_number: u32,
}

/// Turn 完成上下文
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnCompleteContext {
    /// Turn 编号
    pub turn_number: u32,
    /// 结果类型（Responded/UsedTools/Failed/Cancelled）
    pub result_type: String,
    /// 消耗的 token 数
    pub tokens_used: usize,
    /// 工具调用次数
    pub tool_calls: usize,
    /// 执行时长（毫秒）
    pub duration_ms: u64,
    /// 是否被截断
    pub truncated: bool,
    /// 编辑的文件列表
    pub edited_files: Vec<String>,
}

/// 会话上下文
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionContext {
    /// Session ID
    pub session_id: String,
    /// 工作目录
    pub working_dir: String,
    /// 模型名称
    pub model_name: String,
    /// Provider 名称
    pub provider_name: String,
}

/// 错误上下文
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorContext {
    /// 错误类型
    pub error_type: String,
    /// 错误信息
    pub error_message: String,
    /// 发生错误的阶段
    pub phase: String,
    /// Turn 编号（如果适用）
    pub turn_number: Option<u32>,
}

/// Hook 上下文 - 传递给 PR 钩子系统的数据
/// (Renamed from HookContext → HookCtx to avoid collision with upstream HookContext)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookCtx {
    /// 当前工具名称
    pub tool_name: String,
    /// 工具参数（JSON 字符串）
    pub tool_args: String,
    /// 工作目录
    pub working_dir: String,
    /// 当前 session ID
    pub session_id: Option<String>,
    /// 当前 turn 编号
    pub turn_number: u32,
    /// 额外元数据
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, String>>,
}

impl HookCtx {
    pub fn new(tool_name: String, tool_args: String, working_dir: String) -> Self {
        Self {
            tool_name,
            tool_args,
            working_dir,
            session_id: None,
            turn_number: 0,
            metadata: None,
        }
    }

    pub fn with_session(mut self, session_id: String) -> Self {
        self.session_id = Some(session_id);
        self
    }

    pub fn with_turn(mut self, turn_number: u32) -> Self {
        self.turn_number = turn_number;
        self
    }

    pub fn with_metadata(mut self, key: String, value: String) -> Self {
        let metadata = self.metadata.get_or_insert_with(HashMap::new);
        metadata.insert(key, value);
        self
    }
}

/// 工具执行结果上下文（用于 PostExecutionHook）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultContext {
    pub tool_name: String,
    pub tool_args: String,
    pub result: String,
    pub success: bool,
    pub duration_ms: u64,
}

// ============================================================================
// PR traits — Hook 接口定义
// ============================================================================

/// Hook trait - 所有钩子必须实现的基础接口
#[async_trait]
pub trait Hook: Send + Sync {
    /// Hook 名称（用于日志和调试）
    fn name(&self) -> &str;

    /// Hook 描述（用于文档）
    fn description(&self) -> &str {
        ""
    }

    /// 是否应该启用此 hook
    fn is_enabled(&self) -> bool {
        true
    }

    /// Hook 优先级（数字越小越先执行）
    fn priority(&self) -> i32 {
        0
    }
}

/// 工具执行前钩子 - 在工具实际执行前调用
/// 可用于：参数验证/修改、额外检查、日志记录、阻止执行
#[async_trait]
pub trait PreToolExecutionHook: Hook {
    /// 返回 HookResult 决定是否继续执行
    /// - Ok: 继续执行
    /// - Modified(new_args): 使用新参数继续执行
    /// - Denied(reason): 阻止执行
    /// - Warning(msg): 记录警告但继续执行
    async fn on_pre_execute(&self, ctx: &HookCtx) -> HookResult;
}

/// 工具执行后钩子 - 在工具执行完成后调用
/// 可用于：结果处理、日志记录、触发后续操作、结果修改
#[async_trait]
pub trait PostToolExecutionHook: Hook {
    /// 接收工具执行结果
    async fn on_post_execute(&self, ctx: &HookCtx, result_ctx: &ToolResultContext) -> HookResult;
}

/// Turn 完成后钩子 - 在一轮对话结束后调用
/// 可用于：自动提交、代码审查、统计收集
#[async_trait]
pub trait PostTurnHook: Hook {
    /// Turn 完成后的回调
    /// turn_result 包含本轮的最终状态（Responded/ToolUsed/Failed）
    async fn on_post_turn(&self, ctx: &HookCtx, turn_result: &str) -> HookResult;
}

/// 系统 Prompt 扩展钩子 - 在构建系统提示时调用
/// 可用于：注入额外规则、添加自定义指令
#[async_trait]
pub trait SystemPromptHook: Hook {
    /// 返回要添加到系统 prompt 的内容
    async fn extend_system_prompt(&self) -> Option<String>;
}

// ============================================================================
// 新增的工程化 Hook 时机
// ============================================================================

/// 用户消息接收钩子 - 在收到用户消息时调用
/// 可用于：消息过滤、审计、自动回复、上下文增强
#[async_trait]
pub trait OnMessageReceivedHook: Hook {
    /// 用户消息接收时的回调
    /// 返回 Modified 可以修改用户消息内容
    async fn on_message_received(&self, ctx: &UserMessageContext) -> HookResult;
}

/// Turn 开始钩子 - 在 Turn 开始前调用
/// 可用于：注入自定义上下文、设置环境变量、记录日志
#[async_trait]
pub trait OnTurnStartHook: Hook {
    /// Turn 开始时的回调
    async fn on_turn_start(&self, ctx: &TurnStartContext) -> HookResult;
}

/// 工具调用开始钩子 - 在工具调用开始时调用（在权限检查前）
/// 可用于：审计、限流、拦截
#[async_trait]
pub trait OnToolCallStartHook: Hook {
    /// 工具调用开始时的回调
    /// 返回 Denied 可以阻止工具调用
    async fn on_tool_call_start(&self, ctx: &ToolCallStartContext) -> HookResult;
}

/// Turn 完成钩子 - 在 Turn 完成后调用（包含详细信息）
/// 可用于：统计分析、自动操作、报告生成
#[async_trait]
pub trait OnTurnCompleteHook: Hook {
    /// Turn 完成后的回调
    async fn on_turn_complete(&self, ctx: &TurnCompleteContext) -> HookResult;
}

/// 会话开始钩子 - 在会话启动时调用
/// 可用于：初始化、加载自定义上下文、环境检查
#[async_trait]
pub trait OnSessionStartHook: Hook {
    /// 会话开始时的回调
    async fn on_session_start(&self, ctx: &SessionContext) -> HookResult;
}

/// 会话结束钩子 - 在会话结束时调用
/// 可用于：清理、生成报告、保存状态
#[async_trait]
pub trait OnSessionEndHook: Hook {
    /// 会话结束时的回调
    async fn on_session_end(&self, ctx: &SessionContext) -> HookResult;
}

/// 错误发生钩子 - 在错误发生时调用
/// 可用于：错误报告、自动恢复、通知
#[async_trait]
pub trait OnErrorHook: Hook {
    /// 错误发生时的回调
    async fn on_error(&self, ctx: &ErrorContext) -> HookResult;
}

/// 模型响应钩子 - 在模型响应完成后调用
/// 可用于：响应验证、自动修正、日志记录
#[async_trait]
pub trait OnModelResponseHook: Hook {
    /// 模型响应完成后的回调
    async fn on_model_response(&self, response: &str, turn_ctx: &TurnStartContext) -> HookResult;
}

/// 用户 Prompt 提交钩子 - 在用户提交消息时调用 (CC 兼容)
/// 可用于：阻止/注入上下文/预处理
#[async_trait]
pub trait OnUserPromptSubmitHook: Hook {
    /// 用户 prompt 提交时的回调
    async fn on_user_prompt_submit(
        &self,
        payload: &UserPromptSubmitPayload,
    ) -> UserPromptSubmitResult;
}

/// UserPromptSubmit hook 的返回结果
pub enum UserPromptSubmitResult {
    /// 继续，不干预
    Continue,
    /// 注入额外上下文
    Inject(String),
    /// 阻止此 prompt
    Block(String),
    /// Hook 因环境问题（如缺少依赖）执行失败，非主动拒绝
    Warning(String),
}

impl From<UserPromptSubmitResult> for HookResult {
    fn from(r: UserPromptSubmitResult) -> Self {
        match r {
            UserPromptSubmitResult::Continue => HookResult::Ok,
            UserPromptSubmitResult::Inject(s) => HookResult::Modified(s),
            UserPromptSubmitResult::Block(s) => HookResult::Denied(s),
            UserPromptSubmitResult::Warning(_) => HookResult::Warning(String::new()),
        }
    }
}

#[derive(Debug)]
pub struct HookStats {
    pub pre_tool_hooks: usize,
    pub post_tool_hooks: usize,
    pub post_turn_hooks: usize,
    pub system_prompt_hooks: usize,
    pub on_turn_start_hooks: usize,
    pub on_tool_call_start_hooks: usize,
    pub on_turn_complete_hooks: usize,
    pub on_session_start_hooks: usize,
    pub on_session_end_hooks: usize,
    pub on_error_hooks: usize,
    pub on_model_response_hooks: usize,
    pub on_user_prompt_submit_hooks: usize,
}

// ============================================================================
// HookRegistry 已删除。所有 hook 管理现在统一通过 HookEngine。
// ============================================================================

pub use engine::HookEngine;

// ============================================================================
// Upstream (HEAD) tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── HookEvent ────────────────────────────────────────────────

    #[test]
    fn hook_event_serializes_to_snake_case() {
        assert_eq!(
            serde_json::to_string(&HookEvent::PreToolUse).unwrap(),
            r#""pre_tool_use""#
        );
        assert_eq!(
            serde_json::to_string(&HookEvent::PostToolUse).unwrap(),
            r#""post_tool_use""#
        );
        assert_eq!(
            serde_json::to_string(&HookEvent::SessionStart).unwrap(),
            r#""session_start""#
        );
        assert_eq!(
            serde_json::to_string(&HookEvent::SessionEnd).unwrap(),
            r#""session_end""#
        );
        assert_eq!(
            serde_json::to_string(&HookEvent::Notification).unwrap(),
            r#""notification""#
        );
    }

    #[test]
    fn hook_event_deserializes_from_snake_case() {
        let event: HookEvent = serde_json::from_str(r#""pre_tool_use""#).unwrap();
        assert_eq!(event, HookEvent::PreToolUse);

        let event: HookEvent = serde_json::from_str(r#""post_tool_use""#).unwrap();
        assert_eq!(event, HookEvent::PostToolUse);
    }

    // ── HookConfig ───────────────────────────────────────────────

    #[test]
    fn hook_config_roundtrip_json() {
        let cfg = HookConfig {
            event: HookEvent::PreToolUse,
            matcher: Some("bash".into()),
            command: "echo ok".into(),
            timeout_ms: 5000,
            plugin_root: None,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: HookConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.event, HookEvent::PreToolUse);
        assert_eq!(back.matcher.as_deref(), Some("bash"));
        assert_eq!(back.command, "echo ok");
        assert_eq!(back.timeout_ms, 5000);
    }

    #[test]
    fn hook_config_timeout_defaults_to_10000() {
        let json = r#"{
            "event": "session_start",
            "command": "notify-send hello"
        }"#;
        let cfg: HookConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.timeout_ms, 10_000);
        assert!(cfg.matcher.is_none());
    }

    #[test]
    fn hook_config_roundtrip_toml() {
        let toml_str = r#"
event = "pre_tool_use"
matcher = "write"
command = "check-write.sh"
timeout_ms = 3000
"#;
        let cfg: HookConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.event, HookEvent::PreToolUse);
        assert_eq!(cfg.matcher.as_deref(), Some("write"));
        assert_eq!(cfg.timeout_ms, 3000);
    }

    // ── PreHookResult ────────────────────────────────────────────

    #[test]
    fn pre_hook_result_allow_roundtrip() {
        let r = PreHookResult::Allow;
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json, json!({"action": "allow"}));

        let back: PreHookResult = serde_json::from_value(json).unwrap();
        assert_eq!(back, PreHookResult::Allow);
    }

    #[test]
    fn pre_hook_result_block_roundtrip() {
        let r = PreHookResult::Block {
            reason: "unsafe".into(),
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(json, json!({"action": "block", "reason": "unsafe"}));

        let back: PreHookResult = serde_json::from_value(json).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn pre_hook_result_modify_roundtrip() {
        let new_args = json!({"path": "/safe/dir", "content": "ok"});
        let r = PreHookResult::Modify {
            args: new_args.clone(),
        };
        let json = serde_json::to_value(&r).unwrap();
        assert_eq!(
            json,
            json!({"action": "modify", "args": {"path": "/safe/dir", "content": "ok"}})
        );

        let back: PreHookResult = serde_json::from_value(json).unwrap();
        assert_eq!(back, r);
    }

    // ── HookContext (upstream) ───────────────────────────────────

    #[test]
    fn hook_context_full_roundtrip() {
        let ctx = HookContext {
            event: "pre_tool_use".into(),
            tool_name: Some("bash".into()),
            tool_args: Some(json!({"command": "ls"})),
            tool_result: None,
            tool_success: None,
            session_id: "abc-123".into(),
            working_dir: "/home/user/project".into(),
        };
        let json = serde_json::to_string(&ctx).unwrap();
        let back: HookContext = serde_json::from_str(&json).unwrap();
        assert_eq!(back.event, "pre_tool_use");
        assert_eq!(back.tool_name.as_deref(), Some("bash"));
        assert!(back.tool_result.is_none());
        assert!(back.tool_success.is_none());
        assert_eq!(back.session_id, "abc-123");
    }

    #[test]
    fn hook_context_post_tool_use() {
        let ctx = HookContext {
            event: "post_tool_use".into(),
            tool_name: Some("write".into()),
            tool_args: None,
            tool_result: Some("file written".into()),
            tool_success: Some(true),
            session_id: "xyz-789".into(),
            working_dir: "/tmp".into(),
        };
        let v = serde_json::to_value(&ctx).unwrap();
        assert_eq!(v["tool_success"], json!(true));
        assert_eq!(v["tool_result"], json!("file written"));
    }

    #[test]
    fn hook_context_minimal_session_event() {
        let json_str = r#"{
            "event": "session_start",
            "tool_name": null,
            "tool_args": null,
            "tool_result": null,
            "tool_success": null,
            "session_id": "s1",
            "working_dir": "/home"
        }"#;
        let ctx: HookContext = serde_json::from_str(json_str).unwrap();
        assert_eq!(ctx.event, "session_start");
        assert!(ctx.tool_name.is_none());
    }
}
