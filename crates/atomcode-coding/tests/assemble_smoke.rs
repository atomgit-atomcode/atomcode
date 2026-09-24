//! End-to-end assembly smoke test (no network): a scripted [`MockProvider`] drives
//! the mounted product through a tool call and a stop. Proves provider + tools +
//! approval + persona + discipline wire together and the loop runs to completion.

mod support;

use atomcode_coding::CodingAgentConfig;
use atomcode_kernel::event::StopReason;
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::MockProvider;
use atomcode_kernel::tool::ToolCall;
use std::sync::Arc;
use support::{allow, mount, quiet_options, turn};

#[tokio::test]
async fn assembles_and_runs_a_tool_end_to_end() {
    // Round 1: call a Safe tool (list_directory). Round 2: stop with text.
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            StreamEvent::ToolCall(ToolCall {
                id: "1".into(),
                name: "list_directory".into(),
                arguments: r#"{"path":"."}"#.into(),
            }),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));

    let cfg = CodingAgentConfig::new("k", "http://localhost:0", "mock-model", ".");
    let mut mounted = mount(&cfg, quiet_options(), provider).await;
    let outcome = turn(&mut mounted.handle, "list the current directory", allow()).await;

    assert_eq!(
        outcome.tool_results.len(),
        1,
        "list_directory should have executed exactly once"
    );
    assert!(
        outcome.error.is_none(),
        "clean run expected, got: {:?}",
        outcome.error
    );
    assert!(
        outcome.text.contains("done"),
        "final assistant text: {:?}",
        outcome.text
    );
}

// (Removed `assembled_agent_rejects_raw_atomgit_api_bash`: raw AtomGit API
// calls through bash are no longer blocked. Read-only/public queries are
// legitimate, and credential exposure — the real risk — is caught by
// CredentialBashGate, so there is no dedicated AtomGit bash gate to assert.)

fn list_round(id: &str, path: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::ToolCall(ToolCall {
            id: id.into(),
            name: "list_directory".into(),
            arguments: serde_json::json!({ "path": path }).to_string(),
        }),
        StreamEvent::Done { truncated: false },
    ]
}

#[tokio::test]
async fn coding_assembly_enables_the_round_fuse() {
    let project = tempfile::tempdir().unwrap();
    let provider = Arc::new(MockProvider::new(vec![
        list_round("1", "."),
        list_round("2", "./"),
    ]));
    let mut cfg = CodingAgentConfig::new("k", "http://localhost:0", "mock-model", project.path());
    cfg.max_rounds = 2;

    let mut mounted = mount(&cfg, quiet_options(), provider).await;
    let outcome = turn(&mut mounted.handle, "inspect using varied calls", allow()).await;

    assert_eq!(outcome.stop, Some(StopReason::MaxRounds));
    assert_eq!(outcome.tool_results.len(), 2);
}

/// The same call, over and over, is stopped — at the threshold the policy
/// itself carries.
///
/// **The number comes from the configuration under test, not from a literal.**
/// It was written as a hard-coded 4, and when the default moved 4→5 (kernel,
/// 2026-09-24) the script ran out a round early: the guard was working, the
/// mock had nothing left to say, and the criterion reported a `ProviderError`
/// as though the stop had failed.
///
/// Read from `cfg` rather than from `ToolLoopPolicy::default()`, because those
/// are **two** numbers — the kernel's default and this product's fallback
/// (`coding/src/config.rs`'s `resolve_tool_loop_policy`) — and it is the
/// second one that decides what actually runs. A criterion reading the first
/// would go red on a move in the second and say nothing about why.
#[tokio::test]
async fn coding_assembly_enables_exact_stable_loop_detection() {
    let project = tempfile::tempdir().unwrap();
    let mut cfg = CodingAgentConfig::new("k", "http://localhost:0", "mock-model", project.path());
    cfg.max_rounds = 20;
    let stops_at = cfg
        .tool_loop_policy
        .expect("the shipped configuration has a tool-loop policy")
        .stop_threshold() as usize;
    let provider = Arc::new(MockProvider::new(
        (0..stops_at)
            .map(|n| list_round(&n.to_string(), "."))
            .collect(),
    ));

    let mut mounted = mount(&cfg, quiet_options(), provider).await;
    let outcome = turn(&mut mounted.handle, "repeat the same inspection", allow()).await;

    assert_eq!(outcome.stop, Some(StopReason::ToolLoopDetected));
    assert_eq!(
        outcome.tool_results.len(),
        stops_at,
        "it ran exactly as far as the policy allows, then stopped"
    );
}

#[tokio::test]
async fn coding_assembly_can_disable_exact_guard_for_intentional_repetition() {
    let project = tempfile::tempdir().unwrap();
    let provider = Arc::new(MockProvider::new(vec![
        list_round("1", "."),
        list_round("2", "."),
        list_round("3", "."),
        list_round("4", "."),
        vec![
            StreamEvent::TextDelta("intentional repetition complete".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let mut cfg = CodingAgentConfig::new("k", "http://localhost:0", "mock-model", project.path());
    cfg.max_rounds = 20;
    cfg.tool_loop_policy = None;

    let mut mounted = mount(&cfg, quiet_options(), provider).await;
    let outcome = turn(&mut mounted.handle, "inspect exactly four times", allow()).await;

    assert_eq!(outcome.stop, Some(StopReason::Stopped));
    assert_eq!(outcome.tool_results.len(), 4);
}
