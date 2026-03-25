use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

/// Atomic write: write to temp file then rename. Prevents corruption on crash.
/// Retries rename once after a short delay — dev servers (Vite, webpack) may
/// briefly lock files during hot-reload, causing transient rename failures.
async fn atomic_write(path: &str, content: &str) -> Result<()> {
    let temp = format!("{}.atomcode.tmp", path);
    tokio::fs::write(&temp, content).await
        .with_context(|| format!("Failed to write temp file {}", temp))?;
    match tokio::fs::rename(&temp, path).await {
        Ok(()) => Ok(()),
        Err(_) => {
            // Retry once after 150ms — likely a transient file lock from dev server.
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            match tokio::fs::rename(&temp, path).await {
                Ok(()) => Ok(()),
                Err(_) => {
                    // Final fallback: direct write (not atomic, but better than failing).
                    let _ = tokio::fs::remove_file(&temp).await;
                    tokio::fs::write(path, content).await
                        .with_context(|| format!("Failed to write {}", path))?;
                    Ok(())
                }
            }
        }
    }
}

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

pub struct EditFileTool;

#[derive(Deserialize)]
struct EditFileArgs {
    file_path: String,
    old_string: String,
    new_string: String,
    /// If true, replace ALL occurrences (not just the first unique one).
    /// This is the key feature for bulk style changes — change all "rounded-lg"
    /// to "rounded-xl" in one call without touching business logic.
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for EditFileTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "edit_file",
            description: "Replace text in a file. ALWAYS prefer this over write_file for existing files.\n\
                Usage:\n\
                - You MUST read the file with read_file before editing it. The edit will fail if you haven't read it.\n\
                - old_string and new_string must be different.\n\
                - When copying text from read_file output, preserve exact indentation (tabs/spaces) as shown AFTER the line number prefix.\n\
                Modes:\n\
                - replace_all=false (default): old_string must be unique in the file. Replaces one occurrence.\n\
                  If old_string matches multiple times, provide more surrounding context lines to make it unique.\n\
                - replace_all=true: Replaces ALL occurrences. Use for: renaming variables, changing CSS classes, \
                  updating colors, bulk find-replace.\n\
                  Example: {\"old_string\": \"bg-green-500\", \"new_string\": \"bg-blue-500\", \"replace_all\": true}\n\
                Behavior:\n\
                - If old_string is not found, the tool will show the closest match to help you correct it.\n\
                - This is SAFE — it only changes matched text, preserving all surrounding code, imports, and logic.\n\
                - NEVER use write_file to modify existing files. edit_file prevents accidental deletion of code you forgot to include.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Path to the file to edit"
                    },
                    "old_string": {
                        "type": "string",
                        "description": "Text to find. For replace_all=false, must be unique in the file."
                    },
                    "new_string": {
                        "type": "string",
                        "description": "Replacement text"
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "Replace ALL occurrences (true) or just one unique match (false, default). Use true for bulk CSS/style changes."
                    }
                },
                "required": ["file_path", "old_string", "new_string"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: EditFileArgs = serde_json::from_str(args)?;

        let content = tokio::fs::read_to_string(&parsed.file_path)
            .await
            .with_context(|| format!("Failed to read {}", parsed.file_path))?;

        let count = content.matches(&parsed.old_string).count();

        if count == 0 {
            // No match — show closest match with a ready-to-use suggested old_string.
            // This lets the model retry immediately without re-reading the file.
            let (hint, suggested_old) = find_closest_match_with_suggestion(&content, &parsed.old_string);
            let suggestion = if let Some(ref s) = suggested_old {
                format!(
                    "\n\n[SUGGESTED FIX: Use this exact text as old_string (copy it precisely):]\n```\n{}\n```",
                    s
                )
            } else {
                String::new()
            };
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!("Error: old_string not found in {}.\n{}{}", parsed.file_path, hint, suggestion),
                success: false,
            });
        }

        // Safety check: warn about large deletions
        let old_lines = parsed.old_string.lines().count();
        let new_lines = parsed.new_string.lines().count();
        let net_deleted = old_lines.saturating_sub(new_lines);
        let deletion_warning = if net_deleted > 10 {
            format!(
                "\nWARNING: You removed {} more lines than you added. If you only meant to ADD a skeleton/loading section, \
                 use v-if/v-else to show it ALONGSIDE the existing content, not INSTEAD of it.",
                net_deleted
            )
        } else {
            String::new()
        };

        if parsed.replace_all {
            // Safety check: warn about high replacement count
            let replace_warning = if count > 10 {
                format!(
                    "\nWARNING: Replaced {} occurrences. This many replacements may have changed structural \
                     elements (tags, brackets) that should not be bulk-replaced. Verify the file structure.",
                    count
                )
            } else {
                String::new()
            };

            let new_content = content.replace(&parsed.old_string, &parsed.new_string);
            atomic_write(&parsed.file_path, &new_content).await?;
            let diff = build_compact_diff(&parsed.old_string, &parsed.new_string);
            let outline = file_outline(&new_content);
            Ok(ToolResult {
                call_id: String::new(),
                output: format!(
                    "Edited {} (replaced {} occurrence{}).\n{}\n{}",
                    parsed.file_path, count, if count > 1 { "s" } else { "" }, diff, outline,
                ),
                success: true,
            })
        } else {
            if count > 1 {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: format!(
                        "Error: old_string found {} times in {}. Use replace_all=true to replace all, or provide more context to make it unique.",
                        count, parsed.file_path
                    ),
                    success: false,
                });
            }

            let new_content = content.replacen(&parsed.old_string, &parsed.new_string, 1);
            atomic_write(&parsed.file_path, &new_content).await?;

            let removed = parsed.old_string.lines().count();
            let added = parsed.new_string.lines().count();
            let diff = build_compact_diff(&parsed.old_string, &parsed.new_string);
            let outline = file_outline(&new_content);
            Ok(ToolResult {
                call_id: String::new(),
                output: format!(
                    "Edited {} (-{} +{} lines).\n{}\n{}",
                    parsed.file_path, removed, added, diff, outline,
                ),
                success: true,
            })
        }
    }
}

