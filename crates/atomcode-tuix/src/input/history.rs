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
    entries: Vec<HistoryEntry>,
    cache_dir: PathBuf,
}

impl History {
    /// Load history from `path` and configure `cache_dir` for GC + the
    /// future `image_cache_dir` consumers in the event loop. The
    /// cache_dir argument is wired through from
    /// `crate::platform::image_cache_dir()` at startup.
    pub fn load_with_cache<P: Into<PathBuf>>(path: P, cache_dir: PathBuf) -> Self {
        let path = path.into();
        // Each physical line is one entry. Per-line fallback chain so we
        // never reject a row written by an older build:
        //   1. parse as `HistoryEntry` (current format, JSON object)
        //   2. parse as `String` (older JSON-encoded string lines)
        //   3. treat the line as raw plain text (pre-JSON format)
        let entries: Vec<HistoryEntry> = fs::read_to_string(&path)
            .ok()
            .map(|s| {
                s.lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(|l| {
                        if let Ok(e) = serde_json::from_str::<HistoryEntry>(l) {
                            return e;
                        }
                        if let Ok(t) = serde_json::from_str::<String>(l) {
                            return HistoryEntry { text: t, images: Vec::new() };
                        }
                        HistoryEntry { text: l.to_string(), images: Vec::new() }
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self { path, entries, cache_dir }
    }

    /// Back-compat constructor used by tests and any caller that doesn't
    /// care about the cache. Sets `cache_dir` to a sibling `image-cache`
    /// dir under the same parent so GC is a no-op when the dir doesn't
    /// exist.
    pub fn load<P: Into<PathBuf>>(path: P) -> Self {
        let path = path.into();
        let cache_dir = path
            .parent()
            .map(|p| p.join("image-cache"))
            .unwrap_or_else(|| PathBuf::from("."));
        Self::load_with_cache(path, cache_dir)
    }

    /// Default history path: `~/.atomcode/history` on Unix,
    /// `%USERPROFILE%\.atomcode\history` on Windows (or a tempdir
    /// fallback if home is unknown).
    pub fn default_path() -> Option<PathBuf> {
        Some(crate::platform::history_path())
    }

    pub fn entries(&self) -> &Vec<HistoryEntry> {
        &self.entries
    }

    pub fn push(&mut self, line: String) {
        if line.trim().is_empty() {
            return;
        }
        if self.entries.last().map(|e| &e.text) == Some(&line) {
            return;
        }
        self.entries.push(HistoryEntry { text: line, images: Vec::new() });
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
            .map(|e| serde_json::to_string(e).unwrap_or_else(|_| e.text.clone()))
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
        assert_eq!(h.entries(), &Vec::<HistoryEntry>::new());
    }

    #[ignore]
    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hist");
        let mut h = History::load(&path);
        h.push("one".into());
        h.push("two".into());
        h.save().unwrap();

        let h2 = History::load(&path);
        let texts: Vec<&str> = h2.entries().iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["one", "two"]);
    }

    #[ignore]
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
        let texts: Vec<&str> = h2.entries().iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["1\n2\n3", "next"]);
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
        assert_eq!(h.entries().len(), 2);
        assert_eq!(h.entries()[0].text, "hello world");
        assert!(h.entries()[0].images.is_empty());
        assert_eq!(h.entries()[1].text, "another line");
    }

    #[ignore]
    #[test]
    fn duplicate_consecutive_collapsed() {
        let dir = tempdir().unwrap();
        let mut h = History::load(dir.path().join("hist"));
        h.push("x".into());
        h.push("x".into());
        h.push("y".into());
        let texts: Vec<&str> = h.entries().iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["x", "y"]);
    }

    #[ignore]
    #[test]
    fn capped_at_max_entries() {
        let dir = tempdir().unwrap();
        let mut h = History::load(dir.path().join("hist"));
        for i in 0..2000 {
            h.push(format!("cmd{}", i));
        }
        assert!(h.entries().len() <= HISTORY_MAX);
        assert!(!h.entries().iter().any(|e| e.text == "cmd0"));
    }

    #[ignore]
    #[test]
    fn empty_entries_ignored() {
        let dir = tempdir().unwrap();
        let mut h = History::load(dir.path().join("hist"));
        h.push("".into());
        h.push("  ".into());
        h.push("real".into());
        let texts: Vec<&str> = h.entries().iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["real"]);
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

    #[test]
    fn load_legacy_string_lines_become_text_only_entries() {
        // Entries written by older builds: each line is a JSON-encoded
        // string. After upgrade, they must load as HistoryEntry with empty
        // images.
        let dir = tempdir().unwrap();
        let path = dir.path().join("hist");
        fs::write(&path, "\"hello\"\n\"world\"").unwrap();
        let h = History::load(&path);
        assert_eq!(h.entries().len(), 2);
        assert_eq!(h.entries()[0].text, "hello");
        assert!(h.entries()[0].images.is_empty());
        assert_eq!(h.entries()[1].text, "world");
    }

    #[test]
    fn load_new_object_lines_carry_images() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hist");
        fs::write(
            &path,
            "{\"text\":\"a\",\"images\":[{\"hash\":\"deadbeef12345678\",\"mt\":\"image/png\",\"n\":1}]}\n{\"text\":\"b\"}",
        )
        .unwrap();
        let h = History::load(&path);
        assert_eq!(h.entries().len(), 2);
        assert_eq!(h.entries()[0].text, "a");
        assert_eq!(h.entries()[0].images.len(), 1);
        assert_eq!(h.entries()[0].images[0].hash, "deadbeef12345678");
        assert_eq!(h.entries()[1].text, "b");
        assert!(h.entries()[1].images.is_empty());
    }
}
