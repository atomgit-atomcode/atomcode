//! `grep` — regex content search under a directory, gitignore-aware. Read-only ⇒
//! always `Safe`. Smart-case (case-insensitive unless the pattern has an uppercase
//! letter); an invalid regex falls back to a literal search. Build/VCS/cache dirs and
//! `.log` files are skipped. Neutral core — the production graph/semantic annotations
//! are dropped.

use super::read::lenient_usize;
use super::{err, is_skip_dir, not_found_hint, ok, resolve_path};
use crate::world::{FileSystem, LocalFs, SearchLine, SearchQuery};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use grep::regex::RegexMatcherBuilder;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

const DEFAULT_MAX_RESULTS: usize = 50;
const DEFAULT_CONTEXT: usize = 3;
const MAX_CONTEXT: usize = 10;
const MAX_DISPLAY_LINE: usize = 1000;
/// Hard upper cap on `max_results` — bounds the in-memory result buffer even if a
/// caller sends an enormous value.
const MAX_RESULTS_CAP: usize = 10_000;

pub struct GrepTool {
    /// Where the tree being searched lives. The walk and the per-file search
    /// are the world's ([`FileSystem::search`]); the pattern, the caps, what to
    /// skip and how a hit is shown stay here.
    world: Arc<dyn FileSystem>,
}

impl Default for GrepTool {
    fn default() -> Self {
        Self {
            world: Arc::new(LocalFs::unfenced()),
        }
    }
}

impl GrepTool {
    /// Search `world` instead of this machine's disk.
    pub fn with_world(world: Arc<dyn FileSystem>) -> Self {
        Self { world }
    }
}

#[derive(Deserialize)]
struct Args {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default, deserialize_with = "lenient_usize")]
    max_results: Option<usize>,
    #[serde(default, deserialize_with = "lenient_usize")]
    context: Option<usize>,
}

#[async_trait]
impl Tool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }
    fn description(&self) -> &str {
        "Search file contents by regular expression under a directory (gitignore-aware; \
         build/cache dirs and .log files are skipped). Prefer this over `bash grep`/`rg`. \
         Use it to LOCATE code, then read the exact window with `read_file` (offset/limit/\
         ranges) rather than dumping whole files or slicing them with a shell/`python` \
         script. Smart-case: case-insensitive unless the pattern contains an uppercase \
         letter. Escape regex metachars, e.g. `console\\.log\\(`. Relative paths resolve \
         against the working directory."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regex to search for" },
                "path": { "type": "string", "description": "Directory or file to search (default: the working directory)" },
                "max_results": { "type": "integer", "description": "Max matching lines to return (default 50)" },
                "context": { "type": "integer", "description": "Lines of context around each match (default 3, max 10)" }
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
                    "grep: invalid arguments: {e}. Expected {{\"pattern\":\"<regex>\"}}."
                ))
            }
        };
        let raw = a.path.clone().unwrap_or_else(|| ".".to_string());
        let root = resolve_path(&raw, &ctx.working_dir);
        match self.world.info(&root).await {
            Ok(m) if m.exists => {}
            // Denied is not missing — see the same note in `read`.
            Err(e) if e.is_denied() => return err(format!("grep: {e}")),
            _ => {
                return err(format!(
                    "grep: path not found: {}{}",
                    crate::pathnorm::to_display(&root),
                    not_found_hint(&root, &ctx.working_dir).await
                ))
            }
        }
        let max = a
            .max_results
            .unwrap_or(DEFAULT_MAX_RESULTS)
            .clamp(1, MAX_RESULTS_CAP);
        let context = a.context.unwrap_or(DEFAULT_CONTEXT).min(MAX_CONTEXT);

        // Smart-case + literal fallback. The decision is made here and the world
        // gets a final pattern: what counts as a regex is the caller's, not a
        // property of where the files live.
        let has_upper = a.pattern.chars().any(|c| c.is_uppercase());
        let case_insensitive = !has_upper;
        let pattern = if RegexMatcherBuilder::new()
            .case_insensitive(case_insensitive)
            .build(&a.pattern)
            .is_ok()
        {
            a.pattern.clone()
        } else {
            let literal = regex::escape(&a.pattern);
            if let Err(e) = RegexMatcherBuilder::new()
                .case_insensitive(case_insensitive)
                .build(&literal)
            {
                return err(format!("grep: invalid pattern '{}': {e}", a.pattern));
            }
            literal
        };

        let query = SearchQuery {
            pattern,
            case_insensitive,
            context,
            max_matches: max,
            skip_dir: Arc::new(is_skip_dir),
            skip_file: Arc::new(|path: &std::path::Path| {
                path.extension()
                    .map(|x| x.eq_ignore_ascii_case("log"))
                    .unwrap_or(false)
            }),
        };
        let base = ctx.working_dir.clone();
        let display_path = raw.clone();
        let shown_pattern = a.pattern.clone();
        match self.world.search(&root, &query).await {
            Ok(result) if result.lines.is_empty() => ok(format!(
                "No matches found for '{shown_pattern}' in {display_path} ({} files searched)",
                result.files_searched
            )),
            Ok(result) => {
                let lines: Vec<String> = result
                    .lines
                    .iter()
                    .map(|line| render_search_line(line, &base))
                    .collect();
                // Cap on the real MATCH count — not total output rows, which also include
                // context + `--` separators (that over-reported "capped" with any context).
                let capped = result.matches >= max;
                let mut out = lines.join("\n");
                if capped {
                    out.push_str(&format!("\n\n[Results capped at {max} matches]"));
                }
                ok(out)
            }
            Err(e) => err(format!("grep: {e}")),
        }
    }
}