/// Try fuzzy matching: normalize whitespace (trim each line) and try to match.
/// `replace_all` controls whether all matches or just a unique one should be replaced.
fn try_fuzzy_replace(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Option<(String, usize)> {
    let old_normalized: Vec<&str> = old_string.lines().map(|l| l.trim()).collect();
    if old_normalized.is_empty() || old_normalized.iter().all(|l| l.is_empty()) {
        return None;
    }

    let content_lines: Vec<&str> = content.lines().collect();
    let has_trailing_newline = content.ends_with('\n');
    let mut matches: Vec<(usize, usize)> = Vec::new();

    // Only attempt fuzzy match if old_string has substantial content (not just short fragments)
    let total_non_ws: usize = old_normalized.iter().map(|l| l.len()).sum();
    if total_non_ws < 10 {
        return None; // Too short for reliable fuzzy matching
    }

    // Slide window — skip overlapping matches
    let mut i = 0;
    while i + old_normalized.len() <= content_lines.len() {
        let window: Vec<&str> = content_lines[i..i + old_normalized.len()]
            .iter()
            .map(|l| l.trim())
            .collect();
        if window == old_normalized {
            matches.push((i, i + old_normalized.len()));
            i += old_normalized.len(); // skip past this match
        } else {
            i += 1;
        }
    }

    if matches.is_empty() {
        return None;
    }

    // If replace_all=false, require exactly one match
    if !replace_all && matches.len() > 1 {
        return None; // caller will handle the "multiple matches" error
    }

    // Compute the base indent of new_string (to preserve relative indentation)
    let new_lines: Vec<&str> = new_string.lines().collect();
    let new_base_indent = new_lines.iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);

    let mut result_lines: Vec<String> = content_lines.iter().map(|l| l.to_string()).collect();

    // Process matches in reverse to preserve indices
    let to_replace = if replace_all { &matches[..] } else { &matches[..1] };
    for &(start, end) in to_replace.iter().rev() {
        // Detect indentation from the first matched line in the file
        let original_line = content_lines[start];
        let file_indent = original_line.len() - original_line.trim_start().len();
        let file_indent_str: String = original_line.chars().take(file_indent).collect();

        // Build replacement preserving RELATIVE indentation from new_string
        let replacement: Vec<String> = new_lines.iter().map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                let line_indent = l.len() - l.trim_start().len();
                let relative = line_indent.saturating_sub(new_base_indent);
                let total_indent = format!("{}{}", file_indent_str, " ".repeat(relative));
                format!("{}{}", total_indent, l.trim())
            }
        }).collect();

        result_lines.splice(start..end, replacement);
    }

    let mut result = result_lines.join("\n");
    // Preserve trailing newline
    if has_trailing_newline && !result.ends_with('\n') {
        result.push('\n');
    }

    let count = if replace_all { matches.len() } else { 1 };
    Some((result, count))
}

