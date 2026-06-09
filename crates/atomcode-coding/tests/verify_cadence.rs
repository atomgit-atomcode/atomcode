//! End-to-end edit-then-verify discipline test (no network). A scripted provider edits a
//! file then tries to stop WITHOUT a build; the `VerifyCadenceHook` must inject a nudge
//! that drives one more round. The 3rd scripted round is reached ONLY if the nudge fired.

use atomcode_coding::{build_coding_agent_with, CodingAgentConfig};
use atomcode_kernel::agent::AutoRespond;
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::MockProvider;
use atomcode_kernel::tool::ToolCall;
use std::sync::Arc;

#[tokio::test]
async fn unverified_edit_drives_a_followup_round() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(MockProvider::new(vec![
        // Round 1: write a file (a real, successful edit).
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: "1".into(),
                name: "write_file".into(),
                arguments: r#"{"file_path":"f.txt","content":"hello"}"#.into(),
            }),
            StreamEvent::Done { truncated: false },
        ],
        // Round 2: try to STOP (text, no bash) → offer_continuation should nudge → round 3.
        vec![StreamEvent::TextDelta("stopping now".into()), StreamEvent::Done { truncated: false }],
        // Round 3: reached ONLY if the nudge fired. A distinctive marker proves it.
        vec![StreamEvent::TextDelta("VERIFIED-MARKER".into()), StreamEvent::Done { truncated: false }],
    ]));

    let cfg = CodingAgentConfig::new("k", "http://localhost:0", "mock-model", dir.path());
    let outcome = build_coding_agent_with(&cfg, provider)
        .run_to_completion("create f.txt", AutoRespond::AllowAll)
        .await;

    assert_eq!(outcome.tool_results.len(), 1, "exactly one write_file");
    assert!(dir.path().join("f.txt").exists(), "write_file must have created the file (edit succeeded)");
    assert!(
        outcome.text.contains("VERIFIED-MARKER"),
        "the edit-then-verify nudge must drive a 3rd round; final text: {:?}",
        outcome.text
    );
}
