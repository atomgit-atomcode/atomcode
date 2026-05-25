//! Snapshot invariant tests for known-clean session fixtures.
//!
//! What this catches (2026-04-22 feat/0.95x-sprint):
//!   When a contributor adds a new session jsonl to `tests/fixtures/` —
//!   e.g. after reproducing a bug or verifying a fix — these tests ensure
//!   the session doesn't contain patterns the P0 sprint explicitly removed
//!   (continuation placeholders, "summarize and stop" directives, sed -i
//!   workarounds, etc.). If a future commit regresses the framework such
//!   that regenerated sessions start containing those patterns, the test
//!   fails the moment the fixture is refreshed.
//!
//! What this does NOT catch:
//!   - Regressions whose fixture happens to never be refreshed.
//!   - Behavioral regressions that don't manifest as specific substrings
//!     (e.g. "turn count doubles" without new placeholders).
//!   - Model-quality regressions (replay harness would, but costs 1.5 day
//!     of infra — see P1 roadmap entry).
//!
//! So: an honest light guard, not a full regression net. Pair with the
//! tool-level unit tests in `tool/bash.rs`, `tool/edit.rs`, etc., which
//! cover the per-function invariants directly.

use serde_json::Value;
use std::path::PathBuf;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

/// Load a jsonl session fixture. Each line is one event (request snapshot
/// with `messages` array). The LAST event contains the full conversation
/// history.
fn load_last_event(name: &str) -> Value {
    let path = fixtures_dir().join(name);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    let last = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .last()
        .unwrap_or_else(|| panic!("fixture {} is empty", name));
    serde_json::from_str(last).unwrap_or_else(|e| panic!("parse {}: {}", path.display(), e))
}

/// Extract every Assistant message's text + every ToolResult output into
/// a single concatenated string. Invariant assertions grep this for known
/// bad patterns.
fn collect_agent_output(event: &Value) -> String {
    let mut out = String::new();
    let messages = event
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");
    for m in messages {
        if let Some(c) = m.get("content") {
            // Assistant text
            if let Some(a) = c.get("AssistantWithToolCalls") {
                if let Some(t) = a.get("text").and_then(|v| v.as_str()) {
                    out.push_str(t);
                    out.push('\n');
                }
            }
            // Standalone Text (Assistant or User)
            if let Some(t) = c.get("Text").and_then(|v| v.as_str()) {
                out.push_str(t);
                out.push('\n');
            }
            // Tool results
            if let Some(tr) = c.get("ToolResult") {
                if let Some(o) = tr.get("output").and_then(|v| v.as_str()) {
                    out.push_str(o);
                    out.push('\n');
                }
            }
        }
    }
    out
}

fn assert_no_removed_patterns(fixture_name: &str) {
    let ev = load_last_event(fixture_name);
    let all = collect_agent_output(&ev);

    // Patterns the P0 sprint explicitly removed — their reappearance in
    // a regenerated session would indicate a framework regression.
    //
    // NOTE: these are framework-generated strings, not something the model
    // would naturally say. User quoting them in a message (e.g. asking
    // "why did you say '(continuing...)'?") would be an edge case — fix
    // the test then, not the constraint.
    let removed_markers: &[(&str, &str)] = &[
        (
            "(continuing...)",
            "framework injected `(continuing...)` placeholder after empty \
             model response — removed in commit 804fd31 (continuation recovery removal)",
        ),
        (
            "(completed)",
            "framework injected `(completed)` placeholder on slow-empty — \
             removed in commit 804fd31",
        ),
        (
            "summarize and stop instead of continuing",
            "old #5 Auto-STOP nudge was command-style and caused weak models \
             to skip user-requested steps — replaced with `key diagnostic lines` \
             wording in commit 389d604",
        ),
    ];
    for (pat, reason) in removed_markers {
        assert!(
            !all.contains(pat),
            "fixture {} contains `{}` — {}",
            fixture_name,
            pat,
            reason,
        );
    }
}

