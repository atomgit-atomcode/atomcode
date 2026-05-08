//! Integration tests for the hooks lifecycle system.
//!
//! These tests exercise the full pipeline: HookConfig → HookExecutor → shell
//! command execution, verifying environment-variable passing, pre-hook
//! blocking/allowing logic, post-hook side-effects, and multi-hook chaining.

use atomcode_core::hook::executor::HookExecutor;
use atomcode_core::hook::{HookConfig, HookContext, HookEvent, PreHookResult};

fn test_ctx(tool_name: &str, args: &str) -> HookContext {
    HookContext {
        event: "pre_tool_use".into(),
        tool_name: Some(tool_name.into()),
        tool_args: serde_json::from_str(args).ok(),
        tool_result: None,
        tool_success: None,
        session_id: "integration-test".into(),
        working_dir: "/tmp".into(),
    }
}

fn make_hook(event: HookEvent, matcher: Option<&str>, cmd: &str) -> HookConfig {
    HookConfig {
        event,
        matcher: matcher.map(String::from),
        command: cmd.to_string(),
        timeout_ms: 10_000,
        plugin_root: None,
    }
}

// ── Pre-hook: block dangerous commands ──────────────────────────────

#[tokio::test]
async fn pre_hook_blocks_dangerous_command() {
    // Shell script that inspects ATOMCODE_HOOK_CONTEXT for "rm -rf".
    // If the context contains it, the hook blocks; otherwise it allows.
    let script = r#"
        if echo "$ATOMCODE_HOOK_CONTEXT" | grep -q 'rm -rf'; then
            echo '{"action":"block","reason":"rm -rf is forbidden"}'
        else
            echo '{"action":"allow"}'
        fi
    "#;

    let hook = make_hook(HookEvent::PreToolUse, Some("bash"), script);
    let exec = HookExecutor::new(vec![hook]);

    // Dangerous command → blocked
    let ctx_danger = test_ctx("bash", r#"{"command":"rm -rf /"}"#);
    let result = exec.run_pre_tool_use("bash", &ctx_danger).await;
    assert_eq!(
        result,
        PreHookResult::Block {
            reason: "rm -rf is forbidden".into()
        },
        "dangerous command should be blocked"
    );

    // Safe command → allowed
    let ctx_safe = test_ctx("bash", r#"{"command":"ls -la"}"#);
    let result = exec.run_pre_tool_use("bash", &ctx_safe).await;
    assert_eq!(result, PreHookResult::Allow, "safe command should be allowed");
}

// ── Post-hook receives tool name via env ────────────────────────────

#[tokio::test]
async fn post_hook_receives_result() {
    let pid = std::process::id();
    let marker = format!("/tmp/atomcode_hook_test_post_{pid}");

    // The hook writes ATOMCODE_TOOL_NAME into a temp file.
    let script = format!(r#"echo "$ATOMCODE_TOOL_NAME" > {marker}"#);

    let hook = make_hook(HookEvent::PostToolUse, Some("write_file"), &script);
    let exec = HookExecutor::new(vec![hook]);

    let ctx = HookContext {
        event: "post_tool_use".into(),
        tool_name: Some("write_file".into()),
        tool_args: None,
        tool_result: Some("file written successfully".into()),
        tool_success: Some(true),
        session_id: "integration-test".into(),
        working_dir: "/tmp".into(),
    };

    exec.run_post_tool_use("write_file", &ctx).await;

    // Read the marker file and verify contents.
    let content = std::fs::read_to_string(&marker)
        .unwrap_or_else(|e| panic!("failed to read marker file {marker}: {e}"));
    assert_eq!(
        content.trim(),
        "write_file",
        "post-hook should receive the tool name via ATOMCODE_TOOL_NAME"
    );

    // Clean up.
    let _ = std::fs::remove_file(&marker);
}

// ── Multiple hooks chain correctly ──────────────────────────────────

#[tokio::test]
async fn multiple_hooks_chain_correctly() {
    // First hook allows.
    let hook_allow = make_hook(
        HookEvent::PreToolUse,
        Some("bash"),
        r#"echo '{"action":"allow"}'"#,
    );

    // Second hook blocks.
    let hook_block = make_hook(
        HookEvent::PreToolUse,
        Some("bash"),
        r#"echo '{"action":"block","reason":"second hook says no"}'"#,
    );

    let exec = HookExecutor::new(vec![hook_allow, hook_block]);
    let ctx = test_ctx("bash", r#"{"command":"echo hi"}"#);

    let result = exec.run_pre_tool_use("bash", &ctx).await;

    // The executor short-circuits on Block, so the second hook wins.
    assert_eq!(
        result,
        PreHookResult::Block {
            reason: "second hook says no".into()
        },
        "when any hook returns Block, the aggregate result should be Block"
    );
}

// ── Hook context JSON is passed correctly ───────────────────────────

#[tokio::test]
async fn hook_context_json_contains_expected_fields() {
    let pid = std::process::id();
    let marker = format!("/tmp/atomcode_hook_test_ctx_{pid}");

    // Write the full ATOMCODE_HOOK_CONTEXT to a file so we can inspect it.
    let script = format!(r#"echo "$ATOMCODE_HOOK_CONTEXT" > {marker}"#);

    let hook = make_hook(HookEvent::PreToolUse, Some("bash"), &script);
    let exec = HookExecutor::new(vec![hook]);
    let ctx = test_ctx("bash", r#"{"command":"whoami"}"#);

    exec.run_pre_tool_use("bash", &ctx).await;

    let content = std::fs::read_to_string(&marker)
        .unwrap_or_else(|e| panic!("failed to read marker file {marker}: {e}"));
    let parsed: serde_json::Value = serde_json::from_str(content.trim())
        .unwrap_or_else(|e| panic!("ATOMCODE_HOOK_CONTEXT is not valid JSON: {e}"));

    assert_eq!(parsed["event"], "pre_tool_use");
    assert_eq!(parsed["tool_name"], "bash");
    assert_eq!(parsed["session_id"], "integration-test");
    assert_eq!(parsed["working_dir"], "/tmp");
    assert_eq!(parsed["tool_args"]["command"], "whoami");

    // Clean up.
    let _ = std::fs::remove_file(&marker);
}

// ── Non-matching hook is skipped ────────────────────────────────────

#[tokio::test]
async fn non_matching_hook_is_skipped() {
    // Hook only matches "edit_*" tools.
    let hook = make_hook(
        HookEvent::PreToolUse,
        Some("edit_*"),
        r#"echo '{"action":"block","reason":"edit blocked"}'"#,
    );
    let exec = HookExecutor::new(vec![hook]);
    let ctx = test_ctx("bash", r#"{"command":"ls"}"#);

    // "bash" does not match "edit_*", so the hook should be skipped → Allow.
    let result = exec.run_pre_tool_use("bash", &ctx).await;
    assert_eq!(
        result,
        PreHookResult::Allow,
        "hook with non-matching pattern should be skipped"
    );
}

// ── Modify result from hook ─────────────────────────────────────────

#[tokio::test]
async fn pre_hook_can_modify_args() {
    let script = r#"echo '{"action":"modify","args":{"command":"echo sanitized"}}'"#;
    let hook = make_hook(HookEvent::PreToolUse, Some("bash"), script);
    let exec = HookExecutor::new(vec![hook]);
    let ctx = test_ctx("bash", r#"{"command":"rm -rf /"}"#);

    let result = exec.run_pre_tool_use("bash", &ctx).await;
    match result {
        PreHookResult::Modify { args } => {
            assert_eq!(
                args["command"], "echo sanitized",
                "Modify should carry the replacement args"
            );
        }
        other => panic!("expected Modify, got {other:?}"),
    }
}
