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
        if p.starts_with(&ssh_dir) || p == bashrc || p == bash_profile || p == zshrc {
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
            description:
                "Write content to a file. Creates new files or overwrites existing ones.\n\
                Use this for: creating new files, or rewriting an entire file from scratch.\n\
                For small edits to existing files, prefer edit_file instead.\n\
                Parent directories are auto-created if they don't exist."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "Absolute path to the file" },
                    "content": { "type": "string", "description": "The full content to write" }
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
            return ApprovalRequirement::RequireApproval(format!(
                "Writing to sensitive system path: {}",
                parsed.file_path
            ));
        }
        // Overwriting existing files is blocked in execute() — no need to
        // RequireApproval here. Only new file creation is auto-approved.
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        // Parse args defensively. Providers occasionally emit empty ({}) or
        // truncated tool-call arguments on max_tokens cutoff; surfacing the raw
        // serde error ("missing field `file_path` at line 1 column 2") tells
        // the model nothing actionable. Return a success=false result with a
        // recovery hint instead, so the next turn can re-issue the call properly.
        let parsed: WriteFileArgs = match serde_json::from_str(args) {
            Ok(p) => p,
            Err(e) => {
                let hint = if args.trim().is_empty() || args.trim() == "{}" {
                    "tool call arrived with no arguments — likely truncated by max_tokens. \
                     Re-issue write_file with both `file_path` (absolute) and `content`, \
                     or switch to edit_file for targeted changes."
                } else {
                    "could not parse write_file arguments. Re-issue with a valid JSON object \
                     containing `file_path` (absolute) and `content`. For large files, \
                     prefer edit_file to avoid hitting the output token limit."
                };
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: format!("Error: {}. {}", e, hint),
                    success: false,
                });
            }
        };
        let path = std::path::Path::new(&parsed.file_path);

        // Backup before write (git checkpoint + file-level backup)
        ctx.file_history
            .lock()
            .await
            .backup_before_write(&parsed.file_path)
            .await;

        // Check if overwriting existing file — build appropriate output message
        let overwrite_info = if path.exists() {
            let old_lines = std::fs::read_to_string(path)
                .map(|c| c.lines().count())
                .unwrap_or(0);
            Some(old_lines)
        } else {
            None
        };

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let new_lines = parsed.content.lines().count();
        let bytes = parsed.content.len();
        tokio::fs::write(&parsed.file_path, &parsed.content).await?;

        let output = if let Some(old_lines) = overwrite_info {
            let diff = new_lines as i64 - old_lines as i64;
            let sign = if diff >= 0 { "+" } else { "" };
            let mut msg = format!(
                "Overwrote {} (was {} lines, now {} lines, {}{})",
                parsed.file_path, old_lines, new_lines, sign, diff
            );
            // Warn if significant content reduction (might have lost code)
            if old_lines > 20 && new_lines < old_lines / 2 {
                msg.push_str(&format!(
                    "\n⚠ WARNING: File shrank by {}%. Verify no important code was lost. Use /undo to revert if needed.",
                    100 - (new_lines * 100 / old_lines)
                ));
            }
            msg
        } else {
            format!(
                "Created new file {} ({} bytes, {} lines)",
                parsed.file_path, bytes, new_lines
            )
        };

        Ok(ToolResult {
            call_id: String::new(),
            output,
            success: true,
        })
    }
}
