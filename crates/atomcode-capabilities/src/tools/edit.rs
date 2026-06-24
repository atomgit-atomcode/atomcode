//! `edit_file` — replace an exact, UNIQUE text fragment in a file (or all of them
//! with `replace_all`). Mutates the filesystem ⇒ always `Risky`. This is the
//! production editor's neutral TEXT mode only — the line-number / edits-array /
//! symbol modes and the auto-fix / file_store / LSP enrichments are dropped (they
//! need the heavy coding context).

use super::{coerce_eol, err, ok, resolve_path};
use async_trait::async_trait;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;

pub struct EditFileTool;

#[derive(Deserialize)]
struct Args {
    file_path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }
    fn description(&self) -> &str {
        "Replace an exact text fragment in a file. `old_string` must match EXACTLY \
         (including whitespace and indentation) and, unless `replace_all` is true, must \
         be UNIQUE in the file — include enough surrounding context to make it unique. \
         On no-match or an ambiguous match the file is left UNCHANGED. Relative paths \
         resolve against the working directory."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "Path to edit (absolute, or relative to the working directory)" },
                "old_string": { "type": "string", "description": "Exact text to find. Must be unique unless replace_all is true." },
                "new_string": { "type": "string", "description": "Replacement text." },
                "replace_all": { "type": "boolean", "description": "Replace ALL occurrences (default false = require a unique match)." }
            },
            "required": ["file_path", "old_string", "new_string"]
        })
    }
    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Risky // mutates an existing file
    }
    fn always_grant_scope(&self, _args: &str) -> String {
        // Tool-wide: "总是 / Always" approves every edit this session (v1 parity),
        // not just this one exact file/old/new triple.
        String::new()
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return err(format!(
                    "edit_file: invalid arguments: {e}. Expected \
                     {{\"file_path\",\"old_string\",\"new_string\"}}."
                ))
            }
        };
        if a.old_string == a.new_string {
            return err(
                "edit_file: old_string and new_string are identical — nothing to change."
                    .to_string(),
            );
        }
        let path = resolve_path(&a.file_path, &ctx.working_dir);
        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) => return err(format!("edit_file: cannot read {}: {e}", path.display())),
        };

        // Line-ending tolerance: read_file shows the model LF-normalized text (it does
        // `str::lines()`, which strips the `\r` from every `\r\n`), but the file on disk
        // may be CRLF. Match literally first; if that fails, retry with the old_string's
        // line endings coerced to the file's convention — otherwise a model that copied
        // LF text could never match a CRLF file (and would flail forever). The new text
        // always adopts the file's EOL so the edit never introduces mixed endings.
        let file_eol = if content.contains("\r\n") { "\r\n" } else { "\n" };
        let new_string = coerce_eol(&a.new_string, file_eol);
        let literal = content.matches(&a.old_string).count();
        let (old_match, count) = if literal > 0 {
            (a.old_string.clone(), literal)
        } else {
            let coerced = coerce_eol(&a.old_string, file_eol);
            let c = content.matches(&coerced).count();
            (coerced, c)
        };
        if count == 0 {
            return err(format!(
                "edit_file: old_string not found in {}. The file was NOT modified. Re-read \
                 the file and copy the exact text (including whitespace).",
                path.display()
            ));
        }
        if count > 1 && !a.replace_all {
            return err(format!(
                "edit_file: old_string appears {count} times in {} — it must be unique. Add \
                 surrounding context to disambiguate, or set replace_all=true. The file was \
                 NOT modified.",
                path.display()
            ));
        }

        let updated = if a.replace_all {
            content.replace(&old_match, &new_string)
        } else {
            content.replacen(&old_match, &new_string, 1)
        };
        if let Err(e) = tokio::fs::write(&path, &updated).await {
            return err(format!(
                "edit_file: failed to write {}: {e}",
                path.display()
            ));
        }
        let replaced = if a.replace_all { count } else { 1 };
        let diff = build_compact_diff(&a.old_string, &a.new_string);
        ok(format!(
            "Edited {} ({replaced} replacement{})\n{}",
            path.display(),
            if replaced == 1 { "" } else { "s" },
            diff,
        ))
    }
}

