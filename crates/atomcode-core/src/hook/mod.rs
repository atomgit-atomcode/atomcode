pub mod config;
pub mod executor;
pub mod json_config;

use serde::{Deserialize, Serialize};
use serde_json::Value;

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

/// Context passed to a hook command via stdin as JSON.
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

    // ── HookContext ──────────────────────────────────────────────

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
