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
            description: "Search file contents for a pattern. Returns matching lines with context (3 lines before/after each match). Use this instead of bash grep.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Search pattern (regex supported)" },
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

        // Try ripgrep first, fall back to grep
        let output = Command::new("rg")
            .args(&[
                "--line-number", "--no-heading", "--color=never",
                &format!("--max-count={}", max),
                &format!("--context={}", context_lines),
                &parsed.pattern, &resolved_path,
            ])
            .current_dir(&wd)
            .output()
            .await
            .or_else(|_| {
                std::process::Command::new("grep")
                    .args(&["-rn", "--color=never",
                            &format!("-C{}", context_lines),
                            &parsed.pattern, &resolved_path])
                    .current_dir(&wd)
                    .output()
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let result = if stdout.is_empty() {
            format!("No matches found for '{}'", parsed.pattern)
        } else {
            // Limit output lines
            let lines: Vec<&str> = stdout.lines().take(max).collect();
            let total = stdout.lines().count();
            let mut out = lines.join("\n");
            if total > max {
                out.push_str(&format!("\n\n[{} more matches not shown]", total - max));
            }
            out
        };

        Ok(ToolResult {
            call_id: String::new(),
            output: result,
            success: output.status.success() || !stdout.is_empty(),
        })
    }
}
