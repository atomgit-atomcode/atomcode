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

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: GlobArgs = serde_json::from_str(args)?;
        let wd = ctx.working_dir.read().await.clone();
        let path = parsed.path.as_deref().unwrap_or(".");
        let resolved = if std::path::Path::new(path).is_absolute() {
            path.to_string()
        } else {
            wd.join(path).to_string_lossy().to_string()
        };

        // Parse pattern: extract directory prefix and filename glob.
        // e.g., "src/views/**/*.vue" → search_dir="src/views", name_pattern="*.vue"
        // e.g., "**/*.rs" → search_dir=resolved, name_pattern="*.rs"
        // e.g., "*.vue" → search_dir=resolved, name_pattern="*.vue"
        let (search_dir, name_pattern) = {
            let p = &parsed.pattern;
            // Remove leading ** or **/
            let cleaned = p.trim_start_matches("**/").trim_start_matches("**");
            if let Some(last_slash) = cleaned.rfind('/') {
                // Has directory prefix: "views/*.vue" → ("views", "*.vue")
                let dir_part = &cleaned[..last_slash];
                let name_part = &cleaned[last_slash + 1..];
                let full_dir = if std::path::Path::new(dir_part).is_absolute() {
                    dir_part.to_string()
                } else {
                    let base = std::path::Path::new(&resolved);
                    base.join(dir_part).to_string_lossy().to_string()
                };
                (full_dir, name_part.to_string())
            } else {
                // Just a filename pattern: "*.vue"
                (resolved.clone(), cleaned.to_string())
            }
        };

        let mut find_args = vec![search_dir.clone(), "-name".to_string(), name_pattern];
        for skip in super::SKIP_DIRS {
            find_args.push("-not".to_string());
            find_args.push("-path".to_string());
            find_args.push(format!("*/{skip}/*"));
        }
        let output = Command::new("find")
            .args(&find_args)
            .current_dir(&wd)
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
