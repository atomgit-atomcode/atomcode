//! End-to-end: a real kernel `Agent` driving the real neutral tools through the
//! generic `ApprovalMiddleware`. This is the FIRST exercise of the kernel's
//! ToolMiddleware + approval round-trip seam with REAL risky tools (production only
//! ever exercised it with testkit stubs).

use atomcode_capabilities::tools::{coding_tool_names, register_coding_tools, ApprovalMiddleware};
use atomcode_kernel::agent::{Agent, AutoRespond};
use atomcode_kernel::stream::StreamEvent;
use atomcode_kernel::testkit::MockProvider;
use atomcode_kernel::tool::{ToolCall, ToolRegistry};
use std::sync::Arc;

fn tool_call(id: &str, name: &str, args: &str) -> StreamEvent {
    StreamEvent::ToolCall(ToolCall { id: id.into(), name: name.into(), arguments: args.into() })
}
fn done() -> StreamEvent {
    StreamEvent::Done { truncated: false }
}

#[test]
fn full_toolset_registers_and_mounts() {
    let mut reg = ToolRegistry::new();
    register_coding_tools(&mut reg);
    let mounted = reg.mount(coding_tool_names());
    let names: Vec<String> = mounted.defs().into_iter().map(|d| d.name).collect();
    for expected in coding_tool_names() {
        assert!(names.iter().any(|n| n == expected), "{expected} should be mounted; got {names:?}");
    }
    assert_eq!(names.len(), coding_tool_names().len());
}

#[tokio::test]
async fn approval_allows_risky_write_and_it_lands() {
    let d = tempfile::tempdir().unwrap();
    let mut reg = ToolRegistry::new();
    register_coding_tools(&mut reg);
    let provider = Arc::new(MockProvider::new(vec![
        vec![tool_call("c1", "write_file", r#"{"file_path":"out.txt","content":"hello"}"#), done()],
        vec![StreamEvent::TextDelta("written".into()), done()],
    ]));
    let outcome = Agent::builder()
        .provider(provider)
        .tools(reg.mount(coding_tool_names()))
        .middleware(Arc::new(ApprovalMiddleware::in_memory()))
        .working_dir(d.path().to_path_buf())
        .max_rounds(5)
        .build()
        .run_to_completion("write the file", AutoRespond::AllowAll)
        .await;

    assert!(outcome.error.is_none(), "clean run expected: {:?}", outcome.error);
    assert!(d.path().join("out.txt").exists(), "approved risky write must land");
    assert_eq!(std::fs::read_to_string(d.path().join("out.txt")).unwrap(), "hello");
}

#[tokio::test]
async fn approval_denies_risky_write_and_it_is_blocked() {
    let d = tempfile::tempdir().unwrap();
    let mut reg = ToolRegistry::new();
    register_coding_tools(&mut reg);
    let provider = Arc::new(MockProvider::new(vec![
        vec![tool_call("c1", "write_file", r#"{"file_path":"out.txt","content":"hello"}"#), done()],
        vec![StreamEvent::TextDelta("ok".into()), done()],
    ]));
    let outcome = Agent::builder()
        .provider(provider)
        .tools(reg.mount(coding_tool_names()))
        .middleware(Arc::new(ApprovalMiddleware::in_memory()))
        .working_dir(d.path().to_path_buf())
        .max_rounds(5)
        .build()
        .run_to_completion("write the file", AutoRespond::DenyAll)
        .await;

    assert!(!d.path().join("out.txt").exists(), "denied risky write must NOT land");
    // The blocked call surfaces as an error tool-result the model can react to.
    assert!(
        outcome.tool_results.iter().any(|r| r.is_error && r.content.contains("denied")),
        "a denied tool-result should be surfaced; got {:?}",
        outcome.tool_results
    );
}

#[tokio::test]
async fn safe_tools_run_without_approval_prompt() {
    // read/grep/glob/list are Safe → DenyAll must NOT block them (approval only gates
    // Risky calls). Pre-seed a file, then have the model read it under DenyAll.
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("data.txt"), "alpha\nbeta\n").unwrap();
    let mut reg = ToolRegistry::new();
    register_coding_tools(&mut reg);
    let provider = Arc::new(MockProvider::new(vec![
        vec![tool_call("c1", "read_file", r#"{"file_path":"data.txt"}"#), done()],
        vec![StreamEvent::TextDelta("read it".into()), done()],
    ]));
    let outcome = Agent::builder()
        .provider(provider)
        .tools(reg.mount(coding_tool_names()))
        .middleware(Arc::new(ApprovalMiddleware::in_memory()))
        .working_dir(d.path().to_path_buf())
        .max_rounds(5)
        .build()
        .run_to_completion("read the file", AutoRespond::DenyAll)
        .await;

    assert!(
        outcome.tool_results.iter().any(|r| !r.is_error && r.content.contains("alpha")),
        "Safe read must run even under DenyAll; got {:?}",
        outcome.tool_results
    );
}

#[tokio::test]
async fn multi_tool_task_write_then_read_roundtrips() {
    let d = tempfile::tempdir().unwrap();
    let mut reg = ToolRegistry::new();
    register_coding_tools(&mut reg);
    let provider = Arc::new(MockProvider::new(vec![
        vec![tool_call("c1", "write_file", r#"{"file_path":"note.txt","content":"line one\nline two"}"#), done()],
        vec![tool_call("c2", "read_file", r#"{"file_path":"note.txt"}"#), done()],
        vec![StreamEvent::TextDelta("the file says line one".into()), done()],
    ]));
    let outcome = Agent::builder()
        .provider(provider)
        .tools(reg.mount(coding_tool_names()))
        .middleware(Arc::new(ApprovalMiddleware::in_memory()))
        .working_dir(d.path().to_path_buf())
        .max_rounds(6)
        .build()
        .run_to_completion("create and read note.txt", AutoRespond::AllowAll)
        .await;

    assert!(outcome.error.is_none(), "clean run expected: {:?}", outcome.error);
    // The write landed and the subsequent read saw its contents (with line numbers).
    assert!(
        outcome.tool_results.iter().any(|r| !r.is_error && r.content.contains("line one")),
        "read should reflect the just-written content; got {:?}",
        outcome.tool_results
    );
    assert_eq!(std::fs::read_to_string(d.path().join("note.txt")).unwrap(), "line one\nline two");
}