fn build_compact_diff(old: &str, new: &str) -> String {
    let mut diff = String::new();
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let max_show = 4;

    for (i, line) in old_lines.iter().take(max_show).enumerate() {
        diff.push_str(&format!("- {}\n", line));
        if i == max_show - 1 && old_lines.len() > max_show {
            diff.push_str(&format!(
                "  ... ({} more removed)\n",
                old_lines.len() - max_show
            ));
        }
    }

    for (i, line) in new_lines.iter().take(max_show).enumerate() {
        diff.push_str(&format!("+ {}\n", line));
        if i == max_show - 1 && new_lines.len() > max_show {
            diff.push_str(&format!(
                "  ... ({} more added)\n",
                new_lines.len() - max_show
            ));
        }
    }

    diff.trim_end().to_string()
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
        }
    }

    #[tokio::test]
    async fn unique_replace_succeeds() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn main() {\n    let x = 1;\n}\n").unwrap();
        let r = EditFileTool
            .execute(
                r#"{"file_path":"a.rs","old_string":"let x = 1;","new_string":"let x = 2;"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("- let x = 1;"), "{}", r.content);
        assert!(r.content.contains("+ let x = 2;"), "{}", r.content);
        let on_disk = std::fs::read_to_string(d.path().join("a.rs")).unwrap();
        assert!(on_disk.contains("let x = 2;"), "{on_disk}");
    }

    #[test]
    fn compact_diff_truncates_each_side() {
        let old = "1\n2\n3\n4\n5";
        let new = "a\nb\nc\nd\ne";
        let diff = build_compact_diff(old, new);
        assert!(diff.contains("- 1"), "{diff}");
        assert!(diff.contains("... (1 more removed)"), "{diff}");
        assert!(diff.contains("+ a"), "{diff}");
        assert!(diff.contains("... (1 more added)"), "{diff}");
    }

    #[tokio::test]
    async fn ambiguous_match_refuses() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "dup\ndup\n").unwrap();
        let r = EditFileTool
            .execute(
                r#"{"file_path":"a.txt","old_string":"dup","new_string":"x"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(r.is_error, "{}", r.content);
        assert!(r.content.contains("appears 2 times"), "{}", r.content);
        // file unchanged
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "dup\ndup\n"
        );
    }

    #[tokio::test]
    async fn replace_all_handles_duplicates() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "dup\ndup\ndup\n").unwrap();
        let r = EditFileTool
            .execute(
                r#"{"file_path":"a.txt","old_string":"dup","new_string":"x","replace_all":true}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("3 replacements"), "{}", r.content);
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "x\nx\nx\n"
        );
    }

    #[tokio::test]
    async fn missing_string_errors_and_keeps_file() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "hello\n").unwrap();
        let r = EditFileTool
            .execute(
                r#"{"file_path":"a.txt","old_string":"absent","new_string":"x"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(r.is_error, "{}", r.content);
        assert!(r.content.contains("not found"), "{}", r.content);
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "hello\n"
        );
    }

    #[tokio::test]
    async fn edit_is_risky() {
        assert_eq!(EditFileTool.risk("{}"), RiskLevel::Risky);
    }

    // A CRLF (Windows) file edited with a multi-line `old_string` whose line break is
    // `\n` — which is exactly what read_file shows the model, because read_file does
    // `text.lines()` and strips the `\r`. The edit must still succeed, and the file must
    // stay CRLF (no mixed line endings introduced).
    #[tokio::test]
    async fn crlf_file_matches_lf_oldstring_and_preserves_crlf() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("router.js"), "  path: '/help',\r\n  next: 1,\r\n").unwrap();
        let r = EditFileTool
            .execute(
                r#"{"file_path":"router.js","old_string":"  path: '/help',\n  next: 1,","new_string":"  path: '/proxyCase',\n  next: 1,"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(!r.is_error, "CRLF file must match an LF old_string: {}", r.content);
        let on_disk = std::fs::read_to_string(d.path().join("router.js")).unwrap();
        assert_eq!(on_disk, "  path: '/proxyCase',\r\n  next: 1,\r\n", "must stay CRLF: {on_disk:?}");
    }

    #[tokio::test]
    async fn lf_file_is_unaffected_by_eol_tolerance() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "let x = 1;\nlet y = 2;\n").unwrap();
        let r = EditFileTool
            .execute(
                r#"{"file_path":"a.rs","old_string":"let x = 1;\nlet y = 2;","new_string":"let x = 9;\nlet y = 2;"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(std::fs::read_to_string(d.path().join("a.rs")).unwrap(), "let x = 9;\nlet y = 2;\n");
    }
}
