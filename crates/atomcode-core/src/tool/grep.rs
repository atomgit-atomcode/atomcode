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
}

fn default_max_results() -> usize { 50 }

#[async_trait]
impl Tool for GrepTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "grep",
            description: "Search file contents for a pattern using ripgrep (rg) or grep. Returns matching lines with file paths and line numbers.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Search pattern (regex supported)" },
                    "path": { "type": "string", "description": "Directory or file to search (default: working directory)" },
                    "max_results": { "type": "integer", "description": "Max results to return (default 50)" }
                },
                "required": ["pattern"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: GrepArgs = serde_json::from_str(args)?;
        let path = parsed.path.as_deref().unwrap_or(".");
        let max = parsed.max_results;

        // Try ripgrep first, fall back to grep
        let output = Command::new("rg")
            .args(&[
                "--line-number", "--no-heading", "--color=never",
                &format!("--max-count={}", max),
                &parsed.pattern, path,
            ])
            .output()
            .await
            .or_else(|_| {
                // Fallback to grep (synchronous for simplicity)
                std::process::Command::new("grep")
                    .args(&["-rn", "--color=never", &parsed.pattern, path])
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
