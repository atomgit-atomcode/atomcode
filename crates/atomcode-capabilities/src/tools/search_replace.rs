//! `search_replace` — bulk find-and-replace across many files (literal or regex),
//! optionally scoped by a glob. Mutates the filesystem ⇒ always `Risky`. Neutral port of
//! the production tool, minus the coding bookkeeping (file_history / file_store / LSP) —
//! the L1 `ToolContext` has none of that. The (blocking) `ignore` walk + per-file reads
//! run on `spawn_blocking` so a hung filesystem can't stall the async worker.

use super::{err, is_skip_dir, ok, resolve_path};
use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use globset::{Glob, GlobMatcher};
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};

pub struct SearchReplaceTool;

#[derive(Deserialize)]
struct Args {
    search: String,
    replace: String,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    regex: bool,
}

#[async_trait]
impl Tool for SearchReplaceTool {
    fn name(&self) -> &str {
        "search_replace"
    }
    fn description(&self) -> &str {
        "Find and replace text across MANY files at once — replaces every occurrence in \
         every matching file. Use for project-wide renames (a CSS class, an import, a \
         config key, a string literal). For a single file, prefer `edit_file`. \
         `regex:true` enables regex (with `$1`/`$2` capture groups in `replace`); the \
         default is literal matching. `glob` limits scope (e.g. \"*.rs\", \"src/**/*.ts\"); \
         `path` sets the search root (default: working directory)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "search": { "type": "string", "description": "Text or regex pattern to find" },
                "replace": { "type": "string", "description": "Replacement text (use $1, $2 for regex captures)" },
                "glob": { "type": "string", "description": "File pattern to limit scope, e.g. \"*.rs\", \"src/**/*.ts\" (default: all files)" },
                "path": { "type": "string", "description": "Directory to search in (default: working directory)" },
                "regex": { "type": "boolean", "description": "Use regex matching (default: false = literal)" }
            },
            "required": ["search", "replace"]
        })
    }
    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Risky // mutates the filesystem
    }
    fn always_grant_scope(&self, _args: &str) -> String {
        // Tool-wide: "总是 / Always" approves every search-replace this session (v1 parity).
        String::new()
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return err(format!(
                    "search_replace: invalid arguments: {e}. Expected {{\"search\":..., \"replace\":...}}."
                ))
            }
        };
        let root = resolve_path(a.path.as_deref().unwrap_or("."), &ctx.working_dir);
        if !root.exists() {
            return err(format!("search_replace: directory not found: {}", root.display()));
        }

        // Build the matcher: literal patterns are regex-escaped so they match verbatim.
        let re = if a.regex {
            match regex::Regex::new(&a.search) {
                Ok(r) => r,
                Err(e) => return err(format!("search_replace: invalid regex '{}': {e}", a.search)),
            }
        } else {
            // escape() output is always a valid regex.
            regex::Regex::new(&regex::escape(&a.search)).expect("escaped literal is valid regex")
        };
        let glob_filter = match a.glob.as_deref() {
            Some(p) => match FileGlob::new(p) {
                Ok(g) => Some(g),
                Err(e) => return err(format!("search_replace: invalid glob '{p}': {e}")),
            },
            None => None,
        };

        // Phase 1: walk + read + compute replacements off the async worker.
        let scan_root = root.clone();
        let replace = a.replace.clone();
        let (modified, scanned) = tokio::task::spawn_blocking(move || {
            sr_scan(&scan_root, &re, glob_filter.as_ref(), &replace)
        })
        .await
        .unwrap_or_else(|_| (Vec::new(), 0));

        // Phase 2: write the changed files (async).
        let mut total = 0usize;
        let mut report = Vec::new();
        for (path, new_content, count) in modified {
            if let Err(e) = tokio::fs::write(&path, &new_content).await {
                return err(format!("search_replace: failed to write {}: {e}", path.display()));
            }
            total += count;
            report.push(format!("  {} ({count} replacements)", path.display()));
        }

        if report.is_empty() {
            return ok(format!(
                "No matches for '{}' in {} ({scanned} files scanned).",
                a.search,
                root.display()
            ));
        }
        ok(format!(
            "Replaced '{}' → '{}': {total} replacements across {} files.\n{}",
            a.search,
            a.replace,
            report.len(),
            report.join("\n")
        ))
    }
}

/// Synchronous walk + read + replace computation (runs inside `spawn_blocking`). Does NOT
/// write — returns `(path, new_content, replacement_count)` per changed file plus the
/// count of files scanned.
fn sr_scan(
    root: &Path,
    re: &regex::Regex,
    glob_filter: Option<&FileGlob>,
    replace: &str,
) -> (Vec<(PathBuf, String, usize)>, usize) {
    let walk = WalkBuilder::new(root)
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

    let mut modified = Vec::new();
    let mut scanned = 0usize;
    for entry in walk.flatten() {
        if !entry.file_type().map_or(false, |ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        if let Some(g) = glob_filter {
            if !g.is_match(path, root) {
                continue;
            }
        }
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue, // skip binary / unreadable
        };
        scanned += 1;
        if !re.is_match(&content) {
            continue;
        }
        let count = re.find_iter(&content).count();
        let new_content = re.replace_all(&content, replace).to_string();
        if new_content != content {
            modified.push((path.to_path_buf(), new_content, count));
        }
    }
    (modified, scanned)
}

