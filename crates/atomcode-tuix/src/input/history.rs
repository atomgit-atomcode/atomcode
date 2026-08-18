// crates/atomcode-tuix/src/input/history.rs

use std::fs;
use std::io::{self, Write};
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

/// Reference to a single image cached in the project history's `images/`
/// directory. Legacy global entries continue to resolve against the old
/// `~/.atomcode/image-cache` directory, which remains read-only.
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

/// Capacity of the old global file when this type is used in standalone/back-
/// compatibility mode. New project histories use [`PROJECT_HISTORY_MAX`].
pub const HISTORY_MAX: usize = 1000;
pub const PROJECT_HISTORY_MAX: usize = 200;

pub struct History {
    path: PathBuf,
    lock_path: PathBuf,
    entries: Vec<HistoryEntry>,
    project_entries: Vec<HistoryEntry>,
    legacy_entries: Vec<HistoryEntry>,
    pending: Vec<HistoryEntry>,
    cache_dir: PathBuf,
    legacy_cache_dir: Option<PathBuf>,
    max_entries: usize,
}

impl History {
    /// Load history from `path` and configure `cache_dir` for GC. This is the
    /// standalone/back-compat constructor; production TUI startup uses
    /// [`Self::load_project`].
    pub fn load_with_cache<P: Into<PathBuf>>(path: P, cache_dir: PathBuf) -> Self {
        let path = path.into();
        let entries = read_entries(&path);
        Self {
            lock_path: sibling_lock_path(&path),
            path,
            entries: entries.clone(),
            project_entries: entries,
            legacy_entries: Vec::new(),
            pending: Vec::new(),
            cache_dir,
            legacy_cache_dir: None,
            max_entries: HISTORY_MAX,
        }
    }

