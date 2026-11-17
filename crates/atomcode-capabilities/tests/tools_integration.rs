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
    StreamEvent::ToolCall(ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.into(),
    })
}
fn done() -> StreamEvent {
    StreamEvent::Done { truncated: false }
}

#[test]
fn full_toolset_registers_and_mounts() {
    let mut reg = ToolRegistry::new();
    register_coding_tools(
        &mut reg,
        &atomcode_capabilities::ProductDirs::new("/nonexistent/tree", ".ours"),
    );
    let mounted = reg.mount(coding_tool_names());
    let names: Vec<String> = mounted.defs().into_iter().map(|d| d.name).collect();

    // `coding_tool_names()` is deliberately a SUPERSET of what `register_coding_tools`
    // installs: `fetch_output` is wired only when a session exists (never by this
    // function), and `todowrite` / `request_user_input` / `memory` sit behind env gates.
    // `mount()` silently drops names with no registration, so the invariant is
    // "mounted == the registered subset", not "mounted == every name".
    let registered: Vec<&str> = coding_tool_names()
        .iter()
        .copied()
        .filter(|name| !reg.mount(&[name]).defs().is_empty())
        .collect();

    for expected in &registered {
        assert!(
            names.iter().any(|n| n == expected),
            "{expected} is registered but did not mount; got {names:?}"
        );
    }
    assert_eq!(
        names.len(),
        registered.len(),
        "mount() must yield exactly the registered subset; mounted {names:?}, registered {registered:?}"
    );

    // The reason this test exists: the unconditional core toolset is never gated away.
    for core in [
        "read_file",
        "write_file",
        "edit_file",
        "list_directory",
        "open_file",
        "bash",
        "grep",
        "glob",
        "search_replace",
        "ast_grep",
    ] {
        assert!(
            names.iter().any(|n| n == core),
            "{core} is unconditional and must always mount; got {names:?}"
        );
    }
}

#[tokio::test]
async fn approval_allows_risky_write_and_it_lands() {
    let d = tempfile::tempdir().unwrap();
    let mut reg = ToolRegistry::new();
    register_coding_tools(
        &mut reg,
        &atomcode_capabilities::ProductDirs::new("/nonexistent/tree", ".ours"),
    );
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            tool_call(
                "c1",
                "write_file",
                r#"{"file_path":"out.txt","content":"hello"}"#,
            ),
            done(),
        ],
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

    assert!(
        outcome.error.is_none(),
        "clean run expected: {:?}",
        outcome.error
    );
    assert!(
        d.path().join("out.txt").exists(),
        "approved risky write must land"
    );
    assert_eq!(
        std::fs::read_to_string(d.path().join("out.txt")).unwrap(),
        "hello"
    );
}

#[tokio::test]
async fn approval_denies_risky_write_and_it_is_blocked() {
    let d = tempfile::tempdir().unwrap();
    let mut reg = ToolRegistry::new();
    register_coding_tools(
        &mut reg,
        &atomcode_capabilities::ProductDirs::new("/nonexistent/tree", ".ours"),
    );
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            tool_call(
                "c1",
                "write_file",
                r#"{"file_path":"out.txt","content":"hello"}"#,
            ),
            done(),
        ],
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

    assert!(
        !d.path().join("out.txt").exists(),
        "denied risky write must NOT land"
    );
    // The blocked call surfaces as an error tool-result the model can react to.
    assert!(
        outcome
            .tool_results
            .iter()
            .any(|r| r.is_error && r.content.contains("denied")),
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
    register_coding_tools(
        &mut reg,
        &atomcode_capabilities::ProductDirs::new("/nonexistent/tree", ".ours"),
    );
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            tool_call("c1", "read_file", r#"{"file_path":"data.txt"}"#),
            done(),
        ],
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
        outcome
            .tool_results
            .iter()
            .any(|r| !r.is_error && r.content.contains("alpha")),
        "Safe read must run even under DenyAll; got {:?}",
        outcome.tool_results
    );
}

#[tokio::test]
async fn multi_tool_task_write_then_read_roundtrips() {
    let d = tempfile::tempdir().unwrap();
    let mut reg = ToolRegistry::new();
    register_coding_tools(
        &mut reg,
        &atomcode_capabilities::ProductDirs::new("/nonexistent/tree", ".ours"),
    );
    let provider = Arc::new(MockProvider::new(vec![
        vec![
            tool_call(
                "c1",
                "write_file",
                r#"{"file_path":"note.txt","content":"line one\nline two"}"#,
            ),
            done(),
        ],
        vec![
            tool_call("c2", "read_file", r#"{"file_path":"note.txt"}"#),
            done(),
        ],
        vec![
            StreamEvent::TextDelta("the file says line one".into()),
            done(),
        ],
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

    assert!(
        outcome.error.is_none(),
        "clean run expected: {:?}",
        outcome.error
    );
    // The write landed and the subsequent read saw its contents (with line numbers).
    assert!(
        outcome
            .tool_results
            .iter()
            .any(|r| !r.is_error && r.content.contains("line one")),
        "read should reflect the just-written content; got {:?}",
        outcome.tool_results
    );
    assert_eq!(
        std::fs::read_to_string(d.path().join("note.txt")).unwrap(),
        "line one\nline two"
    );
}

#[test]
fn registered_builtin_string_enums_declare_their_type() {
    fn check(schema: &serde_json::Value, path: &str) {
        let Some(object) = schema.as_object() else {
            return;
        };
        if let Some(values) = object.get("enum").and_then(serde_json::Value::as_array) {
            if !values.is_empty() && values.iter().all(serde_json::Value::is_string) {
                assert_eq!(
                    object.get("type"),
                    Some(&serde_json::json!("string")),
                    "untyped builtin string enum: {path}"
                );
            }
        }
        for key in ["properties", "$defs", "definitions"] {
            if let Some(entries) = object.get(key).and_then(serde_json::Value::as_object) {
                for (name, child) in entries {
                    check(child, &format!("{path}/{key}/{name}"));
                }
            }
        }
        for key in [
            "items",
            "prefixItems",
            "anyOf",
            "oneOf",
            "allOf",
            "additionalProperties",
        ] {
            if let Some(child) = object.get(key) {
                if let Some(children) = child.as_array() {
                    for (index, child) in children.iter().enumerate() {
                        check(child, &format!("{path}/{key}/{index}"));
                    }
                } else {
                    check(child, &format!("{path}/{key}"));
                }
            }
        }
    }
    let mut registry = ToolRegistry::new();
    register_coding_tools(
        &mut registry,
        &atomcode_capabilities::ProductDirs::new("/nonexistent/tree", ".ours"),
    );
    let tools = registry.mount(coding_tool_names());
    assert!(!tools.defs().is_empty());
    for tool in tools.defs() {
        check(&tool.parameters, &tool.name);
    }
    let debug = atomcode_capabilities::tools::debug::DebugTool::new(Default::default());
    check(
        &atomcode_kernel::tool::Tool::parameters_schema(&debug),
        "debug",
    );
}
