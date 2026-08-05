//! `list_directory` — recursive, indented directory tree (build/VCS/cache dirs
//! skipped). Non-destructive ⇒ always `Safe`.

use super::{err, is_skip_dir, not_found_hint, ok, resolve_path};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;
use std::path::Path;

const MAX_ENTRIES: usize = 200;
const MAX_DEPTH_CAP: usize = 5;

pub struct ListDirTool;

#[derive(Deserialize)]
struct Args {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    depth: Option<usize>,
}

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_directory"
    }
    fn description(&self) -> &str {
        "List a directory tree (indented; directories end with '/'). `depth` controls \
         recursion (default 2, max 5). Build/VCS/cache directories (node_modules, .git, \
         target, …) are skipped. Relative paths resolve against the working directory."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to list (default: the working directory)" },
                "depth": { "type": "integer", "description": "Max recursion depth (default 2, max 5)" }
            }
        })
    }
    /// No side effects — a pure read. Makes it `parallel_safe` (concurrent
    /// execution) and allowed in plan mode.
    fn read_only_hint(&self) -> bool {
        true
    }
    // listing is non-destructive → risk() defaults to Safe.
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return err(format!(
                    "list_directory: invalid arguments: {e}. Expected \
                     {{\"path\":\"<dir>\",\"depth\":<int>}} (both optional)."
                ))
            }
        };
        let raw = a.path.unwrap_or_else(|| ".".to_string());
        let root = resolve_path(&raw, &ctx.working_dir);
        let depth = a.depth.unwrap_or(2).min(MAX_DEPTH_CAP);

        match tokio::fs::metadata(&root).await {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                return err(format!(
                    "Not a directory: {}",
                    crate::pathnorm::to_display(&root)
                ))
            }
            Err(_) => {
                return err(format!(
                    "Directory not found: {}{}",
                    crate::pathnorm::to_display(&root),
                    not_found_hint(&root, &ctx.working_dir).await
                ))
            }
        }

        let root2 = root.clone();
        let lines = match tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            walk(&root2, 0, depth, &mut out);
            out
        })
        .await
        {
            Ok(v) => v,
            Err(_) => return err("list_directory: scan task failed".to_string()),
        };

        let truncated = lines.len() > MAX_ENTRIES;
        let mut shown = lines;
        if truncated {
            shown.truncate(MAX_ENTRIES);
        }
        let mut out = shown.join("\n");
        if truncated {
            out.push_str(&format!("\n  ... (truncated at {MAX_ENTRIES} entries)"));
        }
        ok(out)
    }
}

fn walk(dir: &Path, depth: usize, max: usize, out: &mut Vec<String>) {
    if depth > max || out.len() > MAX_ENTRIES {
        return;
    }
    let mut entries: Vec<_> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(_) => return, // unreadable subtree → silently skip (e.g. permission denied)
    };
    entries.sort_by_key(|e| e.file_name());
    let indent = "  ".repeat(depth);
    for e in entries {
        if out.len() > MAX_ENTRIES {
            return;
        }
        let name = e.file_name().to_string_lossy().to_string();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            if is_skip_dir(&name) {
                out.push(format!("{indent}{name}/ (skipped)"));
                continue;
            }
            out.push(format!("{indent}{name}/"));
            walk(&e.path(), depth + 1, max, out);
        } else {
            out.push(format!("{indent}{name}"));
        }
    }
}

#[cfg(test)]
mod tests {
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
    async fn lists_tree_with_dirs_marked() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("src")).unwrap();
        std::fs::write(d.path().join("src/main.rs"), "fn main(){}").unwrap();
        std::fs::write(d.path().join("README.md"), "# hi").unwrap();
        let r = ListDirTool.execute(r#"{"path":"."}"#, &ctx(d.path())).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("src/"), "{}", r.content);
        assert!(r.content.contains("  main.rs"), "{}", r.content);
        assert!(r.content.contains("README.md"), "{}", r.content);
    }

    #[tokio::test]
    async fn skips_build_dirs() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("target")).unwrap();
        std::fs::write(d.path().join("target/junk"), "x").unwrap();
        let r = ListDirTool.execute(r#"{"path":"."}"#, &ctx(d.path())).await;
        assert!(r.content.contains("target/ (skipped)"), "{}", r.content);
        assert!(!r.content.contains("junk"), "{}", r.content);
    }

    #[tokio::test]
    async fn invalid_json_args_error() {
        let d = tempfile::tempdir().unwrap();
        let r = ListDirTool.execute("{not valid json", &ctx(d.path())).await;
        assert!(
            r.is_error,
            "malformed args must surface an error, not silently default"
        );
        assert!(r.content.contains("invalid arguments"), "{}", r.content);
    }

    #[tokio::test]
    async fn missing_dir_errors() {
        let d = tempfile::tempdir().unwrap();
        let r = ListDirTool
            .execute(r#"{"path":"nope"}"#, &ctx(d.path()))
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("Directory not found"), "{}", r.content);
    }

    /// Still an error, but it must carry the recovery clue — otherwise the model just guesses
    /// a different wrong path next turn (see `not_found_hint`).
    #[tokio::test]
    async fn missing_dir_error_carries_the_nearest_existing_ancestor() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("settings.gradle"), "").unwrap();
        let r = ListDirTool
            .execute(r#"{"path":"app/src/main"}"#, &ctx(d.path()))
            .await;
        assert!(r.is_error);
        assert!(
            r.content.contains("Nearest existing directory"),
            "{}",
            r.content
        );
        assert!(r.content.contains("settings.gradle"), "{}", r.content);
    }
}
