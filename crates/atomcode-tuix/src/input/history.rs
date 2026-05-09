// crates/atomcode-tuix/src/input/history.rs

use std::fs;
use std::io;
use std::path::PathBuf;

/// One row in the input history file. Replaces the prior plain `String`
/// representation so we can carry image attachments alongside the text.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HistoryEntry {
    pub text: String,
    /// Image attachments associated with this submission. Skipped on
    /// serialization when empty so plain text-only history rows stay
    /// compact (`{"text":"hi"}` rather than `{"text":"hi","images":[]}`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<HistoryImageRef>,
}

/// Reference to a single image cached on disk under
/// `~/.atomcode/image-cache/<hash>.<ext>`. Recorded on submit; consumed
/// on up-arrow recall to rehydrate `pending_images`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct HistoryImageRef {
    /// u64 content hash, lowercase hex, 16 chars. Same value that's
    /// pushed into `UiState::pending_image_hashes` at paste time.
    /// Stored as a string for direct serde without a custom hex codec.
    pub hash: String,
    /// MIME type. Drives the cache filename extension via
    /// `ext_for_mt()`.
    pub mt: String,
    /// The `[Image #N]` marker the entry was originally submitted with.
    /// On hydrate the marker is renumbered to a fresh
    /// `session_image_count` value to avoid collisions; this field is
    /// the lookup key for `line.replace("[Image #<n>]", ...)`.
    pub n: usize,
}

pub const HISTORY_MAX: usize = 1000;

pub struct History {
    path: PathBuf,
    entries: Vec<String>,
}

impl History {
    pub fn load<P: Into<PathBuf>>(path: P) -> Self {
        let path = path.into();
        // Each physical line is one JSON-encoded entry (so entries may
        // contain `\n` — multi-line submissions via Alt+Enter need this).
        // Fallback: any line that fails JSON parse is treated as a raw
        // plain-text entry so histories written by older builds (which
        // stored entries verbatim) continue to load.
        let entries = fs::read_to_string(&path)
            .ok()
            .map(|s| {
                s.lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| serde_json::from_str::<String>(l).unwrap_or_else(|_| l.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        Self { path, entries }
    }

    /// Default history path: `~/.atomcode/history` on Unix,
    /// `%USERPROFILE%\.atomcode\history` on Windows (or a tempdir
    /// fallback if home is unknown).
    pub fn default_path() -> Option<PathBuf> {
        Some(crate::platform::history_path())
    }

    pub fn entries(&self) -> &Vec<String> {
        &self.entries
    }

    pub fn push(&mut self, line: String) {
        if line.trim().is_empty() {
            return;
        }
        if self.entries.last() == Some(&line) {
            return;
        }
        self.entries.push(line);
        if self.entries.len() > HISTORY_MAX {
            let drop = self.entries.len() - HISTORY_MAX;
            self.entries.drain(..drop);
        }
    }

    pub fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        // JSON-encode each entry so multi-line submissions survive the
        // round-trip. A raw `entries.join("\n")` would split a single
        // `"1\n2\n3"` entry into three on the next `load()`.
        let contents: String = self
            .entries
            .iter()
            .map(|e| serde_json::to_string(e).unwrap_or_else(|_| e.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&self.path, contents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn load_nonexistent_returns_empty() {
        let dir = tempdir().unwrap();
        let h = History::load(dir.path().join("hist"));
        assert_eq!(h.entries(), &Vec::<String>::new());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hist");
        let mut h = History::load(&path);
        h.push("one".into());
        h.push("two".into());
        h.save().unwrap();

        let h2 = History::load(&path);
        assert_eq!(h2.entries(), &vec!["one".to_string(), "two".to_string()]);
    }

    #[test]
    fn multi_line_entry_survives_roundtrip() {
        // Regression: a `"1\n2\n3"` entry must round-trip as ONE entry.
        // The pre-JSON serialization split it into three on reload.
        let dir = tempdir().unwrap();
        let path = dir.path().join("hist");
        let mut h = History::load(&path);
        h.push("1\n2\n3".into());
        h.push("next".into());
        h.save().unwrap();

        let h2 = History::load(&path);
        assert_eq!(
            h2.entries(),
            &vec!["1\n2\n3".to_string(), "next".to_string()]
        );
    }

    #[test]
    fn legacy_plaintext_history_still_loads() {
        // Older builds wrote entries verbatim (one line per entry, no
        // JSON encoding). Those files must still load — the fallback in
        // `load()` treats unparseable lines as raw entries.
        let dir = tempdir().unwrap();
        let path = dir.path().join("hist");
        fs::write(&path, "hello world\nanother line").unwrap();
        let h = History::load(&path);
        assert_eq!(
            h.entries(),
            &vec!["hello world".to_string(), "another line".to_string()]
        );
    }

    #[test]
    fn duplicate_consecutive_collapsed() {
        let dir = tempdir().unwrap();
        let mut h = History::load(dir.path().join("hist"));
        h.push("x".into());
        h.push("x".into());
        h.push("y".into());
        assert_eq!(h.entries(), &vec!["x".to_string(), "y".to_string()]);
    }

    #[test]
    fn capped_at_max_entries() {
        let dir = tempdir().unwrap();
        let mut h = History::load(dir.path().join("hist"));
        for i in 0..2000 {
            h.push(format!("cmd{}", i));
        }
        assert!(h.entries().len() <= HISTORY_MAX);
        assert!(!h.entries().iter().any(|s| s == "cmd0"));
    }

    #[test]
    fn empty_entries_ignored() {
        let dir = tempdir().unwrap();
        let mut h = History::load(dir.path().join("hist"));
        h.push("".into());
        h.push("  ".into());
        h.push("real".into());
        assert_eq!(h.entries(), &vec!["real".to_string()]);
    }

    #[test]
    fn history_entry_serde_roundtrip_with_images() {
        let e = HistoryEntry {
            text: "look [Image #2]".to_string(),
            images: vec![HistoryImageRef {
                hash: "deadbeef12345678".to_string(),
                mt: "image/png".to_string(),
                n: 2,
            }],
        };
        let j = serde_json::to_string(&e).unwrap();
        let back: HistoryEntry = serde_json::from_str(&j).unwrap();
        assert_eq!(back.text, e.text);
        assert_eq!(back.images.len(), 1);
        assert_eq!(back.images[0].hash, "deadbeef12345678");
        assert_eq!(back.images[0].mt, "image/png");
        assert_eq!(back.images[0].n, 2);
    }

    #[test]
    fn history_entry_text_only_serializes_without_images_field() {
        let e = HistoryEntry { text: "hi".to_string(), images: vec![] };
        let j = serde_json::to_string(&e).unwrap();
        assert!(!j.contains("images"), "empty images vec must be skipped: {}", j);
        assert_eq!(j, r#"{"text":"hi"}"#);
    }
}
