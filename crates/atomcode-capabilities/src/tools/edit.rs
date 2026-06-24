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
        if a.old_string.is_empty() {
            return err(
                "edit_file: old_string is empty — provide the exact text fragment to replace."
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
        // may be CRLF. Match literally first; on a literal hit the model's strings already
        // agree with the file's bytes, so old/new are used VERBATIM. Only if the literal
        // match fails do we coerce BOTH old_string and new_string to the file's EOL — that
        // rescues an LF-copied edit of a CRLF file without injecting mixed endings, and
        // (unlike coercing unconditionally) leaves verbatim edits of LF files untouched.
        let literal = content.matches(&a.old_string).count();
        let (old_match, new_match, count) = if literal > 0 {
            (a.old_string.clone(), a.new_string.clone(), literal)
        } else {
            let file_eol = if content.contains("\r\n") { "\r\n" } else { "\n" };
            let old_c = coerce_eol(&a.old_string, file_eol);
            let c = content.matches(&old_c).count();
            (old_c, coerce_eol(&a.new_string, file_eol), c)
        };
        if count == 0 {
            // Last-resort whitespace-normalized fuzzy fallback (ported from the v1
            // editor). The common failure it rescues: the model reproduced the snippet
            // with the wrong INDENTATION whitespace (e.g. spaces where the file uses
            // tabs — read_file passes the real tabs, but the model emits spaces), so the
            // exact / EOL-coerced match can't find it and the model resorts to a shell
            // script. `try_fuzzy_replace` matches line-by-line ignoring leading/trailing
            // whitespace, then re-anchors the replacement to the file's REAL indent. It
            // is guarded (≥10 chars of trimmed content, and unique unless replace_all)
            // so it can't fire on short/ambiguous fragments.
            if let Some((fuzzy_result, fuzzy_count)) =
                try_fuzzy_replace(&content, &a.old_string, &a.new_string, a.replace_all)
            {
                if fuzzy_result == content {
                    return err(
                        "edit_file: the fuzzy (whitespace-normalized) match produced no \
                         change — old_string and new_string differ only in whitespace."
                            .to_string(),
                    );
                }
                if let Err(e) = tokio::fs::write(&path, &fuzzy_result).await {
                    return err(format!("edit_file: failed to write {}: {e}", path.display()));
                }
                let diff = build_compact_diff(&a.old_string, &a.new_string);
                return ok(format!(
                    "Edited {} (fuzzy whitespace match, {fuzzy_count} replacement{})\n{}",
                    path.display(),
                    if fuzzy_count == 1 { "" } else { "s" },
                    diff,
                ));
            }
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
        if old_match == new_match {
            // Originals differed (the early guard passed) but EOL-coercion collapsed them
            // to the same bytes → the replacement would be a silent no-op.
            return err(
                "edit_file: old_string and new_string are identical after line-ending \
                 normalization — nothing to change."
                    .to_string(),
            );
        }

        let updated = if a.replace_all {
            content.replace(&old_match, &new_match)
        } else {
            content.replacen(&old_match, &new_match, 1)
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

/// Whitespace-normalized fuzzy replace (faithful port of the v1 editor's
/// `try_fuzzy_replace`). Matches `old_string` against `content` line-by-line with each
/// line `.trim()`-ed, so a model that reproduced indentation with the wrong whitespace
/// (spaces vs the file's tabs, or a slightly-off indent) still matches. The replacement
/// is re-anchored to the file's REAL indentation: the first non-empty line of
/// `new_string` is the anchor, and each line's signed offset from it is re-applied on
/// top of the matched file line's actual leading whitespace (tabs preserved).
///
/// Returns `None` (caller falls back to the normal "not found" error) when: the old
/// string is empty, its trimmed content totals < 10 chars (too short to match safely),
/// no window matches, or `!replace_all` but more than one window matches (ambiguous).
fn try_fuzzy_replace(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Option<(String, usize)> {
    let old_normalized: Vec<&str> = old_string.lines().map(|l| l.trim()).collect();
    if old_normalized.is_empty() || old_normalized.iter().all(|l| l.is_empty()) {
        return None;
    }

    let content_lines: Vec<&str> = content.lines().collect();
    let has_trailing_newline = content.ends_with('\n');
    let mut matches: Vec<(usize, usize)> = Vec::new();

    // Only attempt a fuzzy match if old_string has substantial content (guards against
    // a short fragment matching the wrong place after trimming).
    let total_non_ws: usize = old_normalized.iter().map(|l| l.len()).sum();
    if total_non_ws < 10 {
        return None;
    }

    // Slide a window over content lines (trimmed), skipping past each match.
    let mut i = 0;
    while i + old_normalized.len() <= content_lines.len() {
        let window: Vec<&str> = content_lines[i..i + old_normalized.len()]
            .iter()
            .map(|l| l.trim())
            .collect();
        if window == old_normalized {
            matches.push((i, i + old_normalized.len()));
            i += old_normalized.len();
        } else {
            i += 1;
        }
    }

    if matches.is_empty() {
        return None;
    }
    // For a unique edit, require exactly one match; else the caller's "not found" path
    // is safer than guessing which occurrence the model meant.
    if !replace_all && matches.len() > 1 {
        return None;
    }

    // Anchor indent = the first non-empty line of new_string. Relative offsets (deeper
    // OR outdented) are preserved as signed deltas from the file's actual indent. (Using
    // the first non-empty line rather than the min indent avoids the indent-drift bug
    // that an outdented closing `}` would otherwise cause.)
    let new_lines: Vec<&str> = new_string.lines().collect();
    let new_base_indent = new_lines
        .iter()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .unwrap_or(0);

    let mut result_lines: Vec<String> = content_lines.iter().map(|l| l.to_string()).collect();

    let to_replace = if replace_all { &matches[..] } else { &matches[..1] };
    for &(start, end) in to_replace.iter().rev() {
        let original_line = content_lines[start];
        let file_indent = original_line.len() - original_line.trim_start().len();
        let file_indent_str: String = original_line.chars().take(file_indent).collect();

        let replacement: Vec<String> = new_lines
            .iter()
            .map(|l| {
                if l.trim().is_empty() {
                    String::new()
                } else {
                    let line_indent = l.len() - l.trim_start().len();
                    let signed_relative = line_indent as isize - new_base_indent as isize;
                    let total_indent = if signed_relative >= 0 {
                        // Same/deeper than anchor: keep the file's indent prefix
                        // (preserves the tab/space mix) and extend with plain spaces.
                        format!("{}{}", file_indent_str, " ".repeat(signed_relative as usize))
                    } else {
                        // Outdented from anchor: drop chars from the tail of the file's
                        // indent prefix.
                        let drop = (-signed_relative) as usize;
                        let keep = file_indent.saturating_sub(drop);
                        file_indent_str.chars().take(keep).collect()
                    };
                    format!("{}{}", total_indent, l.trim())
                }
            })
            .collect();

        result_lines.splice(start..end, replacement);
    }

    let mut result = result_lines.join("\n");
    if has_trailing_newline && !result.ends_with('\n') {
        result.push('\n');
    }
    // `lines()` stripped every `\r`, so a CRLF file would otherwise be rewritten to LF
    // across the WHOLE file (incl. untouched lines) — a silent whole-file EOL downgrade.
    // Restore the file's convention, mirroring the exact-match path's EOL preservation.
    if content.contains("\r\n") {
        result = coerce_eol(&result, "\r\n");
    }
    let count = if replace_all { matches.len() } else { 1 };
    Some((result, count))
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

    // A literal match must write new_string VERBATIM — never coerce its line endings.
    // Here a mostly-LF file has one stray CRLF line; editing an LF region must NOT force
    // the replacement to CRLF (that would inject mixed endings, the opposite of intent).
    #[tokio::test]
    async fn literal_match_writes_new_verbatim_no_crlf_injection() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("m.txt"), "head\r\nalpha\nbeta\n").unwrap();
        let r = EditFileTool
            .execute(
                r#"{"file_path":"m.txt","old_string":"alpha\nbeta","new_string":"alpha\nBETA"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        // The edited LF region stays LF; the unrelated CRLF line is untouched.
        assert_eq!(std::fs::read_to_string(d.path().join("m.txt")).unwrap(), "head\r\nalpha\nBETA\n");
    }

    // old_string and new_string that differ ONLY by line-ending form collapse to the
    // same bytes after normalization → a no-op; it must be refused, not reported as a
    // successful edit.
    #[tokio::test]
    async fn eol_only_difference_is_rejected_as_noop() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("c.txt"), "a\r\nb\r\n").unwrap();
        let r = EditFileTool
            .execute(
                r#"{"file_path":"c.txt","old_string":"a\nb","new_string":"a\r\nb"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(r.is_error, "a no-op edit must be refused: {}", r.content);
        assert_eq!(std::fs::read_to_string(d.path().join("c.txt")).unwrap(), "a\r\nb\r\n", "unchanged");
    }

    #[tokio::test]
    async fn empty_old_string_is_rejected() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "abc").unwrap();
        let r = EditFileTool
            .execute(r#"{"file_path":"a.txt","old_string":"","new_string":"X","replace_all":true}"#, &ctx(d.path()))
            .await;
        assert!(r.is_error, "empty old_string must be refused (would insert everywhere): {}", r.content);
        assert_eq!(std::fs::read_to_string(d.path().join("a.txt")).unwrap(), "abc", "unchanged");
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

    // The reported "改不动只能写脚本" case: the file is TAB-indented but the model
    // reproduced the body with SPACE indentation (read_file faithfully passes the tabs;
    // the model dropped them). Exact + EOL match both fail. The whitespace-normalized
    // fuzzy fallback must match line-by-line ignoring leading whitespace, and write back
    // using the file's REAL indentation (tabs preserved).
    #[tokio::test]
    async fn fuzzy_matches_tab_vs_space_indentation_and_preserves_tabs() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("f.rs"), "fn f() {\n\tlet x = 1;\n\tlet y = 2;\n}\n").unwrap();
        let r = EditFileTool
            .execute(
                r#"{"file_path":"f.rs","old_string":"    let x = 1;\n    let y = 2;","new_string":"    let x = 9;\n    let y = 2;"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(!r.is_error, "fuzzy whitespace match must succeed: {}", r.content);
        assert!(r.content.contains("fuzzy"), "should report a fuzzy match: {}", r.content);
        assert_eq!(
            std::fs::read_to_string(d.path().join("f.rs")).unwrap(),
            "fn f() {\n\tlet x = 9;\n\tlet y = 2;\n}\n",
            "the file's tab indentation must be preserved"
        );
    }

    // A fuzzy edit on a CRLF file must NOT rewrite the whole file to LF: `lines()`
    // strips every `\r`, so without restoring the file's EOL the entire file (incl.
    // untouched lines) would be silently downgraded to LF — a whole-file corruption.
    #[tokio::test]
    async fn fuzzy_match_preserves_crlf_line_endings() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("f.rs"), "fn f() {\r\n\tlet x = 1;\r\n\tlet y = 2;\r\n}\r\n").unwrap();
        // Model copied LF text (read_file strips \r) with SPACE indentation.
        let r = EditFileTool
            .execute(
                r#"{"file_path":"f.rs","old_string":"    let x = 1;\n    let y = 2;","new_string":"    let x = 9;\n    let y = 2;"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(
            std::fs::read_to_string(d.path().join("f.rs")).unwrap(),
            "fn f() {\r\n\tlet x = 9;\r\n\tlet y = 2;\r\n}\r\n",
            "CRLF must be preserved across the WHOLE file, not just the edited region"
        );
    }

    // Safety guard: a tiny fragment must NOT fuzzy-match (too ambiguous to be safe).
    #[tokio::test]
    async fn fuzzy_does_not_fire_for_short_fragments() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "\tx\n").unwrap();
        let r = EditFileTool
            .execute(r#"{"file_path":"a.txt","old_string":"  x","new_string":"  y"}"#, &ctx(d.path()))
            .await;
        assert!(r.is_error, "a short fragment must not fuzzy-match: {}", r.content);
        assert_eq!(std::fs::read_to_string(d.path().join("a.txt")).unwrap(), "\tx\n", "unchanged");
    }
}
