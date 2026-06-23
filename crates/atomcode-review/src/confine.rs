//! Path-confinement middleware for the REVIEW agent.
//!
//! Pins every read-only tool's path arguments inside the repo root. A model that
//! asks to `grep /` (scan the whole container → OOM) or read a file outside the
//! repo is BLOCKED here, in the kernel's `before` hook, before the tool runs.
//!
//! Mounted ONLY on the review agent (see [`crate::assemble`]); the kernel, the
//! neutral tools, and other specializations (`code`/`sessions`) are untouched.
//! This is the EMBEDDER's confinement, not a kernel-level sandbox — the neutral
//! tools deliberately do no path-escape enforcement (their trust model leaves it
//! to the embedder).

use async_trait::async_trait;
use atomcode_kernel::middleware::{BeforeOutcome, ToolMiddleware};
use atomcode_kernel::request::RequestCtx;
use atomcode_kernel::tool::{Tool, ToolCall};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

/// JSON arg fields that carry a filesystem path across the review toolset
/// (read_file/grep/glob/list_directory/ast_grep/read_symbol/list_symbols/
/// find_references/blast_radius/file_dependencies/diagnostics). `paths` (ast_grep)
/// is the only array-valued one.
const PATH_KEYS: &[&str] = &["path", "file_path", "file", "dir", "paths"];

/// Blocks any review tool call whose path argument escapes the repo root.
pub struct PathConfineMiddleware {
    root: PathBuf,
}

impl PathConfineMiddleware {
    /// `root` is the review repo (the agent's working_dir). Callers pass an
    /// already-canonicalized path (clix canonicalizes `--repo`); we normalize it
    /// lexically too so containment checks compare like-for-like.
    pub fn new(root: PathBuf) -> Self {
        Self {
            root: normalize_lexical(&root),
        }
    }
}

#[async_trait]
impl ToolMiddleware for PathConfineMiddleware {
    async fn before(
        &self,
        call: &mut ToolCall,
        _tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> BeforeOutcome {
        match check_arguments(&self.root, &call.arguments) {
            Ok(()) => BeforeOutcome::Proceed,
            Err(reason) => BeforeOutcome::deny(reason),
        }
    }
}

/// Reject if any whitelisted path field in `args` resolves outside `root`.
/// Unparseable args pass through — the tool will reject them itself (we only
/// enforce containment, never invent parse errors).
fn check_arguments(root: &Path, args: &str) -> Result<(), String> {
    let v: serde_json::Value = match serde_json::from_str(args) {
        Ok(v) => v,
        Err(_) => return Ok(()),
    };
    for key in PATH_KEYS {
        match v.get(key) {
            Some(serde_json::Value::String(s)) => check_one(root, s)?,
            Some(serde_json::Value::Array(items)) => {
                for it in items {
                    if let Some(s) = it.as_str() {
                        check_one(root, s)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn check_one(root: &Path, raw: &str) -> Result<(), String> {
    let p = Path::new(raw);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    };
    let normalized = normalize_lexical(&joined);
    if !normalized.starts_with(root) {
        return Err(format!(
            "path '{raw}' is outside the review repository; review tools may only access files within the repo"
        ));
    }
    if let Some(existing) = existing_prefix(&joined) {
        if let (Ok(canon_root), Ok(canon_existing)) =
            (std::fs::canonicalize(root), std::fs::canonicalize(existing))
        {
            if !canon_existing.starts_with(canon_root) {
                return Err(format!(
                    "path '{raw}' is outside the review repository; review tools may only access files within the repo"
                ));
            }
        }
    }
    Ok(())
}

/// Longest existing prefix of `p`, used to catch symlink escapes without requiring
/// the final path (or glob tail) to exist.
fn existing_prefix(p: &Path) -> Option<&Path> {
    for candidate in p.ancestors() {
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Lexical normalization: drop `.` and resolve `..` against prior components.
/// This catches `../` escapes before the later existing-prefix symlink check.
fn normalize_lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from("/work/repo")
    }

    #[test]
    fn rejects_grep_root() {
        // The actual production failure: `grep /` scanned the whole container → OOM.
        assert!(check_arguments(&root(), r#"{"pattern":"foo","path":"/"}"#).is_err());
    }

    #[test]
    fn allows_relative_inside_repo() {
        assert!(check_arguments(&root(), r#"{"file_path":"src/a.rs"}"#).is_ok());
    }

    #[test]
    fn allows_absolute_inside_repo() {
        assert!(check_arguments(&root(), r#"{"file_path":"/work/repo/src/a.rs"}"#).is_ok());
    }

    #[test]
    fn rejects_parent_escape() {
        assert!(check_arguments(&root(), r#"{"file_path":"../../etc/passwd"}"#).is_err());
    }

    #[test]
    fn rejects_absolute_outside_repo() {
        assert!(check_arguments(&root(), r#"{"file_path":"/etc/passwd"}"#).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(&outside, repo.join("link")).unwrap();

        let root = std::fs::canonicalize(&repo).unwrap();
        assert!(check_arguments(&root, r#"{"file_path":"link/secret.txt"}"#).is_err());
    }

    #[test]
    fn pattern_with_slash_not_treated_as_path() {
        // grep with no `path` field: a regex pattern containing '/' must NOT be
        // mistaken for a path argument.
        assert!(check_arguments(&root(), r#"{"pattern":"a/b/c"}"#).is_ok());
    }

    #[test]
    fn array_paths_any_escape_blocks() {
        // ast_grep `paths`: one inside, one escaping → blocked; all inside → ok.
        assert!(check_arguments(&root(), r#"{"paths":["src","/etc"]}"#).is_err());
        assert!(check_arguments(&root(), r#"{"paths":["src","lib"]}"#).is_ok());
    }

    #[test]
    fn missing_path_field_passes() {
        // grep default path "." (field absent) → tool resolves to working_dir, safe.
        assert!(check_arguments(&root(), r#"{"pattern":"foo"}"#).is_ok());
    }

    #[test]
    fn unparseable_args_pass_through() {
        assert!(check_arguments(&root(), "not json").is_ok());
    }
}
