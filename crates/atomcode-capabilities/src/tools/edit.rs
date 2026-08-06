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
        let raw = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) => {
                return err(format!(
                    "edit_file: cannot read {}: {e}",
                    crate::pathnorm::to_display(&path)
                ))
            }
        };
        // Decode to UTF-8 for matching, remembering the on-disk encoding so the edit is
        // written back in the SAME encoding. A GBK/GB18030 file (Chinese Windows) is
        // edited in place; an ambiguous non-UTF-8 file is refused, not corrupted.
        let decoded = match crate::tools::encoding::decode_for_edit(&path, &raw) {
            Some(d) => d,
            None => {
                return err(format!(
                    "edit_file: cannot read {} as UTF-8 or a supported legacy text encoding \
                     (GBK/GB18030). Convert it to UTF-8 first. The file was NOT modified.",
                    crate::pathnorm::to_display(&path)
                ))
            }
        };
        let content = decoded.text;
        let file_encoding = decoded.encoding;

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
            let file_eol = if content.contains("\r\n") {
                "\r\n"
            } else {
                "\n"
            };
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
                if let Err(msg) = write_encoded(&path, &fuzzy_result, file_encoding).await {
                    return err(msg);
                }
                let diff = build_compact_diff(&content, &fuzzy_result);
                return ok(format!(
                    "Edited {} (fuzzy whitespace match, {fuzzy_count} replacement{})\n{}",
                    crate::pathnorm::to_display(&path),
                    if fuzzy_count == 1 { "" } else { "s" },
                    diff,
                ));
            }
            // Tier 2: block-anchor match (first+last line anchors, ONE interior line's drift
            // tolerated). Absorbs the "model got one interior line slightly wrong" case that
            // otherwise sends a weak model reaching for `sed`. Unique + at-most-one-drift guarded.
            if let Some((anchor_result, _)) =
                try_block_anchor_replace(&content, &a.old_string, &a.new_string)
            {
                if anchor_result != content {
                    if let Err(msg) = write_encoded(&path, &anchor_result, file_encoding).await {
                        return err(msg);
                    }
                    let diff = build_compact_diff(&content, &anchor_result);
                    return ok(format!(
                        "Edited {} (anchored block match, 1 replacement)\n{}",
                        crate::pathnorm::to_display(&path),
                        diff,
                    ));
                }
            }
            return err(format!(
                "edit_file: old_string not found in {}. The file was NOT modified. Re-read \
                 the file and copy the exact text (including whitespace).",
                crate::pathnorm::to_display(&path)
            ));
        }
        if count > 1 && !a.replace_all {
            return err(format!(
                "edit_file: old_string appears {count} times in {} — it must be unique. Add \
                 surrounding context to disambiguate, or set replace_all=true. The file was \
                 NOT modified.",
                crate::pathnorm::to_display(&path)
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
        if let Err(msg) = write_encoded(&path, &updated, file_encoding).await {
            return err(msg);
        }
        let replaced = if a.replace_all { count } else { 1 };
        let diff = build_compact_diff(&content, &updated);
        ok(format!(
            "Edited {} ({replaced} replacement{})\n{}",
            crate::pathnorm::to_display(&path),
            if replaced == 1 { "" } else { "s" },
            diff,
        ))
    }
}

/// Write edited text back to `path` in its original on-disk `encoding`. Refuses (Err
/// with a user-facing message) rather than write replacement bytes if the text cannot
/// be represented — so a failed re-encode leaves the file untouched, never corrupted.
async fn write_encoded(
    path: &std::path::Path,
    text: &str,
    encoding: crate::tools::encoding::FileEncoding,
) -> Result<(), String> {
    let bytes = crate::tools::encoding::encode(text, encoding).ok_or_else(|| {
        format!(
            "edit_file: cannot re-encode the edit to {}'s original encoding; the file was \
             NOT modified. Convert it to UTF-8 first.",
            crate::pathnorm::to_display(path)
        )
    })?;
    tokio::fs::write(path, bytes).await.map_err(|e| {
        format!(
            "edit_file: failed to write {}: {e}",
            crate::pathnorm::to_display(path)
        )
    })
}

/// A compact GIT UNIFIED DIFF (`@@` hunks, 3 lines of context) between the OLD
/// and NEW whole-file contents, capped so a large edit can't flood the model
/// context / transcript. The TUI re-parses this into a line-numbered, color-
/// coded diff block; the model reads it as a normal unified diff.
fn build_compact_diff(old_file: &str, new_file: &str) -> String {
    const MAX_DIFF_LINES: usize = 60;
    // Bound the Myers diff with a deadline: this runs synchronously on the async
    // executor thread over the WHOLE file, and two large, mostly-different files
    // (e.g. replacing a minified blob) can otherwise spin for a long time and
    // stall the event loop. On timeout `similar` returns a coarser-but-valid
    // diff instead of hanging (same guard codex uses).
    let mut config = similar::TextDiff::configure();
    config.timeout(std::time::Duration::from_millis(200));
    let full = config
        .diff_lines(old_file, new_file)
        .unified_diff()
        .context_radius(3)
        .to_string();
    let full = full.trim_end();
    let lines: Vec<&str> = full.lines().collect();
    if lines.len() <= MAX_DIFF_LINES {
        return full.to_string();
    }
    let mut out = lines[..MAX_DIFF_LINES].join("\n");
    out.push_str(&format!(
        "\n… ({} more diff lines)",
        lines.len() - MAX_DIFF_LINES
    ));
    out
}

/// Number of leading whitespace **characters** in `s`. Counts Unicode
/// whitespace consistently with `chars().take(n)` — both operate on
/// characters, not bytes. This is the correct unit for indent arithmetic:
/// `" ".repeat(n)` and `chars().take(n)` both count characters.
fn leading_ws_chars(s: &str) -> usize {
    s.chars().take_while(|c| c.is_whitespace()).count()
}

/// Re-anchor `new_lines` to the file's REAL indentation at `original_line`: the first
/// non-empty new line is the anchor, and each line's SIGNED indent offset from it is
/// re-applied on top of the matched file line's actual leading whitespace (tabs
/// preserved, multi-byte whitespace counted by CHARACTER). Shared by both fuzzy tiers
/// ([`try_fuzzy_replace`] and [`try_block_anchor_replace`]).
fn reanchored_replacement(new_lines: &[&str], original_line: &str) -> Vec<String> {
    // Anchor indent = the first non-empty line of new_string. Using the first non-empty
    // line (NOT the min indent) avoids the indent-drift an outdented closing `}` causes.
    let new_base_indent = new_lines
        .iter()
        .find(|l| !l.trim().is_empty())
        .map(|l| leading_ws_chars(l))
        .unwrap_or(0);
    let file_indent = leading_ws_chars(original_line);
    let file_indent_str: String = original_line.chars().take(file_indent).collect();
    new_lines
        .iter()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                let line_indent = leading_ws_chars(l);
                let signed_relative = line_indent as isize - new_base_indent as isize;
                let total_indent = if signed_relative >= 0 {
                    // Same/deeper than anchor: keep the file's indent prefix (preserves the
                    // tab/space mix) and extend with plain spaces.
                    format!(
                        "{}{}",
                        file_indent_str,
                        " ".repeat(signed_relative as usize)
                    )
                } else {
                    // Outdented from anchor: drop chars from the tail of the file's indent.
                    let drop = (-signed_relative) as usize;
                    let keep = file_indent.saturating_sub(drop);
                    file_indent_str.chars().take(keep).collect()
                };
                format!("{}{}", total_indent, l.trim())
            }
        })
        .collect()
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

    // Re-anchor the replacement to each match's REAL indentation (see `reanchored_replacement`).
    let new_lines: Vec<&str> = new_string.lines().collect();
    let mut result_lines: Vec<String> = content_lines.iter().map(|l| l.to_string()).collect();
    let to_replace = if replace_all {
        &matches[..]
    } else {
        &matches[..1]
    };
    for &(start, end) in to_replace.iter().rev() {
        let replacement = reanchored_replacement(&new_lines, content_lines[start]);
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

/// BLOCK-ANCHOR fuzzy replace — the tier below [`try_fuzzy_replace`]. When the model
/// reproduced a multi-line block but got an INTERIOR line slightly wrong (a typo, a
/// reordered token, a comment tweak), the whitespace-normalized tier — which requires
/// EVERY trimmed line to match — fails, and a weak model then resorts to a shell script.
/// This tier anchors on the FIRST and LAST trimmed lines and tolerates interior drift,
/// replacing the whole window (re-anchored to the file's real indent via
/// [`reanchored_replacement`]).
///
/// Conservative guards so it can't clobber the wrong block: needs ≥ 3 lines; both
/// anchors non-empty and ≥ 3 trimmed chars (so a bare `{`/`}` can't anchor); the window
/// length equals the old block's; ALL BUT AT MOST ONE line still matches trimmed (so a
/// window that merely shares its first/last line with an unrelated region is rejected —
/// a plain "≥ half" rule would degenerate to "anchors only" for n ≤ 4); and the anchored
/// window must be UNIQUE (no `replace_all` at this tier — guessing which of several to
/// rewrite is unsafe). Returns `None` on any miss so the caller falls back to not-found.
fn try_block_anchor_replace(
    content: &str,
    old_string: &str,
    new_string: &str,
) -> Option<(String, usize)> {
    let old_lines: Vec<&str> = old_string.lines().collect();
    let n = old_lines.len();
    if n < 3 {
        return None; // need first + last anchors AND ≥ 1 interior line
    }
    let first = old_lines[0].trim();
    let last = old_lines[n - 1].trim();
    if first.chars().count() < 3 || last.chars().count() < 3 {
        return None; // anchors too short to identify a block safely
    }

    let content_lines: Vec<&str> = content.lines().collect();
    let has_trailing_newline = content.ends_with('\n');
    let mut matches: Vec<usize> = Vec::new();
    let mut i = 0;
    while i + n <= content_lines.len() {
        if content_lines[i].trim() == first && content_lines[i + n - 1].trim() == last {
            // Require ALL BUT AT MOST ONE line to still match (trimmed) at its position.
            // This matches the intent — the model got a SINGLE interior line slightly wrong —
            // and (unlike a "≥ half" rule, which for n≤4 degenerates to "anchors only" and
            // would ignore the interior) rejects a window that merely shares its first/last
            // line with an unrelated region.
            let matched = (0..n)
                .filter(|&k| content_lines[i + k].trim() == old_lines[k].trim())
                .count();
            if matched + 1 >= n {
                matches.push(i);
            }
        }
        i += 1;
    }
    if matches.len() != 1 {
        return None; // no match, or ambiguous → let the caller error out
    }

    let start = matches[0];
    let new_lines: Vec<&str> = new_string.lines().collect();
    let replacement = reanchored_replacement(&new_lines, content_lines[start]);
    let mut result_lines: Vec<String> = content_lines.iter().map(|l| l.to_string()).collect();
    result_lines.splice(start..start + n, replacement);

    let mut result = result_lines.join("\n");
    if has_trailing_newline && !result.ends_with('\n') {
        result.push('\n');
    }
    if content.contains("\r\n") {
        result = coerce_eol(&result, "\r\n");
    }
    Some((result, 1))
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
    async fn gbk_file_edits_in_place_and_stays_gbk() {
        // A GBK/GB18030-encoded file (common on Chinese Windows) must be editable
        // directly — matched in UTF-8 space, then written back in its ORIGINAL encoding,
        // never silently converted to UTF-8.
        let d = tempfile::tempdir().unwrap();
        let (gbk, _, had_err) = encoding_rs::GB18030.encode("第一行\n第二行\n第三行\n");
        assert!(!had_err);
        std::fs::write(d.path().join("notes.txt"), &gbk[..]).unwrap();

        let r = EditFileTool
            .execute(
                r#"{"file_path":"notes.txt","old_string":"第二行","new_string":"改过的第二行"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);

        let on_disk = std::fs::read(d.path().join("notes.txt")).unwrap();
        // Still GBK: the Chinese bytes are not valid UTF-8, and decode as GB18030.
        assert!(
            std::str::from_utf8(&on_disk).is_err(),
            "file must stay GBK, not be converted to UTF-8"
        );
        let (decoded, _, had_err) = encoding_rs::GB18030.decode(&on_disk);
        assert!(!had_err);
        assert_eq!(decoded, "第一行\n改过的第二行\n第三行\n");
    }

    #[tokio::test]
    async fn ambiguous_non_utf8_file_is_refused_and_left_untouched() {
        // A non-UTF-8 file that does not losslessly round-trip as GB18030 (here a stray
        // 0x80 byte) must be refused rather than corrupted — the file stays byte-identical.
        let d = tempfile::tempdir().unwrap();
        let mut bytes = b"plain text\n".to_vec();
        bytes.push(0x80);
        bytes.extend_from_slice(b"\n");
        std::fs::write(d.path().join("weird.txt"), &bytes).unwrap();

        let r = EditFileTool
            .execute(
                r#"{"file_path":"weird.txt","old_string":"plain","new_string":"changed"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(r.is_error, "{}", r.content);
        assert!(r.content.contains("UTF-8"), "{}", r.content);
        assert_eq!(
            std::fs::read(d.path().join("weird.txt")).unwrap(),
            bytes,
            "refused edit must leave the file byte-identical"
        );
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
        assert!(r.content.contains("-    let x = 1;"), "{}", r.content);
        assert!(r.content.contains("+    let x = 2;"), "{}", r.content);
        let on_disk = std::fs::read_to_string(d.path().join("a.rs")).unwrap();
        assert!(on_disk.contains("let x = 2;"), "{on_disk}");
    }

    #[test]
    fn compact_diff_is_unified_with_line_numbers() {
        // Whole-file old vs new; a real diff must produce a `@@` hunk header whose
        // new-side start reflects the changed line's position in the file.
        let old = "fn main() {\n    let x = 1;\n}\n";
        let new = "fn main() {\n    let x = 2;\n}\n";
        let diff = build_compact_diff(old, new);
        assert!(
            diff.contains("@@"),
            "must be a unified diff with a hunk header: {diff}"
        );
        assert!(
            diff.contains("-    let x = 1;"),
            "removed line present: {diff}"
        );
        assert!(
            diff.contains("+    let x = 2;"),
            "added line present: {diff}"
        );
        // The change is on file line 2, which falls within lines 1-3 shown in the hunk header.
        assert!(
            diff.contains("@@ -1,3 +1,3 @@"),
            "hunk header shows lines 1-3: {diff}"
        );
    }

    #[test]
    fn compact_diff_caps_huge_diffs() {
        let old = String::new();
        let new: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let diff = build_compact_diff(&old, &new);
        assert!(
            diff.lines().count() <= 61,
            "capped: {} lines",
            diff.lines().count()
        );
        assert!(
            diff.contains("more diff lines"),
            "shows a truncation note: {diff}"
        );
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
        std::fs::write(
            d.path().join("router.js"),
            "  path: '/help',\r\n  next: 1,\r\n",
        )
        .unwrap();
        let r = EditFileTool
            .execute(
                r#"{"file_path":"router.js","old_string":"  path: '/help',\n  next: 1,","new_string":"  path: '/proxyCase',\n  next: 1,"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(
            !r.is_error,
            "CRLF file must match an LF old_string: {}",
            r.content
        );
        let on_disk = std::fs::read_to_string(d.path().join("router.js")).unwrap();
        assert_eq!(
            on_disk, "  path: '/proxyCase',\r\n  next: 1,\r\n",
            "must stay CRLF: {on_disk:?}"
        );
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
        assert_eq!(
            std::fs::read_to_string(d.path().join("m.txt")).unwrap(),
            "head\r\nalpha\nBETA\n"
        );
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
        assert_eq!(
            std::fs::read_to_string(d.path().join("c.txt")).unwrap(),
            "a\r\nb\r\n",
            "unchanged"
        );
    }

    #[tokio::test]
    async fn empty_old_string_is_rejected() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "abc").unwrap();
        let r = EditFileTool
            .execute(
                r#"{"file_path":"a.txt","old_string":"","new_string":"X","replace_all":true}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(
            r.is_error,
            "empty old_string must be refused (would insert everywhere): {}",
            r.content
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "abc",
            "unchanged"
        );
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
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.rs")).unwrap(),
            "let x = 9;\nlet y = 2;\n"
        );
    }

    // The reported "改不动只能写脚本" case: the file is TAB-indented but the model
    // reproduced the body with SPACE indentation (read_file faithfully passes the tabs;
    // the model dropped them). Exact + EOL match both fail. The whitespace-normalized
    // fuzzy fallback must match line-by-line ignoring leading whitespace, and write back
    // using the file's REAL indentation (tabs preserved).
    #[tokio::test]
    async fn fuzzy_matches_tab_vs_space_indentation_and_preserves_tabs() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("f.rs"),
            "fn f() {\n\tlet x = 1;\n\tlet y = 2;\n}\n",
        )
        .unwrap();
        let r = EditFileTool
            .execute(
                r#"{"file_path":"f.rs","old_string":"    let x = 1;\n    let y = 2;","new_string":"    let x = 9;\n    let y = 2;"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(
            !r.is_error,
            "fuzzy whitespace match must succeed: {}",
            r.content
        );
        assert!(
            r.content.contains("fuzzy"),
            "should report a fuzzy match: {}",
            r.content
        );
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
        std::fs::write(
            d.path().join("f.rs"),
            "fn f() {\r\n\tlet x = 1;\r\n\tlet y = 2;\r\n}\r\n",
        )
        .unwrap();
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
            .execute(
                r#"{"file_path":"a.txt","old_string":"  x","new_string":"  y"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(
            r.is_error,
            "a short fragment must not fuzzy-match: {}",
            r.content
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            "\tx\n",
            "unchanged"
        );
    }

    // Regression: indent arithmetic must count *characters*, not bytes. When the file
    // is indented with a multi-byte whitespace char (here U+3000 IDEOGRAPHIC SPACE,
    // 3 bytes / 1 char), the old byte-based `file_indent` fed into `chars().take(n)`
    // grabbed content chars into the indent prefix, producing corruption like
    // "\u{3000}x x = 99". The fix (leading_ws_chars) keeps exactly the whitespace.
    // BLOCK-ANCHOR tier: the model reproduced a multi-line block but got ONE interior
    // line slightly wrong (`let b = 20;` vs the file's `let b = 2;`) AND used spaces where
    // the file uses tabs. Exact + whitespace-normalized fuzzy both fail (fuzzy needs EVERY
    // trimmed line to match). Block-anchor matches on the first/last trimmed lines, replaces
    // the real window, and re-anchors to the file's tabs — so the model doesn't reach for sed.
    #[tokio::test]
    async fn block_anchor_matches_interior_drift_and_preserves_tabs() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("f.rs"),
            "fn f() {\n\tlet a = 1;\n\tlet b = 2;\n\tlet c = 3;\n}\n",
        )
        .unwrap();
        let r = EditFileTool
            .execute(
                r#"{"file_path":"f.rs","old_string":"    let a = 1;\n    let b = 20;\n    let c = 3;","new_string":"    let a = 1;\n    let b = 99;\n    let c = 3;"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(!r.is_error, "block-anchor must succeed: {}", r.content);
        assert!(
            r.content.contains("anchored block"),
            "should report an anchored match: {}",
            r.content
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("f.rs")).unwrap(),
            "fn f() {\n\tlet a = 1;\n\tlet b = 99;\n\tlet c = 3;\n}\n",
            "the intended edit applies with the file's tab indentation preserved"
        );
    }

    // Guard: a block that merely SHARES its first/last line with an unrelated region (all
    // interior lines differ) must be REJECTED (< half match), not clobbered.
    #[tokio::test]
    async fn block_anchor_rejects_low_similarity_block() {
        let d = tempfile::tempdir().unwrap();
        let original = "start marker\nreal one\nreal two\nreal three\nend marker\n";
        std::fs::write(d.path().join("a.txt"), original).unwrap();
        let r = EditFileTool
            .execute(
                // first/last match, but all 3 interior lines are wrong → 2/5 < half → reject.
                r#"{"file_path":"a.txt","old_string":"start marker\nWRONG a\nWRONG b\nWRONG c\nend marker","new_string":"start marker\nX\nend marker"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(
            r.is_error,
            "a low-similarity block must be refused: {}",
            r.content
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            original,
            "file must be unchanged"
        );
    }

    // Guard: at-most-ONE drifted line. A 4-line block whose BOTH interior lines differ
    // (only the anchors match) must be REJECTED — a plain "≥ half" rule would have passed
    // this (2/4), clobbering an unrelated region that happens to share first/last lines.
    #[tokio::test]
    async fn block_anchor_rejects_two_drifted_interior_lines() {
        let d = tempfile::tempdir().unwrap();
        let original = "region top\n\treal one\n\treal two\nregion bottom\n";
        std::fs::write(d.path().join("a.txt"), original).unwrap();
        let r = EditFileTool
            .execute(
                // first/last match; BOTH interior lines wrong → matched 2/4 → reject.
                r#"{"file_path":"a.txt","old_string":"region top\nWRONG one\nWRONG two\nregion bottom","new_string":"region top\nX\nregion bottom"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(
            r.is_error,
            "two drifted interior lines must be refused: {}",
            r.content
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            original
        );
    }

    // Coverage: the OUTDENTED-line re-anchor path (`signed_relative < 0`) — a new line less
    // indented than the block's anchor (e.g. a top-level call after an indented statement).
    // The file uses tabs; the fuzzy tier matches and re-anchors, dropping indent for the
    // outdented line.
    #[tokio::test]
    async fn reanchor_handles_outdented_new_line() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("f.rs"),
            "fn f() {\n\tlet a = 1;\n\tlet b = 2;\n}\n",
        )
        .unwrap();
        let r = EditFileTool
            .execute(
                // Model copied with spaces; new_string's 2nd line is OUTDENTED to column 0.
                r#"{"file_path":"f.rs","old_string":"    let a = 1;\n    let b = 2;","new_string":"    let a = 1;\ndone();"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(
            !r.is_error,
            "outdented re-anchor must succeed: {}",
            r.content
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("f.rs")).unwrap(),
            "fn f() {\n\tlet a = 1;\ndone();\n}\n",
            "the kept line stays tab-indented; the outdented line drops to column 0"
        );
    }

    // Guard: two windows share the same first/last anchors → ambiguous → refuse.
    #[tokio::test]
    async fn block_anchor_rejects_ambiguous_windows() {
        let d = tempfile::tempdir().unwrap();
        let original =
            "open block\n  middle here\nclose block\n\nopen block\n  other mid\nclose block\n";
        std::fs::write(d.path().join("a.txt"), original).unwrap();
        let r = EditFileTool
            .execute(
                r#"{"file_path":"a.txt","old_string":"open block\n  drifted\nclose block","new_string":"open block\n  changed\nclose block"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(
            r.is_error,
            "ambiguous anchored windows must be refused: {}",
            r.content
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.txt")).unwrap(),
            original
        );
    }

    // Guard: bare-brace anchors (`{` / `}`, < 3 trimmed chars) can't anchor a block.
    #[tokio::test]
    async fn block_anchor_ignores_short_anchors() {
        let d = tempfile::tempdir().unwrap();
        let original = "if x {\n\tfoo();\n}\n";
        std::fs::write(d.path().join("a.rs"), original).unwrap();
        let r = EditFileTool
            .execute(
                r#"{"file_path":"a.rs","old_string":"{\n    bar();\n}","new_string":"{\n    baz();\n}"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(
            r.is_error,
            "short brace anchors must not fire: {}",
            r.content
        );
        assert_eq!(
            std::fs::read_to_string(d.path().join("a.rs")).unwrap(),
            original
        );
    }

    #[tokio::test]
    async fn fuzzy_preserves_multibyte_whitespace_indentation() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("f.py"),
            "def f():\n\u{3000}x = 1\n\u{3000}y = 2\n",
        )
        .unwrap();
        // Model reproduced the body with plain-space indentation → exact match fails,
        // fuzzy path fires.
        let r = EditFileTool
            .execute(
                r#"{"file_path":"f.py","old_string":"    x = 1\n    y = 2","new_string":"    x = 99\n    y = 2"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(!r.is_error, "fuzzy match must succeed: {}", r.content);
        assert_eq!(
            std::fs::read_to_string(d.path().join("f.py")).unwrap(),
            "def f():\n\u{3000}x = 99\n\u{3000}y = 2\n",
            "the file's multi-byte whitespace indent must be preserved with no content leaking into it"
        );
    }
}
