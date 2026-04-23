//! Smoke test for the replay harness scaffold — no full AgentLoop yet.
//!
//! Verifies that the fixture loader parses a real session jsonl correctly
//! and that `ReplayProvider` emits the expected stream events in order.
//! Catches schema drift between the loader and the stored fixtures before
//! we wire it into a full AgentLoop replay.

use std::path::PathBuf;

use atomcode_core::provider::LlmProvider;
use atomcode_core::stream::StreamEvent;
use futures::StreamExt;

mod replay_harness;
use replay_harness::fixture_loader;
use replay_harness::replay_provider::ReplayProvider;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[test]
fn loader_extracts_initial_user_message_and_responses() {
    let session = fixture_loader::load(&fixture_path("session_p0_sprint_clean.jsonl"));
    assert!(
        session.initial_user_message.contains("undefined_marker_abc123"),
        "expected initial user message to mention the marker; got: {}",
        session.initial_user_message
    );
    // Hermes 21-11-34 session: 7 turns total, but datalog records
    // REQUESTS, not responses. Event N is the snapshot sent BEFORE LLM
    // call N+1; Turn 7's final summary text isn't stored anywhere in
    // the jsonl because no subsequent request followed it. So the
    // recorded-response count == 6 (Turns 1-6), not 7.
    //
    // This is a real replay-harness limitation worth calling out in
    // the fixture + logging design — tracked as a finding in the
    // Stage 2 scaffold PR.
    assert_eq!(
        session.responses.len(),
        6,
        "hermes 21-11-34 should have 6 recorded responses (Turns 1-6); Turn 7 summary is not in the jsonl"
    );
    // At least one response must carry tool_calls (the edit + bash turns).
    assert!(
        session.responses.iter().any(|r| !r.tool_calls.is_empty()),
        "no tool_calls parsed from any response — parser broken"
    );
}

#[test]
fn loader_extracts_tool_results() {
    let session = fixture_loader::load(&fixture_path("session_p0_sprint_clean.jsonl"));
    assert!(!session.tool_results.is_empty(), "no tool_results parsed");
    // At least one result must have exit code marker (P0 #3 invariant,
    // cross-checked here to fail loud if the fixture itself drifted).
    assert!(
        session.tool_results.iter().any(|r| r.output.contains("exit: ")),
        "no bash result with `exit: N` marker — fixture or parser drift"
    );
}

#[tokio::test]
async fn replay_provider_emits_recorded_text_and_tool_calls() {
    let session = fixture_loader::load(&fixture_path("session_p0_sprint_clean.jsonl"));
    let total = session.responses.len();
    let provider = ReplayProvider::new(session.responses);

    assert_eq!(provider.cursor_position(), 0);

    // Call chat_stream for every recorded response. Each call should
    // emit (optionally Delta) + ToolCallDones + Usage + Done in order.
    for i in 0..total {
        let mut stream = provider.chat_stream(&[], None).expect("chat_stream ok");
        let mut saw_done = false;
        let mut saw_usage = false;
        while let Some(ev) = stream.next().await {
            match ev.expect("stream event ok") {
                StreamEvent::Done { .. } => {
                    saw_done = true;
                    break;
                }
                StreamEvent::Usage(_) => saw_usage = true,
                StreamEvent::Delta(_) | StreamEvent::ToolCallDone(_) => {}
                other => panic!("unexpected event on replay: {:?}", other),
            }
        }
        assert!(saw_done, "response {} didn't emit Done", i);
        assert!(saw_usage, "response {} didn't emit Usage", i);
    }
    assert_eq!(provider.cursor_position(), total, "cursor must advance exactly once per call");
}

#[tokio::test]
async fn replay_provider_errors_when_exhausted() {
    // Exhaustion = regression signal: framework tried to make more LLM
    // calls than the recorded session has. Test with a tiny custom fixture
    // to keep this isolated from the shared session jsonls.
    let provider = ReplayProvider::new(vec![
        fixture_loader::RecordedResponse {
            text: "done".to_string(),
            tool_calls: Vec::new(),
            truncated: false,
        },
    ]);
    // First call succeeds.
    let mut s = provider.chat_stream(&[], None).unwrap();
    while s.next().await.is_some() {}
    // Second call should surface the exhaustion error.
    let result = provider.chat_stream(&[], None);
    // `Result<Pin<Box<dyn Stream>>, _>` has no Debug impl, so can't use
    // `unwrap_err()` directly. Match manually.
    match result {
        Ok(_) => panic!("second call past fixture end must error"),
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("exhausted"),
                "error should mention exhaustion; got: {}",
                msg
            );
        }
    }
}
