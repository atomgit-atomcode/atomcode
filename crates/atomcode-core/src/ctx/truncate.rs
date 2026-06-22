use crate::conversation::message::{Message, MessageContent};
use crate::tool::ToolResult;

/// Dispatch to per-tool truncation based on tool name, then enforce universal upper bounds.
///
/// Per-tool truncation is the first line of defense (bash strips build noise, read_file
/// extracts outlines, etc.). The universal caps below are the LAST line of defense —
/// they cap `result.output` regardless of which tool produced it, so a single oversized
/// `ToolResult` can never dominate the ctx budget:
///
/// - `UNIVERSAL_MAX_LINES`: line-count ceiling (head 50 + tail 50 + "[N lines omitted]")
/// - `hard_char_limit`: char ceiling scaled to ~8K tokens, never more than 1/8 of window
///
/// 2026-04-13 context: a 14072-line `find` output contributed to a sent=0 cascade.
/// Per-tool truncate handled that case (head 10 + tail 20), but other pathological
/// outputs (unknown tools, huge grep, edit results with diffs) could still slip through
/// the old `char_limit = max(16000, context_window)` formula which scaled UP with ctx
/// window and let a single message consume 25% of a 64K budget.
pub fn truncate_output(result: &mut ToolResult, tool_name: &str, context_window: usize) {
    match tool_name {
        // bash: no per-tool truncation. The universal line/char caps below
        // are sufficient and purely numeric. Pattern-based "smart
        // extraction" (removed 2026-04-22) assumed English error keywords
        // (`error`/`FAILED`/`panic`) and hard-coded build tool names
        // (`cargo build`/`mvn compile`/`vite build`), which silently
        // dropped non-matching stderr — e.g. a 50-line Chinese compiler
        // trace was collapsed into `[... N lines skipped ...]` with no
        // diagnostic content surviving. Technology-stack neutrality is a
        // project rule (see `project_principles_vs_claude_md.md`), and
        // main's `turn/runner.rs::detect_call_loop` now catches the
        // retry-loop bug class that smart-extraction was trying to
        // prevent.
        "bash" => {}
        "read_file" => {} // Layer A in read.rs is the single authority. No post-hoc truncation.
        "web_fetch" => truncate_generic(result, 150, 20, 40),
        _ => truncate_generic(result, 200, 30, 50),
    }

    // ── Universal line-count ceiling ──
    // Applies after per-tool truncate. Protects against: unknown tools with no
    // per-tool logic, compile error compression that fails to shrink, edge-case
    // formats with embedded huge blobs.
    //
    // SKIP for read_file: it has its own 2000-line intelligent truncation
    // (truncate_read_file) that extracts outlines. The 300-line blanket cap
    // is too aggressive for typical source files (Vue SFC 300-500 lines,
    // Java 200-400 lines) — it cuts navItems/data definitions in the middle,
    // causing edit_file old_string mismatch on the next turn.
    // The hard_char_limit (Layer 3 below) still applies as the safety net.
    if tool_name != "read_file" {
        const UNIVERSAL_MAX_LINES: usize = 300;
        let line_count = result.output.lines().count();
        if line_count > UNIVERSAL_MAX_LINES {
            let lines: Vec<&str> = result.output.lines().collect();
            const HEAD: usize = 50;
            const TAIL: usize = 50;
            let head_part = lines[..HEAD].join("\n");
            let tail_part = lines[lines.len() - TAIL..].join("\n");
            result.output = format!(
                "{}\n\n[... {} lines omitted (universal 300-line cap) ...]\n\n{}",
                head_part,
                line_count - HEAD - TAIL,
                tail_part,
            );
        }
    }

    // ── Universal char-count ceiling ──
    // ── INVARIANT (2026-04-16): read_file MUST be skipped here ──
    // read_file has its own truncation (auto_skeleton + dynamic char_limit
    // in read.rs). This universal cap was the root cause of 26-turn
    // exploration sessions: 950-line file (38K chars) truncated to 8K
    // (200 lines), forcing 20+ turns of grep/read fragments.
    // Fixed in 4fc5cda, accidentally reverted by 4f704cb (whole-file
    // revert to restore verify.rs hit this as collateral damage).
    // Other tools (bash, grep, etc.) still get the char cap.
    // ────────────────────────────────────────────────────────────
    let hard_char_limit = (context_window / 8).min(32_000).max(8_000);
    if tool_name == "read_file" {
        // read_file: no char cap. Managed by read.rs internally:
        // 1. auto_skeleton (file_tokens > budget/5)
        // 2. dynamic char_limit (budget-scaled, not hardcoded)
        // 3. truncate_read_file above (>2000 lines → outline)
    } else if result.output.len() > hard_char_limit {
        // Preserve head AND tail when cutting — tools often put errors/status at the end.
        let chars: Vec<char> = result.output.chars().collect();
        let head_chars = hard_char_limit * 2 / 3;
        let tail_chars = hard_char_limit / 3;
        let head_part: String = chars[..head_chars.min(chars.len())].iter().collect();
        let tail_part: String = chars[chars.len().saturating_sub(tail_chars)..]
            .iter()
            .collect();
        let omitted = chars.len().saturating_sub(head_chars + tail_chars);
        result.output = format!(
            "{}\n\n[... {} chars omitted (universal {} char cap) ...]\n\n{}",
            head_part, omitted, hard_char_limit, tail_part,
        );
    }
}

