//! `change_dir` — move the agent's working directory for subsequent tool calls. Holds a
//! SHARED `Arc<RwLock<PathBuf>>` that MUST be the same handle passed to
//! [`AgentBuilder::working_dir_shared`](atomcode_kernel::agent::AgentBuilder::working_dir_shared):
//! cd writes the new dir into it, and the kernel re-snapshots it into every later
//! `ToolContext::working_dir`, so the change persists across calls. Navigational (no fs
//! mutation, no exec) ⇒ `Safe`.
//!
//! Opt-in: unlike the stateless coding tools, this one is constructed WITH the shared
//! handle, so it is NOT part of [`register_coding_tools`](super::register_coding_tools)
//! — the embedder wires the same `Arc` into the builder and this tool, then registers it.
//!
//! Note: weak models sometimes loop on `change_dir` with empty args; `bash` already
//! tracks `cd` for the common case. Expose this tool only when a real directory-switch
//! affordance is wanted.

use super::{err, ok};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

pub struct ChangeDirTool {
    /// The shared working dir — the SAME `Arc` given to `working_dir_shared`.
    cwd: Arc<RwLock<PathBuf>>,
}

impl ChangeDirTool {
    pub fn new(cwd: Arc<RwLock<PathBuf>>) -> Self {
        Self { cwd }
    }
    /// The shared handle, for passing to
    /// [`AgentBuilder::working_dir_shared`](atomcode_kernel::agent::AgentBuilder::working_dir_shared).
    pub fn cwd_handle(&self) -> Arc<RwLock<PathBuf>> {
        self.cwd.clone()
    }
}

#[derive(Deserialize)]
struct Args {
    path: String,
}

#[async_trait]
impl Tool for ChangeDirTool {
    fn name(&self) -> &str {
        "change_dir"
    }
    fn description(&self) -> &str {
        "Change the working directory used by subsequent tool calls (read_file, bash, \
         grep, …). `path` is absolute or relative to the current working directory; it \
         must exist and be a directory. Returns the new working directory. Prefer this \
         over `cd` inside bash when you want the change to apply to other tools too."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory to switch to (absolute, or relative to the current working directory)" }
            },
            "required": ["path"]
        })
    }
    // navigational only (no fs mutation / no exec) → Safe.
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return err(format!(
                    "change_dir: invalid arguments: {e}. Expected {{\"path\":\"<dir>\"}}."
                ))
            }
        };
        if a.path.trim().is_empty() {
            return err("change_dir: `path` must be non-empty.");
        }
        // Resolve relative to the CURRENT shared cwd (the source of truth).
        let current = self.cwd.read().map(|g| g.clone()).unwrap_or_default();
        let raw = Path::new(&a.path);
        let target = if raw.is_absolute() {
            raw.to_path_buf()
        } else {
            current.join(raw)
        };

        let meta = match std::fs::metadata(&target) {
            Ok(m) => m,
            Err(_) => {
                return err(format!(
                    "change_dir: no such directory: {} (resolved to {})",
                    a.path,
                    crate::pathnorm::to_display(&target)
                ))
            }
        };
        if !meta.is_dir() {
            return err(format!(
                "change_dir: not a directory: {}",
                crate::pathnorm::to_display(&target)
            ));
        }
        // Canonicalize so later by-path lookups are stable (symlink / `..` normalized).
        // pathnorm::canonicalize strips the Windows `\\?\` prefix so the stored cwd
        // (and every by-path lookup keyed on it) never carries a verbatim path.
        let canon = crate::pathnorm::canonicalize(&target).unwrap_or(target);
        match self.cwd.write() {
            Ok(mut g) => *g = canon.clone(),
            Err(_) => return err("change_dir: working-dir lock poisoned"),
        }
        ok(format!(
            "Working directory changed to {}",
            crate::pathnorm::to_display(&canon)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        }
    }

    #[tokio::test]
    async fn cd_into_subdir_updates_shared_handle() {
        let d = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(d.path()).unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        let cwd = Arc::new(RwLock::new(root.clone()));
        let tool = ChangeDirTool::new(cwd.clone());

        let r = tool.execute(r#"{"path":"sub"}"#, &ctx(&root)).await;
        assert!(!r.is_error, "{}", r.content);
        // the SHARED handle now points at the subdir (what the kernel re-snapshots).
        assert_eq!(*cwd.read().unwrap(), root.join("sub"));
        assert!(r.content.contains("changed to"), "{}", r.content);
    }

    #[tokio::test]
    async fn cd_absolute_path() {
        let d = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(d.path()).unwrap();
        let cwd = Arc::new(RwLock::new(PathBuf::from("/")));
        let tool = ChangeDirTool::new(cwd.clone());
        let r = tool
            .execute(
                &format!("{{\"path\":{:?}}}", root.to_string_lossy()),
                &ctx(&root),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(*cwd.read().unwrap(), root);
    }

    #[tokio::test]
    async fn cd_relative_is_resolved_against_current_cwd() {
        let d = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(d.path()).unwrap();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        let cwd = Arc::new(RwLock::new(root.join("a")));
        let tool = ChangeDirTool::new(cwd.clone());
        // relative "b" resolves against the CURRENT cwd ("a"), not the original root.
        let r = tool.execute(r#"{"path":"b"}"#, &ctx(&root.join("a"))).await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(*cwd.read().unwrap(), root.join("a/b"));
    }

    #[tokio::test]
    async fn cd_missing_dir_errors_and_leaves_cwd() {
        let d = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(d.path()).unwrap();
        let cwd = Arc::new(RwLock::new(root.clone()));
        let tool = ChangeDirTool::new(cwd.clone());
        let r = tool.execute(r#"{"path":"nope"}"#, &ctx(&root)).await;
        assert!(r.is_error);
        assert!(r.content.contains("no such directory"), "{}", r.content);
        assert_eq!(*cwd.read().unwrap(), root, "cwd unchanged on failure");
    }

    #[tokio::test]
    async fn cd_to_a_file_errors() {
        let d = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(d.path()).unwrap();
        std::fs::write(root.join("f.txt"), "x").unwrap();
        let cwd = Arc::new(RwLock::new(root.clone()));
        let tool = ChangeDirTool::new(cwd.clone());
        let r = tool.execute(r#"{"path":"f.txt"}"#, &ctx(&root)).await;
        assert!(r.is_error);
        assert!(r.content.contains("not a directory"), "{}", r.content);
    }

    #[tokio::test]
    async fn empty_path_errors() {
        let cwd = Arc::new(RwLock::new(PathBuf::from(".")));
        let r = ChangeDirTool::new(cwd)
            .execute(r#"{"path":""}"#, &ctx(Path::new(".")))
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("non-empty"), "{}", r.content);
    }
}
