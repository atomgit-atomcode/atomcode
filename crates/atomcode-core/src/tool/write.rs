use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

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

    fn validate_args(&self, args: &str) -> std::result::Result<(), String> {
        // Surface a model-friendly diagnostic (provided/missing keys + a
        // one-line example) instead of the raw serde "line 1 column N"
        // error which weak models read as a parser-position complaint and
        // try to "fix" by switching to positional arguments. See
        // `diagnose_args` doc for the failure mode this replaces.
        super::diagnose_args(
            "write_file",
            args,
            &[&["file_path", "content"]],
            "write_file({\"file_path\": \"<absolute path>\", \"content\": \"<file body>\"})",
        )?;
        // Strict struct parse only AFTER the keys are known to be present
        // — catches type mismatches (e.g. content sent as an array).
        serde_json::from_str::<WriteFileArgs>(args)
            .map(|_| ())
            .map_err(|e| {
                format!(
                    "write_file: {e}. Re-issue with file_path as a string and content as a string."
                )
            })
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
        if super::is_sensitive_input_path(&parsed.file_path) {
            return ApprovalRequirement::RequireApproval(format!(
                "Writing to sensitive system path: {}",
                parsed.file_path
            ));
        }
        // Overwriting existing files is blocked in execute() — no need to
        // RequireApproval here. Only new file creation is auto-approved.
        ApprovalRequirement::AutoApprove
    }

    fn approval_with_context(&self, args: &str, ctx: &ToolContext) -> ApprovalRequirement {
        let base = self.approval(args);
        let parsed = match serde_json::from_str::<WriteFileArgs>(args) {
            Ok(parsed) => parsed,
            Err(_) => return base,
        };
        let working_dir = match ctx.working_dir.try_read() {
            Ok(wd) => wd.clone(),
            Err(_) => return base,
        };
        // Scope any prompt to the exact target file so a session [A] remembers
        // THAT file (see `scoped_write_approval`): re-writing the same file
        // stops re-prompting, while a tool-wide grant can't bypass a sensitive
        // write.
        super::scoped_write_approval(&parsed.file_path, &working_dir, base)
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        // Defense-in-depth: validate_args runs at the runner gate, but if
        // it's bypassed (or args mutated between gate and execute), we fall
        // back to the same diagnose_args path so the model sees a uniform
        // recovery hint instead of a raw serde error.
        if let Err(msg) = super::diagnose_args(
            "write_file",
            args,
            &[&["file_path", "content"]],
            "write_file({\"file_path\": \"<absolute path>\", \"content\": \"<file body>\"})",
        ) {
            return Ok(ToolResult {
                call_id: String::new(),
                output: msg,
                success: false,
            });
        }
        let parsed: WriteFileArgs = match serde_json::from_str(args) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: format!(
                        "write_file: {e}. Re-issue with file_path as a string and content as a string."
                    ),
                    success: false,
                });
            }
        };
        let working_dir = ctx.working_dir.read().await.clone();
        let path = match super::inspect_path_access(&parsed.file_path, &working_dir) {
            Ok(access) => access.path,
            Err(err) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: err.to_string(),
                    success: false,
                });
            }
        };

        // Backup before write (git checkpoint + file-level backup)
        ctx.file_history
            .lock()
            .await
            .backup_before_write(&path.to_string_lossy())
            .await;

        // Check if overwriting existing file — build appropriate output message
        let overwrite_info = if path.exists() {
            // tokio::fs so a hung filesystem doesn't block the async worker.
            let old_lines = tokio::fs::read_to_string(&path)
                .await
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
        tokio::fs::write(&path, &parsed.content).await?;

        // D3: drop any FileStore entry for this path. The next peek_file
        // against the old store_id will report "stale" and route the
        // model toward a fresh read_file. Without this invalidation a
        // peek_file could hand the model pre-write content that no
        // longer matches what just landed on disk.
        ctx.file_store.write().await.invalidate(&path);
        // Defense-in-depth: read_cache mtime gate is normally sufficient
        // because tokio::fs::write bumps mtime, but on FS with coarse
        // mtime granularity (ext4 1-second precision, NFS) a write within
        // the same tick as the prior read keeps the same mtime and the
        // gate stops protecting us. Explicit purge eliminates that
        // corner case for any path we just wrote.
        ctx.read_cache
            .write()
            .await
            .retain(|(p, _, _), _| p != &path);

        // Notify LSP that file changed (if LSP is enabled).
        ctx.notify_lsp_file_changed(&path, &parsed.content).await;

        let output = if let Some(old_lines) = overwrite_info {
            let diff = new_lines as i64 - old_lines as i64;
            let sign = if diff >= 0 { "+" } else { "" };
            let mut msg = format!(
                "Overwrote {} (was {} lines, now {} lines, {}{})",
                path.display(),
                old_lines,
                new_lines,
                sign,
                diff
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
                path.display(),
                bytes,
                new_lines
            )
        };

        Ok(ToolResult {
            call_id: String::new(),
            output,
            success: true,
        })
    }
}
