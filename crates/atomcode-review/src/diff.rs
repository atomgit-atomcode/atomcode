//! Diff line-number annotation: prefix every hunk content line with its REAL file line
//! number (parsed from `@@ -old,len +new,len @@` headers), so the model reports accurate
//! `line_start`/`line_end` without counting lines itself — a large accuracy win for
//! `report_finding` anchoring.
//!
//! Output format (metadata lines are kept verbatim — they carry the file names):
//!
//! ```text
//! diff --git a/src/a.rs b/src/a.rs
//! @@ -10,7 +10,10 @@ fn main() {
//! 10:  fn handle(r: &Request) -> Result<()> {
//! 11: -    let old = 1;
//! 11: +    let new = 2;
//! ```
//!
//! Added/context lines use the NEW file line number (what reviewers anchor to);
//! deleted lines use the OLD file line number.

/// Annotate a unified diff with real line numbers. Unparseable input degrades gracefully:
/// lines outside a known hunk are passed through unchanged.
pub fn annotate_diff_line_numbers(diff: &str) -> String {
    let mut out = String::with_capacity(diff.len() + diff.len() / 8);
    let mut old_line: u64 = 0;
    let mut new_line: u64 = 0;
    let mut in_hunk = false;

    for line in diff.split('\n') {
        // File/metadata lines: keep verbatim (they carry the file names) and end any hunk.
        if line.starts_with("diff --git")
            || line.starts_with("index ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
            || line.starts_with("new file mode")
            || line.starts_with("deleted file mode")
            || line.starts_with("old mode")
            || line.starts_with("new mode")
            || line.starts_with("similarity index")
            || line.starts_with("rename from")
            || line.starts_with("rename to")
            || line.starts_with("Binary files")
        {
            in_hunk = false;
            out.push_str(line);
            out.push('\n');
            continue;
        }

        if line.starts_with("@@") {
            if let Some((o, n)) = parse_hunk_header(line) {
                old_line = o;
                new_line = n;
                in_hunk = true;
            } else {
                in_hunk = false;
            }
            out.push_str(line);
            out.push('\n');
            continue;
        }

        if !in_hunk || line.is_empty() {
            out.push_str(line);
            out.push('\n');
            continue;
        }

        match line.as_bytes()[0] {
            b'+' => {
                out.push_str(&format!("{new_line}: {line}\n"));
                new_line += 1;
            }
            b'-' => {
                out.push_str(&format!("{old_line}: {line}\n"));
                old_line += 1;
            }
            b'\\' => {
                // "\ No newline at end of file" — keep verbatim, advances nothing.
                out.push_str(line);
                out.push('\n');
            }
            _ => {
                // Context line: anchor to the NEW file line number.
                out.push_str(&format!("{new_line}: {line}\n"));
                old_line += 1;
                new_line += 1;
            }
        }
    }

    // split('\n') yields a trailing empty segment for a trailing newline — drop the
    // extra newline we appended for it so the output matches the input's ending.
    if (!diff.ends_with('\n') && out.ends_with('\n'))
        || (diff.ends_with('\n') && out.ends_with("\n\n"))
    {
        out.pop();
    }
    out
}

/// Parse `@@ -old_start[,len] +new_start[,len] @@ …` → (old_start, new_start).
fn parse_hunk_header(header: &str) -> Option<(u64, u64)> {
    let rest = header.strip_prefix("@@")?;
    let end = rest.find("@@")?;
    let spec = rest[..end].trim();
    let mut old_start = None;
    let mut new_start = None;
    for part in spec.split_whitespace() {
        if let Some(nums) = part.strip_prefix('-') {
            old_start = nums.split(',').next()?.parse::<u64>().ok();
        } else if let Some(nums) = part.strip_prefix('+') {
            new_start = nums.split(',').next()?.parse::<u64>().ok();
        }
    }
    match (old_start, new_start) {
        (Some(o), Some(n)) => Some((o, n)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn annotates_add_del_context_with_real_numbers() {
        let diff = "diff --git a/a.go b/a.go\n--- a/a.go\n+++ b/a.go\n@@ -10,3 +10,4 @@ func main() {\n ctx\n-old\n+new1\n+new2\n";
        let got = annotate_diff_line_numbers(diff);
        assert!(got.contains("diff --git a/a.go b/a.go\n"), "metadata kept verbatim");
        assert!(got.contains("10:  ctx"), "context uses NEW number, got:\n{got}");
        assert!(got.contains("11: -old"), "deletion uses OLD number, got:\n{got}");
        assert!(got.contains("11: +new1"), "addition uses NEW number, got:\n{got}");
        assert!(got.contains("12: +new2"), "second addition increments, got:\n{got}");
    }

    #[test]
    fn multiple_hunks_reset_counters() {
        let diff = "+++ b/a.go\n@@ -1,1 +1,1 @@\n+x\n@@ -50,1 +60,1 @@\n+y\n";
        let got = annotate_diff_line_numbers(diff);
        assert!(got.contains("1: +x"));
        assert!(got.contains("60: +y"), "second hunk restarts at its own start:\n{got}");
    }

    #[test]
    fn passthrough_outside_hunks_and_no_newline_marker() {
        let diff = "+++ b/a.go\n@@ -1,1 +1,1 @@\n+x\n\\ No newline at end of file\n";
        let got = annotate_diff_line_numbers(diff);
        assert!(got.contains("\\ No newline at end of file\n"), "marker kept verbatim");
        // Garbage input degrades to passthrough.
        assert_eq!(annotate_diff_line_numbers("hello\nworld"), "hello\nworld");
    }
}
