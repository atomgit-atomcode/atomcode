//! Event and Envelope schema for AtomCode telemetry v2.
//!
//! The 7 events are: open_atomcode, llm_chat, use_command, login_success,
//! take_codingplan, panic, telemetry_disabled.
//!
//! Wire format: envelope fields + event-specific payload, both flattened
//! into one JSON object. Event variant is tagged via `event_id`.

use serde::Serialize;
use uuid::Uuid;

// ---------- SessionMode ----------

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    Headless,
    Tui,
    Ide,
    Vscode,
    AtomcodeAir,
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

// ---------- Event payloads ----------

#[derive(Debug, Clone, Copy, Serialize)]
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
    OpenAtomcode,

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
    },

    /// A slash command was executed in TUI.
    /// `type_` is the literal command name (without the leading /).
    UseCommand {
        #[serde(rename = "type")]
        type_: String,
    },

    /// OAuth login completed successfully.
    LoginSuccess,

    /// A coding plan run finished.
    TakeCodingplan {
        #[serde(rename = "type")]
        type_: CodingplanResult,
    },

    /// Panic captured by global hook.
    Panic {
        location: String,
        message_head: String,
        thread: String,
        backtrace_top_5: Vec<String>,
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
    fn record_flattens_envelope_and_event() {
        let r = Record {
            envelope: sample_envelope(),
            event: Event::OpenAtomcode,
        };
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["event_id"], "open_atomcode");
        assert_eq!(v["schema_version"], 1);
        // Envelope flatten: device_id must be at the top level.
        assert!(v.get("device_id").is_some());
    }

    #[test]
    fn use_command_serializes_type_field() {
        let r = Record {
            envelope: sample_envelope(),
            event: Event::UseCommand {
                type_: "compact".into(),
            },
        };
        let v: serde_json::Value = serde_json::to_value(&r).unwrap();
        assert_eq!(v["event_id"], "use_command");
        assert_eq!(v["type"], "compact");
    }

    #[test]
    fn take_codingplan_serializes_success_fail() {
        let ok = Record {
            envelope: sample_envelope(),
            event: Event::TakeCodingplan {
                type_: CodingplanResult::Success,
            },
        };
        let fail = Record {
            envelope: sample_envelope(),
            event: Event::TakeCodingplan {
                type_: CodingplanResult::Fail,
            },
        };
        let ov: serde_json::Value = serde_json::to_value(&ok).unwrap();
        let fv: serde_json::Value = serde_json::to_value(&fail).unwrap();
        assert_eq!(ov["event_id"], "take_codingplan");
        assert_eq!(ov["type"], "success");
        assert_eq!(fv["type"], "fail");
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
    }

    #[test]
    fn all_7_variants_have_event_id_tag() {
        let cases = [
            Event::OpenAtomcode,
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
            },
            Event::UseCommand { type_: "x".into() },
            Event::LoginSuccess,
            Event::TakeCodingplan {
                type_: CodingplanResult::Success,
            },
            Event::Panic {
                location: "x:1".into(),
                message_head: "".into(),
                thread: "main".into(),
                backtrace_top_5: vec![],
            },
            Event::TelemetryDisabled,
        ];
        for e in &cases {
            let v = serde_json::to_value(e).unwrap();
            assert!(v.get("event_id").is_some(), "missing event_id: {:?}", e);
        }
        assert_eq!(cases.len(), 7);
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
        assert_eq!(
            serde_json::to_string(&SessionMode::Ide).unwrap(),
            "\"ide\""
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
