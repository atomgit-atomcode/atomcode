//! `ast_grep` — structural (AST) code search via the native `ast-grep` engine. Matches a
//! single AST pattern (with `$NAME` / `$_` / `$$$` metavariables) against the syntax tree,
//! not raw text — so it precisely finds "this kind of call/declaration" where a regex
//! `grep` would over- or under-match. Read-only ⇒ `Safe`.
//!
//! Implementation: shells out to the `ast-grep` (a.k.a. `sg`) binary — real ast-grep
//! semantics, zero new Rust deps, and no tree-sitter version clash with the `codeintel`
//! grammars. If the binary isn't installed the tool returns a clear, actionable message
//! (same graceful-degradation contract as `open_file`). Consistent with `bash`/`open_file`
//! shelling out to host tools; OS isolation remains the embedder's responsibility.

use super::{err, ok, resolve_path};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::Path;

/// Candidate binary names, in order (`ast-grep` is canonical; `sg` is the short alias).
const BINARIES: &[&str] = &["ast-grep", "sg"];
/// Cap rendered matches so a broad pattern can't flood the context window.
const MAX_MATCHES: usize = 100;

pub struct AstGrepTool;

#[derive(Deserialize)]
struct Args {
    /// A single AST pattern (e.g. `unwrap()` → `$EXPR.unwrap()`).
    #[serde(alias = "pat")]
    pattern: String,
    /// Files / dirs / globs to search. Defaults to the working directory.
    #[serde(default)]
    paths: Vec<String>,
    /// Optional language override (else inferred from each file's extension).
    #[serde(default)]
    lang: Option<String>,
}

#[async_trait]
impl Tool for AstGrepTool {
    fn name(&self) -> &str {
        "ast_grep"
    }
    fn description(&self) -> &str {
        "Structural code search by AST pattern (native ast-grep) — finds syntax, not text. \
         Use when a regex would be imprecise: locating a specific call shape, declaration, \
         or construct. Metavariables: `$NAME` captures one node, `$_` matches one node \
         unbound, `$$$` matches zero or more nodes (uppercase names; each must be a whole \
         AST node). The pattern must parse as one valid node in the target language. \
         `paths` defaults to the working directory; pass `lang` to force a language."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Single AST pattern, e.g. \"$X.unwrap()\" or \"fn $NAME($$$) { $$$ }\"" },
                "paths": { "type": "array", "items": { "type": "string" }, "description": "Files/dirs/globs to search (default: working directory)" },
                "lang": { "type": "string", "description": "Force a language (e.g. rust, typescript); omit to infer from file extensions" }
            },
            "required": ["pattern"]
        })
    }
    // read-only structural search → Safe.
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => return err(format!("ast_grep: invalid arguments: {e}. Expected {{\"pattern\":\"<pat>\"}}.")),
        };
        if a.pattern.trim().is_empty() {
            return err("ast_grep: `pattern` must be non-empty.");
        }
        // Resolve paths against the working dir (default to it). ast-grep also runs WITH
        // cwd set, but resolving here keeps absolute paths explicit in the output.
        let paths: Vec<String> = if a.paths.iter().any(|p| !p.trim().is_empty()) {
            a.paths
                .iter()
                .filter(|p| !p.trim().is_empty())
                .map(|p| resolve_path(p, &ctx.working_dir).to_string_lossy().to_string())
                .collect()
        } else {
            vec![ctx.working_dir.to_string_lossy().to_string()]
        };

        let mut argv: Vec<String> =
            vec!["run".into(), "--pattern".into(), a.pattern.clone(), "--json=compact".into()];
        if let Some(lang) = a.lang.as_deref().filter(|s| !s.trim().is_empty()) {
            argv.push("--lang".into());
            argv.push(lang.to_string());
        }
        argv.extend(paths);

        let out = match run_ast_grep(&argv, &ctx.working_dir).await {
            Ok(o) => o,
            Err(e) => return err(e),
        };
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let detail = stderr.trim();
            // A pattern that fails to parse is the common case — surface ast-grep's reason.
            return err(format!(
                "ast_grep: search failed{}",
                if detail.is_empty() { String::new() } else { format!(": {detail}") }
            ));
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let matches = parse_ast_grep_json(&stdout);
        if matches.is_empty() {
            return ok(format!("No matches for pattern: {}", a.pattern));
        }
        ok(render_matches(&matches, MAX_MATCHES))
    }
}

struct AstMatch {
    file: String,
    line: u32, // 1-based
    text: String,
}

/// Parse `ast-grep --json=compact` output (a JSON array of match objects) into
/// `(file, line, first-line-of-text)`. ast-grep line numbers are 0-based → +1. Robust to
/// missing fields (skips degenerate entries) and to non-array / unparseable input (empty).
fn parse_ast_grep_json(stdout: &str) -> Vec<AstMatch> {
    let v: Value = match serde_json::from_str(stdout.trim()) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let Some(arr) = v.as_array() else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(arr.len());
    for m in arr {
        let file = m.get("file").and_then(|f| f.as_str()).unwrap_or("").to_string();
        if file.is_empty() {
            continue;
        }
        // range.start.line is 0-based.
        let line0 = m
            .get("range")
            .and_then(|r| r.get("start"))
            .and_then(|s| s.get("line"))
            .and_then(|l| l.as_u64())
            .unwrap_or(0);
        let text = m.get("text").and_then(|t| t.as_str()).unwrap_or("");
        let first = text.lines().next().unwrap_or("").trim_end().to_string();
        out.push(AstMatch { file, line: line0 as u32 + 1, text: first });
    }
    out
}