/// A file-scope glob. A pattern with a `/` matches against the path RELATIVE to the
/// search root (e.g. `src/**/*.ts`); a bare pattern matches the FILE NAME only (e.g.
/// `*.rs`).
struct FileGlob {
    has_path: bool,
    matcher: GlobMatcher,
}

impl FileGlob {
    fn new(pattern: &str) -> Result<Self, globset::Error> {
        let normalized = pattern.replace('\\', "/");
        Ok(Self {
            has_path: normalized.contains('/'),
            matcher: Glob::new(&normalized)?.compile_matcher(),
        })
    }

    fn is_match(&self, file_path: &Path, root: &Path) -> bool {
        if self.has_path {
            let rel = file_path.strip_prefix(root).unwrap_or(file_path);
            self.matcher.is_match(rel.to_string_lossy().replace('\\', "/"))
        } else {
            match file_path.file_name().and_then(|n| n.to_str()) {
                Some(name) => self.matcher.is_match(name),
                None => false,
            }
        }
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
        }
    }

    #[tokio::test]
    async fn literal_replace_across_files() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "foo bar foo").unwrap();
        std::fs::write(d.path().join("b.txt"), "no match here").unwrap();
        std::fs::write(d.path().join("c.txt"), "foo").unwrap();
        let r = SearchReplaceTool.execute(r#"{"search":"foo","replace":"baz"}"#, &ctx(d.path())).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("3 replacements across 2 files"), "{}", r.content);
        assert_eq!(std::fs::read_to_string(d.path().join("a.txt")).unwrap(), "baz bar baz");
        assert_eq!(std::fs::read_to_string(d.path().join("c.txt")).unwrap(), "baz");
        assert_eq!(std::fs::read_to_string(d.path().join("b.txt")).unwrap(), "no match here");
    }

    #[tokio::test]
    async fn glob_scopes_by_extension() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("keep.md"), "color").unwrap();
        std::fs::write(d.path().join("x.css"), "color").unwrap();
        let r = SearchReplaceTool
            .execute(r#"{"search":"color","replace":"colour","glob":"*.css"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(std::fs::read_to_string(d.path().join("x.css")).unwrap(), "colour");
        assert_eq!(std::fs::read_to_string(d.path().join("keep.md")).unwrap(), "color", "md untouched");
    }

    #[tokio::test]
    async fn regex_with_capture_groups() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("v.rs"), "let v1 = 1; let v2 = 2;").unwrap();
        let r = SearchReplaceTool
            .execute(r#"{"search":"v(\\d)","replace":"w$1","regex":true}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(std::fs::read_to_string(d.path().join("v.rs")).unwrap(), "let w1 = 1; let w2 = 2;");
    }

    #[tokio::test]
    async fn literal_does_not_treat_search_as_regex() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "a.b a_b axb").unwrap();
        // "a.b" literal must match only "a.b", not "axb" (which `.` would match in regex).
        let r = SearchReplaceTool.execute(r#"{"search":"a.b","replace":"Z"}"#, &ctx(d.path())).await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(std::fs::read_to_string(d.path().join("a.txt")).unwrap(), "Z a_b axb");
    }

    #[tokio::test]
    async fn no_matches_reports_and_is_not_error() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "nothing").unwrap();
        let r = SearchReplaceTool.execute(r#"{"search":"zzz","replace":"x"}"#, &ctx(d.path())).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("No matches"), "{}", r.content);
    }

    #[tokio::test]
    async fn invalid_regex_errors() {
        let d = tempfile::tempdir().unwrap();
        let r = SearchReplaceTool.execute(r#"{"search":"(unclosed","replace":"x","regex":true}"#, &ctx(d.path())).await;
        assert!(r.is_error);
        assert!(r.content.contains("invalid regex"), "{}", r.content);
    }

    #[tokio::test]
    async fn skips_skip_dirs() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("target")).unwrap();
        std::fs::write(d.path().join("target/gen.rs"), "foo").unwrap();
        std::fs::write(d.path().join("src.rs"), "foo").unwrap();
        let r = SearchReplaceTool.execute(r#"{"search":"foo","replace":"bar"}"#, &ctx(d.path())).await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(std::fs::read_to_string(d.path().join("src.rs")).unwrap(), "bar");
        assert_eq!(std::fs::read_to_string(d.path().join("target/gen.rs")).unwrap(), "foo", "target/ skipped");
    }

    #[test]
    fn risk_is_risky() {
        assert_eq!(SearchReplaceTool.risk("{}"), RiskLevel::Risky);
    }
}
