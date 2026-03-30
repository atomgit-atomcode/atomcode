use crate::conversation::message::{Message, MessageContent};
use crate::tool::ToolResult;
use crate::tool::result_store::ToolResultStore;

/// Dispatch to per-tool truncation based on tool name, then apply a hard char limit.
/// `context_window` drives the hard char limit — larger context windows allow more output.
pub fn truncate_output(result: &mut ToolResult, tool_name: &str, context_window: usize) {
    match tool_name {
        "bash" => truncate_bash(result),
        "read_file" => truncate_read_file(result),
        "web_fetch" => truncate_generic(result, 150, 20, 40),
        _ => truncate_generic(result, 200, 30, 50),
    }
    // Hard char limit as a safety net — scales with context window.
    let char_limit = (context_window).max(16000);
    if result.output.len() > char_limit {
        result.output = result.output.chars().take(char_limit).collect::<String>()
            + &format!("\n[output truncated at {} chars]", char_limit);
    }
}

/// Bash: preserve error lines, strip verbose build noise.
/// Errors are the highest-value signal — keep all lines containing "error",
/// "Error", "FAILED", "STDERR", "panic", plus surrounding context.
fn truncate_bash(result: &mut ToolResult) {
    // Smart build output compression: maven/gradle/npm build output is very verbose
    // but only SUCCESS/FAILURE + error lines matter.
    let is_build_output = result.output.contains("BUILD SUCCESS")
        || result.output.contains("BUILD FAILURE")
        || result.output.contains("Compiled successfully")
        || result.output.contains("compiled successfully")
        || result.output.contains("vite build")
        || result.output.contains("vue-tsc");
    if is_build_output {
        let lines: Vec<&str> = result.output.lines().collect();
        let mut key_lines: Vec<String> = Vec::new();
        for line in &lines {
            let trimmed = line.trim();
            if trimmed.contains("ERROR") || trimmed.contains("error")
                || trimmed.contains("FAILURE") || trimmed.contains("SUCCESS")
                || trimmed.contains("BUILD") || trimmed.contains("warning:")
                || trimmed.contains("✓") || trimmed.contains("✗")
                || trimmed.starts_with("> ") // npm script output
                || trimmed.contains("gzip:")  // vite bundle size
            {
                key_lines.push(line.to_string());
            }
        }
        if !key_lines.is_empty() && key_lines.len() < lines.len() / 2 {
            result.output = key_lines.join("\n");
            return;
        }
    }

    let lines: Vec<&str> = result.output.lines().collect();
    if lines.len() <= 80 {
        return; // Short enough — keep everything.
    }

    // Phase 1: Identify error/important lines.
    // Generic error patterns — no language-specific strings.
    let error_patterns = ["error", "Error", "ERROR", "FAILED", "STDERR:",
        "panic", "Panic", "PANIC", "not found", "No such file",
        "Permission denied", "cannot find", "undefined", "unresolved"];
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
    let mut output = String::with_capacity(result.output.len() / 2);
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

    result.output = output;
}

/// read_file: if output is very long, extract an outline — keep top-level
/// declarations (lines at indent level 0-1) plus head/tail for orientation.
/// Tech-stack agnostic: uses indentation depth as a universal proxy for
/// "important structural line" — works across all languages.
///
/// Threshold is 2000 lines. Truncating forces multi-read/multi-edit cycles
/// that waste far more tokens than keeping the full file.
/// At 32K context window, 2000 lines ≈ 8000 tokens = 25% of budget.
/// Files over 2000 lines are extremely rare in practice.
fn truncate_read_file(result: &mut ToolResult) {
    let lines: Vec<&str> = result.output.lines().collect();
    if lines.len() <= 2000 {
        return;
    }

    // Always keep first 30 and last 20 lines (file header/imports + end).
    const HEAD: usize = 30;
    const TAIL: usize = 20;

    let mut important: Vec<bool> = vec![false; lines.len()];

    // Head and tail.
    for i in 0..HEAD.min(lines.len()) {
        important[i] = true;
    }
    for i in lines.len().saturating_sub(TAIL)..lines.len() {
        important[i] = true;
    }

    // Top-level lines in the middle: detect by indentation depth.
    // read_file output has line-number prefix: "  123| content"
    // Extract content after "| " and check its indent level.
    for (i, line) in lines.iter().enumerate() {
        // Extract the actual code content after the line-number prefix.
        let content = if let Some(pos) = line.find("| ") {
            &line[pos + 2..]
        } else {
            line
        };

        // Skip empty/whitespace-only lines.
        if content.trim().is_empty() {
            continue;
        }

        // Count leading whitespace (spaces or tabs).
        let indent = content.len() - content.trim_start().len();
        // Indent 0-1 = top-level declaration (function, class, struct, etc.)
        // across virtually all languages.
        if indent <= 1 && content.trim().len() > 2 {
            important[i] = true;
            // Include the line below (often opening brace, docstring, or type annotation).
            if i + 1 < lines.len() {
                important[i + 1] = true;
            }
        }
    }

    // Assemble with skip markers.
    let mut output = String::with_capacity(result.output.len() / 2);
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

    result.output = output;
}

