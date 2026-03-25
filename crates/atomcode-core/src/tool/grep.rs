use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::process::Command;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

pub struct GrepTool;

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    path: Option<String>,
    #[serde(default = "default_max_results")]
    max_results: usize,
    /// Lines of context around each match (default 3)
    #[serde(default = "default_context")]
    context: usize,
}

fn default_context() -> usize { 3 }

fn default_max_results() -> usize { 50 }

#[async_trait]
impl Tool for GrepTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "grep",
            description: "Search file contents for a pattern using ripgrep. Returns matching lines with surrounding context.\n\
                Usage:\n\
                - Use this to find where a function, variable, string, or UI element is defined or used.\n\
                - Use this BEFORE editing when the user's request is ambiguous — find ALL candidates first.\n\
                - Pattern is regex by default (smart-case: case-insensitive unless uppercase is used).\n\
                - Escape special regex chars: . → \\\\. , ( → \\\\( , [ → \\\\[\n\
                - If regex fails, the tool automatically retries with literal string matching.\n\
                - NEVER use bash grep/rg — always use this tool.\n\
                Examples:\n\
                - Find a function: {\"pattern\": \"def process_data\"}\n\
                - Find a string with dots: {\"pattern\": \"console\\\\.log\"}\n\
                - Find across alternatives: {\"pattern\": \"upload|上传\"}\n\
                - Search specific directory: {\"pattern\": \"import\", \"path\": \"src/views\"}",
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Search pattern (regex by default). Escape dots/parens: console\\.log\\(" },
                    "path": { "type": "string", "description": "Directory or file to search (default: working directory)" },
                    "max_results": { "type": "integer", "description": "Max results to return (default 50)" },
                    "context": { "type": "integer", "description": "Lines of context around each match (default 3)" }
                },
                "required": ["pattern"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: GrepArgs = serde_json::from_str(args)?;
        let path = parsed.path.as_deref().unwrap_or(".");
        let max = parsed.max_results;
        let context_lines = parsed.context.min(10);

        // Resolve path against working directory
        let wd = ctx.working_dir.read().await.clone();
        let resolved_path = if std::path::Path::new(path).is_absolute() {
            path.to_string()
        } else {
            wd.join(path).to_string_lossy().to_string()
        };

        // Try ripgrep first
        let output = Command::new("rg")
            .args(&[
                "--line-number", "--no-heading", "--color=never",
                "--smart-case",  // lowercase pattern = case-insensitive (like Claude Code)
                &format!("--max-count={}", max),
                &format!("--context={}", context_lines),
                &parsed.pattern, &resolved_path,
            ])
            .current_dir(&wd)
            .output()
            .await
            .or_else(|_| {
                // Fallback: grep on Unix, findstr on Windows
                #[cfg(target_os = "windows")]
                {
                    std::process::Command::new("findstr")
                        .args(&["/S", "/N", "/I",
                                &parsed.pattern, &format!("{}\\*", resolved_path)])
                        .current_dir(&wd)
                        .output()
                }
                #[cfg(not(target_os = "windows"))]
                {
                    std::process::Command::new("grep")
                        .args(&["-rn", "--color=never",
                                &format!("-C{}", context_lines),
                                &parsed.pattern, &resolved_path])
                        .current_dir(&wd)
                        .output()
                }
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        // If regex search failed (bad pattern or no results), auto-retry with --fixed-strings
        let has_metachar = parsed.pattern.contains('(') || parsed.pattern.contains('[')
            || parsed.pattern.contains('{') || parsed.pattern.contains('.')
            || parsed.pattern.contains('*') || parsed.pattern.contains('+')
            || parsed.pattern.contains('?') || parsed.pattern.contains('|');

        let (final_stdout, was_literal_fallback) = if stdout.is_empty() && has_metachar {
            // Retry with literal matching — the model likely meant a literal string
            let literal_output = Command::new("rg")
                .args(&[
                    "--line-number", "--no-heading", "--color=never",
                    "--fixed-strings",
                    &format!("--max-count={}", max),
                    &format!("--context={}", context_lines),
                    &parsed.pattern, &resolved_path,
                ])
                .current_dir(&wd)
                .output()
                .await;

            match literal_output {
                Ok(lo) => {
                    let ls = String::from_utf8_lossy(&lo.stdout).to_string();
                    if !ls.is_empty() {
                        (ls, true)
                    } else {
                        (String::new(), false)
                    }
                }
                Err(_) => (String::new(), false),
            }
        } else {
            (stdout.to_string(), false)
        };

        let result = if final_stdout.is_empty() {
            // Build an informative "no match" message
            let mut msg = format!("No matches found for '{}' in {}", parsed.pattern, path);

            // Check if it's a regex error
            if stderr.contains("regex") || stderr.contains("error") {
                msg.push_str(&format!(
                    "\nRegex error: {}. Escape special chars: . → \\\\. , ( → \\\\( , [ → \\\\[",
                    stderr.lines().next().unwrap_or("invalid pattern")
                ));
            } else if has_metachar {
                msg.push_str(
                    "\nTip: Your pattern has regex metacharacters (. ( [ etc). \
                     If you meant a literal search, escape them: console\\.log\\("
                );
            }

            // Count files searched to help diagnose "wrong directory" issues
            if let Ok(file_count_out) = Command::new("rg")
                .args(&["--files", &resolved_path])
                .current_dir(&wd)
                .output()
                .await
            {
                let file_count = String::from_utf8_lossy(&file_count_out.stdout)
                    .lines().count();
                msg.push_str(&format!(" ({} files searched)", file_count));
            }

            msg
        } else {
            let lines: Vec<&str> = final_stdout.lines().take(max).collect();
            let total = final_stdout.lines().count();
            let mut out = if was_literal_fallback {
                format!("[Regex failed, used literal match]\n{}", lines.join("\n"))
            } else {
                lines.join("\n")
            };
            if total > max {
                out.push_str(&format!("\n\n[{} more matches not shown]", total - max));
            }
            out
        };

        Ok(ToolResult {
            call_id: String::new(),
            output: result,
            success: !final_stdout.is_empty(),
        })
    }
}
