use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

/// Atomic write: write to temp file then rename. Prevents corruption on crash.
async fn atomic_write(path: &str, content: &str) -> Result<()> {
    let temp = format!("{}.atomcode.tmp", path);
    tokio::fs::write(&temp, content).await
        .with_context(|| format!("Failed to write temp file {}", temp))?;
    tokio::fs::rename(&temp, path).await
        .with_context(|| format!("Failed to rename {} to {}", temp, path))?;
    Ok(())
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
                Modes:\n\
                - replace_all=false (default): old_string must be unique. Replaces one occurrence.\n\
                - replace_all=true: Replaces ALL occurrences. Use for: renaming variables, changing CSS classes, \
                  updating colors, bulk find-replace. Example: {\"old_string\": \"bg-green-500\", \"new_string\": \"bg-blue-500\", \"replace_all\": true}\n\n\
                IMPORTANT:\n\
                - When editing text from read_file output, preserve the exact indentation (tabs/spaces).\n\
                - If old_string is not found, the tool will suggest the closest match.\n\
                - If old_string matches multiple times and replace_all=false, provide more surrounding context to make it unique.\n\
                - This is SAFE — it only changes matched text, preserving all surrounding code, imports, and logic.",
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
            // NO fuzzy matching — Claude Code doesn't have it and works better without it.
            // Fuzzy matching was silently matching wrong content and causing destructive edits.
            // Failing explicitly forces the model to re-read and provide the correct old_string.

            // No match at all — show closest match
            let hint = find_closest_match(&content, &parsed.old_string);
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!("Error: old_string not found in {}.\n{}", parsed.file_path, hint),
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
            let context = surrounding_context(&new_content, &parsed.new_string);
            Ok(ToolResult {
                call_id: String::new(),
                output: format!(
                    "Edited {} (replaced {} occurrence{}).{}{}\n{}\n{}",
                    parsed.file_path, count, if count > 1 { "s" } else { "" },
                    replace_warning, deletion_warning, diff, context
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

            let diff = build_compact_diff(&parsed.old_string, &parsed.new_string);
            let context = surrounding_context(&new_content, &parsed.new_string);
            let removed = parsed.old_string.lines().count();
            let added = parsed.new_string.lines().count();
            Ok(ToolResult {
                call_id: String::new(),
                output: format!(
                    "Edited {} (-{} +{} lines).{}\n{}\n{}",
                    parsed.file_path, removed, added, deletion_warning, diff, context
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

/// Find the closest matching line in the file to help the model fix old_string.
fn find_closest_match(content: &str, old_string: &str) -> String {
    let old_first_line = old_string.lines().next().unwrap_or("").trim();
    if old_first_line.is_empty() {
        return "No similar text found. Use read_file to re-read the file.".to_string();
    }

    let best = content.lines().enumerate()
        .filter(|(_, line)| {
            let trimmed = line.trim();
            trimmed.contains(old_first_line) ||
            old_first_line.contains(trimmed) ||
            (trimmed.len() > 10 && old_first_line.len() > 10 &&
             trimmed.chars().take(20).collect::<String>() == old_first_line.chars().take(20).collect::<String>())
        })
        .next()
        .map(|(i, _)| {
            let start = i.saturating_sub(1);
            let end = (i + 5).min(content.lines().count());
            let lines: Vec<&str> = content.lines().collect();
            lines[start..end].iter()
                .enumerate()
                .map(|(j, l)| format!("{:>4}| {}", start + j + 1, l))
                .collect::<Vec<_>>()
                .join("\n")
        });

    match best {
        Some(snippet) => format!(
            "Closest match found near:\n{}\nCopy the EXACT text from above for old_string.",
            snippet
        ),
        None => "No similar text found. Use read_file to re-read the file.".to_string(),
    }
}
