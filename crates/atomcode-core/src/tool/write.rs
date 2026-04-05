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
            name: "create_file",
            description: "Create a NEW file. ONLY for files that do NOT exist yet.\n\
                CANNOT overwrite existing files — use edit_file instead.\n\
                Parent directories are NOT auto-created — ensure the directory exists first.\n\
                Uses atomic write (temp file + rename) to prevent corruption.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "Absolute path to the new file to create" },
                    "content": { "type": "string", "description": "The full content for the new file" }
                },
                "required": ["file_path", "content"]
            }),
        }
    }

    fn approval(&self, args: &str) -> ApprovalRequirement {
        let parsed = match serde_json::from_str::<WriteFileArgs>(args) {
            Ok(p) => p,
            Err(_) => {
                // Fail-closed: if we can't parse args, require approval rather than auto-approving.
                return ApprovalRequirement::RequireApproval(
                    "Could not parse create_file arguments for safety check.".to_string(),
                );
            }
        };
        if is_sensitive_path(&parsed.file_path) {
            return ApprovalRequirement::RequireApproval(
                format!("Writing to sensitive system path: {}", parsed.file_path),
            );
        }
        // Overwriting existing files is blocked in execute() — no need to
        // RequireApproval here. Only new file creation is auto-approved.
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: WriteFileArgs = serde_json::from_str(args)?;
        let path = std::path::Path::new(&parsed.file_path);

        // Block create_file on existing non-empty files.
        // Return an error with specific edit_file alternative.
        let is_overwrite = path.exists() && std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false);
        if is_overwrite {
            // Count lines to give the model a concrete edit_file alternative
            let total_lines = std::fs::read_to_string(path)
                .map(|c| c.lines().count())
                .unwrap_or(0);
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!(
                    "ERROR: Cannot overwrite existing file '{}' ({} lines).\n\
                     To rewrite a section: edit_file(file_path=\"{}\", start_line=N, end_line=M, new_string=\"...\")\n\
                     To rewrite the entire file: edit_file(file_path=\"{}\", start_line=1, end_line={}, new_string=\"...\")",
                    parsed.file_path, total_lines,
                    parsed.file_path,
                    parsed.file_path, total_lines,
                ),
                success: false,
            });
        }

        // Backup file before write (file-level checkpointing).
        ctx.file_history.lock().await.backup_before_write(&parsed.file_path).await;

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let bytes = parsed.content.len();
        let lines = parsed.content.lines().count();
        tokio::fs::write(&parsed.file_path, &parsed.content).await?;

        let output = format!("Created new file {} ({} bytes, {} lines)", parsed.file_path, bytes, lines);

        Ok(ToolResult {
            call_id: String::new(),
            output,
            success: true,
        })
    }
}
