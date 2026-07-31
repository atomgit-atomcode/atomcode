//! `glob` — find files by glob pattern under a base directory, gitignore-aware.
//! Read-only ⇒ always `Safe`. Standard glob semantics (`**` crosses directories, `*`
//! does not) via `globset` with `literal_separator(true)`. Build/VCS/cache dirs are
//! skipped; results sorted, capped at 100.

use super::{err, is_absolute_path, is_skip_dir, ok, resolve_path};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use globset::GlobBuilder;
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;

const MAX_RESULTS: usize = 100;

pub struct GlobTool;

#[derive(Deserialize)]
struct Args {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
}

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> &str {
        "Find files by glob pattern (e.g. `**/*.rs`, `src/**/*.ts`) under a base \
         directory, gitignore-aware. `**` crosses directories, `*` does not. Build/VCS/ \
         cache directories are skipped. Relative base paths resolve against the working \
         directory."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern, e.g. **/*.rs" },
                "path": { "type": "string", "description": "Base directory to search (default: the working directory)" }
            },
            "required": ["pattern"]
        })
    }
    /// No side effects — a pure read. Makes it `parallel_safe` (concurrent
    /// execution) and allowed in plan mode.
    fn read_only_hint(&self) -> bool {
        true
    }
    // read-only → risk() defaults to Safe.
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return err(format!(
                    "glob: invalid arguments: {e}. Expected {{\"pattern\":\"<glob>\"}}."
                ))
            }
        };
        // Models routinely paste an absolute path straight into `pattern` (e.g.
        // `G:/VR2024/keystore/*`) with no `path` base. Without honoring that, the walk
        // would run in the working dir and silently match nothing — making an existing
        // file look like it "does not exist". An absolute prefix in the pattern wins
        // over `path`; otherwise fall back to `path` (default: the working dir).
        let (base, match_pattern) = match split_absolute_base(&a.pattern) {
            Some((dir, rest)) => (dir, rest),
            None => {
                let raw = a.path.clone().unwrap_or_else(|| ".".to_string());
                (resolve_path(&raw, &ctx.working_dir), a.pattern.clone())
            }
        };
        match tokio::fs::metadata(&base).await {
            Ok(m) if m.is_dir() => {}
            _ => {
                return err(format!(
                    "glob: base directory does not exist: {}",
                    crate::pathnorm::to_display(&base)
                ))
            }
        }

        let matcher = match GlobBuilder::new(&match_pattern)
            .literal_separator(true)
            .build()
        {
            Ok(g) => g.compile_matcher(),
            Err(e) => return err(format!("glob: invalid pattern '{}': {e}", a.pattern)),
        };

        let wd = ctx.working_dir.clone();
        let base2 = base.clone();
        let pattern = a.pattern.clone();
        let res = tokio::task::spawn_blocking(move || {
            let mut hits: Vec<String> = Vec::new();
            let walk = WalkBuilder::new(&base2)
                .hidden(true)
                .git_ignore(true)
                .git_global(true)
                .git_exclude(true)
                .filter_entry(|e| {
                    if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        if let Some(name) = e.file_name().to_str() {
                            return !is_skip_dir(name);
                        }
                    }
                    true
                })
                .build();
            for entry in walk.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                // Match the path RELATIVE to the base (standard glob semantics).
                let rel = path.strip_prefix(&base2).unwrap_or(path);
                if matcher.is_match(rel) {
                    // Display relative to the working dir for usable paths.
                    let shown = crate::pathnorm::to_display(path.strip_prefix(&wd).unwrap_or(path));
                    hits.push(shown);
                }
            }
            hits.sort();
            hits
        })
        .await;

        match res {
            Ok(hits) if hits.is_empty() => ok(format!("No files matching \"{pattern}\"")),
            Ok(mut hits) => {
                let total = hits.len();
                let extra = total.saturating_sub(MAX_RESULTS);
                if total > MAX_RESULTS {
                    hits.truncate(MAX_RESULTS);
                }
                let mut out = format!("{total} files found:\n{}", hits.join("\n"));
                if extra > 0 {
                    out.push_str(&format!("\n[{extra} more files not shown]"));
                }
                ok(out)
            }
            Err(_) => err("glob: search task failed".to_string()),
        }
    }
}

/// If `pattern` begins with an ABSOLUTE directory prefix (the leading run of literal,
/// glob-free path segments), split it off as a search base and return the remaining
/// pattern relative to it. Splits on BOTH `/` and `\` and normalizes the remainder to
/// `/` so Windows-style paths work regardless of build target (`\` is glob-escape in
/// globset, so it must not survive into the matcher). Returns `None` for purely
/// relative patterns (e.g. `**/*.rs`, `src/**/*.ts`), which keep the existing
/// base-relative behavior.
fn split_absolute_base(pattern: &str) -> Option<(PathBuf, String)> {
    // Everything before the first glob metacharacter is a literal path region.
    let scan_end = pattern.find(['*', '?', '[', '{']).unwrap_or(pattern.len());
    // The base ends at the last separator within that literal region.
    let sep = pattern[..scan_end].rfind(['/', '\\'])?;
    let dir = &pattern[..sep];
    // A `~`-prefixed base is an absolute location too (parity with the `path` arg,
    // which resolves `~` via resolve_path); expand it so `glob("~/proj/**/*.rs")`
    // isn't silently walked relative to cwd.
    let base = if let Some(home) = crate::pathutil::expand_tilde(dir) {
        home
    } else if !dir.is_empty() && is_absolute_path(dir) {
        PathBuf::from(dir)
    } else {
        return None;
    };
    let rest = pattern[sep + 1..].replace('\\', "/");
    // A trailing separator (a pasted directory path) leaves no remainder — list the
    // directory's direct children rather than building an empty matcher that matches
    // nothing (which would falsely report "No files matching").
    let rest = if rest.is_empty() {
        "*".to_string()
    } else {
        rest
    };
    Some((base, rest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::tool::ToolContext;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn split_absolute_base_handles_windows_and_unix_roots() {
        // Windows drive, forward slashes.
        let (base, rest) = split_absolute_base("G:/VR2024/keystore/*").unwrap();
        assert_eq!(base, PathBuf::from("G:/VR2024/keystore"));
        assert_eq!(rest, "*");
        // Windows drive, backslashes + recursive glob → remainder normalized to `/`.
        let (base, rest) = split_absolute_base(r"G:\VR2024\**\*.jks").unwrap();
        assert_eq!(base, PathBuf::from(r"G:\VR2024"));
        assert_eq!(rest, "**/*.jks");
        // An absolute exact file path (no metachar) is a degenerate glob that resolves
        // to its own directory + literal name.
        let (base, rest) = split_absolute_base("/abs/dir/screenshare.jks").unwrap();
        assert_eq!(base, PathBuf::from("/abs/dir"));
        assert_eq!(rest, "screenshare.jks");
        // A trailing separator (a pasted directory path) leaves no remainder; it must
        // become a "list this dir" glob, not an empty matcher that matches nothing.
        let (base, rest) = split_absolute_base("/abs/dir/").unwrap();
        assert_eq!(base, PathBuf::from("/abs/dir"));
        assert_eq!(rest, "*");
        // Relative patterns are left for base-relative matching.
        assert!(split_absolute_base("**/*.rs").is_none());
        assert!(split_absolute_base("src/**/*.ts").is_none());
    }

    #[test]
    fn split_absolute_base_expands_leading_tilde() {
        // A `~/…` base is absolute (home-relative), NOT a cwd-relative walk — parity
        // with the `path` arg. Assert relative to the same home the code reads.
        if let Some(home) = crate::pathutil::home_dir() {
            let (base, rest) = split_absolute_base("~/proj/**/*.rs").unwrap();
            assert_eq!(base, home.join("proj"));
            assert_eq!(rest, "**/*.rs");
            // Bare `~/` base lists the home dir's children.
            let (base, rest) = split_absolute_base("~/*").unwrap();
            assert_eq!(base, home);
            assert_eq!(rest, "*");
        }
    }

    fn ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        }
    }

    #[tokio::test]
    async fn matches_recursive_pattern() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src/sub")).unwrap();
        std::fs::write(d.path().join("src/a.rs"), "").unwrap();
        std::fs::write(d.path().join("src/sub/b.rs"), "").unwrap();
        std::fs::write(d.path().join("src/c.txt"), "").unwrap();
        let r = GlobTool
            .execute(r#"{"pattern":"**/*.rs"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("src/a.rs"), "{}", r.content);
        assert!(r.content.contains("src/sub/b.rs"), "{}", r.content);
        assert!(!r.content.contains("c.txt"), "{}", r.content);
    }

    #[tokio::test]
    async fn single_star_does_not_cross_dirs() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src")).unwrap();
        std::fs::write(d.path().join("top.rs"), "").unwrap();
        std::fs::write(d.path().join("src/deep.rs"), "").unwrap();
        let r = GlobTool
            .execute(r#"{"pattern":"*.rs"}"#, &ctx(d.path()))
            .await;
        assert!(r.content.contains("top.rs"), "{}", r.content);
        assert!(
            !r.content.contains("deep.rs"),
            "* must not cross /: {}",
            r.content
        );
    }

    #[tokio::test]
    async fn no_match_reports_cleanly() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "").unwrap();
        let r = GlobTool
            .execute(r#"{"pattern":"**/*.zig"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("No files matching"), "{}", r.content);
    }

    #[tokio::test]
    async fn absolute_pattern_searches_outside_working_dir() {
        // The target lives OUTSIDE the working directory; the model pastes its
        // absolute path straight into `pattern` with no `path` base (exactly what
        // happened with `G:\VR2024\keystore\screenshare.jks`). It must still be found.
        let target = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(target.path().join("keystore")).unwrap();
        std::fs::write(target.path().join("keystore/screenshare.jks"), "").unwrap();

        let work = tempfile::tempdir().unwrap(); // unrelated cwd, on a "different drive"
        let pattern = format!("{}/keystore/*", target.path().display());
        let args = serde_json::json!({ "pattern": pattern }).to_string();
        let r = GlobTool.execute(&args, &ctx(work.path())).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("screenshare.jks"), "{}", r.content);
    }

    #[tokio::test]
    async fn absolute_recursive_pattern_searches_outside_working_dir() {
        let target = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(target.path().join("keystore")).unwrap();
        std::fs::write(target.path().join("keystore/screenshare.jks"), "").unwrap();

        let work = tempfile::tempdir().unwrap();
        let pattern = format!("{}/**/*.jks", target.path().display());
        let args = serde_json::json!({ "pattern": pattern }).to_string();
        let r = GlobTool.execute(&args, &ctx(work.path())).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("screenshare.jks"), "{}", r.content);
    }

    #[tokio::test]
    async fn absolute_directory_with_trailing_slash_lists_children() {
        // A model pastes a bare absolute directory path (trailing slash, no glob).
        let target = tempfile::tempdir().unwrap();
        std::fs::write(target.path().join("screenshare.jks"), "").unwrap();
        let work = tempfile::tempdir().unwrap();
        let pattern = format!("{}/", target.path().display());
        let args = serde_json::json!({ "pattern": pattern }).to_string();
        let r = GlobTool.execute(&args, &ctx(work.path())).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("screenshare.jks"), "{}", r.content);
    }

    #[tokio::test]
    async fn skips_build_dirs() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("target")).unwrap();
        std::fs::write(d.path().join("target/x.rs"), "").unwrap();
        std::fs::write(d.path().join("keep.rs"), "").unwrap();
        let r = GlobTool
            .execute(r#"{"pattern":"**/*.rs"}"#, &ctx(d.path()))
            .await;
        assert!(r.content.contains("keep.rs"), "{}", r.content);
        assert!(!r.content.contains("target/x.rs"), "{}", r.content);
    }
}