/// `rel:num:content` for a match, `rel-num-content` for context, `--` between
/// non-contiguous groups — exactly the rows the old in-tool sink emitted, now
/// rendered from the world's raw lines.
fn render_search_line(line: &SearchLine, base: &std::path::Path) -> String {
    let rel = |path: &std::path::Path| {
        crate::pathnorm::to_display(path.strip_prefix(base).unwrap_or(path))
    };
    match line {
        SearchLine::Match { path, line, text } => {
            format!("{}:{line}:{}", rel(path), render_line(text))
        }
        SearchLine::Context { path, line, text } => {
            format!("{}-{line}-{}", rel(path), render_line(text))
        }
        SearchLine::Break => "--".to_string(),
    }
}

/// Render a raw line (bytes from the searcher) for display: strip the trailing line
/// ending, lossily decode, and truncate an over-long (e.g. minified) line.
fn render_line(bytes: &[u8]) -> String {
    let cow = String::from_utf8_lossy(bytes);
    let line = cow.strip_suffix('\n').unwrap_or(&cow);
    let line = line.strip_suffix('\r').unwrap_or(line);
    if line.chars().count() > MAX_DISPLAY_LINE {
        line.chars().take(MAX_DISPLAY_LINE).collect::<String>() + "…"
    } else {
        line.to_string()
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
    async fn finds_matches_with_line_numbers() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("a.rs"),
            "fn main() {\n    let TODO = 1;\n    other();\n}\n",
        )
        .unwrap();
        let r = GrepTool::default()
            .execute(r#"{"pattern":"TODO","path":"."}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("a.rs:2:"), "{}", r.content);
    }

    /// The real-world shape (a Windows user, 2026-08-05): a Gradle project whose `app/` has no
    /// `src/`. The model grepped `app/src`, got a bare "path not found", and spent the next
    /// three turns guessing deeper paths. The error must name where the tree actually stops.
    #[tokio::test]
    async fn missing_path_error_carries_the_nearest_existing_ancestor() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("app")).unwrap();
        std::fs::write(d.path().join("app/build.gradle"), "").unwrap();
        let r = GrepTool::default()
            .execute(
                r#"{"pattern":"Serial","path":"app/src/main/java"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(r.is_error, "{}", r.content);
        assert!(
            r.content.contains("Nearest existing directory"),
            "{}",
            r.content
        );
        assert!(r.content.contains("build.gradle"), "{}", r.content);
    }

    // Issue #722 parity (v2): weak models send max_results/context as a string ("50")
    // or float (50.0 / "3.0") instead of an integer; the args must still deserialize.
    #[test]
    fn args_accept_lenient_numeric_max_results_and_context() {
        let a: Args = serde_json::from_str(r#"{"pattern":"x","max_results":"50","context":3.0}"#)
            .expect("string max_results + float context must deserialize");
        assert_eq!(a.max_results, Some(50));
        assert_eq!(a.context, Some(3));

        let b: Args = serde_json::from_str(r#"{"pattern":"x","max_results":"50.0"}"#)
            .expect("float-string max_results must deserialize");
        assert_eq!(b.max_results, Some(50));
    }

    #[tokio::test]
    async fn smart_case_is_insensitive_for_lowercase_pattern() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "Hello World\n").unwrap();
        let r = GrepTool::default()
            .execute(r#"{"pattern":"hello"}"#, &ctx(d.path()))
            .await;
        assert!(r.content.contains("a.txt:1:"), "{}", r.content);
    }

    #[tokio::test]
    async fn zero_matches_is_success() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "nothing here\n").unwrap();
        let r = GrepTool::default()
            .execute(r#"{"pattern":"absent_xyz"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "zero matches must be a success: {}", r.content);
        assert!(r.content.contains("No matches found"), "{}", r.content);
    }

    #[tokio::test]
    async fn invalid_regex_falls_back_to_literal() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "value = foo(bar)\n").unwrap();
        // "foo(bar" is an invalid regex (unbalanced paren) → literal fallback finds it.
        let r = GrepTool::default()
            .execute(r#"{"pattern":"foo(bar"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("a.txt:1:"), "{}", r.content);
    }

    #[tokio::test]
    async fn context_lines_are_marked_and_groups_separated() {
        let d = tempfile::tempdir().unwrap();
        // 10 lines, matches on line 2 and line 8 → two non-contiguous groups at context 1.
        let content = (1..=10)
            .map(|i| match i {
                2 => "NEEDLE two".to_string(),
                8 => "NEEDLE eight".to_string(),
                _ => format!("line {i}"),
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(d.path().join("f.txt"), content + "\n").unwrap();
        let r = GrepTool::default()
            .execute(r#"{"pattern":"NEEDLE","context":1}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        // Match lines use `:`, context lines use `-`.
        assert!(
            r.content.contains("f.txt:2:NEEDLE two"),
            "match line: {}",
            r.content
        );
        assert!(
            r.content.contains("f.txt-1-line 1"),
            "before-context: {}",
            r.content
        );
        assert!(
            r.content.contains("f.txt-3-line 3"),
            "after-context: {}",
            r.content
        );
        assert!(
            r.content.contains("f.txt:8:NEEDLE eight"),
            "second match: {}",
            r.content
        );
        // Non-contiguous groups are separated by `--`.
        assert!(
            r.content.contains("\n--\n"),
            "group separator: {}",
            r.content
        );
    }

    #[tokio::test]
    async fn searches_a_large_file_via_streaming_not_whole_read() {
        let d = tempfile::tempdir().unwrap();
        // ~3 MB of filler + one match near the end. The streaming searcher finds it
        // WITHOUT a whole-file `read_to_string` (which was the OOM/freeze risk).
        let mut big = "filler line\n".repeat(250_000); // ~3 MB
        big.push_str("HAYSTACK_NEEDLE at the end\n");
        std::fs::write(d.path().join("big.txt"), big).unwrap();
        let r = GrepTool::default()
            .execute(r#"{"pattern":"HAYSTACK_NEEDLE"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(
            r.content.contains("big.txt:250001:HAYSTACK_NEEDLE"),
            "{}",
            r.content
        );
    }

    #[tokio::test]
    async fn giant_single_line_file_is_skipped_not_buffered_whole() {
        let d = tempfile::tempdir().unwrap();
        // One line LARGER than the heap cap (e.g. a minified bundle). The searcher errors
        // on it and skips it, instead of buffering the whole line into memory (the OOM case).
        let mut giant = String::from("NEEDLE ");
        giant.push_str(&"x".repeat(crate::world::MAX_LINE_BUF_BYTES + 1024)); // > cap, no newline
        std::fs::write(d.path().join("min.js"), &giant).unwrap();
        std::fs::write(d.path().join("ok.txt"), "NEEDLE\n").unwrap();
        let r = GrepTool::default()
            .execute(r#"{"pattern":"NEEDLE"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "grep must not error/hang on a giant line");
        assert!(
            r.content.contains("ok.txt:1:"),
            "normal match found: {}",
            &r.content[..r.content.len().min(120)]
        );
        assert!(
            !r.content.contains("min.js"),
            "over-cap single-line file must be skipped"
        );
    }

    #[tokio::test]
    async fn capped_message_counts_matches_not_output_rows() {
        let d = tempfile::tempdir().unwrap();
        // 3 scattered matches at context 3 → ~23 output ROWS but only 3 MATCHES.
        let lines: Vec<String> = (1..=30)
            .map(|i| {
                if i % 10 == 5 {
                    format!("HIT {i}")
                } else {
                    format!("line {i}")
                }
            })
            .collect();
        std::fs::write(d.path().join("f.txt"), lines.join("\n") + "\n").unwrap();
        // max_results 10: output rows (23) >= 10 but matches (3) < 10 → must NOT report capped.
        let r = GrepTool::default()
            .execute(
                r#"{"pattern":"HIT","max_results":10,"context":3}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(
            r.content.matches("HIT").count(),
            3,
            "exactly 3 matches: {}",
            r.content
        );
        assert!(
            !r.content.contains("Results capped"),
            "false 'capped' with only 3<10 matches: {}",
            r.content
        );
    }

    #[tokio::test]
    async fn skips_binary_files() {
        let d = tempfile::tempdir().unwrap();
        // A NUL byte ⇒ binary ⇒ the searcher quits and reports nothing for it.
        std::fs::write(d.path().join("blob"), b"\x00 NEEDLE inside binary\n").unwrap();
        std::fs::write(d.path().join("text.txt"), "NEEDLE\n").unwrap();
        let r = GrepTool::default()
            .execute(r#"{"pattern":"NEEDLE"}"#, &ctx(d.path()))
            .await;
        assert!(
            r.content.contains("text.txt:1:"),
            "text match: {}",
            r.content
        );
        assert!(
            !r.content.contains("blob"),
            "binary file must be skipped: {}",
            r.content
        );
    }

    #[tokio::test]
    async fn skips_gitignored_and_build_dirs() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("target")).unwrap();
        std::fs::write(d.path().join("target/junk.rs"), "NEEDLE\n").unwrap();
        std::fs::write(d.path().join("keep.rs"), "NEEDLE\n").unwrap();
        let r = GrepTool::default()
            .execute(r#"{"pattern":"NEEDLE"}"#, &ctx(d.path()))
            .await;
        assert!(r.content.contains("keep.rs:1:"), "{}", r.content);
        assert!(
            !r.content.contains("junk.rs"),
            "target/ should be skipped: {}",
            r.content
        );
    }
}
