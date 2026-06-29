//! CLAIM 20: KERNEL-OWNED TOOL-RESULT SIZE CAP.
//!
//! `ToolResult.content: String` is UNBOUNDED — a runaway mounted tool (a
//! `grep -r /`, a cat of a huge file, an infinite generator) can blow the
//! context window or OOM the host. The kernel CANNOT sandbox (OS/L1 territory),
//! but it CAN and MUST own the one mechanism at its altitude: a bounded
//! tool-result size. These tests drive a real turn through a tool that returns
//! a giant payload and assert the result the model + history + driver all see
//! is CAPPED, not the full payload.
//!
//! Enforcement point is AFTER the after-middleware chain and BEFORE the
//! `convo.push` plus `rt.emit(ToolResult)`, so middleware observes the RAW
//! result, but the stored history, the model, and the driver all see the capped
//! result. That keeps context bounded and history growth predictable
//! (prefix-cache friendly).

use async_trait::async_trait;
use atomcode_kernel::agent::{Agent, AutoRespond};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::RecordingProvider;
use atomcode_kernel::tool::{
    Tool, ToolCall, ToolContext, ToolRegistry, ToolResult,
};
use std::sync::Arc;

/// A tool whose `execute` returns a giant `content` (a runaway third-party tool
/// that does NOT self-cap). `size` bytes of 'a'.
struct HugeTool {
    size: usize,
}

#[async_trait]
impl Tool for HugeTool {
    fn name(&self) -> &str { "huge" }
    fn description(&self) -> &str { "Returns a huge blob of output" }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolResult {
        ToolResult {
            call_id: String::new(),
            content: "a".repeat(self.size),
            is_error: false,
            images: vec![],
        }
    }
}

fn tool_call(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall { id: id.into(), name: name.into(), arguments: args.into() }
}

/// Drive one turn that calls `huge` once (round 1), then stops (round 2).
/// Returns every `AgentEvent::ToolResult` the driver received.
async fn drive_and_collect_results(
    max_cap: Option<usize>,
    huge_size: usize,
) -> Vec<ToolResult> {
    let provider = Arc::new(RecordingProvider::new(vec![
        vec![
            StreamEvent::ToolCall(tool_call("c1", "huge", "{}")),
            StreamEvent::Done { truncated: false },
        ],
        vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::Done { truncated: false },
        ],
    ]));
    let calls = provider.calls();

    let mut reg = ToolRegistry::new();
    reg.register(Arc::new(HugeTool { size: huge_size }));

    let mut builder = Agent::builder()
        .provider(provider)
        .tools(reg.mount(&["huge"]));
    if let Some(c) = max_cap {
        builder = builder.max_tool_result_bytes(c);
    }
    let outcome = builder.build().run_to_completion("go", AutoRespond::AllowAll).await;

    // Cross-check: what the model saw next round (the stored Tool message) must
    // ALSO be capped — assert via the recorded provider calls.
    let recorded = calls.lock().unwrap();
    // The 2nd recorded call (round 2) carries the stored tool-result message.
    if let Some((messages, _, _)) = recorded.get(1) {
        let tool_msg = messages
            .iter()
            .find(|m| matches!(m.role, atomcode_kernel::message::Role::Tool))
            .expect("a tool-result message must be in the round-2 history");
        // What the model sees must equal what the driver received (both capped).
        if let Some(first) = outcome.tool_results.first() {
            assert_eq!(
                tool_msg.text, first.content,
                "the stored/model-visible tool result must match the emitted (capped) result"
            );
        }
    }
    outcome.tool_results
}

const DEFAULT_CAP: usize = 256 * 1024; // kernel default = production MAX_BYTES_PER_RESPONSE

// CLAIM 20a: the DEFAULT cap bounds a 1 MiB runaway result to ~256 KiB + marker,
// not the full 1 MiB — the model and the driver never see the megabyte.
#[tokio::test]
async fn default_cap_bounds_runaway_tool_result() {
    let one_mib = 1024 * 1024;
    let results = drive_and_collect_results(None, one_mib).await;
    assert_eq!(results.len(), 1, "exactly one tool result");
    let r = &results[0];
    assert!(
        r.content.len() < one_mib,
        "result must be capped well below the 1 MiB payload; got {}",
        r.content.len()
    );
    // ~ the default cap + a short marker (marker counts on top of the cap).
    assert!(
        r.content.len() <= DEFAULT_CAP + 200,
        "result must be ~default cap + marker; got {}",
        r.content.len()
    );
    assert!(r.content.contains("[truncated:"), "capped result must carry the marker: tail={:?}", &r.content[r.content.len().saturating_sub(80)..]);
    assert!(r.content.contains("by kernel cap]"), "marker must name the kernel cap");
}

// CLAIM 20b: a builder-configured SMALLER cap is honored.
#[tokio::test]
async fn builder_configured_smaller_cap_is_honored() {
    let results = drive_and_collect_results(Some(1000), 1024 * 1024).await;
    assert_eq!(results.len(), 1);
    let r = &results[0];
    let body = r.content.split('\n').next().unwrap();
    assert!(body.len() <= 1000, "body must be <= the configured 1000-byte cap; got {}", body.len());
    assert!(r.content.contains("[truncated:"), "must carry the marker");
    // Far below default → proves the builder value (not the default) is in force.
    assert!(r.content.len() < 2000, "tiny cap → tiny result; got {}", r.content.len());
}

// CLAIM 20c: a result UNDER the cap is passed through untouched (no marker).
#[tokio::test]
async fn under_cap_result_is_untouched() {
    let results = drive_and_collect_results(Some(65536), 100).await;
    assert_eq!(results.len(), 1);
    let r = &results[0];
    assert_eq!(r.content, "a".repeat(100), "small result must be byte-identical (no cap, no marker)");
    assert!(!r.content.contains("truncated"));
}

// CLAIM 20d: cap=0 means UNBOUNDED — the runaway result passes through whole.
// (Documented escape hatch; the DEFAULT stays bounded.)
#[tokio::test]
async fn cap_zero_is_unbounded() {
    let big = 300 * 1024; // > default, to prove 0 disables even the big cap
    let results = drive_and_collect_results(Some(0), big).await;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].content.len(), big, "cap=0 must not truncate");
    assert!(!results[0].content.contains("truncated"));
}