/// Additional tool-call args patterns that should never appear in a clean
/// session. Searches tool_call arguments specifically.
fn collect_tool_call_args(event: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let messages = event
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");
    for m in messages {
        if let Some(a) = m
            .get("content")
            .and_then(|c| c.get("AssistantWithToolCalls"))
        {
            if let Some(tcs) = a.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tcs {
                    if let Some(args) = tc.get("arguments").and_then(|v| v.as_str()) {
                        out.push(args.to_string());
                    }
                }
            }
        }
    }
    out
}

fn assert_no_shell_workaround_calls(fixture_name: &str) {
    let ev = load_last_event(fixture_name);
    let args_list = collect_tool_call_args(&ev);

    // Shell-based edit bypasses — if these appear the agent worked around
    // edit_file. The framework warns against them (edit.rs failure message
    // + bash.rs workspace-change detection). In a CLEAN fixture these
    // should be absent; their presence would suggest either regression or
    // the fixture itself isn't clean and shouldn't be used as a reference.
    //
    // Exact substring checks — no shell grammar parsing. A legitimate
    // `sed pattern` with no `-i` is fine; `sed -i` is not.
    let bypass_markers = &["sed -i", "perl -pi", "awk -i inplace"];
    for args in &args_list {
        for bad in bypass_markers {
            assert!(
                !args.contains(bad),
                "fixture {} has a tool call with `{}` — shell workaround \
                 pattern (see 426-atom 2026-04-21 session regression). \
                 args: {}",
                fixture_name,
                bad,
                args,
            );
        }
    }
}

fn assert_bash_has_exit_markers(fixture_name: &str) {
    // Every bash ToolResult output should now carry an `exit: N` or
    // `killed:` marker from the P0 #3 fix. Absence would indicate the
    // exit-code-in-marker change was reverted.
    let ev = load_last_event(fixture_name);
    let messages = ev
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages");

    // Identify which indices are bash tool results by looking at the
    // preceding Assistant message's tool call name.
    let mut tool_name_of: std::collections::HashMap<usize, String> =
        std::collections::HashMap::new();
    for (i, m) in messages.iter().enumerate() {
        if let Some(a) = m
            .get("content")
            .and_then(|c| c.get("AssistantWithToolCalls"))
        {
            if let Some(tcs) = a.get("tool_calls").and_then(|v| v.as_array()) {
                // The ToolResults follow this Assistant message, one per call,
                // in order.
                for (k, tc) in tcs.iter().enumerate() {
                    if let Some(name) = tc.get("name").and_then(|v| v.as_str()) {
                        tool_name_of.insert(i + 1 + k, name.to_string());
                    }
                }
            }
        }
    }

    let mut bash_results_checked = 0;
    for (i, m) in messages.iter().enumerate() {
        if tool_name_of.get(&i).map(|n| n == "bash").unwrap_or(false) {
            if let Some(out) = m
                .get("content")
                .and_then(|c| c.get("ToolResult"))
                .and_then(|tr| tr.get("output"))
                .and_then(|v| v.as_str())
            {
                let has_exit = out.contains("exit: ") || out.contains("killed:");
                assert!(
                    has_exit,
                    "fixture {} bash ToolResult at msg[{}] missing `exit: N` / `killed:` marker — \
                     check that P0 #3 (exit code in bash marker) is still in place. output:\n{}",
                    fixture_name, i, out
                );
                bash_results_checked += 1;
            }
        }
    }
    assert!(
        bash_results_checked > 0,
        "fixture {} had no bash ToolResults to check — either fixture is \
         not bash-exercising or tool_name tracking broke",
        fixture_name,
    );
}

// ═══════════════════════════════════════════════════════════════
// Fixtures
// ═══════════════════════════════════════════════════════════════

