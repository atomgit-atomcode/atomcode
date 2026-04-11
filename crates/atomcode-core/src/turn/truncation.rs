use crate::conversation::message::{Message, MessageContent};
use crate::tool::ToolResult;

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
        "[ERROR]", "error:", "cannot find symbol", "package does not exist",
        "incompatible types", "unreported exception", "method does not override",
    ];
    // TypeScript / Node
    let ts_patterns: &[&str] = &[
        "error TS", "Error:", "SyntaxError", "TypeError", "ReferenceError",
        "Module not found", "Cannot find module",
    ];
    // Rust / Cargo
    let rust_patterns: &[&str] = &[
        "error[E", "warning[", "cannot find", "error:", "error[",
        "aborting due to", "could not compile",
    ];
    // Generic build status lines — always valuable.
    let status_patterns: &[&str] = &[
        "BUILD", "FAILURE", "SUCCESS", "FAILED", "PASSED",
        "warning:", "warnings generated", "error generated",
        "✓", "✗", "gzip:",
    ];

    let all_patterns: Vec<&str> = java_patterns.iter()
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
            let is_error = trimmed.contains("error[E") || trimmed.contains("error TS")
                || trimmed.starts_with("error:") || trimmed.starts_with("Error:")
                || trimmed.contains(": error") || trimmed.contains("[ERROR]");
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
        output.push_str(&format!("[{} unique errors — fix ALL before re-running build:]\n", unique_errors.len()));
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

/// Apply truncation to all tool result messages
/// in the last `tool_count` messages of the conversation.
pub fn post_process_tool_results(
    messages: &mut Vec<Message>,
    tool_count: usize,
    current_tool_name: &str,
    context_window: usize,
) {
    let len = messages.len();
    let start = len.saturating_sub(tool_count);

    for i in start..len {
        if let MessageContent::ToolResult(ref r) = messages[i].content {
            let mut result = r.clone();
            truncate_output(&mut result, current_tool_name, context_window);
            messages[i].content = MessageContent::ToolResult(result);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolResult;
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
    fn post_process_truncates_results() {
        let large_output = "x".repeat(20000);
        let mut messages = vec![make_tool_result_message(&large_output)];
        post_process_tool_results(&mut messages, 1, "unknown_tool", 16000);
        // Should be truncated but remain inline ToolResult
        assert!(matches!(messages[0].content, MessageContent::ToolResult(_)));
        if let MessageContent::ToolResult(ref r) = messages[0].content {
            assert!(r.output.len() <= 16100);
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
