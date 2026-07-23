//! `search_replace` — bulk find-and-replace across many files (literal or regex),
//! optionally scoped by a glob. Mutates the filesystem ⇒ always `Risky`. Neutral port of
//! the production tool, minus the coding bookkeeping (file_history / file_store / LSP) —
//! the L1 `ToolContext` has none of that. The (blocking) `ignore` walk + per-file reads
//! run on `spawn_blocking` so a hung filesystem can't stall the async worker.

use super::{coerce_eol, err, is_skip_dir, ok, resolve_path};
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
        if a.search.is_empty() {
            return err(
                "search_replace: search is empty — an empty pattern would corrupt every file."
                    .to_string(),
            );
        }
        let root = resolve_path(a.path.as_deref().unwrap_or("."), &ctx.working_dir);
        if !root.exists() {
            return err(format!(
                "search_replace: directory not found: {}",
                crate::pathnorm::to_display(&root)
            ));
        }

        // Regex mode compiles the pattern; literal mode matches the raw string verbatim
        // (no regex, so `$1` in the replacement stays literal and an EOL mismatch can be
        // tolerated per-file — see sr_scan).
        let re = if a.regex {
            match regex::Regex::new(&a.search) {
                Ok(r) => Some(r),
                Err(e) => return err(format!("search_replace: invalid regex '{}': {e}", a.search)),
            }
        } else {
            None
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
        let search = a.search.clone();
        let replace = a.replace.clone();
        let (modified, scanned) = tokio::task::spawn_blocking(move || {
            sr_scan(
                &scan_root,
                re.as_ref(),
                &search,
                &replace,
                glob_filter.as_ref(),
            )
        })
        .await
        .unwrap_or_else(|_| (Vec::new(), 0));

        // Phase 2: write the changed files (async).
        let mut total = 0usize;
        let mut report = Vec::new();
        for (path, new_content, count) in modified {
            if let Err(e) = tokio::fs::write(&path, &new_content).await {
                return err(format!(
                    "search_replace: failed to write {}: {e}",
                    crate::pathnorm::to_display(&path)
                ));
            }
            total += count;
            report.push(format!(
                "  {} ({count} replacements)",
                crate::pathnorm::to_display(&path)
            ));
        }

        if report.is_empty() {
            return ok(format!(
                "No matches for '{}' in {} ({scanned} files scanned).",
                a.search,
                crate::pathnorm::to_display(&root)
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
/// count of files scanned. `re.is_some()` ⇒ regex mode (capture-group `replace`); else
/// literal mode (verbatim `search`/`replace`, with per-file CRLF/LF tolerance).
fn sr_scan(
    root: &Path,
    re: Option<&regex::Regex>,
    search: &str,
    replace: &str,
    glob_filter: Option<&FileGlob>,
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
        let (new_content, count) = match re {
            Some(re) => {
                if !re.is_match(&content) {
                    continue;
                }
                (
                    re.replace_all(&content, replace).to_string(),
                    re.find_iter(&content).count(),
                )
            }
            None => {
                // Literal mode. Match verbatim first; on a literal hit the search already
                // agrees with the file's bytes, so search/replace are used as-is. Only if
                // that fails do we coerce BOTH to THIS file's EOL — rescuing an LF-copied
                // multi-line search against a CRLF file without injecting mixed endings.
                // Plain string replace keeps `$1` etc. verbatim (no capture-group expansion).
                let literal = content.matches(search).count();
                let (needle, repl, count) = if literal > 0 {
                    (search.to_string(), replace.to_string(), literal)
                } else {
                    let file_eol = if content.contains("\r\n") {
                        "\r\n"
                    } else {
                        "\n"
                    };
                    let n = coerce_eol(search, file_eol);
                    let c = content.matches(&n).count();
                    (n, coerce_eol(replace, file_eol), c)
                };
                if count == 0 {
                    continue;
                }
                (content.replace(&needle, &repl), count)
            }
        };
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
            self.matcher
                .is_match(rel.to_string_lossy().replace('\\', "/"))
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
            requester: None,
        }
    }

    #[tokio::test]
    async fn literal_replace_across_files() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "foo bar foo").unwrap();
        std::fs::write(d.path().join("b.txt"), "no match here").unwrap();
        std::fs::write(d.path().join("c.txt"), "foo").unwrap();
        let r = SearchReplaceTool
            .execute(r#"{"search":"foo","replace":"baz"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(
            r.content.contains("3 replacements across 2 files"),
            "{}",
            r.content
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "baz bar baz"
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("c.txt")).unwrap(),
            "baz"
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("b.txt")).unwrap(),
            "no match here"
        );
    }

    #[tokio::test]
    async fn glob_scopes_by_extension() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("keep.md"), "color").unwrap();
        std::fs::write(d.path().join("x.css"), "color").unwrap();
        let r = SearchReplaceTool
            .execute(
                r#"{"search":"color","replace":"colour","glob":"*.css"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(
            std::fs::read_to_string(d.path().join("x.css")).unwrap(),
            "colour"
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("keep.md")).unwrap(),
            "color",
            "md untouched"
        );
    }

    #[tokio::test]
    async fn literal_multiline_matches_crlf_file_and_preserves_crlf() {
        // A literal multi-line search whose break is `\n` (what read_file shows the
        // model) must still match a CRLF file, and the file must stay CRLF.
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "alpha\r\nbeta\r\n").unwrap();
        let r = SearchReplaceTool
            .execute(
                r#"{"search":"alpha\nbeta","replace":"ALPHA\nbeta"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "ALPHA\r\nbeta\r\n"
        );
    }

    #[tokio::test]
    async fn literal_match_writes_verbatim_no_crlf_injection() {
        // Mostly-LF file with one stray CRLF line. A literal multi-line replace of an LF
        // region must match it verbatim and NOT force the result to CRLF.
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("m.txt"), "head\r\nfoo\nbar\n").unwrap();
        let r = SearchReplaceTool
            .execute(
                r#"{"search":"foo\nbar","replace":"foo\nBAR"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(
            std::fs::read_to_string(d.path().join("m.txt")).unwrap(),
            "head\r\nfoo\nBAR\n"
        );
    }

    #[tokio::test]
    async fn empty_search_is_rejected() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "abc").unwrap();
        let r = SearchReplaceTool
            .execute(r#"{"search":"","replace":"X"}"#, &ctx(d.path()))
            .await;
        assert!(
            r.is_error,
            "empty search must be refused (would corrupt every file): {}",
            r.content
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "abc",
            "unchanged"
        );
    }

    #[tokio::test]
    async fn literal_replace_does_not_expand_dollar_groups() {
        // Literal mode must treat `$1` in the replacement verbatim (not a capture ref).
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "key=val").unwrap();
        let r = SearchReplaceTool
            .execute(r#"{"search":"val","replace":"$1x"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "key=$1x"
        );
    }

    #[tokio::test]
    async fn regex_with_capture_groups() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("v.rs"), "let v1 = 1; let v2 = 2;").unwrap();
        let r = SearchReplaceTool
            .execute(
                r#"{"search":"v(\\d)","replace":"w$1","regex":true}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(
            std::fs::read_to_string(d.path().join("v.rs")).unwrap(),
            "let w1 = 1; let w2 = 2;"
        );
    }

    #[tokio::test]
    async fn literal_does_not_treat_search_as_regex() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "a.b a_b axb").unwrap();
        // "a.b" literal must match only "a.b", not "axb" (which `.` would match in regex).
        let r = SearchReplaceTool
            .execute(r#"{"search":"a.b","replace":"Z"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "Z a_b axb"
        );
    }

    #[tokio::test]
    async fn no_matches_reports_and_is_not_error() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "nothing").unwrap();
        let r = SearchReplaceTool
            .execute(r#"{"search":"zzz","replace":"x"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("No matches"), "{}", r.content);
    }

    #[tokio::test]
    async fn invalid_regex_errors() {
        let d = tempfile::tempdir().unwrap();
        let r = SearchReplaceTool
            .execute(
                r#"{"search":"(unclosed","replace":"x","regex":true}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("invalid regex"), "{}", r.content);
    }

    #[tokio::test]
    async fn skips_skip_dirs() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("target")).unwrap();
        std::fs::write(d.path().join("target/gen.rs"), "foo").unwrap();
        std::fs::write(d.path().join("src.rs"), "foo").unwrap();
        let r = SearchReplaceTool
            .execute(r#"{"search":"foo","replace":"bar"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(
            std::fs::read_to_string(d.path().join("src.rs")).unwrap(),
            "bar"
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("target/gen.rs")).unwrap(),
            "foo",
            "target/ skipped"
        );
    }

    #[test]
    fn risk_is_risky() {
        assert_eq!(SearchReplaceTool.risk("{}"), RiskLevel::Risky);
    }
}