    /// Load a project-scoped history. The legacy global history is read only
    /// when the project has fewer than 200 entries and only fills the unused
    /// capacity; it is never migrated or written.
    pub fn load_project(paths: crate::platform::ProjectHistoryPaths) -> Self {
        let mut project_entries = read_entries(&paths.entries);
        retain_newest(&mut project_entries, PROJECT_HISTORY_MAX);
        let legacy_entries = if project_entries.len() < PROJECT_HISTORY_MAX {
            read_entries(&paths.legacy_entries)
        } else {
            Vec::new()
        };
        let mut history = Self {
            path: paths.entries,
            lock_path: paths.lock,
            entries: Vec::new(),
            project_entries,
            legacy_entries,
            pending: Vec::new(),
            cache_dir: paths.image_cache,
            legacy_cache_dir: Some(paths.legacy_image_cache),
            max_entries: PROJECT_HISTORY_MAX,
        };
        history.rebuild_view();
        history
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

    pub fn cache_dir(&self) -> &std::path::Path {
        &self.cache_dir
    }

    pub fn legacy_cache_dir(&self) -> Option<&std::path::Path> {
        self.legacy_cache_dir.as_deref()
    }

    pub(crate) fn storage_path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn push(&mut self, entry: HistoryEntry) {
        if entry.text.trim().is_empty() {
            return;
        }
        if self.entries.last().map(|e| &e.text) == Some(&entry.text) {
            return;
        }
        self.project_entries.push(entry.clone());
        self.pending.push(entry);
        retain_newest(&mut self.project_entries, self.max_entries);
        retain_newest(&mut self.pending, self.max_entries);
        self.rebuild_view();
    }

    pub fn save(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let lock = open_lock(&self.lock_path)?;
        fs2::FileExt::lock_exclusive(&lock)?;

        let mut merged = read_entries(&self.path);
        for entry in &self.pending {
            // A concurrently saved identical entry is already represented.
            // Move it to the newest position so arrow-up order stays intuitive.
            merged.retain(|candidate| candidate != entry);
            merged.push(entry.clone());
        }
        retain_newest(&mut merged, self.max_entries);
        atomic_write_entries(&self.path, &merged)?;

        self.project_entries = merged;
        self.pending.clear();
        self.rebuild_view();
        let _ = self.gc(); // best-effort; never fails the save
        Ok(())
    }

    fn rebuild_view(&mut self) {
        retain_newest(&mut self.project_entries, self.max_entries);
        let legacy_budget = self.max_entries.saturating_sub(self.project_entries.len());
        let mut legacy = Vec::new();
        if legacy_budget > 0 {
            for entry in self.legacy_entries.iter().rev() {
                if self.project_entries.contains(entry) || legacy.contains(entry) {
                    continue;
                }
                legacy.push(entry.clone());
                if legacy.len() == legacy_budget {
                    break;
                }
            }
            legacy.reverse();
        }
        legacy.extend(self.project_entries.iter().cloned());
        self.entries = legacy;
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

fn read_entries(path: &std::path::Path) -> Vec<HistoryEntry> {
    // Each physical line is one entry. Per-line fallback chain keeps all prior
    // on-disk formats readable without rewriting them.
    fs::read_to_string(path)
        .ok()
        .map(|contents| {
            contents
                .lines()
                .filter(|line| !line.trim().is_empty())
                .map(|line| {
                    serde_json::from_str::<HistoryEntry>(line)
                        .or_else(|_| {
                            serde_json::from_str::<String>(line).map(|text| HistoryEntry {
                                text,
                                images: Vec::new(),
                                pastes: Vec::new(),
                            })
                        })
                        .unwrap_or_else(|_| HistoryEntry {
                            text: line.to_string(),
                            images: Vec::new(),
                            pastes: Vec::new(),
                        })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn retain_newest(entries: &mut Vec<HistoryEntry>, max: usize) {
    if entries.len() > max {
        entries.drain(..entries.len() - max);
    }
}

fn sibling_lock_path(path: &std::path::Path) -> PathBuf {
    let mut name = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("history"))
        .to_os_string();
    name.push(".lock");
    path.with_file_name(name)
}

fn open_lock(path: &std::path::Path) -> io::Result<fs::File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)
}

fn atomic_write_entries(path: &std::path::Path, entries: &[HistoryEntry]) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            temp.write_all(b"\n")?;
        }
        serde_json::to_writer(&mut temp, entry).map_err(io::Error::other)?;
    }
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn text_entry(text: impl Into<String>) -> HistoryEntry {
        HistoryEntry {
            text: text.into(),
            images: Vec::new(),
            pastes: Vec::new(),
        }
    }

    fn project_paths(root: &std::path::Path) -> crate::platform::ProjectHistoryPaths {
        crate::platform::ProjectHistoryPaths {
            entries: root.join("history-v2/project/entries.jsonl"),
            lock: root.join("history-v2/project/write.lock"),
            image_cache: root.join("history-v2/project/images"),
            legacy_entries: root.join("history"),
            legacy_image_cache: root.join("image-cache"),
        }
    }

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
        h.push(HistoryEntry {
            text: "one".into(),
            images: Vec::new(),
            pastes: Vec::new(),
        });
        h.push(HistoryEntry {
            text: "two".into(),
            images: Vec::new(),
            pastes: Vec::new(),
        });
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
        h.push(HistoryEntry {
            text: "1\n2\n3".into(),
            images: Vec::new(),
            pastes: Vec::new(),
        });
        h.push(HistoryEntry {
            text: "next".into(),
            images: Vec::new(),
            pastes: Vec::new(),
        });
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
        h.push(HistoryEntry {
            text: "x".into(),
            images: Vec::new(),
            pastes: Vec::new(),
        });
        h.push(HistoryEntry {
            text: "x".into(),
            images: Vec::new(),
            pastes: Vec::new(),
        });
        h.push(HistoryEntry {
            text: "y".into(),
            images: Vec::new(),
            pastes: Vec::new(),
        });
        assert_eq!(h.entries().len(), 2);
        assert_eq!(h.entries()[0].text, "x");
        assert_eq!(h.entries()[1].text, "y");
    }

    #[test]
    fn capped_at_max_entries() {
        let dir = tempdir().unwrap();
        let mut h = History::load(dir.path().join("hist"));
        for i in 0..2000 {
            h.push(HistoryEntry {
                text: format!("cmd{}", i),
                images: Vec::new(),
                pastes: Vec::new(),
            });
        }
        assert!(h.entries().len() <= HISTORY_MAX);
        assert!(!h.entries().iter().any(|e| e.text == "cmd0"));
    }

    #[test]
    fn empty_entries_ignored() {
        let dir = tempdir().unwrap();
        let mut h = History::load(dir.path().join("hist"));
        h.push(HistoryEntry {
            text: "".into(),
            images: Vec::new(),
            pastes: Vec::new(),
        });
        h.push(HistoryEntry {
            text: "  ".into(),
            images: Vec::new(),
            pastes: Vec::new(),
        });
        h.push(HistoryEntry {
            text: "real".into(),
            images: Vec::new(),
            pastes: Vec::new(),
        });
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
        let e = HistoryEntry {
            text: "hi".to_string(),
            images: vec![],
            pastes: vec![],
        };
        let j = serde_json::to_string(&e).unwrap();
        assert!(
            !j.contains("images"),
            "empty images vec must be skipped: {}",
            j
        );
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
        assert!(
            !cache.join("cccccccccccccccc.png").exists(),
            "orphan should be GC'd"
        );
    }

    #[test]
    fn gc_keeps_unparseable_files() {
        let dir = tempdir().unwrap();
        let cache = dir.path().join("image-cache");
        fs::create_dir(&cache).unwrap();
        fs::write(cache.join("garbage.txt"), b"not a hash").unwrap();
        fs::write(cache.join("short.png"), b"too short hex prefix").unwrap();
        let mut h = History::load_with_cache(dir.path().join("hist"), cache.clone());
        h.save().unwrap();
        assert!(cache.join("garbage.txt").exists());
        assert!(cache.join("short.png").exists());
    }

    #[test]
    fn gc_skips_when_cache_dir_missing() {
        let dir = tempdir().unwrap();
        let cache = dir.path().join("image-cache"); // does not exist
        let mut h = History::load_with_cache(dir.path().join("hist"), cache);
        h.push(HistoryEntry {
            text: "x".into(),
            images: vec![],
            pastes: vec![],
        });
        // Must not error.
        h.save().unwrap();
    }

    #[test]
    fn project_history_uses_legacy_only_to_fill_two_hundred_rows() {
        let dir = tempdir().unwrap();
        let paths = project_paths(dir.path());
        let legacy = (0..250)
            .map(|index| serde_json::to_string(&text_entry(format!("legacy-{index}"))).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&paths.legacy_entries, legacy).unwrap();
        fs::create_dir_all(paths.entries.parent().unwrap()).unwrap();
        let project = (0..20)
            .map(|index| serde_json::to_string(&text_entry(format!("project-{index}"))).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&paths.entries, project).unwrap();

        let history = History::load_project(paths);

        assert_eq!(history.entries().len(), PROJECT_HISTORY_MAX);
        assert_eq!(history.entries().first().unwrap().text, "legacy-70");
        assert_eq!(history.entries().last().unwrap().text, "project-19");
    }

    #[test]
    fn full_project_history_excludes_legacy_rows() {
        let dir = tempdir().unwrap();
        let paths = project_paths(dir.path());
        fs::write(&paths.legacy_entries, "secret from another project").unwrap();
        fs::create_dir_all(paths.entries.parent().unwrap()).unwrap();
        let project = (0..PROJECT_HISTORY_MAX)
            .map(|index| serde_json::to_string(&text_entry(format!("project-{index}"))).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&paths.entries, project).unwrap();

        let history = History::load_project(paths);

        assert_eq!(history.entries().len(), PROJECT_HISTORY_MAX);
        assert!(history
            .entries()
            .iter()
            .all(|entry| entry.text.starts_with("project-")));
    }

    #[test]
    fn project_save_never_modifies_legacy_file_and_caps_on_disk() {
        let dir = tempdir().unwrap();
        let paths = project_paths(dir.path());
        let legacy = "legacy must remain byte-for-byte unchanged\n";
        fs::write(&paths.legacy_entries, legacy).unwrap();
        let mut history = History::load_project(paths.clone());
        for index in 0..250 {
            history.push(text_entry(format!("project-{index}")));
        }

        history.save().unwrap();

        assert_eq!(fs::read_to_string(&paths.legacy_entries).unwrap(), legacy);
        let persisted = read_entries(&paths.entries);
        assert_eq!(persisted.len(), PROJECT_HISTORY_MAX);
        assert_eq!(persisted.first().unwrap().text, "project-50");
        assert_eq!(persisted.last().unwrap().text, "project-249");
    }

    #[test]
    fn stale_project_writers_merge_without_lost_updates() {
        let dir = tempdir().unwrap();
        let paths = project_paths(dir.path());
        let mut first = History::load_project(paths.clone());
        let mut second = History::load_project(paths.clone());
        first.push(text_entry("from first window"));
        second.push(text_entry("from second window"));

        first.save().unwrap();
        second.save().unwrap();

        let reloaded = History::load_project(paths);
        let texts = reloaded
            .entries()
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>();
        assert_eq!(texts, ["from first window", "from second window"]);
    }

    #[test]
    fn project_entry_wins_over_duplicate_legacy_entry() {
        let dir = tempdir().unwrap();
        let paths = project_paths(dir.path());
        fs::write(&paths.legacy_entries, "duplicate\nlegacy-only").unwrap();
        fs::create_dir_all(paths.entries.parent().unwrap()).unwrap();
        fs::write(
            &paths.entries,
            serde_json::to_string(&text_entry("duplicate")).unwrap(),
        )
        .unwrap();

        let history = History::load_project(paths);

        assert_eq!(
            history
                .entries()
                .iter()
                .filter(|entry| entry.text == "duplicate")
                .count(),
            1
        );
        assert_eq!(history.entries().last().unwrap().text, "duplicate");
    }
}
