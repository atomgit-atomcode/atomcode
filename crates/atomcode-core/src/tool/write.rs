use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

pub struct WriteFileTool;

/// Check if a file path is a sensitive system location that should require user approval.
fn is_sensitive_path(path: &str) -> bool {
    let expanded = if path.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            home.join(&path[2..]).to_string_lossy().to_string()
        } else {
            path.to_string()
        }
    } else {
        path.to_string()
    };

    let sensitive_prefixes = ["/etc/", "/usr/", "/var/", "/System/"];
    for prefix in &sensitive_prefixes {
        if expanded.starts_with(prefix) {
            return true;
        }
    }

    // Check ~/.ssh (expanded)
    if let Some(home) = dirs::home_dir() {
        let ssh_dir = home.join(".ssh");
        let bashrc = home.join(".bashrc");
        let bash_profile = home.join(".bash_profile");
        let zshrc = home.join(".zshrc");
        let p = std::path::Path::new(&expanded);
        if p.starts_with(&ssh_dir)
            || p == bashrc
            || p == bash_profile
            || p == zshrc
        {
            return true;
        }
    }

    false
}

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

    fn approval(&self, args: &str) -> ApprovalRequirement {
        if let Ok(parsed) = serde_json::from_str::<WriteFileArgs>(args) {
            if is_sensitive_path(&parsed.file_path) {
                return ApprovalRequirement::RequireApproval(
                    format!("Writing to sensitive system path: {}", parsed.file_path),
                );
            }
        }
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: WriteFileArgs = serde_json::from_str(args)?;
        if let Some(parent) = std::path::Path::new(&parsed.file_path).parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let bytes = parsed.content.len();
        tokio::fs::write(&parsed.file_path, &parsed.content).await?;
        Ok(ToolResult { call_id: String::new(), output: format!("Wrote {} bytes to {}", bytes, parsed.file_path), success: true })
    }
}
