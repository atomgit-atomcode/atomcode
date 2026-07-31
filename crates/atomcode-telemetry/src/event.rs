//! Event and Envelope schema for AtomCode telemetry v2.
//!
//! The events are: open_atomcode, llm_chat, tool_call, use_command,
//! mcp_connect, login_success, take_codingplan, panic, telemetry_disabled.
//!
//! Wire format: envelope fields + event-specific payload, both flattened
//! into one JSON object. Event variant is tagged via `event_id`.

use serde::Serialize;
use uuid::Uuid;

// ---------- SessionMode ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    Headless,
    Tui,
    Ide,
    Vscode,
    Jetbrains,
    #[serde(rename = "webui")]
    Webui,
    #[serde(rename = "atomcode_desktop")]
    AtomcodeAir,
    Channel,
}

// ---------- Envelope (common to every event) ----------

#[derive(Debug, Clone, Serialize)]
pub struct Envelope {
    pub device_id: Uuid,
    pub launch_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    pub session_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<Uuid>,
    pub ts: i64,
    pub schema_version: u32,
    pub app_version: String,
    pub os: String,
    pub arch: String,
    pub locale: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Vendor host (e.g. `api.openai.com`). Derived from the configured
    /// `base_url` host part — falls back to each vendor's official host
    /// when missing/unparseable. See `resolve_provider_host`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_origin: Option<RepoOrigin>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<SessionMode>,
    /// Logical ORIGIN of the LLM call within the app — distinct from `mode` (the
    /// session SURFACE: tui/headless/…). Set per-scope so a sub-agent's spend can be
    /// attributed to its feature (e.g. `"code_review"` for the in-session review tool
    /// and the standalone `atomcodex review`); `None` = the primary agent loop.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub surface: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RepoOrigin {
    pub host: RepoHost,
    pub has_git: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoHost {
    Gitcode,
    Atomgit,
    Github,
    Gitlab,
    Other,
    None,
}

// ---------- Error kind enums ----------

/// LLM 对话错误类型
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LlmErrorKind {
    NetworkError,
    AuthError,
    RateLimited,
    ServerError,
    StreamInterrupted,
    StreamTimeout,
    ContextOverflow,
    Other,
}

/// 工具调用错误类型
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolErrorKind {
    NotFound,
    InvalidArgs,
    ExecutionFailed,
    DeniedByUser,
    BlockedByHook,
    LoopDetected,
    SkillNotFound,
    SkillDisabled,
    SkillEmptyTemplate,
    /// Tool ran successfully (exit 0) but produced stderr output,
    /// suggesting a partial failure or misleading success.
    Warning,
    Other,
}

/// MCP 连接错误类型
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpErrorKind {
    NetworkError,
    AuthError,
    ServerError,
    ExecutionFailed,
    Timeout,
    Other,
}

/// CodingPlan 错误类型
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodingplanErrorKind {
    AuthError,
    AuthExpired,
    ExecutionFailed,
    NetworkError,
    ServerError,
    Other,
}

/// use_command 错误类型
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UseCommandErrorKind {
    ExecutionFailed,
    InvalidArgs,
    NotFound,
    Other,
}

/// MCP 传输方式
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpTransport {
    Stdio,
    Sse,
    StreamableHttp,
}

// ---------- Event payloads ----------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodingplanResult {
    Success,
    Fail,
}

// ---------- The Event enum (6 variants) ----------

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event_id", rename_all = "snake_case")]
pub enum Event {
    /// Fired when AtomCode is launched (interactive CLI, oneshot, or TUI entry).
    /// Not fired for --version / --help / telemetry subcommands.
    OpenAtomcode {
        /// True when --dangerously-skip-permissions / -y was passed,
        /// meaning all tool calls are auto-approved without prompting.
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        dangerously_skip_permissions: bool,
    },

