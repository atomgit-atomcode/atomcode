//! The wire format, pinned — one golden per event shape.
//!
//! # Why this exists
//!
//! What the dashboards consume is not the `Event` enum; it is the JSON a
//! `Record` serializes to. That JSON can change without a single call site
//! changing: a `#[serde(rename)]`, a `skip_serializing_if`, a field moved into
//! or out of the flattened envelope. None of those break a build, and none of
//! them break a test that asserts on the Rust value.
//!
//! So the assertion is on the bytes.
//!
//! # Where the values come from
//!
//! These are NOT a photograph of what this build happens to emit. Each case was
//! transcribed from the emitter that owned the event when the field set was
//! settled, so the golden is an independent statement of the contract:
//!
//! - `mcp_connect` — `git show f296e6e2^:crates/atomcode-core/src/mcp/registry.rs`
//!   (the retired core engine; `f296e6e2` moved MCP to `atomcode-capabilities`
//!   and dropped the emission, so this shape has no producer in the tree today).
//! - `use_command` — `atomcode-tuix/src/event_loop/commands.rs:1615` (success)
//!   and `:4127` (unknown command), plus `atomcode-daemon/src/lib.rs:3245`.
//! - `llm_chat`, `tool_call` — `atomcode-coding/src/telemetry.rs`.
//! - `panic` — `atomcode-cli/src/main.rs:4574`.
//! - `take_codingplan` — `atomcode-codingplan/src/setup.rs:630`.
//! - `login_success` — `atomcode-auth/src/oauth.rs:572`.
//!
//! # Changing a golden
//!
//! Edit the file and say why in the commit. There is deliberately no
//! `UPDATE_GOLDEN=1`: regenerating from the code under test turns the contract
//! into a self-portrait, which is the one thing this file is here to prevent.
//! A missing golden IS written on first run, so bootstrapping a newly added
//! event costs nothing — but an existing one is never overwritten.

use std::path::PathBuf;

use atomcode_telemetry::{
    CodingplanErrorKind, CodingplanResult, Envelope, Event, LlmErrorKind, McpErrorKind,
    McpTransport, Record, RepoHost, RepoOrigin, SessionMode, ToolErrorKind, UseCommandErrorKind,
};
use uuid::Uuid;

/// Every `event_id` the wire can carry.
///
/// The compiler guards half of this: [`stem`] matches exhaustively, so a new
/// variant cannot be added without an arm. This list guards the other half —
/// that the variant also got a golden — because an arm alone would let a new
/// event ship with no pinned shape.
const ALL_EVENT_IDS: &[&str] = &[
    "codingplan_official_build_required",
    "install_completed",
    "llm_chat",
    "login_success",
    "mcp_connect",
    "open_atomcode",
    "panic",
    "take_codingplan",
    "telemetry_disabled",
    "tool_call",
    "use_command",
];

/// The golden file stem for an event, and the reason a new variant breaks the
/// build here rather than shipping unpinned.
fn stem(event: &Event) -> &'static str {
    match event {
        Event::OpenAtomcode { .. } => "open_atomcode",
        Event::LlmChat { .. } => "llm_chat",
        Event::ToolCall { .. } => "tool_call",
        Event::UseCommand { .. } => "use_command",
        Event::McpConnect { .. } => "mcp_connect",
        Event::LoginSuccess { .. } => "login_success",
        Event::InstallCompleted { .. } => "install_completed",
        Event::TakeCodingplan { .. } => "take_codingplan",
        Event::Panic { .. } => "panic",
        Event::TelemetryDisabled => "telemetry_disabled",
        Event::CodingplanOfficialBuildRequired => "codingplan_official_build_required",
    }
}

// ---- envelopes -------------------------------------------------------------

/// What `Telemetry::in_memory` produces: nil ids, no attribution.
///
/// Matching the test sink matters — a capture test that diffs against these
/// goldens must not have to normalise fields this one already fixes.
fn envelope() -> Envelope {
    Envelope {
        device_id: Uuid::nil(),
        launch_id: Uuid::nil(),
        account_id: None,
        session_id: Uuid::nil(),
        turn_id: None,
        ts: 0,
        schema_version: atomcode_telemetry::SCHEMA_VERSION,
        app_version: "0.0.0".into(),
        os: "macos".into(),
        arch: "aarch64".into(),
        locale: "en-US".into(),
        provider: None,
        provider_host: None,
        model: None,
        repo_origin: None,
        mode: None,
        surface: None,
    }
}

