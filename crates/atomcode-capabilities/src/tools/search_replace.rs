//! `search_replace` — bulk find-and-replace across many files (literal or regex),
//! optionally scoped by a glob. Mutates the filesystem ⇒ always `Risky`. Neutral port of
//! the production tool, minus the coding bookkeeping (file_history / file_store / LSP) —
//! the L1 `ToolContext` has none of that. The (blocking) `ignore` walk + per-file reads
//! run on `spawn_blocking` so a hung filesystem can't stall the async worker.

use super::{coerce_eol, err, is_skip_dir, ok, resolve_path};
use crate::world::{FileSystem, LocalFs};
use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use globset::{Glob, GlobMatcher};
use serde::Deserialize;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct SearchReplaceTool {
    /// The tree being rewritten. Walk, read and write all go through it, so a
    /// fenced or read-only world refuses the write it would have refused for
    /// `write_file` — which is the reason this tool waited for `walk`: routing
    /// only its writes would have found matches outside the fence and then
    /// failed to apply some of them, a worse outcome than not routing at all.
    world: Arc<dyn FileSystem>,
}

impl Default for SearchReplaceTool {
    fn default() -> Self {
        Self {
            world: Arc::new(LocalFs::unfenced()),
        }
    }
}

impl SearchReplaceTool {
    /// Rewrite files in `world` instead of on this machine's disk.
    pub fn with_world(world: Arc<dyn FileSystem>) -> Self {
        Self { world }
    }
}

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
        match self.world.info(&root).await {
            Ok(m) if m.exists => {}
            // Denied is not missing — see the same note in `read`.
            Err(e) if e.is_denied() => return err(format!("search_replace: {e}")),
            _ => {
                return err(format!(
                    "search_replace: directory not found: {}",
                    crate::pathnorm::to_display(&root)
                ))
            }
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

        // Phase 1: walk + read + compute replacements, all through the world.
        let (modified, scanned) = match sr_scan(
            self.world.as_ref(),
            &root,
            re.as_ref(),
            &a.search,
            &a.replace,
            glob_filter.as_ref(),
        )
        .await
        {
            Ok(scan) => scan,
            Err(e) => return err(format!("search_replace: {e}")),
        };

        // Phase 2: write the changed files.
        let mut total = 0usize;
        let mut report = Vec::new();
        for (path, new_content, count) in modified {
            if let Err(e) = self.world.write_text(&path, &new_content).await {
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

/// Walk + read + replace computation, through the world. Does NOT write — returns
/// `(path, new_content, replacement_count)` per changed file plus the count of files
/// scanned. `re.is_some()` ⇒ regex mode (capture-group `replace`); else literal mode
/// (verbatim `search`/`replace`, with per-file CRLF/LF tolerance).
async fn sr_scan(
    world: &dyn FileSystem,
    root: &Path,
    re: Option<&regex::Regex>,
    search: &str,
    replace: &str,
    glob_filter: Option<&FileGlob>,
) -> Result<(Vec<(PathBuf, String, usize)>, usize), crate::world::FsError> {
    let skip: crate::world::SkipDir = Arc::new(is_skip_dir);
    let mut modified = Vec::new();
    let mut scanned = 0usize;
    for path in world.walk(root, &skip).await? {
        if let Some(g) = glob_filter {
            if !g.is_match(&path, root) {
                continue;
            }
        }
        let content = match world.read_text(&path).await {
            Ok(c) => c,
            Err(_) => continue, // skip binary / unreadable
        };
        scanned += 1;
        if let Some((new_content, count)) = replace_in(&content, re, search, replace) {
            modified.push((path, new_content, count));
        }
    }
    Ok((modified, scanned))
}

/// The replacement for one file's content, or `None` when nothing changes. Pure, so
/// the world only ever sees a read and a write.
fn replace_in(
    content: &str,
    re: Option<&regex::Regex>,
    search: &str,
    replace: &str,
) -> Option<(String, usize)> {
    let (new_content, count) = match re {
        Some(re) => {
            if !re.is_match(content) {
                return None;
            }
            (
                re.replace_all(content, replace).to_string(),
                re.find_iter(content).count(),
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
                return None;
            }
            (content.replace(&needle, &repl), count)
        }
    };
    (new_content != content).then_some((new_content, count))
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

    #[tokio::test]
    async fn a_read_only_world_finds_the_matches_and_refuses_the_rewrite() {
        // The reason this tool waited for `walk`: with only its writes routed it
        // would find matches the world was never asked about and then fail to
        // apply them. Now the walk, the read and the refusal are all the world's.
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "foo bar").unwrap();
        let tool = SearchReplaceTool::with_world(Arc::new(LocalFs::read_only(d.path())));
        let r = tool
            .execute(r#"{"search":"foo","replace":"baz"}"#, &ctx(d.path()))
            .await;
        assert!(r.is_error, "{}", r.content);
        assert!(r.content.contains("read-only"), "{}", r.content);
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "foo bar",
            "the world refused; the file is untouched"
        );
    }

    #[tokio::test]
    async fn a_fenced_world_refuses_a_root_outside_it_before_walking() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("inside")).unwrap();
        std::fs::write(d.path().join("outside.txt"), "foo").unwrap();
        let tool = SearchReplaceTool::with_world(Arc::new(LocalFs::new(d.path().join("inside"))));
        let r = tool
            .execute(
                r#"{"search":"foo","replace":"baz","path":".."}"#,
                &ctx(&d.path().join("inside")),
            )
            .await;
        assert!(r.is_error, "{}", r.content);
        assert!(
            r.content.contains("outside the world's root"),
            "{}",
            r.content
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("outside.txt")).unwrap(),
            "foo"
        );
    }

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
        let r = SearchReplaceTool::default()
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
        let r = SearchReplaceTool::default()
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
        let r = SearchReplaceTool::default()
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
        let r = SearchReplaceTool::default()
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
        let r = SearchReplaceTool::default()
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
        let r = SearchReplaceTool::default()
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
        let r = SearchReplaceTool::default()
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
        let r = SearchReplaceTool::default()
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
        let r = SearchReplaceTool::default()
            .execute(r#"{"search":"zzz","replace":"x"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("No matches"), "{}", r.content);
    }

    #[tokio::test]
    async fn invalid_regex_errors() {
        let d = tempfile::tempdir().unwrap();
        let r = SearchReplaceTool::default()
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
        let r = SearchReplaceTool::default()
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
        assert_eq!(SearchReplaceTool::default().risk("{}"), RiskLevel::Risky);
    }
}
