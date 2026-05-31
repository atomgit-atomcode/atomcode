// crates/atomcode-tuix/src/event_loop/file_index.rs
//
// `@`-mention infrastructure: token detection + project file index.
//
// See spec: docs/superpowers/specs/2026-05-06-at-mention-design.md

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use ignore::WalkBuilder;

// ---------------------------------------------------------------------------
// Token detection
// ---------------------------------------------------------------------------

/// Detects whether the cursor is currently inside an `@`-mention token.
/// Returns the token text after `@` (excluding the leading `@`), or `None`
/// when not in mention state.
///
/// Rules (ordered):
/// 1. Find rightmost `@` in `buf[..cursor]`. None → `None`.
/// 2. The character before `@` must be whitespace or BOF. Otherwise `None`
///    (avoids `email@host.com`-style false positives).
/// 3. No whitespace inside `@..cursor`. If any, the mention has been
///    finalized → `None`.
/// 4. Token = characters from `@`'s next byte to the next whitespace
///    (or EOF), including bytes after cursor.
pub fn detect_at_mention(buf: &str, cursor: usize) -> Option<String> {
    detect_at_mention_range(buf, cursor)
        .map(|(at_pos, end)| buf[at_pos + 1..end].to_string())
}

/// Companion to `detect_at_mention`. Returns the byte range
/// `(at_pos_inclusive, token_end_exclusive)` for buffer-slice operations.
/// `at_pos` points at the `@` character; `end` is the byte after the last
/// non-whitespace character of the token.
pub fn detect_at_mention_range(buf: &str, cursor: usize) -> Option<(usize, usize)> {
    let prefix = buf.get(..cursor)?;

    // Rule 1: find rightmost `@` in prefix.
    let at_pos = prefix.rfind('@')?;

    // Rule 2: char before `@` must be whitespace or BOF.
    if at_pos > 0 {
        let before = prefix[..at_pos].chars().next_back()?;
        if !before.is_whitespace() {
            return None;
        }
    }

    // Rule 3: no whitespace between `@` and cursor.
    let token_to_cursor = &prefix[at_pos + 1..];
    if token_to_cursor.chars().any(char::is_whitespace) {
        return None;
    }

    // Rule 4: extend token through bytes after cursor up to next whitespace.
    let after_at = &buf[at_pos + 1..];
    let token_len = after_at
        .char_indices()
        .find(|(_, c)| c.is_whitespace())
        .map(|(i, _)| i)
        .unwrap_or(after_at.len());

    Some((at_pos, at_pos + 1 + token_len))
}

/// Convert a relative `Path` produced by `WalkBuilder` into a string that
/// always uses `/` as the separator. Required because `filter()` matches
/// `scope_dir` (always built from user input on `/`) against `e.rel_path`
/// via `starts_with` — on Windows, `Path::to_string_lossy()` returns
/// native `\` separators and breaks every drill-down past the root level.
fn rel_path_to_forward_slash(rel: &std::path::Path) -> String {
    let s = rel.to_string_lossy().into_owned();
    if std::path::MAIN_SEPARATOR == '/' {
        s
    } else {
        s.replace(std::path::MAIN_SEPARATOR, "/")
    }
}

/// Splits a mention token (without leading `@`) into `(scope_dir, filter)`
/// at the rightmost `/`.
///
/// | input             | scope_dir       | filter |
/// |-------------------|-----------------|--------|
/// | `""`              | `""`            | `""`   |
/// | `"cra"`           | `""`            | `"cra"`|
/// | `"crates/"`       | `"crates/"`     | `""`   |
/// | `"crates/atom"`   | `"crates/"`     | `"atom"`|
pub fn split_token(token: &str) -> (String, String) {
    match token.rfind('/') {
        Some(i) => (token[..=i].to_string(), token[i + 1..].to_string()),
        None => (String::new(), token.to_string()),
    }
}

// ---------------------------------------------------------------------------
// FileIndex
// ---------------------------------------------------------------------------