/// Render matches grouped by file (`file:line: text`), capping at `max` with a note.
fn render_matches(matches: &[AstMatch], max: usize) -> String {
    let shown = matches.len().min(max);
    let mut out = String::new();
    let mut last_file = "";
    for m in &matches[..shown] {
        if m.file != last_file {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&m.file);
            out.push('\n');
            last_file = &m.file;
        }
        out.push_str(&format!("  {}: {}\n", m.line, m.text));
    }
    if matches.len() > max {
        out.push_str(&format!("\n[showing {max} of {} matches; narrow the pattern or paths]", matches.len()));
    } else {
        out.push_str(&format!("\n[{} match(es)]", matches.len()));
    }
    out
}

/// Try each candidate binary in order; the first that is INSTALLED runs. A `NotFound`
/// spawn error falls through to the next; if none are found, return an install hint.
async fn run_ast_grep(argv: &[String], cwd: &Path) -> Result<std::process::Output, String> {
    let mut last_err = None;
    for bin in BINARIES {
        let mut cmd = tokio::process::Command::new(bin);
        cmd.args(argv).current_dir(cwd);
        // No console-window flash when spawned from a console-less daemon (Windows-only).
        crate::process_utils::suppress_console_window(&mut cmd);
        match cmd.output().await {
            Ok(o) => return Ok(o),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                last_err = Some(e);
                continue;
            }
            Err(e) => return Err(format!("ast_grep: failed to run `{bin}`: {e}")),
        }
    }
    let _ = last_err;
    Err(format!(
        "ast_grep: the `ast-grep` binary is not installed (tried {}). Install it (e.g. \
         `cargo install ast-grep`, `brew install ast-grep`, or `npm i -g @ast-grep/cli`), \
         or use `grep` for a text search.",
        BINARIES.join("`, `")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ast_grep_json_to_file_line_text() {
        // Shape of `ast-grep run --json=compact` entries (line is 0-based).
        let json = r#"[
            {"file":"src/a.rs","range":{"start":{"line":9,"column":4},"end":{"line":9,"column":20}},"text":"x.unwrap()"},
            {"file":"src/a.rs","range":{"start":{"line":41,"column":0},"end":{"line":43,"column":1}},"text":"y.unwrap()\n  .next()"},
            {"file":"src/b.rs","range":{"start":{"line":0,"column":0}},"text":"z.unwrap()"}
        ]"#;
        let m = parse_ast_grep_json(json);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].file, "src/a.rs");
        assert_eq!(m[0].line, 10, "0-based 9 → 1-based 10");
        assert_eq!(m[0].text, "x.unwrap()");
        assert_eq!(m[1].text, "y.unwrap()", "multi-line match shows first line only");
        assert_eq!(m[2].line, 1);
    }

    #[test]
    fn parse_handles_empty_and_garbage() {
        assert!(parse_ast_grep_json("[]").is_empty());
        assert!(parse_ast_grep_json("").is_empty());
        assert!(parse_ast_grep_json("not json").is_empty());
        // an object (not array) → empty; a match missing `file` is skipped.
        assert!(parse_ast_grep_json(r#"{"file":"x"}"#).is_empty());
        assert!(parse_ast_grep_json(r#"[{"range":{"start":{"line":1}},"text":"t"}]"#).is_empty());
    }

    #[test]
    fn renders_grouped_by_file() {
        let m = vec![
            AstMatch { file: "a.rs".into(), line: 10, text: "x.unwrap()".into() },
            AstMatch { file: "a.rs".into(), line: 20, text: "y.unwrap()".into() },
            AstMatch { file: "b.rs".into(), line: 1, text: "z.unwrap()".into() },
        ];
        let out = render_matches(&m, 100);
        assert!(out.contains("a.rs\n  10: x.unwrap()\n  20: y.unwrap()"), "{out}");
        assert!(out.contains("b.rs\n  1: z.unwrap()"), "{out}");
        assert!(out.contains("[3 match(es)]"), "{out}");
    }

    #[test]
    fn render_caps_and_notes_truncation() {
        let m: Vec<AstMatch> = (0..150)
            .map(|i| AstMatch { file: "a.rs".into(), line: i + 1, text: "t".into() })
            .collect();
        let out = render_matches(&m, 100);
        assert!(out.contains("showing 100 of 150 matches"), "{out}");
        assert!(out.contains("  100: t"), "{out}");
        assert!(!out.contains("  101: t"), "capped: {}", &out[out.len().saturating_sub(60)..]);
    }
}
