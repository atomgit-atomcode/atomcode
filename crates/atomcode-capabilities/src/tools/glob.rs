//! `glob` — find files by glob pattern under a base directory, gitignore-aware.
//! Read-only ⇒ always `Safe`. Standard glob semantics (`**` crosses directories, `*`
//! does not) via `globset` with `literal_separator(true)`. Build/VCS/cache dirs are
//! skipped; results sorted, capped at 100.

use super::{err, is_skip_dir, ok, resolve_path};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use globset::GlobBuilder;
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::json;

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
    // read-only → risk() defaults to Safe.
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => return err(format!("glob: invalid arguments: {e}. Expected {{\"pattern\":\"<glob>\"}}.")),
        };
        let raw = a.path.clone().unwrap_or_else(|| ".".to_string());
        let base = resolve_path(&raw, &ctx.working_dir);
        match tokio::fs::metadata(&base).await {
            Ok(m) if m.is_dir() => {}
            _ => return err(format!("glob: base directory does not exist: {}", base.display())),
        }

        let matcher = match GlobBuilder::new(&a.pattern).literal_separator(true).build() {
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
                    let shown = path.strip_prefix(&wd).unwrap_or(path).display().to_string();
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

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::tool::ToolContext;
    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext { working_dir: dir.to_path_buf(), cancel: CancellationToken::new(), progress: atomcode_kernel::tool::ProgressSink::noop() }
    }

    #[tokio::test]
    async fn matches_recursive_pattern() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("src/sub")).unwrap();
        std::fs::write(d.path().join("src/a.rs"), "").unwrap();
        std::fs::write(d.path().join("src/sub/b.rs"), "").unwrap();
        std::fs::write(d.path().join("src/c.txt"), "").unwrap();
        let r = GlobTool.execute(r#"{"pattern":"**/*.rs"}"#, &ctx(d.path())).await;
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
        let r = GlobTool.execute(r#"{"pattern":"*.rs"}"#, &ctx(d.path())).await;
        assert!(r.content.contains("top.rs"), "{}", r.content);
        assert!(!r.content.contains("deep.rs"), "* must not cross /: {}", r.content);
    }

    #[tokio::test]
    async fn no_match_reports_cleanly() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "").unwrap();
        let r = GlobTool.execute(r#"{"pattern":"**/*.zig"}"#, &ctx(d.path())).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("No files matching"), "{}", r.content);
    }

    #[tokio::test]
    async fn skips_build_dirs() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("target")).unwrap();
        std::fs::write(d.path().join("target/x.rs"), "").unwrap();
        std::fs::write(d.path().join("keep.rs"), "").unwrap();
        let r = GlobTool.execute(r#"{"pattern":"**/*.rs"}"#, &ctx(d.path())).await;
        assert!(r.content.contains("keep.rs"), "{}", r.content);
        assert!(!r.content.contains("target/x.rs"), "{}", r.content);
    }
}