/// Generic truncation: head + tail, skipping middle.
pub(crate) fn truncate_generic(result: &mut ToolResult, max_lines: usize, head: usize, tail: usize) {
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

/// Apply truncation and disk externalization to all tool result messages
/// in the last `tool_count` messages of the conversation.
pub fn post_process_tool_results(
    messages: &mut Vec<Message>,
    tool_count: usize,
    current_tool_name: &str,
    result_store: &ToolResultStore,
    context_window: usize,
) {
    let len = messages.len();
    let start = len.saturating_sub(tool_count);

    // Collect indices of ToolResult messages to process
    let mut to_process: Vec<usize> = Vec::new();
    for i in start..len {
        if matches!(messages[i].content, MessageContent::ToolResult(_)) {
            to_process.push(i);
        }
    }

    // Phase 1: Truncate outputs — extract, truncate, put back to satisfy borrow checker
    for &i in &to_process {
        if let MessageContent::ToolResult(ref r) = messages[i].content {
            let mut result = r.clone();
            truncate_output(&mut result, current_tool_name, context_window);
            messages[i].content = MessageContent::ToolResult(result);
        }
    }

    // Phase 2: Externalize large results to disk (replace ToolResult with ToolResultRef)
    for &i in &to_process {
        let should_externalize = if let MessageContent::ToolResult(ref r) = messages[i].content {
            r.output.len() >= 512
        } else {
            false
        };

        if should_externalize {
            if let MessageContent::ToolResult(ref result) = messages[i].content {
                let result_ref = result_store.store(result);
                messages[i].content = MessageContent::ToolResultRef(result_ref);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolResult;
    use crate::tool::result_store::ToolResultStore;
    use crate::conversation::message::{Message, MessageContent, Role};

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

    fn temp_store() -> (ToolResultStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = ToolResultStore::new(dir.path().to_path_buf());
        (store, dir)
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
        let mut lines: Vec<String> = (0..200).map(|i| format!("verbose build line {}", i)).collect();
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

    // --- truncate_read_file tests ---

    #[test]
    fn truncate_read_file_short_file_unchanged() {
        let output = (0..100).map(|i| format!("   {}| line content", i)).collect::<Vec<_>>().join("\n");
        let mut result = make_result(&output);
        truncate_read_file(&mut result);
        assert_eq!(result.output, output);
    }

    #[test]
    fn truncate_read_file_long_file_extracts_outline() {
        // Create a 2001-line "file" with line-number prefixes
        let lines: Vec<String> = (0..2001).map(|i| {
            if i % 100 == 0 {
                format!("   {}| fn function_{}", i, i)
            } else {
                format!("   {}|     body line {}", i, i)
            }
        }).collect();
        let output = lines.join("\n");
        let mut result = make_result(&output);
        truncate_read_file(&mut result);
        // Should be shorter than original
        assert!(result.output.len() < output.len());
        // Should contain skip markers
        assert!(result.output.contains("lines skipped"));
    }

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

    // --- truncate_output hard char limit test ---

    #[test]
    fn truncate_output_hard_char_limit() {
        // Create output that's way over 16000 chars (minimum limit)
        let output = "x".repeat(20000);
        let mut result = make_result(&output);
        truncate_output(&mut result, "unknown_tool", 16000);
        assert!(result.output.len() <= 16000 + 100); // allow for truncation message
        assert!(result.output.contains("[output truncated at 16000 chars]"));
    }

    // --- post_process_tool_results tests ---

    #[test]
    fn post_process_externalizes_large_results() {
        let (store, _dir) = temp_store();
        let large_output = "x".repeat(600); // over 512 threshold
        let mut messages = vec![make_tool_result_message(&large_output)];
        post_process_tool_results(&mut messages, 1, "bash", &store, 16000);
        // Should have been externalized to ToolResultRef
        assert!(matches!(messages[0].content, MessageContent::ToolResultRef(_)));
    }

    #[test]
    fn post_process_keeps_small_results_inline() {
        let (store, _dir) = temp_store();
        let small_output = "short output";
        let mut messages = vec![make_tool_result_message(small_output)];
        post_process_tool_results(&mut messages, 1, "bash", &store, 16000);
        // Should remain as ToolResult (small enough)
        assert!(matches!(messages[0].content, MessageContent::ToolResult(_)));
    }
}
