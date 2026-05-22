//! Cross-session input history for the TUI input box (↑/↓ recall).
//!
//! Stored as one line per submitted user input in `$ATOMCODE_HOME/input_history.txt`,
//! append-only, capped at `MAX_ENTRIES`. Unlike conversation messages, entries
//! are plain strings with no role/tool structure — this is purely a UX aid for
//! recalling past text, not for restoring conversation context.
//!
//! Multi-line inputs are encoded by escaping `\\` → `\\\\` and `\n` → `\\n` so
//! each entry occupies exactly one line on disk.

use std::io::Write;
use std::path::PathBuf;

const MAX_ENTRIES: usize = 1000;

pub struct InputHistory;

impl InputHistory {
    pub fn path() -> PathBuf {
        crate::config::Config::config_dir().join("input_history.txt")
    }

    /// Load all entries in order (oldest first, newest last).
    pub fn load() -> Vec<String> {
        let data = match std::fs::read_to_string(Self::path()) {
            Ok(d) => d,
            Err(_) => return Vec::new(),
        };
        data.lines()
            .filter(|l| !l.is_empty())
            .map(decode_line)
            .collect()
    }

    /// Append a new entry, trimming the file if it exceeds `MAX_ENTRIES`.
    pub fn append(entry: &str) {
        if entry.trim().is_empty() {
            return;
        }
        let path = Self::path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let mut line = encode_line(entry);
        line.push('\n');

        let append_ok = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .and_then(|mut f| f.write_all(line.as_bytes()))
            .is_ok();
        if !append_ok {
            return;
        }

        // Enforce cap: if we've grown past MAX_ENTRIES, rewrite with tail.
        if let Ok(content) = std::fs::read_to_string(&path) {
            let lines: Vec<&str> = content.lines().collect();
            if lines.len() > MAX_ENTRIES {
                let keep = &lines[lines.len() - MAX_ENTRIES..];
                let mut new_content = keep.join("\n");
                new_content.push('\n');
                let tmp = path.with_extension("txt.tmp");
                if std::fs::write(&tmp, new_content).is_ok() {
                    let _ = std::fs::rename(&tmp, &path);
                }
            }
        }
    }
}

fn encode_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            _ => out.push(c),
        }
    }
    out
}

fn decode_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_simple() {
        assert_eq!(decode_line(&encode_line("hello")), "hello");
    }

    #[test]
    fn roundtrip_multiline() {
        let s = "line1\nline2\nline3";
        assert_eq!(decode_line(&encode_line(s)), s);
    }

    #[test]
    fn roundtrip_with_backslashes() {
        let s = "path\\to\\file and a \n newline";
        assert_eq!(decode_line(&encode_line(s)), s);
    }

    #[test]
    fn encode_strips_cr() {
        assert_eq!(encode_line("a\r\nb"), "a\\nb");
    }
}
