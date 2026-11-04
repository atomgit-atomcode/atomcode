//! Strict unified-diff parsing and exact application. No fuzzy matching or EOL conversion.
use std::collections::BTreeSet;

#[derive(Debug, Clone)]
pub(crate) struct FilePatch {
    pub path: String,
    pub hunks: Vec<Hunk>,
}
#[derive(Debug, Clone)]
pub(crate) struct Hunk {
    old: usize,
    new: usize,
    before: Vec<String>,
    after: Vec<String>,
}

fn path(header: &str, prefix: &str) -> Result<String, String> {
    let value = header
        .strip_prefix(prefix)
        .ok_or("missing file header")?
        .trim_end_matches(['\r', '\n']);
    let value = value.split_once('\t').map_or(value, |(p, _)| p);
    let value = value
        .strip_prefix("a/")
        .or_else(|| value.strip_prefix("b/"))
        .unwrap_or(value);
    if value.is_empty()
        || value.starts_with('/')
        || value.contains(['\\', ':', '\0', '"', '\r', '\n'])
        || value.split('/').any(|part| {
            part.is_empty() || part == "." || part == ".." || part.eq_ignore_ascii_case(".git")
        })
    {
        return Err(format!("unsupported or unsafe patch path: {value:?}"));
    }
    Ok(value.to_owned())
}
fn range(value: &str, sign: char) -> Result<(usize, usize), String> {
    let value = value.strip_prefix(sign).ok_or("invalid hunk range")?;
    let (start, count) = value.split_once(',').unwrap_or((value, "1"));
    let start = start.parse::<usize>().map_err(|_| "invalid hunk start")?;
    let count = count.parse::<usize>().map_err(|_| "invalid hunk count")?;
    if count > 0 && start == 0 {
        return Err("nonempty hunk starts at zero".into());
    }
    Ok((start, count))
}

pub(crate) fn parse(source: &str) -> Result<Vec<FilePatch>, String> {
    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    let mut files = Vec::new();
    let mut seen = BTreeSet::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].starts_with("diff --git ") || lines[i].starts_with("index ") {
            i += 1;
            continue;
        }
        let file_path = path(lines[i], "--- ")?;
        i += 1;
        let new_path = path(lines.get(i).ok_or("missing +++ header")?, "+++ ")?;
        if file_path != new_path {
            return Err("renames, additions and deletions are not supported".into());
        }
        if !seen.insert(file_path.clone()) {
            return Err(format!("duplicate patch target: {file_path}"));
        }
        i += 1;
        let mut hunks = Vec::new();
        while i < lines.len() && lines[i].starts_with("@@ ") {
            let words: Vec<_> = lines[i].split_whitespace().collect();
            if words.len() < 4 || words[0] != "@@" || words[3] != "@@" {
                return Err("invalid hunk header".into());
            }
            let (old, old_count) = range(words[1], '-')?;
            let (new, new_count) = range(words[2], '+')?;
            i += 1;
            let mut before: Vec<String> = Vec::new();
            let mut after: Vec<String> = Vec::new();
            let mut last = None;
            while i < lines.len() {
                let line = lines[i];
                if line.trim_end_matches(['\r', '\n']) == "\\ No newline at end of file" {
                    let tag = last
                        .take()
                        .ok_or("newline marker without preceding content")?;
                    if tag != '+' {
                        let value = before.last_mut().ok_or("invalid newline marker")?;
                        if !value.ends_with('\n') {
                            return Err("duplicate newline marker".into());
                        }
                        value.pop();
                    }
                    if tag != '-' {
                        let value = after.last_mut().ok_or("invalid newline marker")?;
                        if !value.ends_with('\n') {
                            return Err("duplicate newline marker".into());
                        }
                        value.pop();
                    }
                    i += 1;
                    continue;
                }
                if before.len() == old_count && after.len() == new_count {
                    break;
                }
                let tag = line.chars().next().ok_or("empty hunk line")?;
                if !matches!(tag, ' ' | '-' | '+') {
                    return Err(format!("{file_path}:{old}: incomplete hunk"));
                }
                let body = &line[1..];
                if !body.ends_with('\n') {
                    return Err("unterminated patch line; use a no-newline marker".into());
                }
                if tag != '+' {
                    before.push(body.to_owned());
                }
                if tag != '-' {
                    after.push(body.to_owned());
                }
                if before.len() > old_count || after.len() > new_count {
                    return Err(format!("{file_path}:{old}: hunk counts exceeded"));
                }
                last = Some(tag);
                i += 1;
            }
            if before.len() != old_count || after.len() != new_count {
                return Err(format!("{file_path}:{old}: incomplete hunk"));
            }
            for content in [&before, &after] {
                if content
                    .iter()
                    .take(content.len().saturating_sub(1))
                    .any(|line| !line.ends_with('\n'))
                {
                    return Err(format!(
                        "{file_path}:{old}: no-newline marker before remaining hunk content"
                    ));
                }
            }
            if hunks.len() >= 20000 {
                return Err("patch exceeds 20000 hunks per file".into());
            }
            hunks.push(Hunk {
                old,
                new,
                before,
                after,
            });
        }
        if hunks.is_empty() {
            return Err(format!("{file_path}: no hunks"));
        }
        if files.len() >= 256 {
            return Err("patch exceeds 256-file limit".into());
        }
        files.push(FilePatch {
            path: file_path,
            hunks,
        });
    }
    if files.is_empty() {
        return Err("not a text unified diff; use edit_file for semantic edits".into());
    }
    Ok(files)
}

