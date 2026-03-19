use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::{ApprovalRequirement, Tool, ToolDef, ToolResult};

pub struct ReadFileTool;

#[derive(Deserialize)]
struct ReadFileArgs {
    file_path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReadFileTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "read_file",
            description: "Read the contents of a file. Returns the file text with line numbers.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "Absolute path to the file to read" },
                    "offset": { "type": "integer", "description": "Line number to start reading from (1-based). Optional." },
                    "limit": { "type": "integer", "description": "Maximum number of lines to read. Optional, defaults to 2000." }
                },
                "required": ["file_path"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str) -> Result<ToolResult> {
        let parsed: ReadFileArgs = serde_json::from_str(args)?;
        let content = tokio::fs::read_to_string(&parsed.file_path).await?;
        let lines: Vec<&str> = content.lines().collect();
        let offset = parsed.offset.unwrap_or(1).max(1) - 1;
        let limit = parsed.limit.unwrap_or(2000);
        let end = (offset + limit).min(lines.len());
        let output: String = lines[offset..end]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{:>4}| {}", offset + i + 1, line))
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolResult { call_id: String::new(), output, success: true })
    }
}
