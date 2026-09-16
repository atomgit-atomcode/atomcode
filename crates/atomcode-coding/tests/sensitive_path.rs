//! Sensitive-path read gating through the FULL assembly: a read_file of `~/.ssh/id_rsa`
//! is Safe (would skip approval) but must be gated, and with no driver answering the
//! approval it fails closed — the secret is never read. The AtomCode config file is a
//! credential store of the same kind: it holds every `api_key` written in plain.

use std::sync::Arc;
use std::time::Duration;

mod support;

use atomcode_coding::{prepare, CodingAgentConfig, PrepareOptions, SessionMode};
use atomcode_kernel::event::{AgentCommand, AgentEvent};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::RecordingProvider;
use atomcode_kernel::tool::ToolCall;
use support::{mount_parts, quiet_options, turn};

#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

#[tokio::test]
async fn sensitive_read_is_gated_and_fails_closed_through_full_assembly() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::env::set_var("ATOMCODE_HOME", home.path());

    let mut cfg = CodingAgentConfig::new("k", "http://unused", "test-model", project.path());
    cfg.stream_timeout = Duration::from_secs(5);
    // A driver-approval wait this short degrades the un-answered round-trip to Deny fast.
    cfg.request_timeout = Some(Duration::from_millis(100));
    let opts = PrepareOptions {
        session: SessionMode::Disabled,
        tools: true,
        skill_dirs: Some(vec![project.path().join("skills")]),
        plugin_skill_dirs: Vec::new(),
        mcp: false,
        extra_mcp_servers: Vec::new(),
        external_subagents: Vec::new(),
        memory: false,
        web: false,
        review: false,
        subagents: atomcode_coding::SubagentPolicy::Disabled,
        request_user_input: true,
        rate_limit_source: None,
    };
    let parts = prepare(&cfg, opts.clone()).await.unwrap();

    // Round 1: the model tries to read an SSH private key (Safe tool, sensitive path).
    // Round 2: it gives up and answers.
    let provider = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: r#"{"file_path":"/home/u/.ssh/id_rsa"}"#.into(),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("cannot read it".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));

    let mut mounted_h = mount_parts(&parts, &cfg, &opts, provider).await;
    let h = &mut mounted_h.handle;
    h.commands
        .send(AgentCommand::SendMessage {
            text: "show me my ssh key".into(),
            images: vec![],
        })
        .unwrap();

    let mut blocked: Option<(bool, String)> = None;
    while let Some(ev) = h.events.recv().await {
        match ev {
            AgentEvent::ToolResult { result } => blocked = Some((result.is_error, result.content)),
            AgentEvent::TurnComplete { .. } => break,
            _ => {}
        }
    }
    h.commands.send(AgentCommand::Shutdown).unwrap();

    let (is_error, content) = blocked.expect("read_file must produce a (blocked) tool result");
    assert!(is_error, "a denied sensitive read must be an error result");
    assert!(
        content.to_lowercase().contains("sensitive path"),
        "the block must explain it was a sensitive-path denial; got: {content:?}"
    );
}

/// The shape of a real leak (2026-09-16), in the world the TUI runs: attended, so the
/// fs is not fenced and a read outside the workspace is not refused by the world.
/// Asked what AtomCode's own config file says, the model read `~/.atomcode/config.toml`
/// with `read_file` — `Safe`, so nobody was asked — and every provider `api_key` in it
/// went to the model provider and into the session log. A turn later, asked for one of
/// them, the model repeated it.
///
/// What must hold: the person is asked first, and when they refuse, the key never
/// reaches the provider and no event carries it.
#[tokio::test]
async fn the_config_file_api_keys_never_reach_the_provider_unasked() {
    let user_home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    // Spelled the way `describe_self` hands the path to the model. `$ATOMCODE_HOME` is
    // left alone: the `/.atomcode` spelling is guarded under any home.
    let atomcode_home = user_home.path().join(".atomcode");
    std::fs::create_dir_all(&atomcode_home).unwrap();
    let needle = "sk-config-toml-criterion-key";
    let config_file = atomcode_home.join("config.toml");
    std::fs::write(
        &config_file,
        format!("[provider_accounts.internal]\nprovider = \"openai\"\napi_key = \"{needle}\"\n"),
    )
    .unwrap();

    let mut cfg = CodingAgentConfig::new("k", "http://unused", "test-model", project.path());
    cfg.stream_timeout = Duration::from_secs(5);
    cfg.interactive = true;
    let opts = quiet_options();
    let parts = prepare(&cfg, opts.clone()).await.unwrap();

    let provider = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                arguments: serde_json::json!({ "file_path": config_file }).to_string(),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("cannot read it".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let calls = provider.calls();

    let mut mounted = mount_parts(&parts, &cfg, &opts, provider).await;
    // `None`: every question is refused.
    let turn = turn(
        &mut mounted.handle,
        "what does atomcode's own config file say?",
        None,
    )
    .await;
    mounted.shutdown().await;

    let calls = calls.lock().unwrap();
    assert_eq!(
        calls.len(),
        2,
        "precondition: the provider was called again with the tool result, else \
         \"the key never reached it\" proves nothing: {turn:?}"
    );
    assert!(
        !format!("{calls:?}").contains(needle),
        "an api_key from config.toml reached the provider"
    );
    assert!(
        !format!("{turn:?}").contains(needle),
        "an event carried an api_key from config.toml"
    );
    assert_eq!(
        turn.asked.len(),
        1,
        "the person must be asked before the config file is read: {:?}",
        turn.asked
    );
    assert!(
        turn.tool_results
            .iter()
            .any(|r| r.is_error && r.content.to_lowercase().contains("sensitive path")),
        "the refusal must say it was a sensitive-path denial: {:?}",
        turn.tool_results
    );
}