// truncate_bash + try_compress_compile_errors + assemble_important_lines
// were removed 2026-04-22 (~250 lines) to enforce technology-stack
// neutrality. See comment at top of `truncate_output` for why.

// truncate_read_file: DELETED.
// read_file truncation is now handled exclusively by Layer A (auto_skeleton)
// in read.rs. Having two separate outline-extraction algorithms (tree-sitter
// in read.rs vs indent-based here) was redundant and caused confusion about
// which one actually controlled the output.

/// Generic truncation: head + tail, skipping middle.
pub(crate) fn truncate_generic(
    result: &mut ToolResult,
    max_lines: usize,
    head: usize,
    tail: usize,
) {
    let lines: Vec<&str> = result.output.lines().collect();
    if lines.len() > max_lines {
        let head_end = head.min(lines.len());
        let tail_start = lines.len().saturating_sub(tail);
        let head_part: String = lines[..head_end].join("\n");
        let tail_part: String = lines[tail_start..].join("\n");
        result.output = format!(
            "{}\n\n[... {} lines omitted ...]\n\n{}",
            head_part,
            lines.len().saturating_sub(head).saturating_sub(tail),
            tail_part
        );
    }
}

/// Apply truncation to all tool result messages
/// in the last `tool_count` messages of the conversation.
///
/// Two-pass: first per-result truncation, then per-turn budget enforcement.
/// Per-turn budget = 1/4 of context window (max 16K chars). If all results
/// in this turn exceed that, aggressively shrink the largest results.
pub fn post_process_tool_results(
    messages: &mut Vec<Message>,
    tool_count: usize,
    current_tool_name: &str,
    context_window: usize,
) {
    let len = messages.len();
    let start = len.saturating_sub(tool_count);

    // Build call_id → real tool_name lookup so each ToolResult is
    // truncated by the rules of the tool that actually produced it.
    // Without this a mixed-tool turn (e.g. read_file → bash) would
    // truncate every result under whichever tool ran last
    // (`current_tool_name`), which inverts read_file's cap exemption
    // and shrinks file contents to ~30 lines.
    let mut call_id_to_tool: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for msg in messages.iter() {
        if let MessageContent::AssistantWithToolCalls { tool_calls, .. } = &msg.content {
            for tc in tool_calls {
                call_id_to_tool.insert(tc.id.clone(), tc.name.clone());
            }
        }
    }

    // Pass 1: per-result truncation, keyed by each result's real tool.
    // `current_tool_name` is the fallback for results with no paired
    // ATC in the message vec (e.g. orphaned test fixtures).
    for i in start..len {
        if let MessageContent::ToolResult(ref r) = messages[i].content {
            let tool_name = call_id_to_tool
                .get(&r.call_id)
                .map(|s| s.as_str())
                .unwrap_or(current_tool_name);
            let mut result = r.clone();
            truncate_output(&mut result, tool_name, context_window);
            messages[i].content = MessageContent::ToolResult(result);
        }
    }

    // Pass 2: per-turn budget enforcement.
    // INVARIANT (2026-04-16): turn_budget must scale with context_window.
    // Was capped at 16K chars, which at 128K ctx meant a single turn of
    // 3 file reads got "trimmed to fit turn budget" — the model saw
    // different fragments each re-read and couldn't correlate them.
    // Now: ctx/4 with cap at 64K chars, floor 4K.
    let turn_budget = (context_window / 4).min(64_000).max(4_000);
    let mut total_chars: usize = 0;
    for i in start..len {
        if let MessageContent::ToolResult(ref r) = messages[i].content {
            total_chars += r.output.len();
        }
    }

    if total_chars > turn_budget {
        let ratio = turn_budget as f64 / total_chars as f64;
        for i in start..len {
            if let MessageContent::ToolResult(ref r) = messages[i].content {
                let target = (r.output.len() as f64 * ratio) as usize;
                if r.output.len() > target && target > 200 {
                    let mut result = r.clone();
                    let chars: Vec<char> = result.output.chars().collect();
                    let head = target * 2 / 3;
                    let tail = target / 3;
                    let head_part: String = chars[..head.min(chars.len())].iter().collect();
                    let tail_part: String =
                        chars[chars.len().saturating_sub(tail)..].iter().collect();
                    result.output = format!(
                        "{}\n[... trimmed to fit turn budget ...]\n{}",
                        head_part, tail_part,
                    );
                    messages[i].content = MessageContent::ToolResult(result);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::message::{Message, MessageContent, Role};
    use crate::tool::{ToolCall, ToolResult};

    fn make_result(output: &str) -> ToolResult {
        ToolResult {
            call_id: "test_call".to_string(),
            output: output.to_string(),
            success: true,
        }
    }

    fn make_tool_result_message(output: &str) -> Message {
        Message {
            role: Role::Tool,
            content: MessageContent::ToolResult(make_result(output)),
                    synthetic: false,
        }
    }

    fn make_atc(call_id: &str, tool_name: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: MessageContent::AssistantWithToolCalls {
                text: None,
                tool_calls: vec![ToolCall {
                    id: call_id.to_string(),
                    name: tool_name.to_string(),
                    arguments: String::new(),
                }],
                reasoning_content: None,
                thinking_blocks: Vec::new(),
            },
                    synthetic: false,
        }
    }

    fn make_tool_result_with_id(call_id: &str, output: &str) -> Message {
        Message {
            role: Role::Tool,
            content: MessageContent::ToolResult(ToolResult {
                call_id: call_id.to_string(),
                output: output.to_string(),
                success: true,
            }),
                    synthetic: false,
        }
    }

    // --- bash truncation tests (A1, 2026-04-22) ---
    //
    // bash has no per-tool truncation — relies entirely on the universal
    // line/char caps in `truncate_output`. These tests lock in that
    // behavior so future refactors don't silently reintroduce pattern-based
    // extraction.

    #[test]
    fn bash_short_output_passes_through_verbatim() {
        let output: String = (0..100)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let mut result = make_result(&output);
        truncate_output(&mut result, "bash", 64_000);
        assert_eq!(
            result.output, output,
            "bash output under 300 lines must not be touched"
        );
    }

    #[test]
    fn bash_huge_output_hits_universal_line_cap_only() {
        // 500 lines > UNIVERSAL_MAX_LINES (300) → head 50 + tail 50 + marker.
        // Purely numeric — no English error-keyword heuristic fires.
        let output: String = (0..500)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let mut result = make_result(&output);
        truncate_output(&mut result, "bash", 64_000);
        assert!(result.output.contains("line 0"), "head must be preserved");
        assert!(result.output.contains("line 499"), "tail must be preserved");
        assert!(
            result.output.contains("lines omitted"),
            "omission marker required"
        );
        assert!(result.output.lines().count() <= 110);
    }

    #[test]
    fn bash_chinese_stderr_survives_truncation() {
        // Regression test for the 2026-04-22 forensic finding: the old
        // pattern-based `truncate_bash` collapsed any line not matching
        // English `error`/`Error`/`FAILED`/`panic` into
        // `[... N lines skipped ...]`. A 50-line Chinese compiler trace
        // was reduced to head+tail-only with every middle line dropped.
        // Under A1 the output passes through verbatim (below universal
        // caps).
        let output: String = (0..50)
            .map(|_| "编译失败：找不到符号".to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let mut result = make_result(&output);
        truncate_output(&mut result, "bash", 64_000);
        assert_eq!(result.output.matches("编译失败").count(), 50);
    }

    // truncate_read_file tests: DELETED (function removed, Layer A in read.rs handles it)

    // --- truncate_generic tests ---

    #[test]
    fn truncate_generic_under_limit_unchanged() {
        let output = "line1\nline2\nline3\n";
        let mut result = make_result(output);
        truncate_generic(&mut result, 200, 30, 50);
        assert_eq!(result.output, output);
    }

    #[test]
    fn truncate_generic_over_limit_has_head_and_tail() {
        let lines: Vec<String> = (0..300).map(|i| format!("line {}", i)).collect();
        let output = lines.join("\n");
        let mut result = make_result(&output);
        truncate_generic(&mut result, 200, 30, 50);
        // Should be shorter
        assert!(result.output.len() < output.len());
        // Should contain head (line 0) and tail (line 299)
        assert!(result.output.contains("line 0"));
        assert!(result.output.contains("line 299"));
        // Should contain omit marker
        assert!(result.output.contains("lines omitted"));
    }

    // --- truncate_output universal cap tests ---

    #[test]
    fn truncate_output_hard_char_limit() {
        // With ctx_window=16000, new formula gives hard_char_limit = max(16000/8, 8000) = 8000.
        let output = "x".repeat(20000);
        let mut result = make_result(&output);
        truncate_output(&mut result, "unknown_tool", 16000);
        // Result should be at most ~8000 chars + omission marker.
        assert!(
            result.output.len() <= 8_500,
            "got {} chars",
            result.output.len()
        );
        assert!(
            result.output.contains("chars omitted"),
            "got: {}",
            result.output
        );
    }

    #[test]
    fn truncate_output_universal_line_cap() {
        // 500-line output should get capped to ~100 lines (50 head + 50 tail) + markers.
        let output: String = (0..500)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let mut result = make_result(&output);
        truncate_output(&mut result, "unknown_tool", 64_000);
        let line_count = result.output.lines().count();
        assert!(
            line_count <= 110,
            "got {} lines, expected ≤ 110",
            line_count
        );
        assert!(result.output.contains("lines omitted"));
    }

    #[test]
    fn truncate_output_caps_never_grow_with_huge_window() {
        // Even with a 1M ctx window, a single tool_result must stay ≤ 32K chars.
        let output = "x".repeat(200_000);
        let mut result = make_result(&output);
        truncate_output(&mut result, "unknown_tool", 1_000_000);
        assert!(
            result.output.len() <= 33_000,
            "single tool output should never exceed 32K chars, got {}",
            result.output.len()
        );
    }

    // --- post_process_tool_results tests ---

    #[test]
    fn post_process_truncates_results() {
        let large_output = "x".repeat(20000);
        let mut messages = vec![make_tool_result_message(&large_output)];
        post_process_tool_results(&mut messages, 1, "unknown_tool", 16000);
        // Should be truncated but remain inline ToolResult
        assert!(matches!(messages[0].content, MessageContent::ToolResult(_)));
        if let MessageContent::ToolResult(ref r) = messages[0].content {
            // 8K cap + omission marker ≈ 8500 chars worst case.
            assert!(r.output.len() <= 8_500);
        }
    }

    #[test]
    fn post_process_keeps_small_results_unchanged() {
        let small_output = "short output";
        let mut messages = vec![make_tool_result_message(small_output)];
        post_process_tool_results(&mut messages, 1, "bash", 16000);
        assert!(matches!(messages[0].content, MessageContent::ToolResult(_)));
        if let MessageContent::ToolResult(ref r) = messages[0].content {
            assert_eq!(r.output, "short output");
        }
    }

    /// Regression: in a mixed-tool turn, each ToolResult must be truncated
    /// using the rules of the tool that actually produced it — looked up
    /// via call_id → ATC.name — NOT `current_tool_name` (which only
    /// reflects whichever tool ran last). Without this, a `read_file`
    /// result in a `read_file → bash` turn loses its hard-char-limit
    /// exemption and gets shrunk to bash's HEAD+TAIL, defeating the
    /// file-content preservation invariant.
    #[test]
    fn post_process_keys_truncation_by_each_result_tool_not_current() {
        // 400-line "file content" — would trip bash's HEAD 10 + TAIL 20
        // and the universal 300-line cap if keyed as bash, but read_file
        // is explicitly exempt from both.
        let file_content: String = (0..400)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let original_line_count = file_content.lines().count();

        let mut messages = vec![
            make_atc("rf1", "read_file"),
            make_tool_result_with_id("rf1", &file_content),
        ];

        // current_tool_name="bash" as if bash ran last in this turn.
        // The read_file result must still be recognized as read_file.
        post_process_tool_results(&mut messages, 2, "bash", 128_000);

        if let MessageContent::ToolResult(ref r) = messages[1].content {
            assert_eq!(
                r.output.lines().count(),
                original_line_count,
                "read_file content must stay intact when current_tool_name \
                 is a different tool — got {} lines (expected {})",
                r.output.lines().count(),
                original_line_count,
            );
        } else {
            panic!("expected ToolResult at index 1");
        }
    }
}