    /// One LLM turn completed (success or failure).
    LlmChat {
        duration_ms: u32,
        tool_calls_count: u32,
        input_tokens: u32,
        output_tokens: u32,
        cached_tokens: u32,
        had_error: bool,
        context_window: u32,
        system_tokens: u32,
        tool_def_tokens: u32,
        tool_result_tokens: u32,
        message_tokens: u32,
        messages_count: u32,
        /// Error category (None when had_error=false or unclassifiable).
        #[serde(skip_serializing_if = "Option::is_none")]
        error_kind: Option<LlmErrorKind>,
        /// Error detail JSON (None when had_error=false).
        #[serde(skip_serializing_if = "Option::is_none")]
        error_data: Option<String>,
    },

    /// Single tool call result (success or failure).
    ToolCall {
        /// Tool name (e.g. "bash", "edit_file", "mcp__xx__yy").
        name: String,
        /// Whether the call succeeded.
        success: bool,
        /// Execution duration in ms (0 if not executed).
        duration_ms: u32,
        /// Error category (None when success=true).
        #[serde(skip_serializing_if = "Option::is_none")]
        error_kind: Option<ToolErrorKind>,
        /// Error/detail JSON (always present for diagnostics even on success).
        #[serde(skip_serializing_if = "Option::is_none")]
        error_data: Option<String>,
    },

    /// A slash command was executed in TUI or daemon.
    /// `type_` is the literal command name (without the leading /).
    UseCommand {
        #[serde(rename = "type")]
        type_: String,
        /// Whether the command succeeded (None for legacy events).
        #[serde(skip_serializing_if = "Option::is_none")]
        success: Option<bool>,
        /// Error category (None when success=true or legacy).
        #[serde(skip_serializing_if = "Option::is_none")]
        error_kind: Option<UseCommandErrorKind>,
        /// Error/detail JSON.
        #[serde(skip_serializing_if = "Option::is_none")]
        error_data: Option<String>,
    },

    /// MCP server connection attempt (success or failure).
    McpConnect {
        /// MCP server name.
        server_name: String,
        /// Transport type.
        transport: McpTransport,
        /// Whether the connection succeeded.
        success: bool,
        /// Connection duration in ms.
        #[serde(skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u32>,
        /// Error category (None when success=true).
        #[serde(skip_serializing_if = "Option::is_none")]
        error_kind: Option<McpErrorKind>,
        /// Error/detail JSON.
        #[serde(skip_serializing_if = "Option::is_none")]
        error_data: Option<String>,
    },

    /// OAuth login completed successfully.
    LoginSuccess {
        /// Invite code from pending_invite (None for organic installs or legacy clients).
        #[serde(skip_serializing_if = "Option::is_none")]
        invite_code: Option<String>,
        /// Install UUID from pending_invite (None for organic installs or legacy clients).
        #[serde(skip_serializing_if = "Option::is_none")]
        install_uuid: Option<Uuid>,
    },

    /// First launch after install — fired once when a pending_invite file is detected.
    InstallCompleted {
        invite_code: String,
        install_uuid: Uuid,
    },

    /// A coding plan run finished.
    TakeCodingplan {
        #[serde(rename = "type")]
        type_: CodingplanResult,
        /// Error category (None when type_=Success).
        #[serde(skip_serializing_if = "Option::is_none")]
        error_kind: Option<CodingplanErrorKind>,
        /// Error/detail JSON.
        #[serde(skip_serializing_if = "Option::is_none")]
        error_data: Option<String>,
    },

    /// Panic captured by global hook.
    Panic {
        location: String,
        message_head: String,
        thread: String,
        backtrace_top_5: Vec<String>,
        /// Fixed to "panic".
        #[serde(skip_serializing_if = "Option::is_none")]
        error_kind: Option<String>,
        /// Runtime context JSON (session_duration_secs, turns_completed, last_tool_name, last_event).
        #[serde(skip_serializing_if = "Option::is_none")]
        error_data: Option<String>,
    },

    /// Final event before user opts out via `atomcode telemetry disable`.
    /// Only fired if telemetry was currently enabled at the time of the command.
    TelemetryDisabled,

    /// Reserved variant. Will be fired (in a future PR) when an
    /// open-source build of AtomCode attempts to send a request to the
    /// AtomGit LLM gateway. Locking the wire-format `event_id` here
    /// keeps the firing-site PR small.
    CodingplanOfficialBuildRequired,
}

