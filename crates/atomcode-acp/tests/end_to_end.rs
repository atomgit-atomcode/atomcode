//! End-to-end integration test: an in-process ACP client drives the fully-wired
//! agent through `initialize → session/new → session/prompt` over a duplex
//! [`Channel`] (no subprocess, no network), backed by a scripted stub provider.
//!
//! Harness decision (Task 1 spike): the `agent-client-protocol` crate exposes
//! `Channel::duplex() -> (Channel, Channel)`, two endpoints wired to each other,
//! each implementing `ConnectTo<R>` for any role. We run the real agent
//! (`atomcode_acp::serve_over`) over one endpoint and a `Client.builder()` over
//! the other — the same handlers production uses, just over an in-memory pipe.
//!
//! The stub is `atomcode_kernel::testkit::MockProvider`, the kernel's own
//! scriptable `LlmProvider` (exported unconditionally — no feature flag). It is
//! injected via `AcpServeOptions.provider`, so `engine::spawn_session` skips
//! `build_provider` entirely: the agent never touches the network. The fact that
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
use atomcode_acp::engine::EngineConfig;
use atomcode_acp::{serve_over, AcpServeOptions};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::MockProvider;

/// A dummy, non-routable engine config. The provider is injected, so none of
/// these reach the network — `base_url` is never dialed.
fn dummy_engine() -> EngineConfig {
    EngineConfig {
        api_key: "test-key".into(),
        base_url: "http://127.0.0.1:1".into(),
        model: "stub-model".into(),
        provider_type: "openai".into(),
        context_window: 200_000,
        max_tokens: Some(8192),
    }
}

#[tokio::test]
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
        provider: Some(Arc::new(stub)),
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
    assert_eq!(init.protocol_version, ProtocolVersion::V1, "protocol echoed");
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
    let hello = got.iter().find(|u| u["sessionUpdate"] == "agent_message_chunk");
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
