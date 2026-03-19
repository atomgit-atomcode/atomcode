use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::{ApprovalRequirement, Tool, ToolDef, ToolResult};

pub struct EditFileTool;

#[derive(Deserialize)]
struct EditFileArgs {
    file_path: String,
    old_string: String,
    new_string: String,
}

#[async_trait]
impl Tool for EditFileTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "edit_file",
            description: "Replace a specific string in a file. The old_string must match exactly (including whitespace and indentation). Use this for targeted edits instead of rewriting entire files.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Path to the file to edit"
                    },
                    "old_string": {
                        "type": "string",
                        "description": "The exact text to find and replace. Must be unique in the file."
                    },
                    "new_string": {
                        "type": "string",
                        "description": "The replacement text"
                    }
                },
                "required": ["file_path", "old_string", "new_string"]
            }),
        }
    }

    fn approval(&self, args: &str) -> ApprovalRequirement {
        let parsed: std::result::Result<EditFileArgs, _> = serde_json::from_str(args);
        let desc = match parsed {
            Ok(a) => {
                let old_preview: String = a.old_string.lines().take(3).collect::<Vec<_>>().join("\n");
                let new_preview: String = a.new_string.lines().take(3).collect::<Vec<_>>().join("\n");
                let old_lines = a.old_string.lines().count();
                let new_lines = a.new_string.lines().count();
                format!(
                    "Edit {}\n-{}{}\n+{}{}",
                    a.file_path,
                    old_preview,
                    if old_lines > 3 { format!("\n  ... ({} lines)", old_lines) } else { String::new() },
                    new_preview,
                    if new_lines > 3 { format!("\n  ... ({} lines)", new_lines) } else { String::new() },
                )
            }
            Err(_) => "Edit file (could not parse arguments)".to_string(),
        };
        ApprovalRequirement::RequireApproval(desc)
    }

    async fn execute(&self, args: &str) -> Result<ToolResult> {
        let parsed: EditFileArgs = serde_json::from_str(args)?;

        let content = tokio::fs::read_to_string(&parsed.file_path)
            .await
            .with_context(|| format!("Failed to read {}", parsed.file_path))?;

        // Check that old_string exists and is unique
        let count = content.matches(&parsed.old_string).count();
        if count == 0 {
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!(
                    "Error: old_string not found in {}. Make sure it matches exactly.",
                    parsed.file_path
                ),
                success: false,
            });
        }
        if count > 1 {
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!(
                    "Error: old_string found {} times in {}. It must be unique. Provide more context to make it unique.",
                    count, parsed.file_path
                ),
                success: false,
            });
        }

        let new_content = content.replacen(&parsed.old_string, &parsed.new_string, 1);
        tokio::fs::write(&parsed.file_path, &new_content).await?;

        let removed = parsed.old_string.lines().count();
        let added = parsed.new_string.lines().count();
        Ok(ToolResult {
            call_id: String::new(),
            output: format!(
                "Edited {} (-{} +{} lines)",
                parsed.file_path, removed, added
            ),
            success: true,
        })
    }
}