/// Show the surrounding context after an edit so the model doesn't need to re-read.
/// Returns ~10 lines around the replacement location with line numbers.
fn surrounding_context(new_content: &str, new_string: &str) -> String {
    let lines: Vec<&str> = new_content.lines().collect();
    let new_first = new_string.lines().next().unwrap_or("").trim();

    if new_first.is_empty() || lines.len() <= 15 {
        return String::new(); // Small file or empty replacement — not needed
    }

    // Find where the replacement is
    let center = lines.iter().position(|l| l.trim() == new_first).unwrap_or(0);
    let start = center.saturating_sub(5);
    let end = (center + 10).min(lines.len());

    let mut ctx = String::from("[Context around edit — do NOT re-read this file:]\n");
    for i in start..end {
        ctx.push_str(&format!("{:>4}| {}\n", i + 1, lines[i]));
    }
    if end < lines.len() {
        ctx.push_str(&format!("     ... ({} more lines)\n", lines.len() - end));
    }
    ctx
}

/// Post-edit context: give the model the file's current state so it doesn't re-read.
///
/// - Files <= 500 lines: return the FULL file with line numbers.
///   This eliminates re-reads entirely — the model has everything in the latest message.
/// - Files > 500 lines: return outline + 40 lines of surrounding context around the edit.
fn post_edit_context(new_content: &str, new_string: &str) -> String {
    let lines: Vec<&str> = new_content.lines().collect();

    if lines.len() <= 500 {
        // Full file — model has zero reason to re-read.
        let mut out = format!(
            "\n[Full file after edit ({} lines) — do NOT re-read this file:]\n",
            lines.len()
        );
        for (i, line) in lines.iter().enumerate() {
            out.push_str(&format!("{:>4}| {}\n", i + 1, line));
        }
        return out;
    }

    // Large file: outline + surrounding context around the edit location.
    let outline = file_outline(new_content);

    // Find where the new content was inserted.
    let new_first = new_string.lines().next().unwrap_or("").trim();
    let center = if !new_first.is_empty() {
        lines.iter().position(|l| l.trim().contains(new_first)).unwrap_or(0)
    } else {
        0
    };

    let start = center.saturating_sub(20);
    let end = (center + 20).min(lines.len());

    let mut ctx = format!(
        "\n[File after edit ({} lines). Context around edit (lines {}-{}):]:\n",
        lines.len(), start + 1, end
    );
    for i in start..end {
        ctx.push_str(&format!("{:>4}| {}\n", i + 1, lines[i]));
    }
    if end < lines.len() {
        ctx.push_str(&format!("     ... ({} more lines)\n", lines.len() - end));
    }

    format!("{}\n{}", outline, ctx)
}

/// Build a structural outline of the file after edit.
/// Shows top-level lines (indent 0-1) with line numbers so the model
/// knows the file's structure and can plan its next edit without re-reading.
/// Only generated for files > 100 lines (small files don't need it).
fn file_outline(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= 100 {
        return String::new(); // Small file — diff is enough context.
    }

    let mut outline = format!("[File outline ({} lines) — do NOT re-read this file:]\n", lines.len());
    let mut count = 0;
    let max_outline_lines = 30;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        // Indent 0-1 = top-level declaration. Also include <template>, <script>, <style> tags.
        if indent <= 1 || trimmed.starts_with('<') && (
            trimmed.starts_with("<template") || trimmed.starts_with("</template")
            || trimmed.starts_with("<script") || trimmed.starts_with("</script")
            || trimmed.starts_with("<style") || trimmed.starts_with("</style")
        ) {
            // Truncate long lines for the outline
            let display = if trimmed.chars().count() > 60 {
                format!("{}...", trimmed.chars().take(57).collect::<String>())
            } else {
                trimmed.to_string()
            };
            outline.push_str(&format!("{:>4}| {}\n", i + 1, display));
            count += 1;
            if count >= max_outline_lines {
                outline.push_str(&format!("     ... ({} more lines)\n", lines.len() - i - 1));
                break;
            }
        }
    }
    outline
}

