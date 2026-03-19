use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::process::Command;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

pub struct GlobTool;

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
    path: Option<String>,
}

#[async_trait]
impl Tool for GlobTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "glob",
            description: "Find files matching a glob pattern (e.g. '**/*.rs', 'src/**/*.ts'). Returns file paths.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob pattern (e.g. **/*.rs, src/**/*.ts)" },
                    "path": { "type": "string", "description": "Base directory (default: working directory)" }
                },
                "required": ["pattern"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: GlobArgs = serde_json::from_str(args)?;
        let path = parsed.path.as_deref().unwrap_or(".");

        // Use find with name pattern (universal)
        let output = Command::new("find")
            .args(&[path, "-name", &parsed.pattern, "-not", "-path", "*/node_modules/*",
                     "-not", "-path", "*/.git/*", "-not", "-path", "*/target/*"])
            .output()
            .await?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut files: Vec<&str> = stdout.lines().collect();
        files.sort();

        let result = if files.is_empty() {
            format!("No files matching '{}'", parsed.pattern)
        } else {
            let total = files.len();
            let shown: Vec<&str> = files.into_iter().take(100).collect();
            let mut out = shown.join("\n");
            if total > 100 {
                out.push_str(&format!("\n\n[{} more files not shown]", total - 100));
            }
            format!("{} files found:\n{}", total, out)
        };

        Ok(ToolResult {
            call_id: String::new(),
            output: result,
            success: true,
        })
    }
}
