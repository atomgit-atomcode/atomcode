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
        "bash" => truncate_bash(result),
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

/// Bash: preserve error lines, strip verbose build noise.
/// Errors are the highest-value signal — keep all lines containing "error",
/// "Error", "FAILED", "STDERR", "panic", plus surrounding context.
fn truncate_bash(result: &mut ToolResult) {
    let lines: Vec<&str> = result.output.lines().collect();
    if lines.len() <= 80 {
        return; // Short enough — keep everything.
    }

    // --- Phase 0: Compile error smart compression ---
    // Detect build tool output and extract only error-bearing lines + context.
    // This fires for Maven/Gradle, TypeScript/Node, Rust/Cargo, and generic builds.
    let compressed = try_compress_compile_errors(&lines);
    if let Some(compressed_output) = compressed {
        if compressed_output.len() < result.output.len() {
            result.output = compressed_output;
            return;
        }
    }

    // --- Phase 1: General error-line extraction (non-build output) ---
    // Generic error patterns — no language-specific strings.
    let error_patterns = [
        "error",
        "Error",
        "ERROR",
        "FAILED",
        "STDERR:",
        "panic",
        "Panic",
        "PANIC",
        "not found",
        "No such file",
        "Permission denied",
        "cannot find",
        "undefined",
        "unresolved",
    ];
    let mut important: Vec<bool> = vec![false; lines.len()];

    for (i, line) in lines.iter().enumerate() {
        if error_patterns.iter().any(|p| line.contains(p)) {
            // Mark this line and 2 lines of context above/below.
            let start = i.saturating_sub(2);
            let end = (i + 3).min(lines.len());
            for j in start..end {
                important[j] = true;
            }
        }
    }

    // Phase 2: Always keep head (first 10 lines) and tail (last 20 lines).
    const HEAD: usize = 10;
    const TAIL: usize = 20;
    for i in 0..HEAD.min(lines.len()) {
        important[i] = true;
    }
    for i in lines.len().saturating_sub(TAIL)..lines.len() {
        important[i] = true;
    }

    // Phase 3: Assemble, collapsing unimportant runs into "[N lines skipped]".
    let output = assemble_important_lines(&lines, &important);
    result.output = output;
}

/// Attempt to compress compile/build error output by extracting only error lines
/// with surrounding context, plus head/tail for build status summary.
/// Returns `Some(compressed)` if the output looks like build output and was compressed,
/// `None` if it doesn't look like build output.
fn try_compress_compile_errors(lines: &[&str]) -> Option<String> {
    let full_text = lines.join("\n");

    // Detect build tool output.
    let is_build = full_text.contains("BUILD SUCCESS")
        || full_text.contains("BUILD FAILURE")
        || full_text.contains("Compiled successfully")
        || full_text.contains("compiled successfully")
        || full_text.contains("Compiling")
        || full_text.contains("vite build")
        || full_text.contains("vue-tsc")
        || full_text.contains("tsc --")
        || full_text.contains("npm run build")
        || full_text.contains("cargo build")
        || full_text.contains("cargo check")
        || full_text.contains("mvn compile")
        || full_text.contains("mvn package")
        || full_text.contains("gradle build");

    if !is_build {
        return None;
    }

    // Compile error patterns by ecosystem — match lines that carry diagnostic value.
    // Java / Maven / Gradle
    let java_patterns: &[&str] = &[
        "[ERROR]",
        "error:",
        "cannot find symbol",
        "package does not exist",
        "incompatible types",
        "unreported exception",
        "method does not override",
    ];
    // TypeScript / Node
    let ts_patterns: &[&str] = &[
        "error TS",
        "Error:",
        "SyntaxError",
        "TypeError",
        "ReferenceError",
        "Module not found",
        "Cannot find module",
    ];
    // Rust / Cargo
    let rust_patterns: &[&str] = &[
        "error[E",
        "warning[",
        "cannot find",
        "error:",
        "error[",
        "aborting due to",
        "could not compile",
    ];
    // Generic build status lines — always valuable.
    let status_patterns: &[&str] = &[
        "BUILD",
        "FAILURE",
        "SUCCESS",
        "FAILED",
        "PASSED",
        "warning:",
        "warnings generated",
        "error generated",
        "✓",
        "✗",
        "gzip:",
    ];

    let all_patterns: Vec<&str> = java_patterns
        .iter()
        .chain(ts_patterns.iter())
        .chain(rust_patterns.iter())
        .chain(status_patterns.iter())
        .copied()
        .collect();

    // Mark error/diagnostic lines + 2 lines of context around each.
    let mut important: Vec<bool> = vec![false; lines.len()];

    for (i, line) in lines.iter().enumerate() {
        if all_patterns.iter().any(|p| line.contains(p)) {
            let start = i.saturating_sub(2);
            let end = (i + 3).min(lines.len());
            for j in start..end {
                important[j] = true;
            }
        }
    }

    // Always keep first 5 and last 5 lines (command invocation + build status summary).
    const HEAD: usize = 5;
    const TAIL: usize = 5;
    for i in 0..HEAD.min(lines.len()) {
        important[i] = true;
    }
    for i in lines.len().saturating_sub(TAIL)..lines.len() {
        important[i] = true;
    }

    let important_count = important.iter().filter(|&&v| v).count();

    // Only compress if we actually removed a meaningful amount of lines.
    if important_count >= lines.len() * 3 / 4 {
        return None; // Not enough savings — let the general path handle it.
    }

    // Build a deduped error summary so the model sees ALL unique errors at a glance.
    let mut unique_errors: Vec<String> = Vec::new();
    {
        let mut seen = std::collections::HashSet::new();
        for line in lines {
            let trimmed = line.trim();
            // Extract error codes/types: "error[E0433]", "error TS2304", "Error:", etc.
            let is_error = trimmed.contains("error[E")
                || trimmed.contains("error TS")
                || trimmed.starts_with("error:")
                || trimmed.starts_with("Error:")
                || trimmed.contains(": error")
                || trimmed.contains("[ERROR]");
            if is_error {
                // Normalize: take first 100 chars as dedup key
                let key: String = trimmed.chars().take(100).collect();
                if seen.insert(key) {
                    unique_errors.push(trimmed.to_string());
                }
            }
        }
    }

    let mut output = String::new();
    if unique_errors.len() > 1 {
        output.push_str(&format!(
            "[{} unique errors — fix ALL before re-running build:]\n",
            unique_errors.len()
        ));
        for (i, err) in unique_errors.iter().take(15).enumerate() {
            output.push_str(&format!("  {}. {}\n", i + 1, err));
        }
        output.push('\n');
    }

    output.push_str(&assemble_important_lines(lines, &important));
    output.push_str(&format!(
        "\n[{} lines of build output compressed to {} lines — showing errors only]",
        lines.len(),
        important_count,
    ));
    Some(output)
}

