// crates/atomcode-tuix/src/event_loop/file_index.rs
//
// `@`-mention infrastructure: token detection + project file index.
//
// See spec: docs/superpowers/specs/2026-05-06-at-mention-design.md

use std::cell::RefCell;
use std::path::PathBuf;

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
/// session. Built on first `filter()` call.
pub struct FileIndex {
    root: PathBuf,
    entries: RefCell<Option<Vec<Entry>>>,
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
        }
    }

    /// Returns matching entries under `scope_dir` filtered by substring
    /// `filter` (case-insensitive). Sorted by direct-child priority,
    /// dir-first, alphabetical. Capped at 30.
    pub fn filter(&self, scope_dir: &str, filter: &str) -> Vec<Entry> {
        // Lazy build on first call.
        if self.entries.borrow().is_none() {
            let walked = self.walk();
            *self.entries.borrow_mut() = Some(walked);
        }
        let entries = self.entries.borrow();
        let entries = entries.as_ref().expect("just initialised above");

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

    fn walk(&self) -> Vec<Entry> {
        let mut out = Vec::new();
        let walker = WalkBuilder::new(&self.root)
            .hidden(false) // keep dotfiles
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .ignore(true)
            .parents(true)
            .require_git(false) // apply .gitignore even without `.git/`
            .max_filesize(None)
            .build();

        for result in walker {
            let Ok(dent) = result else { continue };
            let Ok(rel) = dent.path().strip_prefix(&self.root) else {
                continue;
            };
            if rel.as_os_str().is_empty() {
                continue; // skip the root itself
            }
            let is_dir = dent.file_type().map_or(false, |t| t.is_dir());
            let mut s = rel.to_string_lossy().into_owned();

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

    /// Test-only: construct an index with hand-built entries, bypassing walk.
    #[cfg(test)]
    pub fn from_entries(root: PathBuf, entries: Vec<Entry>) -> Self {
        Self {
            root,
            entries: RefCell::new(Some(entries)),
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

    #[test]
    fn walk_includes_top_level_files_and_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(&tmp.path().join("Cargo.toml"), "[package]");
        fs::create_dir_all(tmp.path().join("crates")).unwrap();

        let idx = FileIndex::new(tmp.path().to_path_buf());
        let result = idx.filter("", "");
        let names: Vec<&str> = result.iter().map(|e| e.rel_path.as_str()).collect();

        assert!(names.contains(&"Cargo.toml"), "got: {:?}", names);
        assert!(names.contains(&"crates/"), "got: {:?}", names);
    }

    #[test]
    fn walk_keeps_dotfiles() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(&tmp.path().join(".env"), "KEY=val");

        let idx = FileIndex::new(tmp.path().to_path_buf());
        let result = idx.filter("", "");
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
        let result = idx.filter("", "");
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
        let result = idx.filter("", "");
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
        let result = idx.filter("", "");
        let names: Vec<&str> = result.iter().map(|e| e.rel_path.as_str()).collect();
        assert!(names.contains(&"normal.txt"));
        assert!(
            !names.iter().any(|n| n.contains(' ')),
            "paths with spaces should be skipped: {:?}",
            names
        );
    }
}
