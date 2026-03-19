use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::{ApprovalRequirement, Tool, ToolDef, ToolResult};

pub struct WriteFileTool;

#[derive(Deserialize)]
struct WriteFileArgs {
    file_path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteFileTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "write_file",
            description: "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Creates parent directories as needed.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "Absolute path to the file to write" },
                    "content": { "type": "string", "description": "The full content to write to the file" }
                },
                "required": ["file_path", "content"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str) -> Result<ToolResult> {
        let parsed: WriteFileArgs = serde_json::from_str(args)?;
        if let Some(parent) = std::path::Path::new(&parsed.file_path).parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let bytes = parsed.content.len();
        tokio::fs::write(&parsed.file_path, &parsed.content).await?;
        Ok(ToolResult { call_id: String::new(), output: format!("Wrote {} bytes to {}", bytes, parsed.file_path), success: true })
    }
}