/// Build a compact diff showing removed/added lines (max 8 lines total).
fn build_compact_diff(old: &str, new: &str) -> String {
    let mut diff = String::new();
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    let max_show = 4; // max lines per side

    // Show removed lines (prefixed with -)
    for (i, line) in old_lines.iter().take(max_show).enumerate() {
        diff.push_str(&format!("- {}\n", line));
        if i == max_show - 1 && old_lines.len() > max_show {
            diff.push_str(&format!("  ... ({} more removed)\n", old_lines.len() - max_show));
        }
    }

    // Show added lines (prefixed with +)
    for (i, line) in new_lines.iter().take(max_show).enumerate() {
        diff.push_str(&format!("+ {}\n", line));
        if i == max_show - 1 && new_lines.len() > max_show {
            diff.push_str(&format!("  ... ({} more added)\n", new_lines.len() - max_show));
        }
    }

    diff.trim_end().to_string()
}

/// Find the closest match and return (hint_message, suggested_old_string).
/// The suggested_old_string is the exact text from the file that the model
/// should use — it can copy-paste this into old_string to retry immediately
/// without re-reading the file.
fn find_closest_match_with_suggestion(content: &str, old_string: &str) -> (String, Option<String>) {
    let old_lines: Vec<&str> = old_string.lines().collect();
    let content_lines: Vec<&str> = content.lines().collect();

    if old_lines.is_empty() {
        return ("old_string is empty. Use read_file to re-read the file.".to_string(), None);
    }

    let old_first_trimmed = old_lines[0].trim();
    if old_first_trimmed.is_empty() && old_lines.len() > 1 {
        let hint = find_closest_match(content, old_string);
        return (hint, None);
    }

    // Try to find where the first line matches (trimmed) in the file
    for (i, line) in content_lines.iter().enumerate() {
        if line.trim() == old_first_trimmed {
            // Found potential match start. Extract the same number of lines from file.
            let end = (i + old_lines.len()).min(content_lines.len());
            let actual_lines = &content_lines[i..end];

            // Check if it's a plausible match (at least 30% of lines match trimmed)
            let matching = actual_lines.iter().zip(old_lines.iter())
                .filter(|(a, b)| a.trim() == b.trim())
                .count();

            if matching >= old_lines.len() / 3 || matching >= 2 {
                let suggested = actual_lines.join("\n");
                let hint = find_closest_match(content, old_string);
                return (hint, Some(suggested));
            }
        }
    }

    let hint = find_closest_match(content, old_string);
    (hint, None)
}

/// Find the closest matching region in the file to help the model fix old_string.
/// Three strategies: (1) whitespace-normalized multi-line match, (2) first-line match, (3) keyword search.
fn find_closest_match(content: &str, old_string: &str) -> String {
    let old_lines: Vec<&str> = old_string.lines().collect();
    let content_lines: Vec<&str> = content.lines().collect();

    if old_lines.is_empty() {
        return "old_string is empty. Use read_file to re-read the file.".to_string();
    }

    let old_first_trimmed = old_lines[0].trim();
    if old_first_trimmed.is_empty() && old_lines.len() > 1 {
        // First line is empty — try second line
        return find_closest_match_inner(content, &content_lines, old_lines[1].trim(), &old_lines);
    }

    find_closest_match_inner(content, &content_lines, old_first_trimmed, &old_lines)
}