/// Lazy project file/directory index, gitignore-filtered, cached for the
/// session. Built on first `filter()` call in a background thread so the
/// event loop is never blocked by a synchronous file-system walk.
pub struct FileIndex {
    root: PathBuf,
    entries: RefCell<Option<Vec<Entry>>>,
    /// When `Some`, a background build is in progress. The receiver
    /// returns the completed entries once the walk finishes.
    pending: RefCell<Option<std::sync::mpsc::Receiver<Vec<Entry>>>>,
    /// True once the initial background build has been kicked off, so we
    /// don't spawn a second thread while the first is still running.
    building: RefCell<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Path relative to `root`. Directories end with `/`.
    pub rel_path: String,
    pub is_dir: bool,
    /// Nesting depth (root == 0; root/x == 1; root/x/y == 2).
    pub depth: usize,
}

impl FileIndex {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            entries: RefCell::new(None),
            pending: RefCell::new(None),
            building: RefCell::new(false),
        }
    }

    /// Kick off a background build if none is running and no cached
    /// entries exist. Returns `true` when a background thread was
    /// spawned (first-ever call), `false` otherwise.
    ///
    /// **Staged warm-up**: before spawning the full-tree walk, the
    /// root's direct children are collected synchronously via a quick
    /// `read_dir` and stored immediately. This lets `filter()` return
    /// results on the very first `@` keystroke (showing top-level
    /// files/dirs) without waiting for the full walk to finish.
    /// The background thread replaces the cache with the complete
    /// index when it completes.
    pub fn build_async(&self) -> bool {
        if self.entries.borrow().is_some() {
            return false; // already cached (shallow or full)
        }
        if *self.building.borrow() {
            return false; // already building
        }
        *self.building.borrow_mut() = true;

        // Stage 1: quick synchronous scan of root's direct children.
        // This is a single `read_dir` syscall — effectively instant.
        *self.entries.borrow_mut() = Some(Self::scan_shallow(&self.root));

        // Stage 2: spawn background thread for the full walk.
        let (tx, rx) = std::sync::mpsc::channel();
        *self.pending.borrow_mut() = Some(rx);
        let root = self.root.clone();
        std::thread::spawn(move || {
            let walked = Self::walk_inner(root);
            let _ = tx.send(walked);
        });
        true
    }

    /// Returns matching entries under `scope_dir` filtered by substring
    /// `filter` (case-insensitive). Sorted by direct-child priority,
    /// dir-first, alphabetical. Capped at 30.
    ///
    /// If the index has not been built yet, attempts a non-blocking
    /// drain of the background walk. Returns whatever is available —
    /// empty `Vec` when the thread is still working is fine; the
    /// caller will re-invoke `filter` on the next keystroke and get
    /// the fresh results then.
    pub fn filter(&self, scope_dir: &str, filter: &str) -> Vec<Entry> {
        // Kick off background build on first call if not already building/cached.
        self.build_async();

        // Drain background walk result if available.
        // Note: entries is never None after build_async (it has a shallow
        // snapshot), so we check pending instead.
        if self.pending.borrow().is_some() {
            let mut pending = self.pending.borrow_mut();
            if let Some(rx) = pending.as_mut() {
                match rx.try_recv() {
                    Ok(walked) => {
                        *self.entries.borrow_mut() = Some(walked);
                        *self.building.borrow_mut() = false;
                        *pending = None;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        // Background walk still in progress — use shallow entries.
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        // Thread panicked or dropped — fall back to synchronous walk.
                        let walked = Self::walk_inner(self.root.clone());
                        *self.entries.borrow_mut() = Some(walked);
                        *self.building.borrow_mut() = false;
                        *pending = None;
                    }
                }
            }
        }
        // entries is guaranteed to be Some (shallow at minimum), but be defensive.
        let entries = self.entries.borrow();
        let entries = match entries.as_ref() {
            Some(e) => e,
            None => return Vec::new(),
        };

        let filter_lower = filter.to_lowercase();
        let scope_depth = if scope_dir.is_empty() {
            0
        } else {
            scope_dir.matches('/').count()
        };

        let mut matched: Vec<Entry> = entries
            .iter()
            .filter(|e| e.rel_path.starts_with(scope_dir))
            .filter(|e| e.rel_path != scope_dir)
            .filter(|e| {
                if filter_lower.is_empty() {
                    // Empty filter = pure drill-down view: only direct
                    // children of `scope_dir`. Cross-level matching kicks
                    // in only once the user starts typing a filter.
                    return e.depth == scope_depth + 1;
                }
                let after_scope = &e.rel_path[scope_dir.len()..];
                after_scope.to_lowercase().contains(&filter_lower)
            })
            .cloned()
            .collect();

        matched.sort_by(|a, b| {
            // Direct children of scope_dir first.
            let a_direct = a.depth == scope_depth + 1;
            let b_direct = b.depth == scope_depth + 1;
            b_direct
                .cmp(&a_direct)
                // Then dirs before files within same level.
                .then_with(|| b.is_dir.cmp(&a.is_dir))
                // Then alpha.
                .then_with(|| a.rel_path.cmp(&b.rel_path))
        });

        matched.truncate(30);
        matched
    }

    fn walk_inner(root: PathBuf) -> Vec<Entry> {
        let mut out = Vec::new();
        let walker = WalkBuilder::new(&root)
            .hidden(false)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .ignore(true)
            .parents(true)
            .require_git(false)
            .max_filesize(None)
            .build();

        for result in walker {
            let Ok(dent) = result else { continue };
            let Ok(rel) = dent.path().strip_prefix(&root) else {
                continue;
            };
            if rel.as_os_str().is_empty() {
                continue; // skip the root itself
            }
            let is_dir = dent.file_type().map_or(false, |t| t.is_dir());
            let mut s = rel_path_to_forward_slash(rel);

            // v1 limitation: skip paths containing whitespace (would break
            // detect_at_mention's whitespace-as-terminator rule).
            if s.contains(char::is_whitespace) {
                continue;
            }

            // Hide `.git/` and its contents — gitignore-respecting walk
            // doesn't auto-skip it (the directory itself isn't tracked).
            // The user almost never wants to `@`-reference internal git
            // metadata; surfacing it just clutters the popup.
            if s == ".git" || s == ".git/" || s.starts_with(".git/") {
                continue;
            }

            if is_dir {
                s.push('/');
            }
            let depth = rel.components().count();
            out.push(Entry {
                rel_path: s,
                is_dir,
                depth,
            });
        }
        out
    }

    /// Fast synchronous scan of a single directory's direct children
    /// (no recursion). Used by the staged warm-up to show immediate
    /// results before the full-tree `walk_inner` completes.
    ///
    /// Behaviour is deliberately kept in sync with `walk_inner`:
    /// - Hidden files/dirs are **not** skipped (walk_inner uses `.hidden(false)`)
    /// - Only `.git/` is explicitly excluded
    /// - Paths containing whitespace are excluded
    fn scan_shallow(root: &Path) -> Vec<Entry> {
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(root) else {
            return out;
        };
        for result in rd {
            let Ok(dent) = result else {
                continue;
            };
            let Ok(name) = dent.file_name().into_string() else {
                continue;
            };
            // Skip `.git/` specifically (same as walk_inner).
            if name == ".git" {
                continue;
            }
            // v1 limitation: skip paths containing whitespace.
            if name.contains(char::is_whitespace) {
                continue;
            }
            let is_dir = dent.file_type().map_or(false, |t| t.is_dir());
            let mut rel = name;
            if is_dir {
                rel.push('/');
            }
            out.push(Entry {
                rel_path: rel,
                is_dir,
                depth: 1,
            });
        }
        out
    }

    /// Test-only: construct an index with hand-built entries, bypassing walk.
    #[cfg(test)]
    pub fn from_entries(root: PathBuf, entries: Vec<Entry>) -> Self {
        Self {
            root,
            entries: RefCell::new(Some(entries)),
            pending: RefCell::new(None),
            building: RefCell::new(false),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    // ---- rel_path_to_forward_slash ----

    #[test]
    fn rel_path_to_forward_slash_normalizes_native_separators() {
        // Build a multi-component path the way `WalkBuilder` produces them
        // — via `PathBuf::collect`, which inserts the platform's native
        // separator. Output must always be forward-slashed regardless of
        // platform; on Unix this is identity, on Windows it normalizes
        // backslashes so `filter()`'s `/`-based scope_dir prefix matching
        // succeeds past the top level (regression: drilldown into any
        // second-level dir like `@docs/` returned an empty popup on
        // Windows because entries were stored as `docs\foo.md`).
        let p: std::path::PathBuf = ["docs", "sub", "file.md"].iter().collect();
        assert_eq!(rel_path_to_forward_slash(&p), "docs/sub/file.md");
    }

    // ---- detect_at_mention ----

    #[test]
    fn detect_no_at_returns_none() {
        assert_eq!(detect_at_mention("hello world", 5), None);
    }

    #[test]
    fn detect_bare_at_returns_empty_token() {
        assert_eq!(detect_at_mention("@", 1), Some(String::new()));
    }

    #[test]
    fn detect_at_with_filter() {
        assert_eq!(detect_at_mention("@cra", 4), Some("cra".to_string()));
    }

    #[test]
    fn detect_at_in_middle_of_prompt() {
        let buf = "summarize @cra";
        assert_eq!(detect_at_mention(buf, buf.len()), Some("cra".to_string()));
    }

    #[test]
    fn detect_email_at_does_not_trigger() {
        let buf = "email@host.com";
        assert_eq!(detect_at_mention(buf, buf.len()), None);
    }

    #[test]
    fn detect_after_trailing_space_returns_none() {
        let buf = "@crates/ ";
        assert_eq!(detect_at_mention(buf, buf.len()), None);
    }

    #[test]
    fn detect_with_cursor_in_middle_of_token() {
        // Buffer: "@crates/" — cursor at position 4 (just after "@cra").
        // Token still extends through "@crates/".
        let buf = "@crates/";
        assert_eq!(detect_at_mention(buf, 4), Some("crates/".to_string()));
    }

    #[test]
    fn detect_with_two_mentions_picks_active_one() {
        // Buffer: "@cra @oth" — cursor at end → second mention.
        let buf = "@cra @oth";
        assert_eq!(detect_at_mention(buf, buf.len()), Some("oth".to_string()));
    }

    #[test]
    fn detect_at_after_newline_triggers() {
        let buf = "first line\n@cra";
        assert_eq!(detect_at_mention(buf, buf.len()), Some("cra".to_string()));
    }

    #[test]
    fn detect_at_at_buffer_start_with_subsequent_at_picks_correctly() {
        // Cursor before second @ → first mention is active.
        let buf = "@cra @oth";
        assert_eq!(detect_at_mention(buf, 4), Some("cra".to_string()));
    }

    // ---- detect_at_mention_range ----

    #[test]
    fn detect_range_returns_byte_positions() {
        let buf = "summarize @crates/foo";
        let range = detect_at_mention_range(buf, buf.len()).expect("Some");
        assert_eq!(&buf[range.0..range.1], "@crates/foo");
    }

    // ---- split_token ----

    #[test]
    fn split_token_root() {
        assert_eq!(split_token(""), (String::new(), String::new()));
    }

    #[test]
    fn split_token_dir_only() {
        assert_eq!(
            split_token("crates/"),
            ("crates/".to_string(), String::new())
        );
    }

    #[test]
    fn split_token_dir_with_filter() {
        assert_eq!(
            split_token("crates/atom"),
            ("crates/".to_string(), "atom".to_string())
        );
    }

    #[test]
    fn split_token_no_slash_is_filter_only() {
        assert_eq!(split_token("cra"), (String::new(), "cra".to_string()));
    }

    // ---- FileIndex.filter (mock, no walk) ----

    fn mock_index() -> FileIndex {
        FileIndex::from_entries(
            PathBuf::from("/tmp"),
            vec![
                Entry { rel_path: "Cargo.toml".into(), is_dir: false, depth: 1 },
                Entry { rel_path: "crates/".into(), is_dir: true, depth: 1 },
                Entry { rel_path: "docker/".into(), is_dir: true, depth: 1 },
                Entry { rel_path: ".atomcode/".into(), is_dir: true, depth: 1 },
                Entry { rel_path: "crates/atomcode-cli/".into(), is_dir: true, depth: 2 },
                Entry { rel_path: "crates/atomcode-tuix/".into(), is_dir: true, depth: 2 },
                Entry { rel_path: "crates/atomcode-tuix/Cargo.toml".into(), is_dir: false, depth: 3 },
                Entry { rel_path: "docker/Dockerfile".into(), is_dir: false, depth: 2 },
            ],
        )
    }

    #[test]
    fn filter_empty_returns_only_direct_children() {
        let idx = mock_index();
        let result = idx.filter("", "");
        let names: Vec<&str> = result.iter().map(|e| e.rel_path.as_str()).collect();
        // Direct children are present.
        assert!(names.contains(&"crates/"));
        assert!(names.contains(&"Cargo.toml"));
        // First entry should be a directory.
        assert!(result[0].is_dir, "expected dir first: {:?}", result[0]);
        // Descendants are NOT present without an explicit filter or
        // drill-down — empty filter means "show this level only".
        assert!(
            !names.contains(&"crates/atomcode-tuix/"),
            "depth-2 should be hidden under empty filter: {:?}",
            names
        );
        assert!(
            !names.contains(&"crates/atomcode-tuix/Cargo.toml"),
            "depth-3 should be hidden: {:?}",
            names
        );
    }

    #[test]
    fn filter_substring_matches_across_levels() {
        let idx = mock_index();
        let result = idx.filter("", "tuix");
        let names: Vec<&str> = result.iter().map(|e| e.rel_path.as_str()).collect();
        // Should include the depth-2 dir even though we filtered from root.
        assert!(
            names.contains(&"crates/atomcode-tuix/"),
            "got: {:?}",
            names
        );
    }

    #[test]
    fn filter_within_scope_excludes_outside() {
        let idx = mock_index();
        let result = idx.filter("crates/", "");
        let names: Vec<&str> = result.iter().map(|e| e.rel_path.as_str()).collect();
        assert!(names.iter().any(|n| n.starts_with("crates/")));
        assert!(
            !names.iter().any(|n| n.starts_with("docker/")),
            "should not contain docker/: {:?}",
            names
        );
    }

    #[test]
    fn filter_sorts_direct_children_first() {
        let idx = mock_index();
        let result = idx.filter("crates/", "");
        // Direct children of crates/ (depth 2) should come before deeper.
        let first = &result[0];
        assert_eq!(first.depth, 2, "first should be depth-2: {:?}", first);
    }

    // ---- FileIndex.walk (real tempdir) ----

    fn write_file(path: &std::path::Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    /// Busy-wait helper for tests: keeps calling `filter()` until the
    /// background walk completes and returns the full-tree entries.
    /// Times out after 5 seconds so a stuck test fails fast.
    fn filter_walk(idx: &FileIndex, scope_dir: &str, filter: &str) -> Vec<Entry> {
        for _ in 0..500 {
            let result = idx.filter(scope_dir, filter);
            // Background thread is done when `pending` is consumed (set to None).
            if idx.pending.borrow().is_none() {
                return result;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!(
            "filter_walk timed out after 5s (scope_dir={:?}, filter={:?})",
            scope_dir, filter
        );
    }

    #[test]
    fn walk_includes_top_level_files_and_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(&tmp.path().join("Cargo.toml"), "[package]");
        fs::create_dir_all(tmp.path().join("crates")).unwrap();

        let idx = FileIndex::new(tmp.path().to_path_buf());
        let result = filter_walk(&idx, "", "");
        let names: Vec<&str> = result.iter().map(|e| e.rel_path.as_str()).collect();

        assert!(names.contains(&"Cargo.toml"), "got: {:?}", names);
        assert!(names.contains(&"crates/"), "got: {:?}", names);
    }

    #[test]
    fn walk_keeps_dotfiles() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(&tmp.path().join(".env"), "KEY=val");

        let idx = FileIndex::new(tmp.path().to_path_buf());
        let result = filter_walk(&idx, "", "");
        let names: Vec<&str> = result.iter().map(|e| e.rel_path.as_str()).collect();
        assert!(names.contains(&".env"), "got: {:?}", names);
    }

    #[test]
    fn walk_respects_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(&tmp.path().join(".gitignore"), "ignored.txt\n");
        write_file(&tmp.path().join("ignored.txt"), "x");
        write_file(&tmp.path().join("kept.txt"), "y");

        let idx = FileIndex::new(tmp.path().to_path_buf());
        let result = filter_walk(&idx, "", "");
        let names: Vec<&str> = result.iter().map(|e| e.rel_path.as_str()).collect();
        assert!(names.contains(&"kept.txt"));
        assert!(
            !names.contains(&"ignored.txt"),
            "gitignored file should be skipped: {:?}",
            names
        );
    }

    #[test]
    fn walk_skips_dot_git_directory() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join(".git/objects")).unwrap();
        write_file(&tmp.path().join(".git/HEAD"), "ref: refs/heads/main");
        write_file(&tmp.path().join("Cargo.toml"), "[package]");

        let idx = FileIndex::new(tmp.path().to_path_buf());
        let result = filter_walk(&idx, "", "");
        let names: Vec<&str> = result.iter().map(|e| e.rel_path.as_str()).collect();
        assert!(names.contains(&"Cargo.toml"));
        assert!(
            !names.iter().any(|n| n.starts_with(".git")),
            "should skip .git/: got {:?}",
            names
        );
    }

    #[test]
    fn walk_skips_paths_with_whitespace() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(&tmp.path().join("normal.txt"), "x");
        write_file(&tmp.path().join("with space.txt"), "y");

        let idx = FileIndex::new(tmp.path().to_path_buf());
        let result = filter_walk(&idx, "", "");
        let names: Vec<&str> = result.iter().map(|e| e.rel_path.as_str()).collect();
        assert!(names.contains(&"normal.txt"));
        assert!(
            !names.iter().any(|n| n.contains(' ')),
            "paths with spaces should be skipped: {:?}",
            names
        );
    }

    #[test]
    fn build_async_only_spawns_once() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(&tmp.path().join("a.txt"), "x");
        let idx = FileIndex::new(tmp.path().to_path_buf());

        // First call spawns the thread.
        assert!(idx.build_async());
        // Second call should be a no-op.
        assert!(!idx.build_async());
        // Third call still no-op.
        assert!(!idx.build_async());

        // Walking is still in progress (or may have finished on a fast FS).
        // Either way, entries are available after the background thread
        // completes.
        let result = filter_walk(&idx, "", "");
        assert!(!result.is_empty());
    }

    #[test]
    fn filter_returns_results_immediately_via_shallow_scan() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(&tmp.path().join("hello.txt"), "x");
        let idx = FileIndex::new(tmp.path().to_path_buf());

        // First call triggers staged warm-up: shallow scan (read_dir)
        // returns direct children instantly, background thread fills in
        // the full tree for substring search.
        let first = idx.filter("", "");
        assert!(
            !first.is_empty(),
            "shallow scan should return immediate results on first call"
        );
        assert_eq!(first[0].rel_path, "hello.txt");

        // Second call after full walk completes should still have results.
        let second = filter_walk(&idx, "", "");
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].rel_path, "hello.txt");
    }
}
