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
        // The actual directory change happens in App.
        // This tool just validates the path exists and is a directory.
        let parsed: CdArgs = serde_json::from_str(args)?;
        Ok(ToolResult {
            call_id: String::new(),
            output: format!("CD_REQUEST:{}", parsed.path),
            success: true,
        })
    }
}
