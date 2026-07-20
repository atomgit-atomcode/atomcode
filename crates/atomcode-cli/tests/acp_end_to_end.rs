//! End-to-end integration test: an in-process ACP client drives the fully-wired
//! agent through `initialize → session/new → session/prompt` over a duplex
//! [`Channel`] (no subprocess, no network), backed by a scripted stub provider.
//!
//! Harness decision (Task 1 spike): the `agent-client-protocol` crate exposes
//! `Channel::duplex() -> (Channel, Channel)`, two endpoints wired to each other,
//! each implementing `ConnectTo<R>` for any role. We run the real agent
//! (`atomcode::acp::serve_over`) over one endpoint and a `Client.builder()` over
//! the other — the same handlers production uses, just over an in-memory pipe.
//!
//! The stub is `atomcode_kernel::testkit::MockProvider`, the kernel's own
//! scriptable `LlmProvider` (exported unconditionally — no feature flag). It is
//! injected via `AcpServeOptions.provider_factory`, so each session gets a
//! factory-built stub and the agent never touches the network. The fact that
//! `session/new` returns a sessionId at all proves the real
//! `prepare → assemble → spawn` pipeline ran with the injected stub.

use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::schema::v1::{
    ContentBlock, InitializeRequest, NewSessionRequest, PromptRequest, SessionNotification,
    TextContent,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{Channel, Client, ConnectionTo};
use atomcode::acp::engine::EngineConfig;
use atomcode::acp::{serve_over, AcpServeOptions};
use atomcode_coding::{CodingAgentConfig, CodingProviderFactory, ProviderBuildError};
use atomcode_kernel::provider::LlmProvider;
use atomcode_kernel::stream::{ProviderError, StreamEvent};
use atomcode_kernel::testkit::MockProvider;

struct StubProviderFactory(Arc<dyn LlmProvider>);

impl CodingProviderFactory for StubProviderFactory {
    fn build(
        &self,
        _config: &CodingAgentConfig,
        _session_id: Option<&str>,
    ) -> Result<Arc<dyn LlmProvider>, ProviderBuildError> {
        Ok(Arc::clone(&self.0))
    }
}

/// A dummy, non-routable engine config. The provider is injected, so none of
/// these reach the network — `base_url` is never dialed.
fn dummy_engine() -> EngineConfig {
    let mut config = atomcode_coding::CodingAgentConfig::new(
        "test-key",
        "http://127.0.0.1:1",
        "stub-model",
        ".",
    );
    config.context_window = 200_000;
    config.chat_options.max_tokens = Some(8192);
    EngineConfig::from_coding_config(config)
}

// `#[serial]`: both tests in this binary mutate the process-global `ATOMCODE_HOME`
// (each to its own tempdir); libtest runs them on parallel threads, so without
// serialization one test's `prepare()` could read the other's `set_var` value and
// write its snapshot into the wrong tempdir. Serializing them closes that race.
#[tokio::test]
#[serial_test::serial]
async fn initialize_new_prompt_streams_and_stops() {
    // Isolate global config (memory/hooks/MCP) so `prepare` is fast & hermetic:
    // an empty ATOMCODE_HOME means no global memory.md, no hooks.json, no MCP.
    let home = tempfile::tempdir().expect("home tempdir");
    std::env::set_var("ATOMCODE_HOME", home.path());
    // A clean working dir with no `.mcp.json` → no MCP servers spawned.
    let cwd = tempfile::tempdir().expect("cwd tempdir");

    // 1. Stub provider scripted for ONE user turn: text "hello", then a normal
    //    (content-bearing) completion. `Done { truncated:false }` after a
    //    TextDelta is a legitimate EndTurn stop (not the empty-200 retry case).
    let stub = MockProvider::new(vec![vec![
        StreamEvent::TextDelta("hello".into()),
        StreamEvent::Done { truncated: false },
    ]])
    .with_ctx_window(200_000);

    // 2. Build the agent over one duplex endpoint with the injected stub.
    let (agent_channel, client_channel) = Channel::duplex();
    let opts = AcpServeOptions {
        engine: Some(dummy_engine()),
        provider_factory: Some(Arc::new(StubProviderFactory(Arc::new(stub)))),
        auto_approve: false,
    };
    let agent_task = tokio::spawn(async move { serve_over(opts, agent_channel).await });

    // Collect every session/update notification the client receives.
    let updates: Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    let cwd_path = cwd.path().to_path_buf();
    let updates_for_handler = Arc::clone(&updates);

    // 3. In-process ACP CLIENT over the paired endpoint.
    let client_run = Client
        .builder()
        .on_receive_notification(
            move |notif: SessionNotification, _cx| {
                let updates = Arc::clone(&updates_for_handler);
                async move {
                    updates
                        .lock()
                        .unwrap()
                        .push(serde_json::to_value(&notif.update).unwrap());
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(client_channel, |conn: ConnectionTo<_>| async move {
            // initialize
            let init = conn
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            // session/new — capturing a sessionId proves prepare/assemble/spawn
            // ran with the INJECTED stub provider.
            let new = conn
                .send_request(NewSessionRequest::new(cwd_path))
                .block_task()
                .await?;
            let sid = new.session_id.clone();
            // session/prompt
            let prompt = conn
                .send_request(PromptRequest::new(
                    sid.clone(),
                    vec![ContentBlock::Text(TextContent::new("hi"))],
                ))
                .block_task()
                .await?;
            Ok((init, sid, prompt))
        });

    let (init, _sid, prompt) = tokio::time::timeout(Duration::from_secs(30), client_run)
        .await
        .expect("client interaction timed out")
        .expect("client run failed");

    // initialize: protocol echoed + image prompt capability advertised.
    let init_json = serde_json::to_value(&init).unwrap();
    assert_eq!(
        init.protocol_version,
        ProtocolVersion::V1,
        "protocol echoed"
    );
    assert_eq!(
        init_json["agentCapabilities"]["promptCapabilities"]["image"], true,
        "image prompt capability must be advertised: {init_json}"
    );

    // prompt: terminal end_turn stop reason.
    let prompt_json = serde_json::to_value(&prompt).unwrap();
    assert_eq!(
        prompt_json["stopReason"], "end_turn",
        "prompt must end with end_turn: {prompt_json}"
    );

    // streaming: an agent_message_chunk carrying exactly "hello" was received.
    let got = updates.lock().unwrap().clone();
    let hello = got
        .iter()
        .find(|u| u["sessionUpdate"] == "agent_message_chunk");
    let hello = hello.unwrap_or_else(|| panic!("no agent_message_chunk in updates: {got:?}"));
    assert_eq!(
        hello["content"]["text"], "hello",
        "streamed chunk text must be 'hello': {hello}"
    );

    // 5. Clean shutdown: connect_with already returned (client connection closed),
    //    which drops the client endpoint; the agent connection then ends. Abort
    //    the agent task defensively so the test process exits promptly.
    agent_task.abort();
}

/// Regression for FIX #1: the kernel emits a trailing `TurnComplete` AFTER an
/// `Error` event. The turn loop must DRAIN that `TurnComplete` (not return on the
/// `Error`), otherwise it stays buffered in the session's events channel and
/// poisons the NEXT prompt on the same session (the second prompt would read the
/// stale `TurnComplete` first and finish instantly with no streamed content).
///
/// Turn 1 scripts a mid-stream `StreamEvent::Error` (cleanly fails the turn → an
/// `AgentEvent::Error` followed by `TurnComplete{ProviderError}`). Turn 2 on the
/// SAME session scripts a normal `"hello"` + end_turn. The test asserts turn 2 is
/// NOT poisoned: it ends `end_turn` AND streams the "hello" chunk.
#[tokio::test]
#[serial_test::serial]
async fn error_turn_does_not_poison_next_prompt_on_same_session() {
    let home = tempfile::tempdir().expect("home tempdir");
    std::env::set_var("ATOMCODE_HOME", home.path());
    let cwd = tempfile::tempdir().expect("cwd tempdir");

    // Turn 1: a mid-stream provider error (non-retryable). Turn 2: a normal stop.
    let stub = MockProvider::new(vec![
        vec![StreamEvent::Error(ProviderError {
            retryable: false,
            message: "scripted mid-stream failure".into(),
            http_status: None,
            code: None,
            retry_after_secs: None,
        })],
        vec![
            StreamEvent::TextDelta("hello".into()),
            StreamEvent::Done { truncated: false },
        ],
    ])
    .with_ctx_window(200_000);

    let (agent_channel, client_channel) = Channel::duplex();
    let opts = AcpServeOptions {
        engine: Some(dummy_engine()),
        provider_factory: Some(Arc::new(StubProviderFactory(Arc::new(stub)))),
        auto_approve: false,
    };
    let agent_task = tokio::spawn(async move { serve_over(opts, agent_channel).await });

    // Collect notifications from the SECOND prompt to prove it streamed content.
    let updates: Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let cwd_path = cwd.path().to_path_buf();
    let updates_for_handler = Arc::clone(&updates);

    let client_run = Client
        .builder()
        .on_receive_notification(
            move |notif: SessionNotification, _cx| {
                let updates = Arc::clone(&updates_for_handler);
                async move {
                    updates
                        .lock()
                        .unwrap()
                        .push(serde_json::to_value(&notif.update).unwrap());
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .connect_with(client_channel, |conn: ConnectionTo<_>| async move {
            let _init = conn
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let new = conn
                .send_request(NewSessionRequest::new(cwd_path))
                .block_task()
                .await?;
            let sid = new.session_id.clone();
            // Prompt 1: expected to fail (the scripted mid-stream error). The ACP
            // server answers this with a JSON-RPC error; tolerate either form.
            let first = conn
                .send_request(PromptRequest::new(
                    sid.clone(),
                    vec![ContentBlock::Text(TextContent::new("first"))],
                ))
                .block_task()
                .await;
            // Prompt 2 on the SAME session: must succeed and stream "hello".
            let second = conn
                .send_request(PromptRequest::new(
                    sid.clone(),
                    vec![ContentBlock::Text(TextContent::new("second"))],
                ))
                .block_task()
                .await?;
            Ok((first.is_err(), second))
        });

    let (first_errored, second) = tokio::time::timeout(Duration::from_secs(30), client_run)
        .await
        .expect("client interaction timed out")
        .expect("client run failed");

    assert!(
        first_errored,
        "first prompt (scripted mid-stream error) should respond with a JSON-RPC error"
    );

    // The crux: prompt 2 is NOT poisoned by turn 1's trailing TurnComplete.
    let second_json = serde_json::to_value(&second).unwrap();
    assert_eq!(
        second_json["stopReason"], "end_turn",
        "second prompt must end with end_turn (not be poisoned by stale TurnComplete): {second_json}"
    );
    let got = updates.lock().unwrap().clone();
    let hello = got
        .iter()
        .find(|u| u["sessionUpdate"] == "agent_message_chunk");
    let hello =
        hello.unwrap_or_else(|| panic!("no agent_message_chunk from second prompt: {got:?}"));
    assert_eq!(
        hello["content"]["text"], "hello",
        "second prompt must stream 'hello': {hello}"
    );

    agent_task.abort();
}
