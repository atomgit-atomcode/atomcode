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
    CloseSessionRequest, ContentBlock, CreateElicitationRequest, CreateElicitationResponse,
    DeleteSessionRequest, ElicitationAcceptAction, ElicitationAction, ElicitationCapabilities,
    ElicitationContentValue, ElicitationFormCapabilities, InitializeRequest, ListSessionsRequest,
    NewSessionRequest, PromptRequest, ResumeSessionRequest, SessionConfigOption,
    SessionConfigOptionCategory, SessionConfigOptionValue, SessionConfigSelectOption, SessionId,
    SessionNotification, SetSessionConfigOptionRequest, TextContent,
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
        ..Default::default()
    };
    let agent_task = tokio::spawn(async move { serve_over(opts, agent_channel).await });

    // Collect every session/update notification the client receives.
    let updates: Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    let cwd_path = cwd.path().to_path_buf();
    let updates_for_handler = Arc::clone(&updates);
    // Clone for the move-closure below; the original stays available for the
    // post-flight assertions (session/list cwd check).
    let cwd_path_in_client = cwd_path.clone();

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
                .send_request(NewSessionRequest::new(cwd_path_in_client.clone()))
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
            // session/list: exactly the one live session, with its cwd.
            let listed = conn
                .send_request(ListSessionsRequest::new())
                .block_task()
                .await?;
            // session/close: teardown the session. The persisted history stays
            // (close releases resources; delete removes history).
            let closed = conn
                .send_request(CloseSessionRequest::new(sid.clone()))
                .block_task()
                .await?;
            // session/list after close: the persisted record is still history.
            let after_close = conn
                .send_request(ListSessionsRequest::new())
                .block_task()
                .await?;
            // session/delete: removes the persisted record.
            let deleted = conn
                .send_request(DeleteSessionRequest::new(sid.clone()))
                .block_task()
                .await?;
            // session/list after delete: history is empty again.
            let after_delete = conn
                .send_request(ListSessionsRequest::new())
                .block_task()
                .await?;
            Ok((
                init,
                sid,
                prompt,
                listed,
                closed,
                after_close,
                deleted,
                after_delete,
            ))
        });

    let (init, sid, prompt, listed, closed, after_close, deleted, after_delete) =
        tokio::time::timeout(Duration::from_secs(30), client_run)
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

    // auto-title: the first real user prompt derives a display title and is
    // broadcast once via the stable `session_info_update` notification.
    // The title comes from the USER prompt ("hi"), not the agent's reply.
    let info = got
        .iter()
        .find(|u| u["sessionUpdate"] == "session_info_update")
        .unwrap_or_else(|| panic!("no session_info_update in updates: {got:?}"));
    assert_eq!(
        info["title"], "hi",
        "session_info_update must carry the derived title: {info}"
    );
    assert_eq!(
        got.iter()
            .filter(|u| u["sessionUpdate"] == "session_info_update")
            .count(),
        1,
        "session_info_update must be sent exactly once per session: {got:?}"
    );

    // session lifecycle: list shows the live session with its cwd; close tears
    // it down but keeps the persisted history; delete removes it from history.
    assert_eq!(
        listed.sessions.len(),
        1,
        "one live session after session/new"
    );
    let listed_sid = listed.sessions[0].session_id.clone();
    assert_eq!(
        listed_sid, sid,
        "listed session matches the created session"
    );
    assert_eq!(
        listed.sessions[0].cwd, cwd_path,
        "listed session carries its cwd"
    );
    serde_json::to_value(&closed).unwrap(); // close response is an empty object
    assert_eq!(
        after_close.sessions.len(),
        1,
        "close releases the runtime but the session stays in history"
    );
    assert_eq!(
        after_close.sessions[0].session_id, sid,
        "persisted history carries the same wire id"
    );
    serde_json::to_value(&deleted).unwrap(); // delete response is an empty object
    assert!(
        after_delete.sessions.is_empty(),
        "delete removes the session from history"
    );

    // 5. Clean shutdown: connect_with already returned (client connection closed),
    //    which drops the client endpoint; the agent connection then ends. Abort
    //    the agent task defensively so the test process exits promptly.
    agent_task.abort();
}

/// Persisted conversation lifecycle: create → prompt → close → list shows the
/// persisted history → `session/resume` reconnects to the SAME session (no
/// replay, per v1) → the next prompt continues on the restored context →
/// `session/delete` removes it from history. Error cases: unknown session ids,
/// non-ACP ids, and cwd mismatches all fail closed with JSON-RPC errors.
#[tokio::test]
#[serial_test::serial]
async fn resume_reconnects_to_persisted_session() {
    let home = tempfile::tempdir().expect("home tempdir");
    std::env::set_var("ATOMCODE_HOME", home.path());
    let cwd = tempfile::tempdir().expect("cwd tempdir");

    // Two scripted turns: turn 1 before close, turn 2 after resume.
    let stub = MockProvider::new(vec![
        vec![
            StreamEvent::TextDelta("first".into()),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("second".into()),
            StreamEvent::Done { truncated: false },
        ],
    ])
    .with_ctx_window(200_000);

    let (agent_channel, client_channel) = Channel::duplex();
    let opts = AcpServeOptions {
        engine: Some(dummy_engine()),
        provider_factory: Some(Arc::new(StubProviderFactory(Arc::new(stub)))),
        auto_approve: false,
        ..Default::default()
    };
    let agent_task = tokio::spawn(async move { serve_over(opts, agent_channel).await });

    let updates: Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let updates_for_handler = Arc::clone(&updates);
    let cwd_path = cwd.path().to_path_buf();
    let wrong_cwd = tempfile::tempdir().expect("wrong cwd tempdir");
    let wrong_cwd_path = wrong_cwd.path().to_path_buf();

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
        .connect_with(client_channel, move |conn: ConnectionTo<_>| async move {
            conn.send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            let new = conn
                .send_request(NewSessionRequest::new(cwd_path.clone()))
                .block_task()
                .await?;
            let sid = new.session_id.clone();

            // Turn 1 (before close).
            let first = conn
                .send_request(PromptRequest::new(
                    sid.clone(),
                    vec![ContentBlock::Text(TextContent::new("one"))],
                ))
                .block_task()
                .await?;
            let first_json = serde_json::to_value(&first).unwrap();
            assert_eq!(first_json["stopReason"], "end_turn");

            conn.send_request(CloseSessionRequest::new(sid.clone()))
                .block_task()
                .await?;

            // The closed session stays in history with the same wire id.
            let listed = conn
                .send_request(ListSessionsRequest::new())
                .block_task()
                .await?;
            assert_eq!(listed.sessions.len(), 1);
            assert_eq!(listed.sessions[0].session_id, sid);

            // resume: silent restore, `{}` (+ modes/config state).
            let resumed = conn
                .send_request(ResumeSessionRequest::new(sid.clone(), cwd_path.clone()))
                .block_task()
                .await?;
            serde_json::to_value(&resumed).unwrap();

            // Turn 2 (after resume) — proves the live session was rebuilt.
            let second = conn
                .send_request(PromptRequest::new(
                    sid.clone(),
                    vec![ContentBlock::Text(TextContent::new("two"))],
                ))
                .block_task()
                .await?;
            let second_json = serde_json::to_value(&second).unwrap();
            assert_eq!(second_json["stopReason"], "end_turn");

            // Error cases fail closed (no silent fresh):
            // - unknown session id;
            let unknown = conn
                .send_request(ResumeSessionRequest::new(
                    SessionId::new("acp-definitely-missing"),
                    cwd_path.clone(),
                ))
                .block_task()
                .await;
            assert!(unknown.is_err(), "unknown session must error");
            // - cwd mismatch;
            let mismatch = conn
                .send_request(ResumeSessionRequest::new(sid.clone(), wrong_cwd_path))
                .block_task()
                .await;
            assert!(mismatch.is_err(), "cwd mismatch must error");
            // - resuming the LIVE session is a lease conflict, not a takeover.
            let live = conn
                .send_request(ResumeSessionRequest::new(sid.clone(), cwd_path.clone()))
                .block_task()
                .await;
            assert!(live.is_err(), "resuming a live session must error");

            // delete removes the history.
            conn.send_request(DeleteSessionRequest::new(sid.clone()))
                .block_task()
                .await?;
            let after_delete = conn
                .send_request(ListSessionsRequest::new())
                .block_task()
                .await?;
            assert!(after_delete.sessions.is_empty());
            Ok(())
        });

    tokio::time::timeout(Duration::from_secs(30), client_run)
        .await
        .expect("resume client run timed out")
        .expect("resume client run failed");

    let got = updates.lock().unwrap().clone();
    let texts: Vec<&str> = got
        .iter()
        .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|u| u["content"]["text"].as_str())
        .collect();
    assert!(
        texts.iter().any(|t| *t == "first"),
        "turn 1 streamed before close: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| *t == "second"),
        "turn 2 streamed after resume: {texts:?}"
    );

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
        ..Default::default()
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

