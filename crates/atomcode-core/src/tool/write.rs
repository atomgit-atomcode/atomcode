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
            description: "Write content to a file (creates or overwrites).\n\
                DANGER: This REPLACES the entire file. Any existing code not included will be permanently lost.\n\
                When to use:\n\
                - Creating a NEW file that doesn't exist yet.\n\
                - Complete rewrites ONLY when the user explicitly requests it.\n\
                When NOT to use:\n\
                - Modifying existing files — use edit_file instead. It only changes matched text, preserving everything else.\n\
                - NEVER read a file, then write_file with small changes. Use edit_file for targeted modifications.\n\
                Behavior:\n\
                - Overwriting an existing non-empty file requires user approval.\n\
                - Parent directories are NOT auto-created — ensure the directory exists first.\n\
                - Uses atomic write (temp file + rename) to prevent corruption.".to_string(),
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
        let parsed = match serde_json::from_str::<WriteFileArgs>(args) {
            Ok(p) => p,
            Err(_) => {
                // Fail-closed: if we can't parse args, require approval rather than auto-approving.
                return ApprovalRequirement::RequireApproval(
                    "Could not parse write_file arguments for safety check.".to_string(),
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

        // Block write_file on existing non-empty files at code level.
        // Weak models ignore prompt rules ("NEVER write_file on existing files")
        // and RequireApproval just shifts the burden to the user. Return an error
        // so the model is forced to use edit_file instead.
        let is_overwrite = path.exists() && std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false);
        if is_overwrite {
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!(
                    "ERROR: Cannot overwrite existing file '{}' with write_file. \
                     Use edit_file with start_line/end_line to make targeted changes. \
                     write_file is only for creating NEW files.",
                    parsed.file_path
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
