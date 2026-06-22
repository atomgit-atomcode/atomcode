//! `write_file` — create or overwrite a file (auto-creating parent dirs). Mutates
//! the filesystem ⇒ always `Risky`. Neutral core ported from the production writer,
//! minus the coding enrichments (file_history backup, file_store/read_cache
//! invalidation, LSP notify).

use super::{err, ok, resolve_path};
use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;

pub struct WriteFileTool;

#[derive(Deserialize)]
struct Args {
    file_path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Write content to a file: creates it (and any missing parent directories) or \
         OVERWRITES it if it exists. Use this for new files or full rewrites; for small \
         changes to an existing file prefer edit_file. Relative paths resolve against \
         the working directory."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "Path to write (absolute, or relative to the working directory)" },
                "content": { "type": "string", "description": "The full content to write" }
            },
            "required": ["file_path", "content"]
        })
    }
    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Risky // creates / overwrites files
    }
    fn always_grant_scope(&self, _args: &str) -> String {
        // Tool-wide: "总是 / Always" approves every write this session (v1 parity).
        String::new()
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return err(format!(
                    "write_file: invalid arguments: {e}. Expected \
                     {{\"file_path\":\"<path>\",\"content\":\"<text>\"}}."
                ))
            }
        };
        let path = resolve_path(&a.file_path, &ctx.working_dir);

        // Capture pre-existing line count for an overwrite diff message.
        let old_lines = tokio::fs::read_to_string(&path).await.ok().map(|s| s.lines().count());

        if let Some(parent) = path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return err(format!(
                    "write_file: failed to create parent directory {}: {e}",
                    parent.display()
                ));
            }
        }
        let new_lines = a.content.lines().count();
        let bytes = a.content.len();
        if let Err(e) = tokio::fs::write(&path, &a.content).await {
            return err(format!("write_file: failed to write {}: {e}", path.display()));
        }

        let msg = match old_lines {
            Some(old) => {
                let diff = new_lines as i64 - old as i64;
                let sign = if diff >= 0 { "+" } else { "" };
                let mut m =
                    format!("Overwrote {} (was {old} lines, now {new_lines} lines, {sign}{diff})", path.display());
                // Warn on a large shrink — the model may have dropped content.
                if old > 20 && new_lines < old / 2 {
                    m.push_str(&format!(
                        "\n⚠ WARNING: file shrank by {}%. Verify no important content was lost.",
                        100 - (new_lines * 100 / old)
                    ));
                }
                m
            }
            None => format!("Created {} ({bytes} bytes, {new_lines} lines)", path.display()),
        };
        ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::tool::ToolContext;
    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext { working_dir: dir.to_path_buf(), cancel: CancellationToken::new(), progress: atomcode_kernel::tool::ProgressSink::noop() }
    }

    #[tokio::test]
    async fn creates_new_file_and_parents() {
        let d = tempfile::tempdir().unwrap();
        let r = WriteFileTool
            .execute(r#"{"file_path":"nested/dir/a.txt","content":"hello\nworld\n"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.starts_with("Created"), "{}", r.content);
        let on_disk = std::fs::read_to_string(d.path().join("nested/dir/a.txt")).unwrap();
        assert_eq!(on_disk, "hello\nworld\n");
    }

    #[tokio::test]
    async fn overwrite_reports_line_diff() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "1\n2\n3\n").unwrap();
        let r = WriteFileTool
            .execute(r#"{"file_path":"a.txt","content":"1\n2\n3\n4\n5\n"}"#, &ctx(d.path()))
            .await;
        assert!(r.content.contains("Overwrote"), "{}", r.content);
        assert!(r.content.contains("was 3 lines, now 5 lines, +2"), "{}", r.content);
    }

    #[tokio::test]
    async fn large_shrink_warns() {
        let d = tempfile::tempdir().unwrap();
        let big: String = (0..100).map(|i| format!("line {i}\n")).collect();
        std::fs::write(d.path().join("a.txt"), big).unwrap();
        let r = WriteFileTool.execute(r#"{"file_path":"a.txt","content":"tiny\n"}"#, &ctx(d.path())).await;
        assert!(r.content.contains("WARNING: file shrank"), "{}", r.content);
    }

    #[tokio::test]
    async fn write_is_risky() {
        assert_eq!(WriteFileTool.risk("{}"), RiskLevel::Risky);
    }
}