/// Every optional field populated.
///
/// The minimal envelope cannot catch a rename on a field it omits, and the
/// omission itself is a contract: `skip_serializing_if` on `account_id` is why
/// an anonymous record has no such key at all rather than a null.
fn envelope_full() -> Envelope {
    Envelope {
        account_id: Some("42".into()),
        turn_id: Some(Uuid::nil()),
        provider: Some("openai".into()),
        provider_host: Some("api.openai.com".into()),
        model: Some("gpt-4o".into()),
        repo_origin: Some(RepoOrigin {
            host: RepoHost::Gitcode,
            has_git: true,
        }),
        mode: Some(SessionMode::Tui),
        surface: Some("code_review".into()),
        ..envelope()
    }
}

// ---- the cases -------------------------------------------------------------

/// Each pinned shape: the golden's file name, and the record it must render to.
///
/// Several events appear twice. The second one is always the failure shape,
/// because `error_kind` is what a dashboard splits on and it is absent from the
/// success record — a success-only golden would pin none of it.
fn cases() -> Vec<(String, Record)> {
    let mut out: Vec<(String, Record)> = Vec::new();
    let mut push = |suffix: &str, envelope: Envelope, event: Event| {
        let name = match suffix {
            "" => stem(&event).to_string(),
            s => format!("{}_{}", stem(&event), s),
        };
        out.push((name, Record { envelope, event }));
    };

    // The envelope's own contract, carried on the simplest event there is.
    push(
        "full_envelope",
        envelope_full(),
        Event::OpenAtomcode {
            dangerously_skip_permissions: true,
        },
    );
    push(
        "",
        envelope(),
        Event::OpenAtomcode {
            dangerously_skip_permissions: true,
        },
    );

    push(
        "",
        envelope(),
        Event::LlmChat {
            duration_ms: 1_234,
            tool_calls_count: 2,
            input_tokens: 900,
            output_tokens: 120,
            cached_tokens: 768,
            had_error: false,
            context_window: 128_000,
            system_tokens: 400,
            tool_def_tokens: 300,
            tool_result_tokens: 100,
            message_tokens: 100,
            messages_count: 6,
            error_kind: None,
            error_data: None,
        },
    );
    push(
        "error",
        envelope(),
        Event::LlmChat {
            duration_ms: 30_000,
            tool_calls_count: 0,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            had_error: true,
            context_window: 128_000,
            system_tokens: 0,
            tool_def_tokens: 0,
            tool_result_tokens: 0,
            message_tokens: 0,
            messages_count: 6,
            error_kind: Some(LlmErrorKind::NetworkError),
            error_data: Some(r#"{"message":"connection reset by peer"}"#.into()),
        },
    );

    push(
        "",
        envelope(),
        Event::ToolCall {
            name: "bash".into(),
            success: true,
            duration_ms: 42,
            error_kind: None,
            error_data: None,
        },
    );
    // `atomcode-coding/src/telemetry.rs:433` classifies a middleware block —
    // an approval the person denied — by the deterministic `blocked:` prefix.
    push(
        "denied",
        envelope(),
        Event::ToolCall {
            name: "write_file".into(),
            success: false,
            duration_ms: 0,
            error_kind: Some(ToolErrorKind::DeniedByUser),
            error_data: None,
        },
    );

    // tuix normalises through `canonical_command_name` and strips the leading
    // `/` before it reports, so `/new` is reported as `session`, not `new`.
    push(
        "",
        envelope(),
        Event::UseCommand {
            type_: "session".into(),
            success: Some(true),
            error_kind: None,
            error_data: None,
        },
    );
    push(
        "not_found",
        envelope(),
        Event::UseCommand {
            type_: "nope".into(),
            success: Some(false),
            error_kind: Some(UseCommandErrorKind::NotFound),
            error_data: Some(
                r#"{"command":"nope","duration_ms":0,"message":"Unknown command: nope"}"#.into(),
            ),
        },
    );

    push(
        "",
        envelope(),
        Event::McpConnect {
            server_name: "t".into(),
            transport: McpTransport::Stdio,
            success: true,
            duration_ms: Some(150),
            error_kind: None,
            error_data: Some(
                r#"{"config_source":"driver","duration_ms":150,"server_name":"t","tool_count":0,"transport":"stdio"}"#
                    .into(),
            ),
        },
    );
    push(
        "error",
        envelope(),
        Event::McpConnect {
            server_name: "t".into(),
            transport: McpTransport::Stdio,
            success: false,
            duration_ms: Some(20),
            error_kind: Some(McpErrorKind::ExecutionFailed),
            // `<CWD>` rather than the path itself: the reporter scrubs before
            // it truncates, which is the one place this build deliberately
            // sends less than the retired engine did. See
            // `atomcode-coding/src/telemetry.rs`, `McpTelemetry::safe_message`.
            error_data: Some(
                r#"{"config_source":"driver","duration_ms":20,"message":"Failed to spawn MCP server: <CWD>/no-such-server","server_name":"t","transport":"stdio"}"#
                    .into(),
            ),
        },
    );

    push(
        "",
        envelope(),
        Event::LoginSuccess {
            invite_code: Some("INVITE".into()),
            install_uuid: Some(Uuid::nil()),
        },
    );
    // An organic install has neither, and both keys must then be absent.
    push(
        "organic",
        envelope(),
        Event::LoginSuccess {
            invite_code: None,
            install_uuid: None,
        },
    );

    push(
        "",
        envelope(),
        Event::InstallCompleted {
            invite_code: "INVITE".into(),
            install_uuid: Uuid::nil(),
        },
    );

    push(
        "",
        envelope(),
        Event::TakeCodingplan {
            type_: CodingplanResult::Success,
            error_kind: None,
            error_data: None,
        },
    );
    push(
        "error",
        envelope(),
        Event::TakeCodingplan {
            type_: CodingplanResult::Fail,
            error_kind: Some(CodingplanErrorKind::AuthError),
            error_data: Some(r#"{"message":"Not logged in","step":"login"}"#.into()),
        },
    );

    push(
        "",
        envelope(),
        Event::Panic {
            location: "~/p/src/main.rs:10:5".into(),
            message_head: "index out of bounds".into(),
            thread: "main".into(),
            backtrace_top_5: vec!["core::panicking::panic_bounds_check".into()],
            error_kind: Some("panic".into()),
            error_data: Some(
                r#"{"last_event":null,"last_tool_name":null,"session_duration_secs":12,"turns_completed":null}"#
                    .into(),
            ),
        },
    );

    push("", envelope(), Event::TelemetryDisabled);
    push("", envelope(), Event::CodingplanOfficialBuildRequired);

    out
}

// ---- the checks ------------------------------------------------------------

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/wire")
}

#[test]
fn every_event_shape_renders_to_its_golden() {
    let dir = golden_dir();
    std::fs::create_dir_all(&dir).expect("golden dir");
    let mut wrote = Vec::new();
    let mut failed = Vec::new();

    for (name, record) in cases() {
        let rendered = serde_json::to_string_pretty(&serde_json::to_value(&record).unwrap())
            .expect("a record serializes");
        let path = dir.join(format!("{name}.json"));
        match std::fs::read_to_string(&path) {
            // Bootstrapping a newly added event writes the file; an existing
            // one is never touched (see this file's header).
            Err(_) => {
                std::fs::write(&path, format!("{rendered}\n")).expect("write golden");
                wrote.push(name);
            }
            Ok(stored) => {
                if stored.trim() != rendered.trim() {
                    failed.push(format!(
                        "--- {name} ---\nstored:\n{}\nrendered:\n{rendered}",
                        stored.trim()
                    ));
                }
            }
        }
    }

    assert!(
        wrote.is_empty(),
        "wrote {} new golden(s): {wrote:?} — review them against the emitter they \
         came from and commit them",
        wrote.len()
    );
    assert!(
        failed.is_empty(),
        "the wire format changed for {} shape(s). This is a downstream-visible \
         change: fix the code, or edit the golden and say why in the commit.\n\n{}",
        failed.len(),
        failed.join("\n\n")
    );
}

#[test]
fn every_event_variant_has_at_least_one_golden() {
    let mut covered: Vec<String> = cases()
        .iter()
        .map(|(_, record)| {
            serde_json::to_value(&record.event).unwrap()["event_id"]
                .as_str()
                .expect("event_id is a string")
                .to_string()
        })
        .collect();
    covered.sort();
    covered.dedup();

    let expected: Vec<String> = ALL_EVENT_IDS.iter().map(|s| s.to_string()).collect();
    assert_eq!(
        covered, expected,
        "every event the wire can carry needs a pinned shape; add a case and \
         list its event_id in ALL_EVENT_IDS"
    );
}

/// The tag `serde` writes must be the stem this file files the golden under.
///
/// Both are hand-maintained, and a rename that updated only one of them would
/// leave a golden pinning a shape under the wrong name — still green, and
/// pinning nothing that the dashboard would recognise.
#[test]
fn the_serialized_tag_matches_the_golden_stem() {
    for (name, record) in cases() {
        let id = serde_json::to_value(&record.event).unwrap()["event_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            name == id || name.starts_with(&format!("{id}_")),
            "golden `{name}` holds an event whose event_id is `{id}`"
        );
        assert_eq!(stem(&record.event), id, "stem() disagrees with serde");
    }
}
