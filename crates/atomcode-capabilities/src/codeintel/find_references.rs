//! `find_references` — whole-word textual references to a symbol across the repo,
//! gitignore-aware. Non-destructive ⇒ always `Safe`. Production shells out to ripgrep;
//! the neutral port does a word-boundary scan with the `ignore` walker (no rg dependency).

use super::{err, ok, resolve_path};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::json;
use std::path::Path;

const MAX_REFS: usize = 200;
const MAX_LINE: usize = 300;

pub struct FindReferencesTool;

#[derive(Deserialize)]
struct Args {
    symbol: String,
    #[serde(default)]
    path: Option<String>,
}

#[async_trait]
impl Tool for FindReferencesTool {
    fn name(&self) -> &str {
        "find_references"
    }
    fn description(&self) -> &str {
        "Find all references to a symbol (function/class/variable) across the project as \
         whole-word textual matches, gitignore-aware. Returns file:line: content. Relative \
         paths resolve against the working directory."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "symbol": { "type": "string", "description": "Symbol name to find references for" },
                "path": { "type": "string", "description": "Directory to search (default: the working directory)" }
            },
            "required": ["symbol"]
        })
    }
    // read-only → risk() defaults to Safe.
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => return err(format!("find_references: invalid arguments: {e}. Expected {{\"symbol\":\"<name>\"}}.")),
        };
        if a.symbol.trim().is_empty() {
            return err("find_references: symbol must not be empty.".to_string());
        }
        let raw = a.path.clone().unwrap_or_else(|| ".".to_string());
        let dir = resolve_path(&raw, &ctx.working_dir);
        let symbol = a.symbol.clone();
        tokio::task::spawn_blocking(move || render(&dir, &raw, &symbol))
            .await
            .unwrap_or_else(|_| err("find_references: search task failed"))
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Does `line` contain `word` as a whole word (not part of a larger identifier)?
fn line_has_word(line: &str, word: &str) -> bool {
    let mut from = 0;
    while let Some(pos) = line[from..].find(word) {
        let start = from + pos;
        let end = start + word.len();
        let before_ok = start == 0 || !line[..start].chars().next_back().map(is_word_char).unwrap_or(false);
        let after_ok = end >= line.len() || !line[end..].chars().next().map(is_word_char).unwrap_or(false);
        if before_ok && after_ok {
            return true;
        }
        // Advance past the first char of this match — a CHAR-boundary step (a raw +1 byte
        // could land inside a multibyte char and panic on the next `line[from..]` slice).
        let step = line[start..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        from = start + step;
    }
    false
}

fn render(dir: &Path, display_dir: &str, symbol: &str) -> ToolResult {
    if std::fs::metadata(dir).is_err() {
        return err(format!("find_references: path not found: {}", dir.display()));
    }
    let mut refs: Vec<String> = Vec::new();
    let mut capped = false;
    'walk: for entry in WalkBuilder::new(dir).hidden(true).git_ignore(true).git_global(true).git_exclude(true).build().flatten() {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let content = match std::fs::read_to_string(p) {
            Ok(c) => c,
            Err(_) => continue, // binary / unreadable
        };
        let rel = p.strip_prefix(dir).unwrap_or(p).display().to_string();
        for (i, line) in content.lines().enumerate() {
            if line_has_word(line, symbol) {
                let mut text = line.trim().to_string();
                if text.chars().count() > MAX_LINE {
                    text = text.chars().take(MAX_LINE).collect::<String>() + "…";
                }
                refs.push(format!("  {}:{}: {}", rel, i + 1, text));
                if refs.len() >= MAX_REFS {
                    capped = true;
                    break 'walk;
                }
            }
        }
    }

    if refs.is_empty() {
        return ok(format!("No references to '{symbol}' found in {display_dir}."));
    }
    let mut out = format!("References for '{symbol}' in {display_dir}:\n\nUSAGES ({}):\n", refs.len());
    out.push_str(&refs.join("\n"));
    if capped {
        out.push_str(&format!("\n\n[Results capped at {MAX_REFS} matches]"));
    }
    ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::tool::ToolContext;
    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext { working_dir: dir.to_path_buf(), cancel: CancellationToken::new(), progress: atomcode_kernel::tool::ProgressSink::noop() }
    }

    #[test]
    fn whole_word_only() {
        assert!(line_has_word("    foo();", "foo"));
        assert!(line_has_word("let foo = 1", "foo"));
        assert!(!line_has_word("let foobar = 1", "foo"));
        assert!(!line_has_word("food()", "foo"));
        assert!(line_has_word("a.foo.b", "foo"));
    }

    #[test]
    fn word_match_is_utf8_safe() {
        // Regression: a raw byte+1 advance panicked when a multibyte char abutted a match.
        let _ = line_has_word("fnété", "é");
        let _ = line_has_word("café_bar é x", "é");
        let _ = line_has_word("aéaéaé", "x");
        assert!(line_has_word("foo 计算 bar", "计算"));
        assert!(!line_has_word("xé", "x")); // 'é' is a word char → 'x' not a whole word
    }

    #[tokio::test]
    async fn finds_references_across_files() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn helper() {}\n").unwrap();
        std::fs::write(d.path().join("b.rs"), "fn x() { helper(); }\nfn y() { helper(); }\n").unwrap();
        let r = FindReferencesTool.execute(r#"{"symbol":"helper"}"#, &ctx(d.path())).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("a.rs:1:"), "{}", r.content);
        assert!(r.content.contains("b.rs:1:") && r.content.contains("b.rs:2:"), "{}", r.content);
        assert!(r.content.contains("USAGES (3)"), "{}", r.content);
    }

    #[tokio::test]
    async fn no_references_is_success() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn x() {}\n").unwrap();
        let r = FindReferencesTool.execute(r#"{"symbol":"absent_sym"}"#, &ctx(d.path())).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("No references"), "{}", r.content);
    }
}
