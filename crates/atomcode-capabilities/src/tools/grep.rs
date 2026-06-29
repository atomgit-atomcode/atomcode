//! `grep` — regex content search under a directory, gitignore-aware. Read-only ⇒
//! always `Safe`. Smart-case (case-insensitive unless the pattern has an uppercase
//! letter); an invalid regex falls back to a literal search. Build/VCS/cache dirs and
//! `.log` files are skipped. Neutral core — the production graph/semantic annotations
//! are dropped.

use super::read::lenient_usize;
use super::{err, is_skip_dir, ok, resolve_path};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use ignore::WalkBuilder;
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::json;

const DEFAULT_MAX_RESULTS: usize = 50;
const DEFAULT_CONTEXT: usize = 3;
const MAX_CONTEXT: usize = 10;
const MAX_DISPLAY_LINE: usize = 1000;

pub struct GrepTool;

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
         build/cache dirs and .log files are skipped). Smart-case: case-insensitive \
         unless the pattern contains an uppercase letter. Escape regex metachars, e.g. \
         `console\\.log\\(`. Relative paths resolve against the working directory."
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
    // read-only → risk() defaults to Safe.
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => return err(format!("grep: invalid arguments: {e}. Expected {{\"pattern\":\"<regex>\"}}.")),
        };
        let raw = a.path.clone().unwrap_or_else(|| ".".to_string());
        let root = resolve_path(&raw, &ctx.working_dir);
        if tokio::fs::metadata(&root).await.is_err() {
            return err(format!("grep: path not found: {}", root.display()));
        }
        let max = a.max_results.unwrap_or(DEFAULT_MAX_RESULTS).max(1);
        let context = a.context.unwrap_or(DEFAULT_CONTEXT).min(MAX_CONTEXT);

        // Smart-case + literal fallback.
        let has_upper = a.pattern.chars().any(|c| c.is_uppercase());
        let re = match RegexBuilder::new(&a.pattern).case_insensitive(!has_upper).build() {
            Ok(re) => re,
            Err(_) => match RegexBuilder::new(&regex::escape(&a.pattern)).case_insensitive(!has_upper).build() {
                Ok(re) => re,
                Err(e) => return err(format!("grep: invalid pattern '{}': {e}", a.pattern)),
            },
        };

        let base = ctx.working_dir.clone();
        let pattern = a.pattern.clone();
        let display_path = raw.clone();
        let res = tokio::task::spawn_blocking(move || search(&root, &re, max, context, &base)).await;
        match res {
            Ok((lines, files)) if lines.is_empty() => ok(format!(
                "No matches found for '{pattern}' in {display_path} ({files} files searched)"
            )),
            Ok((lines, _)) => {
                let capped = lines.len() >= max;
                let mut out = lines.join("\n");
                if capped {
                    out.push_str(&format!("\n\n[Results capped at {max} matches]"));
                }
                ok(out)
            }
            Err(_) => err("grep: search task failed".to_string()),
        }
    }
}

/// Returns (formatted match+context lines, files searched). Stops once `max` matches
/// are collected.
fn search(root: &std::path::Path, re: &regex::Regex, max: usize, context: usize, base: &std::path::Path) -> (Vec<String>, usize) {
    let mut out: Vec<String> = Vec::new();
    let mut match_count = 0usize;
    let mut files_searched = 0usize;

    let walk = WalkBuilder::new(root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .filter_entry(|e| {
            // Drop our extra skip-dirs (gitignore already covers most).
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                if let Some(name) = e.file_name().to_str() {
                    return !is_skip_dir(name);
                }
            }
            true
        })
        .build();

    let mut first_chunk = true;
    for entry in walk.flatten() {
        if match_count >= max {
            break;
        }
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().map(|x| x.eq_ignore_ascii_case("log")).unwrap_or(false) {
            continue;
        }
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue, // binary / unreadable
        };
        files_searched += 1;
        let lines: Vec<&str> = content.lines().collect();
        let rel = path.strip_prefix(base).unwrap_or(path).display().to_string();

        // Which line indices match.
        let matches: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| re.is_match(l)).map(|(i, _)| i).collect();
        if matches.is_empty() {
            continue;
        }
        // Build the union of display windows.
        let mut display: Vec<usize> = Vec::new();
        for &m in &matches {
            let lo = m.saturating_sub(context);
            let hi = (m + context).min(lines.len().saturating_sub(1));
            for i in lo..=hi {
                if display.last() != Some(&i) {
                    display.push(i);
                }
            }
        }
        display.sort_unstable();
        display.dedup();

        let match_set: std::collections::HashSet<usize> = matches.iter().copied().collect();
        let mut prev: Option<usize> = None;
        for &i in &display {
            if match_count >= max {
                break;
            }
            if let Some(p) = prev {
                if i > p + 1 && !first_chunk {
                    out.push("--".to_string());
                }
            }
            let sep = if match_set.contains(&i) { ":" } else { "-" };
            let mut line = lines[i].to_string();
            if line.chars().count() > MAX_DISPLAY_LINE {
                line = line.chars().take(MAX_DISPLAY_LINE).collect::<String>() + "…";
            }
            out.push(format!("{rel}{sep}{}{sep}{line}", i + 1));
            if match_set.contains(&i) {
                match_count += 1;
            }
            prev = Some(i);
            first_chunk = false;
        }
    }
    (out, files_searched)
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
    async fn finds_matches_with_line_numbers() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn main() {\n    let TODO = 1;\n    other();\n}\n").unwrap();
        let r = GrepTool.execute(r#"{"pattern":"TODO","path":"."}"#, &ctx(d.path())).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("a.rs:2:"), "{}", r.content);
    }

    // Issue #722 parity (v2): weak models send max_results/context as a string ("50")
    // or float (50.0 / "3.0") instead of an integer; the args must still deserialize.
    #[test]
    fn args_accept_lenient_numeric_max_results_and_context() {
        let a: Args =
            serde_json::from_str(r#"{"pattern":"x","max_results":"50","context":3.0}"#)
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
        let r = GrepTool.execute(r#"{"pattern":"hello"}"#, &ctx(d.path())).await;
        assert!(r.content.contains("a.txt:1:"), "{}", r.content);
    }

    #[tokio::test]
    async fn zero_matches_is_success() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "nothing here\n").unwrap();
        let r = GrepTool.execute(r#"{"pattern":"absent_xyz"}"#, &ctx(d.path())).await;
        assert!(!r.is_error, "zero matches must be a success: {}", r.content);
        assert!(r.content.contains("No matches found"), "{}", r.content);
    }

    #[tokio::test]
    async fn invalid_regex_falls_back_to_literal() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "value = foo(bar)\n").unwrap();
        // "foo(bar" is an invalid regex (unbalanced paren) → literal fallback finds it.
        let r = GrepTool.execute(r#"{"pattern":"foo(bar"}"#, &ctx(d.path())).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("a.txt:1:"), "{}", r.content);
    }

    #[tokio::test]
    async fn skips_gitignored_and_build_dirs() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("target")).unwrap();
        std::fs::write(d.path().join("target/junk.rs"), "NEEDLE\n").unwrap();
        std::fs::write(d.path().join("keep.rs"), "NEEDLE\n").unwrap();
        let r = GrepTool.execute(r#"{"pattern":"NEEDLE"}"#, &ctx(d.path())).await;
        assert!(r.content.contains("keep.rs:1:"), "{}", r.content);
        assert!(!r.content.contains("junk.rs"), "target/ should be skipped: {}", r.content);
    }
}
