use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::{ApprovalRequirement, Tool, ToolDef, ToolResult};

pub struct CdTool;

#[derive(Deserialize)]
struct CdArgs {
    path: String,
}

#[async_trait]
impl Tool for CdTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "change_dir",
            description: "Change the working directory. All subsequent file operations and bash commands will execute in the new directory.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "The directory path to change to. Can be absolute or relative to current working directory."
                    }
                },
                "required": ["path"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str) -> Result<ToolResult> {
        let parsed: CdArgs = serde_json::from_str(args)?;
        let path = parsed.path.as_str();

        // Resolve the path (expand ~ if needed)
        let target = if path.starts_with('~') {
            dirs::home_dir()
                .map(|h| h.join(path.strip_prefix("~/").unwrap_or(&path[1..])))
                .unwrap_or_else(|| PathBuf::from(path))
        } else {
            PathBuf::from(path)
        };

        // Validate the target is a directory
        if target.is_dir() {
            let resolved = std::fs::canonicalize(&target).unwrap_or(target);
            Ok(ToolResult {
                call_id: String::new(),
                output: format!("Changed working directory to {}", resolved.display()),
                success: true,
            })
        } else if target.exists() {
            Ok(ToolResult {
                call_id: String::new(),
                output: format!("Not a directory: {}", target.display()),
                success: false,
            })
        } else {
            Ok(ToolResult {
                call_id: String::new(),
                output: format!("Path does not exist: {}", target.display()),
                success: false,
            })
        }
    }
}