#[test]
fn p0_sprint_clean_session_is_free_of_removed_patterns() {
    // hermes 2026-04-22_21-11-34 — the reference "everything works" session
    // after all P0 commits landed. 7 turns, 6 tool calls, proper markdown
    // summary at end, #5 nudge fired with new wording.
    assert_no_removed_patterns("session_p0_sprint_clean.jsonl");
}

#[test]
fn p0_sprint_clean_session_has_no_shell_workarounds() {
    assert_no_shell_workaround_calls("session_p0_sprint_clean.jsonl");
}

#[test]
fn p0_sprint_clean_session_bash_has_exit_markers() {
    assert_bash_has_exit_markers("session_p0_sprint_clean.jsonl");
}

#[test]
fn p0_sprint_clean_session_tool_call_result_parity() {
    assert_tool_call_result_parity("session_p0_sprint_clean.jsonl");
}

#[test]
fn path_404_recovery_session_tool_call_result_parity() {
    assert_tool_call_result_parity("session_404_recovery.jsonl");
}

#[test]
fn path_404_recovery_session_is_free_of_removed_patterns() {
    // hermes 2026-04-22_20-12-44 — 4-turn session proving #4 path-prefix
    // ranking works: read_file `/hermes/hermes-tauri/src/main.rs` 404'd,
    // the Did-you-mean suggested the correct `src-tauri/src/main.rs`.
    assert_no_removed_patterns("session_404_recovery.jsonl");
}

#[test]
fn path_404_recovery_session_has_no_shell_workarounds() {
    assert_no_shell_workaround_calls("session_404_recovery.jsonl");
}

/// Every `AssistantWithToolCalls` message must be followed by exactly the
/// same number of `ToolResult` messages. If `merge_edit_calls` runs AFTER
/// `finalize_stream_with_tool_calls`, the assistant message declares N
/// tool calls but only M < N results are appended — this poisons the
/// next provider request (the bug fixed in runner.rs 2026-04-30).
fn assert_tool_call_result_parity(fixture_name: &str) {
    let ev = load_last_event(fixture_name);
    let messages = ev
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");

    let mut i = 0;
    while i < messages.len() {
        if let Some(a) = messages[i]
            .get("content")
            .and_then(|c| c.get("AssistantWithToolCalls"))
        {
            if let Some(tcs) = a.get("tool_calls").and_then(|v| v.as_array()) {
                let expected = tcs.len();
                // Count consecutive ToolResult messages after this assistant message.
                let mut actual = 0;
                let mut j = i + 1;
                while j < messages.len() {
                    if messages[j]
                        .get("content")
                        .and_then(|c| c.get("ToolResult"))
                        .is_some()
                    {
                        actual += 1;
                        j += 1;
                    } else {
                        break;
                    }
                }
                assert_eq!(
                    expected, actual,
                    "fixture {} msg[{}]: assistant declares {} tool calls but \
                     followed by {} ToolResult messages — tool_call/tool_result \
                     mismatch poisons the next provider request",
                    fixture_name, i, expected, actual,
                );
            }
        }
        i += 1;
    }
}

/// Meta-test: make sure the fixture loader + collector actually visit
/// Assistant text. If the jsonl schema ever changes (field rename) and
/// `collect_agent_output` silently returns empty, every invariant would
/// trivially pass. This test asserts the collector captured non-trivial
/// content, so schema drift fails loud.
#[test]
fn fixture_collector_sees_assistant_content() {
    let ev = load_last_event("session_p0_sprint_clean.jsonl");
    let all = collect_agent_output(&ev);
    assert!(
        all.len() > 1000,
        "collected agent output suspiciously short ({} bytes) — jsonl schema may have changed, \
         collect_agent_output needs updating",
        all.len()
    );
    // Final turn produces a structured summary — this exact substring
    // confirms we're reading Assistant final text, not just tool results.
    assert!(
        all.contains("任务总结") || all.contains("任务结果") || all.contains("## "),
        "expected a markdown summary header in final Assistant text; got first 300 chars:\n{}",
        all.chars().take(300).collect::<String>()
    );
}