// ---------- Record (wire format) ----------

#[derive(Debug, Clone, Serialize)]
pub struct Record {
    #[serde(flatten)]
    pub envelope: Envelope,
    #[serde(flatten)]
    pub event: Event,
}

// ---------- Tests ----------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json;

    fn sample_envelope() -> Envelope {
        Envelope {
            device_id: Uuid::nil(),
            launch_id: Uuid::nil(),
            account_id: None,
            session_id: Uuid::nil(),
            turn_id: None,
            ts: 0,
            schema_version: 1,
            app_version: "0.0.0".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            locale: "en-US".into(),
            provider: None,
            provider_host: None,
            model: None,
            repo_origin: None,
            mode: None,
            surface: None,
        }
    }

    #[test]
    fn envelope_omits_none_fields() {
        let s = serde_json::to_string(&sample_envelope()).unwrap();
        assert!(!s.contains("account_id"));
        assert!(!s.contains("turn_id"));
        assert!(!s.contains("provider"));
        assert!(!s.contains("repo_origin"));
        assert!(!s.contains("mode"));
    }

    #[test]
    fn envelope_carries_session_mode() {
        let mut env = sample_envelope();
        env.mode = Some(SessionMode::Headless);
        let v: serde_json::Value = serde_json::to_value(&env).unwrap();
        assert_eq!(v["mode"], "headless");

        env.mode = Some(SessionMode::Tui);
        let v: serde_json::Value = serde_json::to_value(&env).unwrap();
        assert_eq!(v["mode"], "tui");
    }

    #[test]
    fn envelope_carries_surface() {
        let mut env = sample_envelope();
        // None → field omitted entirely.
        let s = serde_json::to_string(&env).unwrap();
        assert!(
            !s.contains("surface"),
            "a None surface must be omitted; got {s}"
        );
        // Some → serialized as the raw tag.
        env.surface = Some("code_review".into());
        let v: serde_json::Value = serde_json::to_value(&env).unwrap();
        assert_eq!(v["surface"], "code_review");
    }

    #[test]
    fn record_flattens_envelope_and_event() {
        let r = Record {
            envelope: sample_envelope(),
            event: Event::OpenAtomcode {
                dangerously_skip_permissions: false,
            },
        };
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["event_id"], "open_atomcode");
        assert_eq!(v["schema_version"], 1);
        // Envelope flatten: device_id must be at the top level.
        assert!(v.get("device_id").is_some());
        // dangerously_skip_permissions=false is skipped by skip_serializing_if.
        assert!(v.get("dangerously_skip_permissions").is_none());
    }

    #[test]
    fn open_atomcode_includes_dangerously_skip_permissions_when_true() {
        let r = Record {
            envelope: sample_envelope(),
            event: Event::OpenAtomcode {
                dangerously_skip_permissions: true,
            },
        };
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["event_id"], "open_atomcode");
        assert_eq!(v["dangerously_skip_permissions"], true);
    }

    #[test]
    fn use_command_serializes_type_field() {
        let r = Record {
            envelope: sample_envelope(),
            event: Event::UseCommand {
                type_: "compact".into(),
                success: None,
                error_kind: None,
                error_data: None,
            },
        };
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["event_id"], "use_command");
        assert_eq!(v["type"], "compact");
        // skip_serializing_if: None fields must not appear
        assert!(v.get("success").is_none());
        assert!(v.get("error_kind").is_none());
        assert!(v.get("error_data").is_none());
    }

    #[test]
    fn use_command_with_error_fields() {
        let r = Record {
            envelope: sample_envelope(),
            event: Event::UseCommand {
                type_: "reload".into(),
                success: Some(false),
                error_kind: Some(UseCommandErrorKind::ExecutionFailed),
                error_data: Some(r#"{"command":"reload","message":"parse error"}"#.into()),
            },
        };
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["event_id"], "use_command");
        assert_eq!(v["type"], "reload");
        assert_eq!(v["success"], false);
        assert_eq!(v["error_kind"], "execution_failed");
    }

    #[test]
    fn take_codingplan_serializes_success_fail() {
        let ok = Record {
            envelope: sample_envelope(),
            event: Event::TakeCodingplan {
                type_: CodingplanResult::Success,
                error_kind: None,
                error_data: None,
            },
        };
        let fail = Record {
            envelope: sample_envelope(),
            event: Event::TakeCodingplan {
                type_: CodingplanResult::Fail,
                error_kind: Some(CodingplanErrorKind::AuthError),
                error_data: Some(r#"{"step":"login"}"#.into()),
            },
        };
        let ov: serde_json::Value = serde_json::to_value(&ok).unwrap();
        let fv: serde_json::Value = serde_json::to_value(&fail).unwrap();
        assert_eq!(ov["event_id"], "take_codingplan");
        assert_eq!(ov["type"], "success");
        assert!(ov.get("error_kind").is_none());
        assert_eq!(fv["type"], "fail");
        assert_eq!(fv["error_kind"], "auth_error");
    }

    #[test]
    fn llm_chat_payload_shape() {
        let r = Record {
            envelope: sample_envelope(),
            event: Event::LlmChat {
                duration_ms: 100,
                tool_calls_count: 2,
                input_tokens: 500,
                output_tokens: 300,
                cached_tokens: 0,
                had_error: false,
                context_window: 200000,
                system_tokens: 100,
                tool_def_tokens: 200,
                tool_result_tokens: 0,
                message_tokens: 50,
                messages_count: 5,
                error_kind: None,
                error_data: None,
            },
        };
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["event_id"], "llm_chat");
        assert_eq!(v["duration_ms"], 100);
        assert_eq!(v["tool_calls_count"], 2);
        assert_eq!(v["had_error"], false);
        assert_eq!(v["context_window"], 200000);
        assert_eq!(v["system_tokens"], 100);
        assert_eq!(v["tool_def_tokens"], 200);
        assert_eq!(v["message_tokens"], 50);
        assert_eq!(v["messages_count"], 5);
        assert!(v.get("context_used").is_none());
        // skip_serializing_if: None error fields must not appear
        assert!(v.get("error_kind").is_none());
        assert!(v.get("error_data").is_none());
    }

    #[test]
    fn llm_chat_with_error() {
        let r = Record {
            envelope: sample_envelope(),
            event: Event::LlmChat {
                duration_ms: 5000,
                tool_calls_count: 0,
                input_tokens: 100,
                output_tokens: 0,
                cached_tokens: 0,
                had_error: true,
                context_window: 200000,
                system_tokens: 100,
                tool_def_tokens: 0,
                tool_result_tokens: 0,
                message_tokens: 0,
                messages_count: 1,
                error_kind: Some(LlmErrorKind::AuthError),
                error_data: Some(r#"{"status_code":401,"message":"Invalid API key"}"#.into()),
            },
        };
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["had_error"], true);
        assert_eq!(v["error_kind"], "auth_error");
        assert!(v["error_data"].is_string());
    }

    #[test]
    fn tool_call_success_omits_error_fields() {
        let r = Record {
            envelope: sample_envelope(),
            event: Event::ToolCall {
                name: "bash".into(),
                success: true,
                duration_ms: 150,
                error_kind: None,
                error_data: None,
            },
        };
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["event_id"], "tool_call");
        assert_eq!(v["name"], "bash");
        assert_eq!(v["success"], true);
        assert_eq!(v["duration_ms"], 150);
        assert!(v.get("error_kind").is_none());
        assert!(v.get("error_data").is_none());
    }

    #[test]
    fn tool_call_with_error() {
        let r = Record {
            envelope: sample_envelope(),
            event: Event::ToolCall {
                name: "edit_file".into(),
                success: false,
                duration_ms: 5,
                error_kind: Some(ToolErrorKind::DeniedByUser),
                error_data: Some(
                    serde_json::json!({
                        "tool_name": "edit_file",
                        "reason": "User rejected file write",
                        "resolution": "Confirm the edit when prompted"
                    })
                    .to_string(),
                ),
            },
        };
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["event_id"], "tool_call");
        assert_eq!(v["name"], "edit_file");
        assert_eq!(v["success"], false);
        assert_eq!(v["error_kind"], "denied_by_user");
        assert!(v["error_data"].is_string());
        // Verify the error_data JSON contains reason + resolution
        let ed: serde_json::Value =
            serde_json::from_str(v["error_data"].as_str().unwrap()).unwrap();
        assert_eq!(ed["reason"], "User rejected file write");
        assert_eq!(ed["resolution"], "Confirm the edit when prompted");
    }

    #[test]
    fn tool_call_warning_with_stderr() {
        let r = Record {
            envelope: sample_envelope(),
            event: Event::ToolCall {
                name: "bash".into(),
                success: true,
                duration_ms: 43,
                error_kind: Some(ToolErrorKind::Warning),
                error_data: Some(
                    serde_json::json!({
                        "tool_name": "bash",
                        "duration_ms": 43,
                        "args_summary": "bash(command=rm -rf /tmp/test.txt)",
                        "output_tail": "rm: /tmp/test.txt: No such file or directory\n[elapsed: 0.0s, exit: 0]",
                        "reason": "Command succeeded (exit 0) but produced stderr output",
                        "resolution": "Review stderr for potential issues; the command may not have had the intended effect",
                    }).to_string(),
                ),
            },
        };
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["event_id"], "tool_call");
        assert_eq!(v["success"], true);
        assert_eq!(v["error_kind"], "warning");
        let ed: serde_json::Value =
            serde_json::from_str(v["error_data"].as_str().unwrap()).unwrap();
        assert_eq!(
            ed["reason"],
            "Command succeeded (exit 0) but produced stderr output"
        );
        assert_eq!(
            ed["resolution"],
            "Review stderr for potential issues; the command may not have had the intended effect"
        );
    }

    #[test]
    fn mcp_connect_success_omits_error_fields() {
        let r = Record {
            envelope: sample_envelope(),
            event: Event::McpConnect {
                server_name: "github".into(),
                transport: McpTransport::Stdio,
                success: true,
                duration_ms: Some(320),
                error_kind: None,
                error_data: None,
            },
        };
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["event_id"], "mcp_connect");
        assert_eq!(v["server_name"], "github");
        assert_eq!(v["transport"], "stdio");
        assert_eq!(v["success"], true);
        assert_eq!(v["duration_ms"], 320);
        assert!(v.get("error_kind").is_none());
        assert!(v.get("error_data").is_none());
    }

    #[test]
    fn mcp_connect_with_error() {
        let r = Record {
            envelope: sample_envelope(),
            event: Event::McpConnect {
                server_name: "remote-api".into(),
                transport: McpTransport::Sse,
                success: false,
                duration_ms: Some(5000),
                error_kind: Some(McpErrorKind::Timeout),
                error_data: Some(
                    serde_json::json!({
                        "server_name": "remote-api",
                        "transport": "sse",
                        "reason": "Connection timed out after 5s",
                        "resolution": "Check MCP server URL and network connectivity"
                    })
                    .to_string(),
                ),
            },
        };
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["event_id"], "mcp_connect");
        assert_eq!(v["server_name"], "remote-api");
        assert_eq!(v["transport"], "sse");
        assert_eq!(v["success"], false);
        assert_eq!(v["error_kind"], "timeout");
        let ed: serde_json::Value =
            serde_json::from_str(v["error_data"].as_str().unwrap()).unwrap();
        assert_eq!(ed["reason"], "Connection timed out after 5s");
        assert_eq!(
            ed["resolution"],
            "Check MCP server URL and network connectivity"
        );
    }

    #[test]
    fn panic_without_error_fields() {
        let r = Record {
            envelope: sample_envelope(),
            event: Event::Panic {
                location: "src/main.rs:42".into(),
                message_head: "index out of bounds".into(),
                thread: "main".into(),
                backtrace_top_5: vec!["func_a".into(), "func_b".into()],
                error_kind: None,
                error_data: None,
            },
        };
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["event_id"], "panic");
        assert_eq!(v["location"], "src/main.rs:42");
        assert_eq!(v["message_head"], "index out of bounds");
        assert!(v.get("error_kind").is_none());
        assert!(v.get("error_data").is_none());
    }

    #[test]
    fn panic_with_error_context() {
        let r = Record {
            envelope: sample_envelope(),
            event: Event::Panic {
                location: "src/agent.rs:100".into(),
                message_head: "assertion failed".into(),
                thread: "tokio-runtime".into(),
                backtrace_top_5: vec![],
                error_kind: Some("panic".into()),
                error_data: Some(
                    serde_json::json!({
                        "session_duration_secs": 300,
                        "turns_completed": 5,
                        "last_tool_name": "bash",
                        "last_event": "llm_chat"
                    })
                    .to_string(),
                ),
            },
        };
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["event_id"], "panic");
        assert_eq!(v["error_kind"], "panic");
        let ed: serde_json::Value =
            serde_json::from_str(v["error_data"].as_str().unwrap()).unwrap();
        assert_eq!(ed["session_duration_secs"], 300);
        assert_eq!(ed["turns_completed"], 5);
    }

    #[test]
    fn all_variants_have_event_id_tag() {
        let cases = [
            Event::OpenAtomcode {
                dangerously_skip_permissions: false,
            },
            Event::LlmChat {
                duration_ms: 0,
                tool_calls_count: 0,
                input_tokens: 0,
                output_tokens: 0,
                cached_tokens: 0,
                had_error: false,
                context_window: 0,
                system_tokens: 0,
                tool_def_tokens: 0,
                tool_result_tokens: 0,
                message_tokens: 0,
                messages_count: 0,
                error_kind: None,
                error_data: None,
            },
            Event::ToolCall {
                name: "bash".into(),
                success: true,
                duration_ms: 0,
                error_kind: None,
                error_data: None,
            },
            Event::UseCommand {
                type_: "x".into(),
                success: None,
                error_kind: None,
                error_data: None,
            },
            Event::McpConnect {
                server_name: "test".into(),
                transport: McpTransport::Stdio,
                success: true,
                duration_ms: None,
                error_kind: None,
                error_data: None,
            },
            Event::LoginSuccess {
                invite_code: None,
                install_uuid: None,
            },
            Event::TakeCodingplan {
                type_: CodingplanResult::Success,
                error_kind: None,
                error_data: None,
            },
            Event::Panic {
                location: "x:1".into(),
                message_head: "".into(),
                thread: "main".into(),
                backtrace_top_5: vec![],
                error_kind: None,
                error_data: None,
            },
            Event::TelemetryDisabled,
        ];
        for e in &cases {
            let v = serde_json::to_value(e).unwrap();
            assert!(v.get("event_id").is_some(), "missing event_id: {:?}", e);
        }
        assert_eq!(cases.len(), 9);
    }

    #[test]
    fn telemetry_disabled_serializes_with_correct_event_id() {
        let r = Record {
            envelope: sample_envelope(),
            event: Event::TelemetryDisabled,
        };
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["event_id"], "telemetry_disabled");
    }

    #[test]
    fn session_mode_ide_serializes_as_ide() {
        assert_eq!(serde_json::to_string(&SessionMode::Ide).unwrap(), "\"ide\"");
    }

    #[test]
    fn session_mode_webui_serializes_as_webui() {
        assert_eq!(
            serde_json::to_string(&SessionMode::Webui).unwrap(),
            "\"webui\""
        );
    }

    #[test]
    fn session_mode_channel_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&SessionMode::Channel).unwrap(),
            "\"channel\""
        );
    }

    #[test]
    fn session_mode_atomcode_air_serializes_as_atomcode_desktop() {
        assert_eq!(
            serde_json::to_string(&SessionMode::AtomcodeAir).unwrap(),
            "\"atomcode_desktop\""
        );
    }
}

#[cfg(test)]
mod codingplan_required_event_tests {
    use super::*;

    #[test]
    fn codingplan_official_build_required_serialises_with_snake_case_event_id() {
        let e = Event::CodingplanOfficialBuildRequired;
        let v = serde_json::to_value(&e).expect("serialise");
        assert_eq!(
            v["event_id"], "codingplan_official_build_required",
            "got: {v}"
        );
    }
}