fn find_closest_match_inner(
    _content: &str,
    content_lines: &[&str],
    first_line_trimmed: &str,
    old_lines: &[&str],
) -> String {
    if first_line_trimmed.is_empty() {
        return "old_string appears empty after trimming. Use read_file to re-read the file.".to_string();
    }

    // Strategy 1: Find where the first line matches (trimmed) and show divergence point
    let mut candidates: Vec<(usize, usize)> = Vec::new(); // (line_idx, match_score)

    for (i, line) in content_lines.iter().enumerate() {
        let trimmed = line.trim();
        // Exact trimmed match of first line
        if trimmed == first_line_trimmed {
            // Check how many subsequent lines also match (trimmed)
            let mut match_count = 1;
            for j in 1..old_lines.len() {
                if i + j >= content_lines.len() { break; }
                if content_lines[i + j].trim() == old_lines[j].trim() {
                    match_count += 1;
                } else {
                    break;
                }
            }
            candidates.push((i, match_count));
        }
        // Substring match of first line
        else if trimmed.contains(first_line_trimmed) || first_line_trimmed.contains(trimmed) {
            candidates.push((i, 0));
        }
        // Prefix match (first 25 chars)
        else if trimmed.len() > 15 && first_line_trimmed.len() > 15
            && trimmed.chars().take(25).collect::<String>()
                == first_line_trimmed.chars().take(25).collect::<String>()
        {
            candidates.push((i, 0));
        }
    }

    // Sort by match_count (highest first)
    candidates.sort_by(|a, b| b.1.cmp(&a.1));

    if let Some(&(best_idx, match_count)) = candidates.first() {
        let start = best_idx.saturating_sub(1);
        // Cap snippet to 20 lines max — large snippets waste context without helping
        let end = (best_idx + old_lines.len().min(18) + 2).min(content_lines.len());

        let mut snippet = String::new();
        for i in start..end {
            snippet.push_str(&format!("{:>4}| {}\n", i + 1, content_lines[i]));
        }
        if best_idx + old_lines.len() + 2 > end {
            snippet.push_str(&format!("     ... ({} more lines in file)\n", content_lines.len() - end));
        }

        // If some lines matched but not all, show exactly where the divergence is
        if match_count > 0 && match_count < old_lines.len() && best_idx + match_count < content_lines.len() {
            let diverge_idx = match_count;
            let file_line = content_lines[best_idx + diverge_idx].trim();
            let old_line = old_lines[diverge_idx].trim();

            // Detect indentation mismatch
            let file_indent = content_lines[best_idx].len() - content_lines[best_idx].trim_start().len();
            let old_indent = old_lines[0].len() - old_lines[0].trim_start().len();

            let mut hint = format!(
                "First {} line(s) match (trimmed) but line {} diverges:\n\
                 YOUR old_string line {}: \"{}\"\n\
                 ACTUAL file line {}:     \"{}\"\n",
                match_count, diverge_idx + 1,
                diverge_idx + 1, old_line,
                best_idx + diverge_idx + 1, file_line,
            );

            if file_indent != old_indent {
                hint.push_str(&format!(
                    "INDENTATION MISMATCH: file uses {} spaces, your old_string uses {} spaces.\n",
                    file_indent, old_indent,
                ));
            }

            return format!(
                "Partial match at lines {}-{} ({}/{} lines match).\n{}\n{}\n\
                 Copy the EXACT text from above (including indentation) for old_string.",
                best_idx + 1, end, match_count, old_lines.len(), snippet, hint
            );
        }

        // Indentation-only mismatch detection
        if match_count == 0 {
            let file_indent = content_lines[best_idx].len() - content_lines[best_idx].trim_start().len();
            let old_indent = old_lines[0].len() - old_lines[0].trim_start().len();
            if file_indent != old_indent
                && content_lines[best_idx].trim() == old_lines[0].trim()
            {
                return format!(
                    "INDENTATION MISMATCH at line {}. File uses {} spaces, your old_string uses {} spaces.\n\
                     Actual file content:\n{}\n\
                     Copy the EXACT text (with correct indentation) for old_string.",
                    best_idx + 1, file_indent, old_indent, snippet
                );
            }
        }

        return format!(
            "Closest match found near line {}:\n{}\n\
             Copy the EXACT text from above for old_string (preserve indentation).",
            best_idx + 1, snippet
        );
    }

    // Strategy 2: keyword-based search — find lines containing distinctive words from old_string
    let keywords: Vec<&str> = first_line_trimmed.split_whitespace()
        .filter(|w| w.len() > 3 && !matches!(*w, "const" | "let" | "var" | "this" | "self" | "return" | "from" | "import" | "function"))
        .take(3)
        .collect();

    if !keywords.is_empty() {
        for (i, line) in content_lines.iter().enumerate() {
            let lower = line.to_lowercase();
            if keywords.iter().all(|kw| lower.contains(&kw.to_lowercase())) {
                let start = i.saturating_sub(2);
                let end = (i + 5).min(content_lines.len());
                let mut snippet = String::new();
                for j in start..end {
                    snippet.push_str(&format!("{:>4}| {}\n", j + 1, content_lines[j]));
                }
                return format!(
                    "No exact match, but keywords [{}] found near line {}:\n{}\n\
                     Use read_file with offset={} limit=20 to see the exact content.",
                    keywords.join(", "), i + 1, snippet, start + 1
                );
            }
        }
    }

    format!(
        "No similar text found in the file ({} lines total). \
         The content may have changed. Use read_file to re-read the file.",
        content_lines.len()
    )
}
