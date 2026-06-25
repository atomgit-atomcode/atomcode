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
    /// Original bodies of any folded `[Pasted #N …]` placeholders in
    /// `text`, in placeholder order (index 0 = paste #1). Mirrors the
    /// `Buffer.pastes` registry that was live when the line was
    /// submitted. Without this, an up-arrow recall of a message that
    /// contained a paste would bring back only the compact placeholder
    /// — the buffer's live `pastes` registry is cleared after each
    /// submit, so `expand_pastes` had nothing to substitute and the
    /// agent received the literal `[Pasted #N +M lines]` token instead
    /// of the pasted body (issue #843). On recall the buffer rehydrates
    /// its `pastes` from this field so expansion works again. Skipped on
    /// serialization when empty so plain rows stay compact.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pastes: Vec<String>,
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
                            return HistoryEntry { text: t, images: Vec::new(), pastes: Vec::new() };
                        }
                        HistoryEntry { text: l.to_string(), images: Vec::new(), pastes: Vec::new() }
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

    pub fn push(&mut self, entry: HistoryEntry) {
        if entry.text.trim().is_empty() {
            return;
        }
        if self.entries.last().map(|e| &e.text) == Some(&entry.text) {
            return;
        }
        self.entries.push(entry);
        if self.entries.len() > HISTORY_MAX {
            let drop = self.entries.len() - HISTORY_MAX;
            self.entries.drain(..drop);
        }
    }

    pub fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let contents: String = self
            .entries
            .iter()
            .map(|e| serde_json::to_string(e).unwrap_or_else(|_| {
                // Defensive fallback — HistoryEntry should always serialize
                // cleanly via serde, but if a future field broke that,
                // emit a JSON-string of the text so a malformed entry
                // doesn't poison the rest of the file.
                serde_json::to_string(&e.text).unwrap_or_else(|_| e.text.clone())
            }))
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&self.path, contents)?;
        let _ = self.gc(); // best-effort; never fails the save
        Ok(())
    }

    /// Best-effort garbage collection: remove any file in `cache_dir`
    /// whose 16-char-hex prefix is not referenced by any current
    /// history entry. Called automatically after each `save()`.
    fn gc(&self) -> io::Result<()> {
        use std::collections::HashSet;
        let referenced: HashSet<&str> = self
            .entries
            .iter()
            .flat_map(|e| e.images.iter().map(|i| i.hash.as_str()))
            .collect();
        let dir = match fs::read_dir(&self.cache_dir) {
            Ok(d) => d,
            Err(_) => return Ok(()), // dir missing — nothing to GC
        };
        for entry in dir.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            let prefix = match name_str.split('.').next() {
                Some(p) if p.len() == 16 && p.chars().all(|c| c.is_ascii_hexdigit()) => p,
                _ => continue, // unrecognized — leave it alone
            };
            if !referenced.contains(prefix) {
                let _ = fs::remove_file(entry.path()); // best-effort
            }
        }
        Ok(())
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

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hist");
        let mut h = History::load(&path);
        h.push(HistoryEntry { text: "one".into(), images: Vec::new(), pastes: Vec::new() });
        h.push(HistoryEntry { text: "two".into(), images: Vec::new(), pastes: Vec::new() });
        h.save().unwrap();

        let h2 = History::load(&path);
        assert_eq!(h2.entries().len(), 2);
        assert_eq!(h2.entries()[0].text, "one");
        assert_eq!(h2.entries()[1].text, "two");
    }

    #[test]
    fn multi_line_entry_survives_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("hist");
        let mut h = History::load(&path);
        h.push(HistoryEntry { text: "1\n2\n3".into(), images: Vec::new(), pastes: Vec::new() });
        h.push(HistoryEntry { text: "next".into(), images: Vec::new(), pastes: Vec::new() });
        h.save().unwrap();

        let h2 = History::load(&path);
        assert_eq!(h2.entries().len(), 2);
        assert_eq!(h2.entries()[0].text, "1\n2\n3");
        assert_eq!(h2.entries()[1].text, "next");
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

    #[test]
    fn duplicate_consecutive_collapsed() {
        let dir = tempdir().unwrap();
        let mut h = History::load(dir.path().join("hist"));
        h.push(HistoryEntry { text: "x".into(), images: Vec::new(), pastes: Vec::new() });
        h.push(HistoryEntry { text: "x".into(), images: Vec::new(), pastes: Vec::new() });
        h.push(HistoryEntry { text: "y".into(), images: Vec::new(), pastes: Vec::new() });
        assert_eq!(h.entries().len(), 2);
        assert_eq!(h.entries()[0].text, "x");
        assert_eq!(h.entries()[1].text, "y");
    }

    #[test]
    fn capped_at_max_entries() {
        let dir = tempdir().unwrap();
        let mut h = History::load(dir.path().join("hist"));
        for i in 0..2000 {
            h.push(HistoryEntry { text: format!("cmd{}", i), images: Vec::new(), pastes: Vec::new() });
        }
        assert!(h.entries().len() <= HISTORY_MAX);
        assert!(!h.entries().iter().any(|e| e.text == "cmd0"));
    }

    #[test]
    fn empty_entries_ignored() {
        let dir = tempdir().unwrap();
        let mut h = History::load(dir.path().join("hist"));
        h.push(HistoryEntry { text: "".into(), images: Vec::new(), pastes: Vec::new() });
        h.push(HistoryEntry { text: "  ".into(), images: Vec::new(), pastes: Vec::new() });
        h.push(HistoryEntry { text: "real".into(), images: Vec::new(), pastes: Vec::new() });
        assert_eq!(h.entries().len(), 1);
        assert_eq!(h.entries()[0].text, "real");
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
            pastes: vec![],
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
        let e = HistoryEntry { text: "hi".to_string(), images: vec![], pastes: vec![] };
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

    #[test]
    fn gc_removes_orphan_cache_files() {
        let dir = tempdir().unwrap();
        let cache = dir.path().join("image-cache");
        fs::create_dir(&cache).unwrap();
        fs::write(cache.join("aaaaaaaaaaaaaaaa.png"), b"a").unwrap();
        fs::write(cache.join("bbbbbbbbbbbbbbbb.png"), b"b").unwrap();
        fs::write(cache.join("cccccccccccccccc.png"), b"c").unwrap();
        let mut h = History::load_with_cache(dir.path().join("hist"), cache.clone());
        // Reference only `aaaa…` and `bbbb…`.
        h.push(HistoryEntry {
            text: "x".into(),
            images: vec![HistoryImageRef {
                hash: "aaaaaaaaaaaaaaaa".into(),
                mt: "image/png".into(),
                n: 1,
            }],
            pastes: vec![],
        });
        h.push(HistoryEntry {
            text: "y".into(),
            images: vec![HistoryImageRef {
                hash: "bbbbbbbbbbbbbbbb".into(),
                mt: "image/png".into(),
                n: 1,
            }],
            pastes: vec![],
        });
        h.save().unwrap();
        assert!(cache.join("aaaaaaaaaaaaaaaa.png").exists());
        assert!(cache.join("bbbbbbbbbbbbbbbb.png").exists());
        assert!(!cache.join("cccccccccccccccc.png").exists(), "orphan should be GC'd");
    }

    #[test]
    fn gc_keeps_unparseable_files() {
        let dir = tempdir().unwrap();
        let cache = dir.path().join("image-cache");
        fs::create_dir(&cache).unwrap();
        fs::write(cache.join("garbage.txt"), b"not a hash").unwrap();
        fs::write(cache.join("short.png"), b"too short hex prefix").unwrap();
        let h = History::load_with_cache(dir.path().join("hist"), cache.clone());
        h.save().unwrap();
        assert!(cache.join("garbage.txt").exists());
        assert!(cache.join("short.png").exists());
    }

    #[test]
    fn gc_skips_when_cache_dir_missing() {
        let dir = tempdir().unwrap();
        let cache = dir.path().join("image-cache");  // does not exist
        let mut h = History::load_with_cache(dir.path().join("hist"), cache);
        h.push(HistoryEntry { text: "x".into(), images: vec![], pastes: vec![] });
        // Must not error.
        h.save().unwrap();
    }
}
