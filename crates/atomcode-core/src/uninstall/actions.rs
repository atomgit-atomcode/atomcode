//! Side-effecting operations: rm, rc-file edits, Windows PATH, self-delete.

/// Remove the canonical `# Added by AtomCode installer\nexport PATH="<prefix>:$PATH"`
/// block(s) from a shell rc file's content. Strict matching: requires both
/// the comment and the export line targeting `prefix`. User-written PATH
/// lines without the comment are left alone.
///
/// Returns `Some(new_content)` if at least one block was removed,
/// `None` otherwise.
pub fn strip_atomcode_path_block(content: &str, prefix: &str) -> Option<String> {
    let comment = "# Added by AtomCode installer";
    let target_export = format!("export PATH=\"{prefix}:$PATH\"");

    let lines: Vec<&str> = content.lines().collect();
    let mut keep = vec![true; lines.len()];
    let mut removed_any = false;

    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() == comment {
            // Look ahead for the export line; allow at most one blank line between.
            let mut j = i + 1;
            while j < lines.len() && lines[j].trim().is_empty() { j += 1; }
            if j < lines.len() && lines[j].trim() == target_export.trim() {
                // Drop comment + intervening blanks + export line.
                for k in i..=j { keep[k] = false; }
                // Also drop one trailing blank line if present, to avoid leaving a double blank.
                if j + 1 < lines.len() && lines[j + 1].trim().is_empty() {
                    keep[j + 1] = false;
                }
                removed_any = true;
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }

    if !removed_any { return None; }

    let mut out = String::with_capacity(content.len());
    for (idx, line) in lines.iter().enumerate() {
        if keep[idx] {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !content.ends_with('\n') {
        if let Some(last) = out.strip_suffix('\n') { out = last.to_string(); }
    }
    Some(out)
}

#[cfg(test)]
mod path_line_tests {
    use super::strip_atomcode_path_block;

    const PREFIX: &str = "/Users/test/.local/bin";

    #[test]
    fn strips_canonical_block() {
        let input = "\
# user stuff
alias gs=\"git status\"

# Added by AtomCode installer
export PATH=\"/Users/test/.local/bin:$PATH\"

# more user stuff
";
        let expect = "\
# user stuff
alias gs=\"git status\"

# more user stuff
";
        assert_eq!(strip_atomcode_path_block(input, PREFIX).as_deref(), Some(expect));
    }

    #[test]
    fn returns_none_when_no_block() {
        let input = "alias gs=\"git status\"\n";
        assert_eq!(strip_atomcode_path_block(input, PREFIX), None);
    }

    #[test]
    fn strips_multiple_blocks_from_repeat_installs() {
        let input = "\
# Added by AtomCode installer
export PATH=\"/Users/test/.local/bin:$PATH\"

alias x=1

# Added by AtomCode installer
export PATH=\"/Users/test/.local/bin:$PATH\"
";
        let out = strip_atomcode_path_block(input, PREFIX).unwrap();
        assert!(!out.contains("AtomCode installer"));
        assert!(out.contains("alias x=1"));
    }

    #[test]
    fn does_not_touch_user_written_path_lines() {
        let input = "\
export PATH=\"/Users/test/.local/bin:$PATH\"
# unrelated comment
";
        // No installer comment → must return None even though prefix matches.
        assert_eq!(strip_atomcode_path_block(input, PREFIX), None);
    }

    #[test]
    fn ignores_block_with_different_prefix() {
        let input = "\
# Added by AtomCode installer
export PATH=\"/opt/somewhere/else:$PATH\"
";
        assert_eq!(strip_atomcode_path_block(input, PREFIX), None);
    }

    #[test]
    fn handles_block_at_end_of_file() {
        let input = "alias x=1\n\n# Added by AtomCode installer\nexport PATH=\"/Users/test/.local/bin:$PATH\"\n";
        let out = strip_atomcode_path_block(input, PREFIX).unwrap();
        assert_eq!(out.trim_end(), "alias x=1");
    }
}
