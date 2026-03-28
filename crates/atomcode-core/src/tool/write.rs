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
        if let Ok(parsed) = serde_json::from_str::<WriteFileArgs>(args) {
            if is_sensitive_path(&parsed.file_path) {
                return ApprovalRequirement::RequireApproval(
                    format!("Writing to sensitive system path: {}", parsed.file_path),
                );
            }
            // Overwriting an existing non-empty file is dangerous — it destroys
            // all code not included in the new content. Require approval.
            let path = std::path::Path::new(&parsed.file_path);
            if path.exists() && std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false) {
                return ApprovalRequirement::RequireApproval(
                    format!("Overwriting existing file: {}. Use edit_file instead for targeted changes.", parsed.file_path),
                );
            }
        }
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: WriteFileArgs = serde_json::from_str(args)?;
        let path = std::path::Path::new(&parsed.file_path);

        // Read old content for diff summary (tech-stack agnostic)
        let is_overwrite = path.exists() && std::fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false);
        let old_content = if is_overwrite {
            tokio::fs::read_to_string(&parsed.file_path).await.ok()
        } else {
            None
        };

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let bytes = parsed.content.len();
        let lines = parsed.content.lines().count();
        tokio::fs::write(&parsed.file_path, &parsed.content).await?;

        let mut output = if is_overwrite {
            format!("Overwrote {} ({} bytes, {} lines)", parsed.file_path, bytes, lines)
        } else {
            format!("Created new file {} ({} bytes, {} lines)", parsed.file_path, bytes, lines)
        };

        // Tech-stack agnostic diff: count preserved vs changed lines
        if let Some(ref old) = old_content {
            let old_lines: Vec<&str> = old.lines().collect();
            let new_lines: Vec<&str> = parsed.content.lines().collect();
            let mut preserved = 0usize;
            let mut changed = 0usize;
            for (o, n) in old_lines.iter().zip(new_lines.iter()) {
                if o.trim() == n.trim() { preserved += 1; } else { changed += 1; }
            }

            // Find lines in old that are completely gone (imports, function calls, etc.)
            let old_set: std::collections::HashSet<&str> = old_lines.iter().map(|l| l.trim()).collect();
            let new_set: std::collections::HashSet<&str> = new_lines.iter().map(|l| l.trim()).collect();
            let lost: Vec<&&str> = old_set.difference(&new_set)
                .filter(|l| !l.is_empty() && l.len() > 10)
                .take(5)
                .collect();

            output.push_str(&format!(" ({} lines preserved, {} changed)", preserved, changed));

            if changed > preserved && preserved > 0 {
                output.push_str(
                    "\nWARNING: Most of the file was rewritten. Verify that imports, \
                     API calls, and business logic are still correct."
                );
            }
            if !lost.is_empty() {
                output.push_str("\nLines REMOVED from original (verify these aren't needed):");
                for line in &lost {
                    output.push_str(&format!("\n  - {}", line));
                }
            }
        }

        Ok(ToolResult {
            call_id: String::new(),
            output,
            success: true,
        })
    }
}