/// Draft v2 end-to-end: a `Client.v2()` drives the SAME wired agent (the
/// protocol router negotiates v2 on this connection) through initialize →
/// session/new → session/prompt. The v2 lifecycle is exercised: `session/prompt`
/// acks immediately with `{}`, then `state_update` notifications stream
/// `running` → chunks → `idle` with `end_turn`.
#[tokio::test]
#[serial_test::serial]
async fn v2_client_negotiates_and_runs_prompt_lifecycle() {
    let home = tempfile::tempdir().expect("home tempdir");
    std::env::set_var("ATOMCODE_HOME", home.path());
    let cwd = tempfile::tempdir().expect("cwd tempdir");

    let stub = MockProvider::new(vec![vec![
        StreamEvent::TextDelta("hello-v2".into()),
        StreamEvent::Done { truncated: false },
    ]])
    .with_ctx_window(200_000);

    let (agent_channel, client_channel) = Channel::duplex();
    let opts = AcpServeOptions {
        engine: Some(dummy_engine()),
        provider_factory: Some(Arc::new(StubProviderFactory(Arc::new(stub)))),
        auto_approve: false,
        ..Default::default()
    };
    let agent_task = tokio::spawn(async move { serve_over(opts, agent_channel).await });

    let updates: Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let updates_for_handler = Arc::clone(&updates);
    // Clone for the move-closure below; the original stays available for the
    // post-flight assertions.
    let updates_in_client = Arc::clone(&updates);
    let cwd_path = cwd.path().to_path_buf();

    use agent_client_protocol::schema::v2::{
        CloseSessionRequest as V2CloseSessionRequest, ContentBlock as V2ContentBlock,
        InitializeRequest as V2InitializeRequest, NewSessionRequest as V2NewSessionRequest,
        OtherReplayFrom, PromptRequest as V2PromptRequest, ReplayFrom, ReplayFromStart,
        ResumeSessionRequest as V2ResumeSessionRequest, TextContent as V2TextContent,
        UpdateSessionNotification,
    };

    let client_run = Client
        .v2()
        .on_receive_notification(
            move |notif: UpdateSessionNotification, _cx| {
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
        .connect_with(
            client_channel,
            move |cx: ConnectionTo<agent_client_protocol::Agent>| async move {
                // initialize negotiates v2.
                let init = cx
                    .send_request(V2InitializeRequest::new(
                        ProtocolVersion::V2,
                        agent_client_protocol::schema::v2::Implementation::new(
                            "acp-test-client",
                            "1.0",
                        ),
                    ))
                    .block_task()
                    .await?;
                assert_eq!(init.protocol_version, ProtocolVersion::V2, "echo v2");

                let new = cx
                    .send_request(V2NewSessionRequest::new(cwd_path.clone()))
                    .block_task()
                    .await?;
                let sid = new.session_id.clone();

                // prompt: the ACK is immediate (`{}`); completion is async via updates.
                let prompt = cx
                    .send_request(V2PromptRequest::new(
                        sid.clone(),
                        vec![V2ContentBlock::Text(V2TextContent::new("hi"))],
                    ))
                    .block_task()
                    .await?;
                serde_json::to_value(&prompt).unwrap(); // empty ack object

                // Wait for the idle state_update with end_turn.
                tokio::time::timeout(Duration::from_secs(15), async {
                    loop {
                        let got = updates_in_client.lock().unwrap().clone();
                        if let Some(idle) = got
                            .iter()
                            .find(|u| u["sessionUpdate"] == "state_update" && u["state"] == "idle")
                        {
                            break idle.clone();
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("timed out waiting for v2 idle state update");

                // close the v2 session.
                let _closed = cx
                    .send_request(V2CloseSessionRequest::new(sid.clone()))
                    .block_task()
                    .await?;

                // resume WITH `replayFrom: start`: the persisted conversation
                // is replayed as full-content `user_message` / `agent_message`
                // updates BEFORE the `{}` response.
                let replay = cx
                    .send_request(
                        V2ResumeSessionRequest::new(sid.clone(), cwd_path.clone())
                            .replay_from(ReplayFrom::Start(ReplayFromStart::default())),
                    )
                    .block_task()
                    .await?;
                serde_json::to_value(&replay).unwrap();

                // close again to free the lease, then silent restore still works.
                let _closed_again = cx
                    .send_request(V2CloseSessionRequest::new(sid.clone()))
                    .block_task()
                    .await?;
                let resumed = cx
                    .send_request(V2ResumeSessionRequest::new(sid.clone(), cwd_path.clone()))
                    .block_task()
                    .await?;
                serde_json::to_value(&resumed).unwrap();

                // An unknown replay cursor (e.g. a future "checkpoint") must be
                // REJECTED before any restore side effect, not treated as
                // "replay from start".
                let unknown_cursor = cx
                    .send_request(
                        V2ResumeSessionRequest::new(sid.clone(), cwd_path.clone()).replay_from(
                            ReplayFrom::Other(OtherReplayFrom::new(
                                "checkpoint",
                                Default::default(),
                            )),
                        ),
                    )
                    .block_task()
                    .await;
                assert!(
                    unknown_cursor.is_err(),
                    "unknown replayFrom cursor must be rejected"
                );
                Ok(())
            },
        );

    tokio::time::timeout(Duration::from_secs(30), client_run)
        .await
        .expect("v2 client run timed out")
        .expect("v2 client run failed");

    let got = updates.lock().unwrap().clone();
    let running = got
        .iter()
        .find(|u| u["sessionUpdate"] == "state_update" && u["state"] == "running");
    assert!(running.is_some(), "running state_update seen: {got:?}");
    let hello = got
        .iter()
        .find(|u| u["sessionUpdate"] == "agent_message_chunk");
    assert_eq!(
        hello.and_then(|u| u["content"]["text"].as_str()),
        Some("hello-v2")
    );
    let idle = got
        .iter()
        .find(|u| u["sessionUpdate"] == "state_update" && u["state"] == "idle")
        .expect("idle state_update seen");
    assert_eq!(idle["stopReason"], "end_turn");

    // The `replayFrom: start` resume must have replayed the persisted
    // conversation as full-content message updates before the `{}` response.
    let replay_user = got.iter().find(|u| u["sessionUpdate"] == "user_message");
    assert_eq!(
        replay_user.and_then(|u| u["content"][0]["text"].as_str()),
        Some("hi"),
        "replay must include the persisted user message: {got:?}"
    );
    let replay_agent = got.iter().find(|u| u["sessionUpdate"] == "agent_message");
    assert_eq!(
        replay_agent.and_then(|u| u["content"][0]["text"].as_str()),
        Some("hello-v2"),
        "replay must include the persisted agent reply: {got:?}"
    );

    agent_task.abort();
}

/// Session config options over the wired agent: `session/new` advertises the
/// execution modes plus the `mode` / `reasoning_effort` config options, and
/// `session/set_config_option` switches the runtime mode and reloads the
/// provider for the effort — both on a real kernel session.
#[tokio::test]
#[serial_test::serial]
async fn session_config_options_mode_and_effort() {
    let home = tempfile::tempdir().expect("home tempdir");
    std::env::set_var("ATOMCODE_HOME", home.path());
    let cwd = tempfile::tempdir().expect("cwd tempdir");

    // One user turn is enough: both config switches happen before the prompt.
    let stub = MockProvider::new(vec![vec![
        StreamEvent::TextDelta("hello".into()),
        StreamEvent::Done { truncated: false },
    ]])
    .with_ctx_window(200_000);

    let effort_resolver: Arc<atomcode::acp::SessionModelResolver> = Arc::new(|effort: &str| {
        use atomcode_kernel::provider::ReasoningEffort;
        let mut cfg = atomcode_coding::CodingAgentConfig::new(
            "test-key",
            "http://127.0.0.1:1",
            "stub-model",
            ".",
        );
        cfg.context_window = 200_000;
        cfg.chat_options.max_tokens = Some(8192);
        cfg.chat_options.reasoning_effort = match effort {
            "off" => None,
            "high" => Some(ReasoningEffort::High),
            _ => Some(ReasoningEffort::Max),
        };
        Some(cfg)
    });

    let (agent_channel, client_channel) = Channel::duplex();
    let opts = AcpServeOptions {
        engine: Some(dummy_engine()),
        provider_factory: Some(Arc::new(StubProviderFactory(Arc::new(stub)))),
        auto_approve: false,
        session_config_options: vec![
            // The CLI entry point builds these from the kernel mode catalog and
            // the `/effort` ladder; mirror that shape here.
            SessionConfigOption::select(
                "mode",
                "Mode",
                "build",
                [
                    ("build", "Build"),
                    ("accept_edits", "AcceptEdits"),
                    ("bypass", "Auto"),
                    ("plan", "Plan"),
                ]
                .into_iter()
                .map(|(value, name)| {
                    SessionConfigSelectOption::new(value.to_string(), name.to_string())
                })
                .collect::<Vec<_>>(),
            )
            .category(SessionConfigOptionCategory::Mode),
            SessionConfigOption::select(
                "reasoning_effort",
                "Reasoning effort",
                "off",
                [
                    ("off", "Off (API default)"),
                    ("high", "High"),
                    ("max", "Max"),
                ]
                .into_iter()
                .map(|(value, name)| {
                    SessionConfigSelectOption::new(value.to_string(), name.to_string())
                })
                .collect::<Vec<_>>(),
            )
            .category(SessionConfigOptionCategory::ThoughtLevel),
        ],
        session_model_resolver: None,
        session_effort_resolver: Some(effort_resolver),
    };
    let agent_task = tokio::spawn(async move { serve_over(opts, agent_channel).await });

    let updates: Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let cwd_path = cwd.path().to_path_buf();
    let cwd_path_in_client = cwd_path.clone();
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
            let init = conn
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            serde_json::to_value(&init).unwrap();

            let new = conn
                .send_request(NewSessionRequest::new(cwd_path_in_client))
                .block_task()
                .await?;
            let sid = new.session_id.clone();

            // The session setup advertises the four execution modes.
            let new_json = serde_json::to_value(&new).unwrap();
            assert_eq!(
                new_json["modes"]["currentModeId"], "build",
                "session setup must advertise the current mode: {new_json}"
            );
            let mode_ids: Vec<String> = new_json["modes"]["availableModes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["id"].as_str().unwrap().to_string())
                .collect();
            assert_eq!(
                mode_ids,
                ["build", "accept_edits", "bypass", "plan"],
                "all kernel execution modes advertised"
            );
            let option_ids: Vec<String> = new
                .config_options
                .as_ref()
                .unwrap()
                .iter()
                .map(|o| o.id.0.as_ref().to_string())
                .collect();
            assert!(
                option_ids.contains(&"mode".to_string())
                    && option_ids.contains(&"reasoning_effort".to_string()),
                "catalog must carry mode + reasoning_effort: {option_ids:?}"
            );

            // Mode switch through the config-option channel (the same
            // validation + broadcast path as `session/set_mode`).
            let set_mode = conn
                .send_request(SetSessionConfigOptionRequest::new(
                    sid.clone(),
                    "mode",
                    SessionConfigOptionValue::value_id("plan"),
                ))
                .block_task()
                .await?;
            let set_mode_json = serde_json::to_value(&set_mode).unwrap();
            let mode_option = set_mode_json["configOptions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|o| o["id"] == "mode")
                .unwrap();
            assert_eq!(mode_option["currentValue"], "plan", "{set_mode_json}");

            // Effort switch on the real runtime: the provider reload completes.
            let set_effort = conn
                .send_request(SetSessionConfigOptionRequest::new(
                    sid.clone(),
                    "reasoning_effort",
                    SessionConfigOptionValue::value_id("max"),
                ))
                .block_task()
                .await?;
            let set_effort_json = serde_json::to_value(&set_effort).unwrap();
            let effort_option = set_effort_json["configOptions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|o| o["id"] == "reasoning_effort")
                .unwrap();
            assert_eq!(effort_option["currentValue"], "max", "{set_effort_json}");

            // The session still runs a normal turn after both switches.
            let prompt = conn
                .send_request(PromptRequest::new(
                    sid,
                    vec![ContentBlock::Text(TextContent::new("hi"))],
                ))
                .block_task()
                .await?;
            Ok((set_mode_json, set_effort_json, prompt))
        });

    let (set_mode_json, set_effort_json, prompt) =
        tokio::time::timeout(Duration::from_secs(30), client_run)
            .await
            .expect("config-option client run timed out")
            .expect("config-option client run failed");

    // The prompt terminal is still a normal end_turn after the switches.
    let prompt_json = serde_json::to_value(&prompt).unwrap();
    assert_eq!(
        prompt_json["stopReason"], "end_turn",
        "turn after mode/effort switches must complete: {prompt_json}"
    );

    // Both updates were broadcast: the mode switch as `current_mode_update`
    // and each option change as `config_option_update`.
    let got = updates.lock().unwrap().clone();
    assert!(
        got.iter()
            .any(|u| u["sessionUpdate"] == "current_mode_update" && u["currentModeId"] == "plan"),
        "current_mode_update for the mode switch: {got:?}"
    );
    let option_updates: Vec<_> = got
        .iter()
        .filter(|u| u["sessionUpdate"] == "config_option_update")
        .collect();
    assert_eq!(option_updates.len(), 2, "two config_option_update: {got:?}");
    // The last catalog broadcast reflects both applied values.
    let last = option_updates
        .last()
        .and_then(|u| serde_json::from_value::<serde_json::Value>(u["configOptions"].clone()).ok())
        .unwrap();
    let last_options = last.as_array().unwrap();
    let mode_option = last_options.iter().find(|o| o["id"] == "mode").unwrap();
    assert_eq!(mode_option["currentValue"], "plan");
    let effort_option = last_options
        .iter()
        .find(|o| o["id"] == "reasoning_effort")
        .unwrap();
    assert_eq!(effort_option["currentValue"], "max");

    serde_json::to_value(&set_mode_json).unwrap();
    serde_json::to_value(&set_effort_json).unwrap();
    agent_task.abort();
}

/// Slash commands + plan updates over the wired agent: `session/new`
/// advertises `available_commands_update`, a model turn that calls `todowrite`
/// re-emits the derived ACP `plan`, `/status` and `/todo` execute locally
/// without consuming the model script, and a normal prompt afterwards still
/// runs.
#[tokio::test]
#[serial_test::serial]
async fn slash_commands_and_plan_updates() {
    let home = tempfile::tempdir().expect("home tempdir");
    std::env::set_var("ATOMCODE_HOME", home.path());
    let cwd = tempfile::tempdir().expect("cwd tempdir");

    // MockProvider pops one script per `chat_stream` CALL. The tool-carrying turn
    // needs two scripts (the ToolCall, then the post-tool continuation; the
    // kernel makes one more closing call), and the final script proves slash
    // turns (which never reach the model) did not burn the queue.
    let stub: Arc<MockProvider> = Arc::new(MockProvider::new(vec![
        vec![
            StreamEvent::ToolCall(atomcode_kernel::tool::ToolCall {
                id: "call-todo-1".into(),
                name: "todowrite".into(),
                arguments: r#"{"todos":[{"content":"step a","status":"pending"},{"content":"step b","status":"in_progress"}]}"#
                    .into(),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("planned".into()),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("still alive".into()),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("still alive".into()),
            StreamEvent::Done { truncated: false },
        ],
    ])
    .with_ctx_window(200_000));

    let (agent_channel, client_channel) = Channel::duplex();
    let stub_provider: Arc<dyn LlmProvider> = stub.clone();
    let opts = AcpServeOptions {
        engine: Some(dummy_engine()),
        provider_factory: Some(Arc::new(StubProviderFactory(stub_provider))),
        auto_approve: true,
        ..Default::default()
    };
    let agent_task = tokio::spawn(async move { serve_over(opts, agent_channel).await });

    let updates: Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let cwd_path = cwd.path().to_path_buf();
    let cwd_path_in_client = cwd_path.clone();
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
            let init = conn
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            serde_json::to_value(&init).unwrap();

            let new = conn
                .send_request(NewSessionRequest::new(cwd_path_in_client))
                .block_task()
                .await?;
            let sid = new.session_id.clone();

            // Turn 1: the model calls `todowrite`; the completed tool call must
            // surface as a `plan` update before the turn finishes.
            let planned = conn
                .send_request(PromptRequest::new(
                    sid.clone(),
                    vec![ContentBlock::Text(TextContent::new("plan it"))],
                ))
                .block_task()
                .await?;
            serde_json::to_value(&planned).unwrap();

            // `/status` runs locally and ends the turn without a model call.
            let status = conn
                .send_request(PromptRequest::new(
                    sid.clone(),
                    vec![ContentBlock::Text(TextContent::new("/status"))],
                ))
                .block_task()
                .await?;
            let status_json = serde_json::to_value(&status).unwrap();
            assert_eq!(
                status_json["stopReason"], "end_turn",
                "slash command ends the turn: {status_json}"
            );

            // `/todo` reports the derived plan text (accumulated across turns).
            conn.send_request(PromptRequest::new(
                sid.clone(),
                vec![ContentBlock::Text(TextContent::new("/todo"))],
            ))
            .block_task()
            .await?;

            // Turn 2: the model script is still intact after two slash turns.
            let again = conn
                .send_request(PromptRequest::new(
                    sid,
                    vec![ContentBlock::Text(TextContent::new("again"))],
                ))
                .block_task()
                .await?;
            let again_json = serde_json::to_value(&again).unwrap();
            assert_eq!(
                again_json["stopReason"], "end_turn",
                "turn after slash commands must complete: {again_json}"
            );
            Ok(())
        });

    tokio::time::timeout(Duration::from_secs(30), client_run)
        .await
        .expect("slash/plan client run timed out")
        .expect("slash/plan client run failed");

    let got = updates.lock().unwrap().clone();

    // session/new advertised the slash surface from the single command table.
    let commands = got
        .iter()
        .find(|u| u["sessionUpdate"] == "available_commands_update")
        .unwrap_or_else(|| panic!("no available_commands_update in updates: {got:?}"));
    let command_names: Vec<&str> = commands["availableCommands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    for name in ["status", "plan", "todo", "help"] {
        assert!(
            command_names.contains(&name),
            "missing /{name}: {command_names:?}"
        );
    }

    // The model's `todowrite` call re-emitted the derived plan.
    let plan = got
        .iter()
        .find(|u| u["sessionUpdate"] == "plan")
        .unwrap_or_else(|| panic!("no plan update in updates: {got:?}"));
    let entries = plan["entries"].as_array().unwrap();
    assert_eq!(entries[0]["content"], "step a");
    assert_eq!(entries[0]["status"], "pending");
    assert_eq!(entries[1]["content"], "step b");
    assert_eq!(entries[1]["status"], "in_progress");

    // Slash command replies streamed as agent message chunks.
    let texts: Vec<String> = got
        .iter()
        .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|u| u["content"]["text"].as_str().map(str::to_string))
        .collect();
    assert!(
        texts.iter().any(|t| t.contains("mode:")),
        "/status reply missing session status: {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("step b")),
        "/todo reply missing derived plan: {texts:?}"
    );
    assert!(
        texts.contains(&"still alive".to_string()),
        "turn 2 reached the model: {texts:?}"
    );

    agent_task.abort();
}

/// Per-LLM-round message ids: two model calls in ONE turn must stream their
/// text with DIFFERENT `messageId` values (v1 semantics: chunks sharing an id
/// belong to one message; a changed id starts a new one). The old
/// implementation stamped one id per whole turn, which merged multi-round
/// output into a single message on the client.
#[tokio::test]
#[serial_test::serial]
async fn message_ids_advance_per_model_round_within_a_turn() {
    let home = tempfile::tempdir().expect("home tempdir");
    std::env::set_var("ATOMCODE_HOME", home.path());
    let cwd = tempfile::tempdir().expect("cwd tempdir");

    // One turn, two model calls: round 1 emits text + a tool call, round 2
    // emits text again after the tool result. Both texts must carry different
    // messageIds. Extra scripts keep the kernel's post-tool closing calls fed
    // (a tool-carrying turn consumes more model calls than its visible rounds).
    let stub: Arc<MockProvider> = Arc::new(
        MockProvider::new(vec![
            vec![
                StreamEvent::TextDelta("first round".into()),
                StreamEvent::ToolCall(atomcode_kernel::tool::ToolCall {
                    id: "call-msg-id-1".into(),
                    name: "todowrite".into(),
                    arguments: r#"{"todos":[{"content":"step a","status":"pending"}]}"#.into(),
                }),
                StreamEvent::Done { truncated: false },
            ],
            vec![
                StreamEvent::TextDelta("second round".into()),
                StreamEvent::Done { truncated: false },
            ],
            vec![
                StreamEvent::TextDelta("closing".into()),
                StreamEvent::Done { truncated: false },
            ],
            vec![
                StreamEvent::TextDelta("closing".into()),
                StreamEvent::Done { truncated: false },
            ],
        ])
        .with_ctx_window(200_000),
    );

    let (agent_channel, client_channel) = Channel::duplex();
    let stub_provider: Arc<dyn LlmProvider> = stub.clone();
    let opts = AcpServeOptions {
        engine: Some(dummy_engine()),
        provider_factory: Some(Arc::new(StubProviderFactory(stub_provider))),
        auto_approve: true,
        ..Default::default()
    };
    let agent_task = tokio::spawn(async move { serve_over(opts, agent_channel).await });

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
            let prompt = conn
                .send_request(PromptRequest::new(
                    new.session_id.clone(),
                    vec![ContentBlock::Text(TextContent::new("do it"))],
                ))
                .block_task()
                .await?;
            Ok(prompt)
        });

    let prompt = tokio::time::timeout(Duration::from_secs(30), client_run)
        .await
        .expect("client interaction timed out")
        .expect("client run failed");
    let json = serde_json::to_value(&prompt).unwrap();
    assert_eq!(json["stopReason"], "end_turn", "{json}");

    let got = updates.lock().unwrap().clone();
    let rounds: Vec<(String, String)> = got
        .iter()
        .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
        .map(|u| {
            (
                u["content"]["text"].as_str().unwrap_or("").to_string(),
                u["messageId"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect();
    // The two visible rounds must both be present with non-empty messageIds,
    // and (the crux) they must NOT share one id.
    let first = rounds
        .iter()
        .find(|(text, _)| text == "first round")
        .unwrap_or_else(|| panic!("no 'first round' chunk: {got:?}"))
        .clone();
    let second = rounds
        .iter()
        .find(|(text, _)| text == "second round")
        .unwrap_or_else(|| panic!("no 'second round' chunk: {got:?}"))
        .clone();
    assert!(
        !first.1.is_empty() && !second.1.is_empty(),
        "messageIds must be present on text chunks: {got:?}"
    );
    assert_ne!(
        first.1, second.1,
        "chunks of different model rounds must carry different messageIds: {got:?}"
    );

    agent_task.abort();
}

/// `request_user_input` → `elicitation/create` (form) round-trip (P1-3).
///
/// The client advertises `clientCapabilities.elicitation.form`; the stub model
/// calls the `request_user_input` tool; the agent forwards it as an ACP
/// `elicitation/create` (form) request; the client answers `accept` with a
/// selected option; the kernel receives the `UserInputResponse` and the turn
/// continues to a normal `end_turn`.
#[tokio::test]
#[serial_test::serial]
async fn request_user_input_maps_to_elicitation_create() {
    let home = tempfile::tempdir().expect("home tempdir");
    std::env::set_var("ATOMCODE_HOME", home.path());
    let cwd = tempfile::tempdir().expect("cwd tempdir");

    // Turn: the model asks a structured question via `request_user_input`,
    // then (after the tool result) streams a normal reply. Extra scripts keep
    // the kernel's post-tool closing calls fed.
    let stub: Arc<MockProvider> = Arc::new(MockProvider::new(vec![
        vec![
            StreamEvent::ToolCall(atomcode_kernel::tool::ToolCall {
                id: "call-ask-1".into(),
                name: "request_user_input".into(),
                arguments: r#"{"header":"Pick","question":"Which option?","mode":"single","options":[{"label":"A"},{"label":"B"}]}"#
                    .into(),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("chose B".into()),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("closing".into()),
            StreamEvent::Done { truncated: false },
        ],
    ])
    .with_ctx_window(200_000));

    let (agent_channel, client_channel) = Channel::duplex();
    let stub_provider: Arc<dyn LlmProvider> = stub.clone();
    let opts = AcpServeOptions {
        engine: Some(dummy_engine()),
        provider_factory: Some(Arc::new(StubProviderFactory(stub_provider))),
        auto_approve: true,
        ..Default::default()
    };
    let agent_task = tokio::spawn(async move { serve_over(opts, agent_channel).await });

    // The client records the elicitation requests it receives.
    let elicitations: Arc<std::sync::Mutex<Vec<CreateElicitationRequest>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let elicitations_for_handler = Arc::clone(&elicitations);

    let cwd_path = cwd.path().to_path_buf();
    let client_run = Client
        .builder()
        .on_receive_request(
            {
                let elicitations = Arc::clone(&elicitations_for_handler);
                async move |req: CreateElicitationRequest, responder, _cx| {
                    elicitations.lock().unwrap().push(req.clone());
                    // Accept the form with the "B" option (the `answer`
                    // property our schema builds for a single choice).
                    let content = std::collections::BTreeMap::from([(
                        "answer".to_string(),
                        ElicitationContentValue::String("B".to_string()),
                    )]);
                    responder.respond(CreateElicitationResponse::new(ElicitationAction::Accept(
                        ElicitationAcceptAction::new().content(content),
                    )))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(client_channel, |conn: ConnectionTo<_>| async move {
            let init = conn
                .send_request(
                    InitializeRequest::new(ProtocolVersion::V1).client_capabilities(
                        agent_client_protocol::schema::v1::ClientCapabilities::new().elicitation(
                            ElicitationCapabilities::new().form(ElicitationFormCapabilities::new()),
                        ),
                    ),
                )
                .block_task()
                .await?;
            serde_json::to_value(&init).unwrap();

            let new = conn
                .send_request(NewSessionRequest::new(cwd_path))
                .block_task()
                .await?;
            let sid = new.session_id.clone();
            let prompt = conn
                .send_request(PromptRequest::new(
                    sid.clone(),
                    vec![ContentBlock::Text(TextContent::new("ask me something"))],
                ))
                .block_task()
                .await?;
            Ok((prompt, sid))
        });

    let (prompt, sid) = tokio::time::timeout(Duration::from_secs(30), client_run)
        .await
        .expect("elicitation client run timed out")
        .expect("elicitation client run failed");

    // The turn completed normally after the structured answer was fed back.
    let prompt_json = serde_json::to_value(&prompt).unwrap();
    assert_eq!(
        prompt_json["stopReason"], "end_turn",
        "prompt must end with end_turn after the elicitation round-trip: {prompt_json}"
    );

    // The agent forwarded exactly one form-mode elicitation request.
    let got = elicitations.lock().unwrap().clone();
    assert_eq!(got.len(), 1, "exactly one elicitation/create: {got:?}");
    let got_json = serde_json::to_value(&got[0]).unwrap();
    assert_eq!(
        got_json["mode"], "form",
        "elicitation must use form mode: {got_json}"
    );
    assert_eq!(
        got_json["message"], "Which option?",
        "elicitation message must carry the question: {got_json}"
    );
    assert_eq!(
        got_json["requestedSchema"]["properties"]["answer"]["enum"],
        serde_json::json!(["A", "B"]),
        "form schema must expose the options: {got_json}"
    );
    assert_eq!(
        got_json["sessionId"],
        sid.0.as_ref(),
        "elicitation must be session-scoped: {got_json}"
    );

    agent_task.abort();
}

/// `request_user_input` fail-closed when the client does not advertise
/// `clientCapabilities.elicitation.form` (P2-1 regression).
///
/// The model calls the `request_user_input` tool, but the agent must NOT emit
/// `elicitation/create` to a client that cannot render it. The round-trip is
/// answered with `Null` (the tool's "unsupported environment" result) and the
/// turn still reaches a normal `end_turn`. This guards the form gate in
/// `handle_request_user_input` against regressing to always-send.
#[tokio::test]
#[serial_test::serial]
async fn request_user_input_fails_closed_without_form_capability() {
    let home = tempfile::tempdir().expect("home tempdir");
    std::env::set_var("ATOMCODE_HOME", home.path());
    let cwd = tempfile::tempdir().expect("cwd tempdir");

    // Same turn shape as the form round-trip test: ask a structured question,
    // then stream a normal reply.
    let stub: Arc<MockProvider> = Arc::new(MockProvider::new(vec![
        vec![
            StreamEvent::ToolCall(atomcode_kernel::tool::ToolCall {
                id: "call-ask-1".into(),
                name: "request_user_input".into(),
                arguments: r#"{"header":"Pick","question":"Which option?","mode":"single","options":[{"label":"A"},{"label":"B"}]}"#
                    .into(),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("answered anyway".into()),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("closing".into()),
            StreamEvent::Done { truncated: false },
        ],
    ])
    .with_ctx_window(200_000));

    let (agent_channel, client_channel) = Channel::duplex();
    let stub_provider: Arc<dyn LlmProvider> = stub.clone();
    let opts = AcpServeOptions {
        engine: Some(dummy_engine()),
        provider_factory: Some(Arc::new(StubProviderFactory(stub_provider))),
        auto_approve: true,
        ..Default::default()
    };
    let agent_task = tokio::spawn(async move { serve_over(opts, agent_channel).await });

    // Record any elicitation requests: the agent must send exactly zero.
    let elicitations: Arc<std::sync::Mutex<Vec<CreateElicitationRequest>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let elicitations_for_handler = Arc::clone(&elicitations);

    let cwd_path = cwd.path().to_path_buf();
    let client_run = Client
        .builder()
        .on_receive_request(
            {
                let elicitations = Arc::clone(&elicitations_for_handler);
                async move |req: CreateElicitationRequest, responder, _cx| {
                    // Record any (unexpected) request; answer accept with "B" so
                    // the turn still terminates if this path is ever reached.
                    elicitations.lock().unwrap().push(req.clone());
                    let content = std::collections::BTreeMap::from([(
                        "answer".to_string(),
                        ElicitationContentValue::String("B".to_string()),
                    )]);
                    responder.respond(CreateElicitationResponse::new(ElicitationAction::Accept(
                        ElicitationAcceptAction::new().content(content),
                    )))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(client_channel, |conn: ConnectionTo<_>| async move {
            // NOTE: no `clientCapabilities.elicitation.form` advertisement.
            let init = conn
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            serde_json::to_value(&init).unwrap();

            let new = conn
                .send_request(NewSessionRequest::new(cwd_path))
                .block_task()
                .await?;
            let sid = new.session_id.clone();
            let prompt = conn
                .send_request(PromptRequest::new(
                    sid.clone(),
                    vec![ContentBlock::Text(TextContent::new("ask me something"))],
                ))
                .block_task()
                .await?;
            Ok((prompt, sid))
        });

    let (prompt, _sid) = tokio::time::timeout(Duration::from_secs(30), client_run)
        .await
        .expect("fail-closed client run timed out")
        .expect("fail-closed client run failed");

    // The turn completes normally (fail-closed, not an error) even though the
    // structured question could not be shown to the client.
    let prompt_json = serde_json::to_value(&prompt).unwrap();
    assert_eq!(
        prompt_json["stopReason"], "end_turn",
        "prompt must end with end_turn after fail-closed elicitation: {prompt_json}"
    );

    // The agent must never emit `elicitation/create` without form support.
    assert!(
        elicitations.lock().unwrap().is_empty(),
        "elicitation/create must not be sent without form capability"
    );

    agent_task.abort();
}

/// `additionalDirectories` (stable v1 capability): the initialize response
/// advertises it, `session/new` accepts the extra roots, `session/list`
/// reports them back on `SessionInfo.additionalDirectories`, and a relative
/// root is rejected with invalid params (protocol MUST: absolute paths only).
#[tokio::test]
#[serial_test::serial]
async fn additional_directories_round_trip_and_validation() {
    let home = tempfile::tempdir().expect("home tempdir");
    std::env::set_var("ATOMCODE_HOME", home.path());
    let cwd = tempfile::tempdir().expect("cwd tempdir");

    let stub: Arc<MockProvider> = Arc::new(
        MockProvider::new(vec![vec![
            StreamEvent::TextDelta("ok".into()),
            StreamEvent::Done { truncated: false },
        ]])
        .with_ctx_window(200_000),
    );

    let (agent_channel, client_channel) = Channel::duplex();
    let stub_provider: Arc<dyn LlmProvider> = stub.clone();
    let opts = AcpServeOptions {
        engine: Some(dummy_engine()),
        provider_factory: Some(Arc::new(StubProviderFactory(stub_provider))),
        auto_approve: false,
        ..Default::default()
    };
    let agent_task = tokio::spawn(async move { serve_over(opts, agent_channel).await });

    let cwd_path = cwd.path().to_path_buf();
    let client_run =
        Client
            .builder()
            .connect_with(client_channel, |conn: ConnectionTo<_>| async move {
                let init = conn
                    .send_request(InitializeRequest::new(ProtocolVersion::V1))
                    .block_task()
                    .await?;
                let init_json = serde_json::to_value(&init).unwrap();
                assert!(
                    init_json["agentCapabilities"]["sessionCapabilities"]["additionalDirectories"]
                        .is_object(),
                    "initialize must advertise additionalDirectories: {init_json}"
                );

                // session/new with two extra roots → accepted; list reports them.
                let new = conn
                    .send_request(
                        NewSessionRequest::new(cwd_path.clone()).additional_directories(vec![
                            std::path::PathBuf::from("/shared-lib"),
                            std::path::PathBuf::from("/docs"),
                        ]),
                    )
                    .block_task()
                    .await?;
                let sid = new.session_id.clone();
                let listed = conn
                    .send_request(ListSessionsRequest::new())
                    .block_task()
                    .await?;
                assert_eq!(listed.sessions.len(), 1);
                assert_eq!(
                    listed.sessions[0].additional_directories,
                    vec![
                        std::path::PathBuf::from("/shared-lib"),
                        std::path::PathBuf::from("/docs")
                    ],
                    "session/list must report the extra roots"
                );

                // A relative additionalDirectories entry → invalid params.
                let bad = conn
                    .send_request(
                        NewSessionRequest::new(cwd_path.clone()).additional_directories(vec![
                            std::path::PathBuf::from("relative/path"),
                        ]),
                    )
                    .block_task()
                    .await;
                assert!(
                    bad.is_err(),
                    "relative additionalDirectories entry must be rejected"
                );

                // Cleanup: close the accepted session so the tempdir can go away.
                let _closed = conn
                    .send_request(CloseSessionRequest::new(sid))
                    .block_task()
                    .await?;
                Ok(())
            });

    tokio::time::timeout(Duration::from_secs(30), client_run)
        .await
        .expect("additionalDirectories client run timed out")
        .expect("additionalDirectories client run failed");

    agent_task.abort();
}

/// v2 approval shape (P2-4 方案 A): a Risky tool call (destructive bash)
/// triggers a `session/request_permission` built from the **v2** schema —
/// required `title` plus structured `subject.toolCall` plus the three standard
/// options — instead of the v1 wire shape; the client's `allow_once` feeds the
/// kernel and the turn ends normally with `end_turn`.
#[tokio::test]
#[serial_test::serial]
async fn v2_approval_uses_v2_request_shape() {
    let home = tempfile::tempdir().expect("home tempdir");
    std::env::set_var("ATOMCODE_HOME", home.path());
    let cwd = tempfile::tempdir().expect("cwd tempdir");

    // Destructive bash → Risky → approval round-trip. The target path does not
    // exist, so the (allowed) tool call is harmless. Extra scripts keep the
    // kernel's post-tool closing calls fed.
    let stub: Arc<MockProvider> = Arc::new(
        MockProvider::new(vec![
            vec![
                StreamEvent::ToolCall(atomcode_kernel::tool::ToolCall {
                    id: "call-rm-1".into(),
                    name: "bash".into(),
                    arguments: r#"{"command":"rm -rf /tmp/atomcode-e2e-nonexistent"}"#.into(),
                }),
                StreamEvent::Done { truncated: false },
            ],
            vec![
                StreamEvent::TextDelta("done".into()),
                StreamEvent::Done { truncated: false },
            ],
            vec![
                StreamEvent::TextDelta("closing".into()),
                StreamEvent::Done { truncated: false },
            ],
        ])
        .with_ctx_window(200_000),
    );

    let (agent_channel, client_channel) = Channel::duplex();
    let stub_provider: Arc<dyn LlmProvider> = stub.clone();
    let opts = AcpServeOptions {
        engine: Some(dummy_engine()),
        provider_factory: Some(Arc::new(StubProviderFactory(stub_provider))),
        auto_approve: false, // REQUIRED: approvals must round-trip to the client
        ..Default::default()
    };
    let agent_task = tokio::spawn(async move { serve_over(opts, agent_channel).await });

    use agent_client_protocol::schema::v2::{
        CloseSessionRequest as V2CloseSessionRequest, ContentBlock as V2ContentBlock,
        InitializeRequest as V2InitializeRequest, NewSessionRequest as V2NewSessionRequest,
        PermissionOptionId as V2PermissionOptionId, PromptRequest as V2PromptRequest,
        RequestPermissionOutcome as V2RequestPermissionOutcome,
        RequestPermissionRequest as V2RequestPermissionRequest,
        RequestPermissionResponse as V2RequestPermissionResponse,
        SelectedPermissionOutcome as V2SelectedPermissionOutcome, TextContent as V2TextContent,
        UpdateSessionNotification,
    };

    // The client records the permission request it receives.
    let permission: Arc<std::sync::Mutex<Option<serde_json::Value>>> =
        Arc::new(std::sync::Mutex::new(None));
    let permission_for_handler = Arc::clone(&permission);
    let updates: Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let updates_for_handler = Arc::clone(&updates);
    // Clone for the move-closure below; the original stays available for the
    // post-flight assertions.
    let updates_in_client = Arc::clone(&updates);

    let cwd_path = cwd.path().to_path_buf();
    let client_run = Client
        .v2()
        .on_receive_request(
            {
                let permission = Arc::clone(&permission_for_handler);
                async move |req: V2RequestPermissionRequest, responder, _cx| {
                    *permission.lock().unwrap() = Some(serde_json::to_value(&req).unwrap());
                    responder.respond(V2RequestPermissionResponse::new(
                        V2RequestPermissionOutcome::Selected(V2SelectedPermissionOutcome::new(
                            V2PermissionOptionId::new("allow_once"),
                        )),
                    ))
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            move |notif: UpdateSessionNotification, _cx| {
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
        .connect_with(
            client_channel,
            move |cx: ConnectionTo<agent_client_protocol::Agent>| async move {
                let init = cx
                    .send_request(V2InitializeRequest::new(
                        ProtocolVersion::V2,
                        agent_client_protocol::schema::v2::Implementation::new(
                            "acp-test-client",
                            "1.0",
                        ),
                    ))
                    .block_task()
                    .await?;
                serde_json::to_value(&init).unwrap();

                let new = cx
                    .send_request(V2NewSessionRequest::new(cwd_path))
                    .block_task()
                    .await?;
                let sid = new.session_id.clone();

                let prompt = cx
                    .send_request(V2PromptRequest::new(
                        sid.clone(),
                        vec![V2ContentBlock::Text(V2TextContent::new("delete that"))],
                    ))
                    .block_task()
                    .await?;
                serde_json::to_value(&prompt).unwrap(); // empty ack

                // Wait for the idle state_update with end_turn.
                tokio::time::timeout(Duration::from_secs(15), async {
                    loop {
                        let got = updates_in_client.lock().unwrap().clone();
                        if let Some(idle) = got
                            .iter()
                            .find(|u| u["sessionUpdate"] == "state_update" && u["state"] == "idle")
                        {
                            break idle.clone();
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("timed out waiting for v2 approval-turn idle state update");

                // close the v2 session so the tempdir can go away.
                let _closed = cx
                    .send_request(V2CloseSessionRequest::new(sid.clone()))
                    .block_task()
                    .await?;
                Ok(())
            },
        );

    tokio::time::timeout(Duration::from_secs(30), client_run)
        .await
        .expect("v2 approval client run timed out")
        .expect("v2 approval client run failed");

    // The client received exactly one v2-shaped permission request.
    let got = permission
        .lock()
        .unwrap()
        .clone()
        .expect("no session/request_permission was received");
    assert_eq!(
        got["title"], "bash",
        "v2 title must carry the tool name: {got}"
    );
    assert_eq!(
        got["subject"]["toolCall"]["title"], "bash",
        "v2 subject must be a ToolCall subject with the tool name: {got}"
    );
    let options: Vec<String> = got["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["optionId"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        options,
        vec!["allow_once", "allow_always", "reject_once"],
        "v2 approval must offer exactly the three standard options: {got}"
    );

    // The turn completed normally after the allowed tool call.
    let got_updates = updates.lock().unwrap().clone();
    let idle = got_updates
        .iter()
        .find(|u| u["sessionUpdate"] == "state_update" && u["state"] == "idle")
        .expect("idle state_update missing");
    assert_eq!(
        idle["stopReason"], "end_turn",
        "v2 turn must end with end_turn after approval: {idle}"
    );

    agent_task.abort();
}

/// v2 config options + command surface end-to-end: `session/new` advertises the
/// config catalog as `config_options`; right after, the agent broadcasts the
/// slash-command surface as `available_commands_update` (v2 `Text` input).
/// `session/set_config_option` applies the `mode` select, broadcasts the full
/// updated catalog as `config_option_update`, and returns it in the response —
/// all through the v2 wire shape (`configId`, not v1 `id`).
#[tokio::test]
#[serial_test::serial]
async fn v2_config_options_and_available_commands() {
    let home = tempfile::tempdir().expect("home tempdir");
    std::env::set_var("ATOMCODE_HOME", home.path());
    let cwd = tempfile::tempdir().expect("cwd tempdir");

    // No model round-trip needed: the config switch only rebuilds config.
    let stub = MockProvider::new(vec![]).with_ctx_window(200_000);

    let (agent_channel, client_channel) = Channel::duplex();
    let opts = AcpServeOptions {
        engine: Some(dummy_engine()),
        provider_factory: Some(Arc::new(StubProviderFactory(Arc::new(stub)))),
        auto_approve: false,
        session_config_options: vec![
            SessionConfigOption::select(
                "mode",
                "Mode",
                "build",
                [
                    ("build", "Build"),
                    ("accept_edits", "AcceptEdits"),
                    ("bypass", "Auto"),
                    ("plan", "Plan"),
                ]
                .into_iter()
                .map(|(value, name)| {
                    SessionConfigSelectOption::new(value.to_string(), name.to_string())
                })
                .collect::<Vec<_>>(),
            )
            .category(SessionConfigOptionCategory::Mode),
            SessionConfigOption::select(
                "reasoning_effort",
                "Reasoning effort",
                "off",
                [
                    ("off", "Off (API default)"),
                    ("high", "High"),
                    ("max", "Max"),
                ]
                .into_iter()
                .map(|(value, name)| {
                    SessionConfigSelectOption::new(value.to_string(), name.to_string())
                })
                .collect::<Vec<_>>(),
            )
            .category(SessionConfigOptionCategory::ThoughtLevel),
        ],
        session_model_resolver: None,
        session_effort_resolver: None,
    };
    let agent_task = tokio::spawn(async move { serve_over(opts, agent_channel).await });

    use agent_client_protocol::schema::v2::{
        CloseSessionRequest as V2CloseSessionRequest, InitializeRequest as V2InitializeRequest,
        NewSessionRequest as V2NewSessionRequest, SessionConfigId as V2SessionConfigId,
        SessionConfigOptionValue as V2SessionConfigOptionValue,
        SetSessionConfigOptionRequest as V2SetSessionConfigOptionRequest,
        UpdateSessionNotification,
    };

    let updates: Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let cwd_path = cwd.path().to_path_buf();
    let cwd_path_in_client = cwd_path.clone();
    let updates_for_handler = Arc::clone(&updates);
    let client_run = Client
        .v2()
        .on_receive_notification(
            move |notif: UpdateSessionNotification, _cx| {
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
        .connect_with(client_channel, |cx: ConnectionTo<_>| async move {
            let init = cx
                .send_request(V2InitializeRequest::new(
                    ProtocolVersion::V2,
                    agent_client_protocol::schema::v2::Implementation::new(
                        "acp-test-client",
                        "1.0",
                    ),
                ))
                .block_task()
                .await?;
            assert_eq!(init.protocol_version, ProtocolVersion::V2, "echo v2");

            let new = cx
                .send_request(V2NewSessionRequest::new(cwd_path_in_client))
                .block_task()
                .await?;
            let sid = new.session_id.clone();

            // `session/new` advertises the config catalog in the v2 shape:
            // `config_id` (not v1 `id`) with the mode select current value.
            let new_json = serde_json::to_value(&new).unwrap();
            let option_ids: Vec<String> = new_json["configOptions"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| o["configId"].as_str().unwrap().to_string())
                .collect();
            assert!(
                option_ids.contains(&"mode".to_string())
                    && option_ids.contains(&"reasoning_effort".to_string()),
                "v2 catalog must carry mode + reasoning_effort: {option_ids:?}"
            );

            // `session/set_config_option` on the v2 wire: apply `mode = plan`.
            let set_mode = cx
                .send_request(V2SetSessionConfigOptionRequest::new(
                    sid.clone(),
                    V2SessionConfigId::new("mode"),
                    V2SessionConfigOptionValue::id(
                        agent_client_protocol::schema::v2::SessionConfigValueId::new("plan"),
                    ),
                ))
                .block_task()
                .await?;
            let set_mode_json = serde_json::to_value(&set_mode).unwrap();
            let mode_option = set_mode_json["configOptions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|o| o["configId"] == "mode")
                .unwrap();
            assert_eq!(mode_option["currentValue"], "plan", "{set_mode_json}");

            // close so the tempdir can go away.
            let _closed = cx
                .send_request(V2CloseSessionRequest::new(sid.clone()))
                .block_task()
                .await?;
            Ok(())
        });

    tokio::time::timeout(Duration::from_secs(30), client_run)
        .await
        .expect("v2 config client run timed out")
        .expect("v2 config client run failed");

    let got = updates.lock().unwrap().clone();
    // Slash-command surface advertised right after setup, on the v2 wire
    // (`Text` input type).
    let commands = got
        .iter()
        .find(|u| u["sessionUpdate"] == "available_commands_update")
        .expect("available_commands_update broadcast after new");
    let names: Vec<&str> = commands["availableCommands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"model") && names.contains(&"effort"),
        "slash commands advertised: {names:?}"
    );
    let model = commands["availableCommands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "model")
        .unwrap();
    assert_eq!(
        model["input"]["type"], "text",
        "v2 command input must be the text tagged union: {model}"
    );

    // Exactly one config_option_update with the mode select now `plan`.
    let option_updates: Vec<_> = got
        .iter()
        .filter(|u| u["sessionUpdate"] == "config_option_update")
        .collect();
    assert_eq!(
        option_updates.len(),
        1,
        "one config_option_update for the mode switch: {got:?}"
    );
    let updated_mode = option_updates[0]["configOptions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["configId"] == "mode")
        .unwrap();
    assert_eq!(updated_mode["currentValue"], "plan");

    agent_task.abort();
}

/// v2 `plan_update`: a model turn that calls `todowrite` re-emits the derived
/// todo list as `plan_update` with a stable `planId` and `type: "items"` content.
#[tokio::test]
#[serial_test::serial]
async fn v2_plan_update_from_todo() {
    let home = tempfile::tempdir().expect("home tempdir");
    std::env::set_var("ATOMCODE_HOME", home.path());
    let cwd = tempfile::tempdir().expect("cwd tempdir");

    let stub: Arc<MockProvider> = Arc::new(
        MockProvider::new(vec![
            vec![
                StreamEvent::ToolCall(atomcode_kernel::tool::ToolCall {
                    id: "call-todo-1".into(),
                    name: "todowrite".into(),
                    arguments: r#"{"todos":[{"content":"step a","status":"pending"},{"content":"step b","status":"in_progress"}]}"#
                        .into(),
                }),
                StreamEvent::Done { truncated: false },
            ],
            vec![
                StreamEvent::TextDelta("planned".into()),
                StreamEvent::Done { truncated: false },
            ],
            vec![
                StreamEvent::TextDelta("closing".into()),
                StreamEvent::Done { truncated: false },
            ],
        ])
        .with_ctx_window(200_000),
    );

    let (agent_channel, client_channel) = Channel::duplex();
    let stub_provider: Arc<dyn LlmProvider> = stub.clone();
    let opts = AcpServeOptions {
        engine: Some(dummy_engine()),
        provider_factory: Some(Arc::new(StubProviderFactory(stub_provider))),
        auto_approve: true,
        ..Default::default()
    };
    let agent_task = tokio::spawn(async move { serve_over(opts, agent_channel).await });

    use agent_client_protocol::schema::v2::{
        CloseSessionRequest as V2CloseSessionRequest, ContentBlock as V2ContentBlock,
        InitializeRequest as V2InitializeRequest, NewSessionRequest as V2NewSessionRequest,
        PromptRequest as V2PromptRequest, TextContent as V2TextContent, UpdateSessionNotification,
    };

    let updates: Arc<std::sync::Mutex<Vec<serde_json::Value>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let cwd_path = cwd.path().to_path_buf();
    let cwd_path_in_client = cwd_path.clone();
    let updates_for_handler = Arc::clone(&updates);
    let updates_in_client = Arc::clone(&updates);
    let client_run = Client
        .v2()
        .on_receive_notification(
            move |notif: UpdateSessionNotification, _cx| {
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
        .connect_with(client_channel, |cx: ConnectionTo<_>| async move {
            let init = cx
                .send_request(V2InitializeRequest::new(
                    ProtocolVersion::V2,
                    agent_client_protocol::schema::v2::Implementation::new(
                        "acp-test-client",
                        "1.0",
                    ),
                ))
                .block_task()
                .await?;
            assert_eq!(init.protocol_version, ProtocolVersion::V2, "echo v2");

            let new = cx
                .send_request(V2NewSessionRequest::new(cwd_path_in_client))
                .block_task()
                .await?;
            let sid = new.session_id.clone();

            let prompt = cx
                .send_request(V2PromptRequest::new(
                    sid.clone(),
                    vec![V2ContentBlock::Text(V2TextContent::new("plan it"))],
                ))
                .block_task()
                .await?;
            serde_json::to_value(&prompt).unwrap(); // empty ack

            // Wait for the idle state_update (turn terminal).
            tokio::time::timeout(Duration::from_secs(15), async {
                loop {
                    let got = updates_in_client.lock().unwrap().clone();
                    if got
                        .iter()
                        .any(|u| u["sessionUpdate"] == "state_update" && u["state"] == "idle")
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("timed out waiting for v2 plan-turn idle state update");

            let _closed = cx
                .send_request(V2CloseSessionRequest::new(sid.clone()))
                .block_task()
                .await?;
            Ok(())
        });

    tokio::time::timeout(Duration::from_secs(30), client_run)
        .await
        .expect("v2 plan client run timed out")
        .expect("v2 plan client run failed");

    let got = updates.lock().unwrap().clone();
    let plan = got
        .iter()
        .find(|u| u["sessionUpdate"] == "plan_update")
        .expect("plan_update emitted for the todowrite turn");
    assert_eq!(plan["plan"]["type"], "items", "plan content type: {plan}");
    assert!(
        plan["plan"]["planId"]
            .as_str()
            .unwrap()
            .starts_with("plan-"),
        "plan_update must carry a stable planId: {plan}"
    );
    let entries = plan["plan"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "two todo entries: {plan}");
    assert_eq!(entries[0]["content"], "step a");
    assert_eq!(entries[1]["status"], "in_progress");

    agent_task.abort();
}
