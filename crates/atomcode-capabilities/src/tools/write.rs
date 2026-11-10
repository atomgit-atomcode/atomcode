//! `write_file` — create or overwrite a file (auto-creating parent dirs). Mutates
//! the filesystem ⇒ always `Risky`. Neutral core ported from the production writer,
//! minus the coding enrichments (file_history backup, file_store/read_cache
//! invalidation, LSP notify).

use super::{err, ok, resolve_path};
use crate::world::{FileSystem, LocalFs};
use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

pub struct WriteFileTool {
    /// Where the write actually lands. Defaults to plain local I/O, which is
    /// exactly what this tool did before the seam existed — so mounting it
    /// unchanged changes nothing.
    world: Arc<dyn FileSystem>,
}

impl Default for WriteFileTool {
    fn default() -> Self {
        Self {
            world: Arc::new(LocalFs::unfenced()),
        }
    }
}

impl WriteFileTool {
    /// Route this tool's writes through `world` — a fenced root, a read-only
    /// view, a sandbox. The tool cannot tell which, which is the point.
    pub fn with_world(world: Arc<dyn FileSystem>) -> Self {
        Self { world }
    }
}

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
        let old_lines = self
            .world
            .read_text(&path)
            .await
            .ok()
            .map(|s| s.lines().count());

        if let Some(parent) = path.parent() {
            if let Err(e) = self.world.create_dir_all(parent).await {
                return err(format!(
                    "write_file: failed to create parent directory {}: {e}",
                    crate::pathnorm::to_display(parent)
                ));
            }
        }
        let new_lines = a.content.lines().count();
        let bytes = a.content.len();
        // Model-facing paths use forward slashes on Windows so the model can paste
        // them into a subsequent bash (Git Bash) command without `\` being eaten.
        let disp = crate::pathnorm::to_display(&path);
        // write_file always writes UTF-8, intentionally: it does a WHOLE-file overwrite
        // (usually a create), so there is no original encoding to preserve. Unlike
        // `edit_file` — which decodes a GBK/GB18030 file, edits in place, and re-encodes
        // back to the original encoding (see tools::encoding) — overwriting an existing
        // GBK file here converts it to UTF-8. That asymmetry is deliberate; steer legacy-
        // encoding-preserving changes through `edit_file`.
        if let Err(e) = self.world.write_text(&path, &a.content).await {
            return err(format!("write_file: failed to write {disp}: {e}"));
        }

        let mut msg = match old_lines {
            Some(old) => {
                let diff = new_lines as i64 - old as i64;
                let sign = if diff >= 0 { "+" } else { "" };
                let mut m = format!(
                    "Overwrote {disp} (was {old} lines, now {new_lines} lines, {sign}{diff})"
                );
                // Warn on a large shrink — the model may have dropped content.
                if old > 20 && new_lines < old / 2 {
                    m.push_str(&format!(
                        "\n⚠ WARNING: file shrank by {}%. Verify no important content was lost.",
                        100 - (new_lines * 100 / old)
                    ));
                }
                m
            }
            None => format!("Created {disp} ({bytes} bytes, {new_lines} lines)"),
        };
        if let Some(warning) = windows_script_encoding_warning(&path, &a.content) {
            msg.push_str(warning);
        }
        ok(msg)
    }
}

// Inspect content, not the host OS: scripts may be authored for Windows remotely.
fn windows_script_encoding_warning(path: &std::path::Path, content: &str) -> Option<&'static str> {
    if content.is_ascii() {
        return None;
    }
    let extension = path.extension()?.to_str()?;
    if extension.eq_ignore_ascii_case("ps1") {
        if content.starts_with('\u{feff}') {
            return None;
        }
        Some("\nWARNING: This PowerShell script contains non-ASCII text without a BOM. Windows PowerShell 5.1 -File reads it using the legacy ANSI code page and may create wrongly named paths. Use the pwsh tool to execute the command directly, PowerShell 7 if available, or explicitly save this .ps1 as UTF-8 with BOM before running it in 5.1. Changing console output encoding does not fix script decoding.")
    } else if extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat") {
        Some("\nWARNING: This batch script contains non-ASCII UTF-8 text. cmd.exe may read it using a different active code page and create wrongly named paths. Prefer the pwsh tool with literal paths instead of a batch file; do not assume adding a BOM makes cmd.exe read UTF-8 correctly.")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn windows_script_encoding_warning_targets_unsafe_script_content() {
        use std::path::Path;
        for extension in ["ps1", "PS1", "cmd", "BAT"] {
            let path = format!("script.{extension}");
            assert!(
                super::windows_script_encoding_warning(Path::new(&path), "mkdir '工作区'")
                    .is_some()
            );
            assert!(super::windows_script_encoding_warning(Path::new(&path), "echo ok").is_none());
        }
        assert!(super::windows_script_encoding_warning(
            Path::new("script.ps1"),
            "\u{feff}mkdir '工作区'"
        )
        .is_none());
        assert!(super::windows_script_encoding_warning(Path::new("notes.md"), "工作区").is_none());
    }
    use super::*;
    use atomcode_kernel::tool::ToolContext;
    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        }
    }

    #[tokio::test]
    async fn script_encoding_warning_reaches_the_model_without_changing_file_bytes() {
        let d = tempfile::tempdir().unwrap();
        for (file, content, warning) in [
            ("task.ps1", "New-Item '工作区'", true),
            ("task.ps1", "\u{feff}New-Item '工作区'", false),
            ("task.CMD", "mkdir 工作区", true),
            ("task.bat", "\u{feff}mkdir 工作区", true),
            ("notes.md", "工作区", false),
        ] {
            let result = WriteFileTool::default()
                .execute(
                    &serde_json::json!({"file_path": file, "content": content}).to_string(),
                    &ctx(d.path()),
                )
                .await;
            assert_eq!(
                result.content.contains("WARNING:"),
                warning,
                "{}",
                result.content
            );
            assert_eq!(
                std::fs::read(d.path().join(file)).unwrap(),
                content.as_bytes()
            );
        }
    }

    #[tokio::test]
    async fn creates_new_file_and_parents() {
        let d = tempfile::tempdir().unwrap();
        let r = WriteFileTool::default()
            .execute(
                r#"{"file_path":"nested/dir/a.txt","content":"hello\nworld\n"}"#,
                &ctx(d.path()),
            )
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
        let r = WriteFileTool::default()
            .execute(
                r#"{"file_path":"a.txt","content":"1\n2\n3\n4\n5\n"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(r.content.contains("Overwrote"), "{}", r.content);
        assert!(
            r.content.contains("was 3 lines, now 5 lines, +2"),
            "{}",
            r.content
        );
    }

    #[tokio::test]
    async fn large_shrink_warns() {
        let d = tempfile::tempdir().unwrap();
        let big: String = (0..100).map(|i| format!("line {i}\n")).collect();
        std::fs::write(d.path().join("a.txt"), big).unwrap();
        let r = WriteFileTool::default()
            .execute(
                r#"{"file_path":"a.txt","content":"tiny\n"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(r.content.contains("WARNING: file shrank"), "{}", r.content);
    }

    #[tokio::test]
    async fn write_is_risky() {
        assert_eq!(WriteFileTool::default().risk("{}"), RiskLevel::Risky);
    }
}