pub(crate) fn apply(file: &FilePatch, source: &str) -> Result<String, String> {
    let lines: Vec<&str> = source.split_inclusive('\n').collect();
    let mut cursor = 0;
    let mut output = String::new();
    let mut output_lines = 0;
    for (index, hunk) in file.hunks.iter().enumerate() {
        let start = if hunk.before.is_empty() {
            hunk.old
        } else {
            hunk.old - 1
        };
        let fail = || {
            format!("{}:{}: hunk {} conflicts with current content (expected {} source lines); no files written", file.path, hunk.old, index + 1, hunk.before.len())
        };
        if start < cursor || start > lines.len() || hunk.before.len() > lines.len() - start {
            return Err(fail());
        }
        if !lines[start..start + hunk.before.len()]
            .iter()
            .copied()
            .eq(hunk.before.iter().map(String::as_str))
        {
            return Err(fail());
        }
        output.extend(lines[cursor..start].iter().copied());
        output_lines += start - cursor;
        let expected_new = if hunk.after.is_empty() {
            hunk.new
        } else {
            hunk.new - 1
        };
        if output_lines != expected_new {
            return Err(format!(
                "{}:{}: inconsistent new-file hunk position",
                file.path, hunk.new
            ));
        }
        if hunk.after.last().is_some_and(|line| !line.ends_with('\n'))
            && (index + 1 < file.hunks.len() || start + hunk.before.len() < lines.len())
        {
            return Err(format!(
                "{}:{}: no-newline marker is not at end of file",
                file.path, hunk.new
            ));
        }
        output.extend(hunk.after.iter().map(String::as_str));
        output_lines += hunk.after.len();
        cursor = start + hunk.before.len();
    }
    output.extend(lines[cursor..].iter().copied());
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_unicode_and_missing_final_newline() {
        let patch = parse("--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-旧\n\\ No newline at end of file\n+新\n\\ No newline at end of file\n").unwrap();
        assert_eq!(apply(&patch[0], "旧").unwrap(), "新");
        assert!(apply(&patch[0], "旧\n").is_err());
    }
    #[test]
    fn rejects_traversal_and_incomplete_hunks() {
        assert!(parse("--- a/../secret\n+++ b/../secret\n@@ -1 +1 @@\n-a\n+b\n").is_err());
        assert!(parse("--- a/a\n+++ b/a\n@@ -1,2 +1,2 @@\n-a\n+b\n").is_err());
        assert!(parse("not a diff").is_err());
    }
    #[test]
    fn exact_multiple_hunks() {
        let patch = parse("--- a/f\n+++ b/f\n@@ -1 +1 @@\n-a\n+A\n@@ -3 +3 @@\n-c\n+C\n").unwrap();
        assert_eq!(apply(&patch[0], "a\nb\nc\n").unwrap(), "A\nb\nC\n");
    }
    #[test]
    fn crlf_patch_headers_and_content_preserve_crlf_bytes() {
        let patch = parse("--- a/f\r\n+++ b/f\r\n@@ -1 +1 @@\r\n-old\r\n+new\r\n").unwrap();
        assert_eq!(apply(&patch[0], "old\r\n").unwrap(), "new\r\n");
        assert!(
            apply(&patch[0], "old\n").is_err(),
            "never silently normalize source EOLs"
        );
    }
    #[test]
    fn rejects_no_newline_markers_before_remaining_content() {
        assert!(parse("--- a/f\n+++ b/f\n@@ -1 +1,2 @@\n-old\n+first\n\\ No newline at end of file\n+second\n").is_err());
        let patch =
            parse("--- a/f\n+++ b/f\n@@ -1 +1 @@\n-old\n+new\n\\ No newline at end of file\n")
                .unwrap();
        assert!(apply(&patch[0], "old\ntail\n").is_err());
    }
}