/// Assemble output from lines marked as important, collapsing unimportant runs
/// into `[... N lines skipped ...]` markers.
fn assemble_important_lines(lines: &[&str], important: &[bool]) -> String {
    let mut output = String::with_capacity(lines.len() * 40);
    let mut skipping = false;
    let mut skip_count = 0usize;

    for (i, line) in lines.iter().enumerate() {
        if important[i] {
            if skipping {
                output.push_str(&format!("\n[... {} lines skipped ...]\n", skip_count));
                skipping = false;
                skip_count = 0;
            }
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(line);
        } else {
            skipping = true;
            skip_count += 1;
        }
    }
    if skipping {
        output.push_str(&format!("\n[... {} lines skipped ...]", skip_count));
    }

    output
}

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
        let head_part: String = lines[..head].join("\n");
        let tail_part: String = lines[lines.len() - tail..].join("\n");
        result.output = format!(
            "{}\n\n[... {} lines omitted ...]\n\n{}",
            head_part,
            lines.len() - head - tail,
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

    // Pass 1: per-result truncation
    for i in start..len {
        if let MessageContent::ToolResult(ref r) = messages[i].content {
            let mut result = r.clone();
            truncate_output(&mut result, current_tool_name, context_window);
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
    use crate::tool::ToolResult;

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
        }
    }

    // --- truncate_bash tests ---

    #[test]
    fn truncate_bash_short_output_unchanged() {
        let output = "line1\nline2\nline3\n";
        let mut result = make_result(output);
        truncate_bash(&mut result);
        assert_eq!(result.output, output);
    }

    #[test]
    fn truncate_bash_preserves_error_lines() {
        // Create output with >80 lines, including an error line
        let mut lines: Vec<String> = (0..100).map(|i| format!("normal line {}", i)).collect();
        lines[50] = "error: something went wrong".to_string();
        let output = lines.join("\n");
        let mut result = make_result(&output);
        truncate_bash(&mut result);
        // Error line and context should be preserved
        assert!(result.output.contains("error: something went wrong"));
    }

    #[test]
    fn truncate_bash_build_output_compression() {
        // Build output with BUILD SUCCESS keyword
        let mut lines: Vec<String> = (0..200)
            .map(|i| format!("verbose build line {}", i))
            .collect();
        lines[10] = "[INFO] BUILD SUCCESS".to_string();
        lines[11] = "error: compilation failed".to_string();
        let output = lines.join("\n");
        let mut result = make_result(&output);
        truncate_bash(&mut result);
        // Should be compressed significantly
        assert!(result.output.len() < output.len());
        // Should contain BUILD SUCCESS and error lines
        assert!(result.output.contains("BUILD SUCCESS"));
        assert!(result.output.contains("error: compilation failed"));
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
}
