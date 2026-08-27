// crates/atomcode-capabilities/src/file_index.rs
//
// `@`-mention infrastructure: token detection + project file index.
// Shared by the TUI (via a re-export shim at
// `atomcode-tuix/src/event_loop/file_index.rs`) and the daemon `/fs/search`
// endpoint, so the webui `@`-mention picker matches CLI behavior exactly.
//
// See spec: docs/superpowers/specs/2026-05-06-at-mention-design.md

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ignore::WalkBuilder;

/// Well-known agent/CLI config directories that must appear in the `@`
/// index even when `.gitignore` excludes them. These are exactly the dirs
/// atomcode already treats as skill/command sources (see the skills registry
/// and `standard_skill_dirs`), and users routinely
/// gitignore them while still wanting to `@`-reference the skills/config
/// inside. The general gitignore filter — which keeps `node_modules/`,
/// `target/`, build output, etc. out of the popup — stays intact for
/// everything else; only these names are force-indexed.
const ALWAYS_INDEX_DIRS: &[&str] = &[".claude", ".atomcode", ".agents"];

/// How long a completed full-tree index stays "fresh" before the next
/// `filter()` call kicks a background re-walk. This is what lets files
/// created mid-session (`touch new.rs`, then `@new`) show up without a
/// `/cd` or restart. The stale index keeps serving until the re-walk
/// lands, so the popup never blocks or flickers. Kept short enough to
/// feel live, long enough that rapid typing doesn't thrash the walker
/// (at most one re-walk per interval — `built_at` only advances when a
/// walk completes).
const STALE_TTL: Duration = Duration::from_secs(3);

/// Hard cap on how many entries a single index walk collects. The `@`-mention
/// popup only ever shows 30 rows and substring-searches the cache, so any real
/// project is served fine well below this. Its purpose is a CPU/memory
/// backstop: the gitignore-aware walk is otherwise unbounded, so launching
/// atomcode in a giant tree (an accidental `~` / `/`, or a repo with a huge
/// non-ignored generated dir) used to peg a core at 100% for minutes walking
/// millions of files (macOS `~/Library` alone). Stopping at this many entries
/// keeps the worst case sub-second.
const MAX_INDEX_ENTRIES: usize = 50_000;

/// Per-directory cap for the allowlist pass (`ALWAYS_INDEX_DIRS`). The
/// allowlist walks these dirs with gitignore OFF and runs before the main
/// walk, so without a bound a large `.claude` (accumulated session history,
/// plugin caches, a vendored skill repo) could monopolize `MAX_INDEX_ENTRIES`
/// and starve the user's actual source files out of the `@` popup — a
/// regression vs. the old behaviour where a gitignored `.claude` contributed
/// nothing. The popup only shows 30 rows and substring-searches, so a few
/// thousand skill/command entries per dir is already far more than useful.
const ALLOWLIST_DIR_MAX_ENTRIES: usize = 2_000;

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
    detect_at_mention_range(buf, cursor).map(|(at_pos, end)| buf[at_pos + 1..end].to_string())
}

pub fn format_at_mention_replacement(selected_path: &str) -> String {
    if selected_path.ends_with('/') {
        format!("@{}", selected_path)
    } else {
        format!("@{} ", selected_path)
    }
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

/// Convert one walked dir-entry — its path already made relative to the
/// index root — into an `Entry` and push it, applying the filters shared by
/// the allowlist pass and the main gitignore walk: skip the walk root
/// itself, skip whitespace-containing paths (they'd break
/// `detect_at_mention`'s whitespace-as-terminator rule), skip `.git/`
/// internals at any depth (nobody `@`-references git metadata), and dedup via
/// `seen` so a dir surfaced by BOTH passes is indexed once. Returns `true`
/// once `out` reaches `max_entries`, signalling the caller to stop walking.
fn push_indexed(
    out: &mut Vec<Entry>,
    seen: &mut HashSet<String>,
    rel: &Path,
    is_dir: bool,
    max_entries: usize,
) -> bool {
    if rel.as_os_str().is_empty() {
        return false; // the walk root itself
    }
    // Skip `.git` at ANY depth — a gitignore-respecting walk doesn't
    // auto-skip `.git` (the dir isn't tracked), and the allowlist pass
    // (git_ignore(false)) can descend into a nested `.claude/.git/` of a
    // vendored skill repo. Component-wise so both the root `.git/` and
    // nested ones are dropped; nobody `@`-references git metadata.
    if rel.components().any(|c| c.as_os_str() == ".git") {
        return false;
    }
    let mut s = rel_path_to_forward_slash(rel);
    if s.contains(char::is_whitespace) {
        return false;
    }
    if is_dir {
        s.push('/');
    }
    if !seen.insert(s.clone()) {
        return false; // already indexed by the other pass
    }
    let depth = rel.components().count();
    out.push(Entry {
        rel_path: s,
        is_dir,
        depth,
    });
    out.len() >= max_entries
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

/// Filter + sort a pre-walked entry set. The SINGLE matching implementation
/// shared by the async [`FileIndex::filter`] (TUI popup, over the cached index)
/// and the blocking [`FileIndex::search_blocking`] (daemon `/fs/search`), so the
/// webui `@`-mention picker and the CLI popup can never diverge.
///
/// Within `scope_dir`: an EMPTY `filter` is a pure drill-down (direct children
/// of `scope_dir` only); a non-empty `filter` matches its case-insensitive
/// substring anywhere in the path AFTER the scope, across levels — so
/// `@applystock` finds `.../ApplyStockController.java` several dirs deep.
/// Sorted direct-children-first, dirs-before-files, then alphabetical. Capped at 30.
pub fn filter_entries(entries: &[Entry], scope_dir: &str, filter: &str) -> Vec<Entry> {
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

// ---------------------------------------------------------------------------
// FileIndex
// ---------------------------------------------------------------------------

/// Lazy project file/directory index, gitignore-filtered, cached for the
/// session. Built on first `filter()` call in a background thread so the
/// event loop is never blocked by a synchronous file-system walk.
pub struct FileIndex {
    root: std::cell::RefCell<PathBuf>,
    entries: RefCell<Option<Vec<Entry>>>,
    /// When `Some`, a background build is in progress. The receiver
    /// returns the completed entries once the walk finishes.
    pending: RefCell<Option<std::sync::mpsc::Receiver<Vec<Entry>>>>,
    /// True once the initial background build has been kicked off, so we
    /// don't spawn a second thread while the first is still running.
    building: RefCell<bool>,
    /// When the currently-cached *full* index finished walking. `None`
    /// until the first full walk completes (the shallow warm-up doesn't
    /// count). Drives the TTL-based background refresh in `maybe_refresh`.
    built_at: RefCell<Option<Instant>>,
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
            root: std::cell::RefCell::new(root),
            entries: RefCell::new(None),
            pending: RefCell::new(None),
            building: RefCell::new(false),
            built_at: RefCell::new(None),
        }
    }

    /// Kick off a background build if none is running and no cached
    /// entries exist. Returns `true` when a background thread was
    /// spawned (first-ever call), `false` otherwise.
    ///
    /// **Staged warm-up**: before spawning the full-tree walk, the
    /// root's direct children are collected synchronously via a shallow
    /// (depth-1) gitignore-aware scan and stored immediately. This lets `filter()` return
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

        // Stage 1: quick synchronous depth-1 scan of root's direct children.
        // Bounded to one directory level (gitignore-aware) — effectively instant.
        *self.entries.borrow_mut() = Some(Self::scan_shallow(&self.root.borrow()));

        // Stage 2: spawn background thread for the full walk.
        let (tx, rx) = std::sync::mpsc::channel();
        *self.pending.borrow_mut() = Some(rx);
        let root = self.root.borrow().clone();
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
        // Pick up a completed walk (initial or TTL refresh) if one landed.
        self.drain_pending();
        // If the cache has aged past STALE_TTL, kick a background re-walk so
        // mid-session file creations become discoverable. Serves stale
        // entries until the fresh walk lands (drained on a later keystroke).
        self.maybe_refresh();

        // entries is guaranteed to be Some (shallow at minimum), but be defensive.
        let entries = self.entries.borrow();
        let entries = match entries.as_ref() {
            Some(e) => e,
            None => return Vec::new(),
        };
        filter_entries(entries, scope_dir, filter)
    }

    /// Synchronous full-tree search for one-shot callers that have no
    /// session-lived cache to amortize against — currently the daemon
    /// `/fs/search` endpoint that powers the webui `@`-mention picker. Walks
    /// the tree gitignore-aware (the SAME [`walk_inner`](Self::walk_inner) the
    /// async index uses) and applies the SAME [`filter_entries`] matching/sort,
    /// so webui results are byte-identical to the TUI popup.
    ///
    /// Unlike [`filter`](Self::filter), this does the FULL walk before matching
    /// (no shallow warm-up), because a per-request index has no later keystroke
    /// on which to drain a background walk — the first call must be complete.
    /// It blocks on the filesystem walk; call it from `spawn_blocking` in async
    /// contexts. `MAX_INDEX_ENTRIES` still backstops a pathological tree.
    pub fn search_blocking(root: &Path, scope_dir: &str, filter: &str) -> Vec<Entry> {
        let entries = Self::walk_inner(root.to_path_buf());
        filter_entries(&entries, scope_dir, filter)
    }

    /// Non-blocking drain of the background walk receiver. When a walk has
    /// completed, swap its result into the cache, stamp `built_at`, and clear
    /// the in-flight markers. Shared by the initial build and TTL refreshes.
    fn drain_pending(&self) {
        if self.pending.borrow().is_none() {
            return;
        }
        let mut pending = self.pending.borrow_mut();
        let Some(rx) = pending.as_mut() else { return };
        match rx.try_recv() {
            Ok(walked) => {
                *self.entries.borrow_mut() = Some(walked);
                *self.built_at.borrow_mut() = Some(Instant::now());
                *self.building.borrow_mut() = false;
                *pending = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                // Background walk still in progress — keep serving the cache.
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // Thread panicked or dropped — fall back to a synchronous walk.
                let walked = Self::walk_inner(self.root.borrow().clone());
                *self.entries.borrow_mut() = Some(walked);
                *self.built_at.borrow_mut() = Some(Instant::now());
                *self.building.borrow_mut() = false;
                *pending = None;
            }
        }
    }

    /// Spawn a background re-walk when the cached full index has aged past
    /// `STALE_TTL` and nothing is already building. The stale cache keeps
    /// serving until `drain_pending` swaps in the fresh result, so the popup
    /// never blocks. `built_at == None` means the initial full walk hasn't
    /// finished yet — `build_async` owns that, so there's nothing to refresh.
    fn maybe_refresh(&self) {
        if *self.building.borrow() {
            return; // a build/refresh is already in flight
        }
        let stale = matches!(*self.built_at.borrow(), Some(t) if t.elapsed() >= STALE_TTL);
        if !stale {
            return;
        }
        *self.building.borrow_mut() = true;
        let (tx, rx) = std::sync::mpsc::channel();
        *self.pending.borrow_mut() = Some(rx);
        let root = self.root.borrow().clone();
        std::thread::spawn(move || {
            let _ = tx.send(Self::walk_inner(root));
        });
    }

    fn walk_inner(root: PathBuf) -> Vec<Entry> {
        // A full recursive index of an entire home directory or filesystem
        // root is never a useful `@`-mention scope, and traverses millions of
        // files (macOS `~/Library`, caches, every node_modules outside a repo)
        // — pegging a core at 100% CPU for minutes. Serve only the top level
        // there; `MAX_INDEX_ENTRIES` still backstops any other huge tree.
        if Self::is_home_or_filesystem_root(&root) {
            return Self::walk_with_depth(root, Some(1), MAX_INDEX_ENTRIES);
        }
        Self::walk_with_depth(root, None, MAX_INDEX_ENTRIES)
    }

    /// True when `root` is a filesystem root (`/`, `C:\`, …) or the user's
    /// home directory — the two cases where a full recursive walk is both
    /// pathologically expensive and useless for `@`-mentions.
    fn is_home_or_filesystem_root(root: &Path) -> bool {
        // Filesystem roots have no parent component.
        if root.parent().is_none() {
            return true;
        }
        if let Some(home) = dirs::home_dir() {
            // Prefer canonical comparison (resolves symlinks / trailing
            // slashes); fall back to a literal match if either path can't be
            // canonicalised.
            return match (std::fs::canonicalize(root), std::fs::canonicalize(&home)) {
                (Ok(rc), Ok(hc)) => rc == hc,
                _ => root == home,
            };
        }
        false
    }

    /// Gitignore-respecting walk shared by the full index (`max_depth = None`)
    /// and the staged warm-up (`max_depth = Some(1)` → the root's direct
    /// children only). Routing both through one function guarantees the
    /// shallow and full views apply identical filtering (gitignore, `.git/`,
    /// whitespace), so warm-up results never include entries the full index
    /// later hides.
    fn walk_with_depth(root: PathBuf, max_depth: Option<usize>, max_entries: usize) -> Vec<Entry> {
        let mut out = Vec::new();
        // Dedup key shared across the allowlist pass and the main walk, so an
        // allowlisted dir that is NOT gitignored isn't indexed twice.
        let mut seen: HashSet<String> = HashSet::new();

        // Allowlist pass: force-index the well-known agent/CLI config dirs
        // (`.claude`, …) even when `.gitignore` excludes them. Runs FIRST, but
        // each dir is bounded by `ALLOWLIST_DIR_MAX_ENTRIES` so a large
        // `.claude` can't monopolize the budget and starve the main walk's
        // source files. The main gitignore-aware walk below fills the rest, so
        // `node_modules/` / `target/` stay filtered out as before.
        for name in ALWAYS_INDEX_DIRS {
            let dir = root.join(name);
            if !dir.is_dir() {
                continue;
            }
            // Per-dir budget, still clamped by the global cap: hitting the
            // per-dir cap just stops THIS dir and moves on; hitting the global
            // cap stops everything (checked after the loop).
            let dir_cap = out
                .len()
                .saturating_add(ALLOWLIST_DIR_MAX_ENTRIES)
                .min(max_entries);
            // Depth is measured from `root`; the dir sits at depth 1, so its
            // subtree budget is one less. `Some(0)` makes the sub-walk yield
            // only the dir itself (its `<name>/` entry), matching the main
            // walk's depth-1 "top-level dirs only" shallow view.
            let sub_depth = max_depth.map(|d| d.saturating_sub(1));
            let mut b = WalkBuilder::new(&dir);
            b.hidden(false)
                .git_ignore(false)
                .git_global(false)
                .git_exclude(false)
                .ignore(false)
                .parents(false)
                .require_git(false)
                .max_filesize(None);
            b.max_depth(sub_depth);
            for result in b.build() {
                let Ok(dent) = result else { continue };
                // Paths are under `dir`, itself under `root`, so strip_prefix
                // yields `<name>/…` — the same shape the main walk produces.
                let Ok(rel) = dent.path().strip_prefix(&root) else {
                    continue;
                };
                let is_dir = dent.file_type().is_some_and(|t| t.is_dir());
                // Stop at the per-dir cap; break (not return) so remaining
                // agent dirs and the main walk still run.
                if push_indexed(&mut out, &mut seen, rel, is_dir, dir_cap) {
                    break;
                }
            }
            // Global cap genuinely exhausted → nothing more can be added.
            if out.len() >= max_entries {
                return out;
            }
        }

        let mut builder = WalkBuilder::new(&root);
        builder
            .hidden(false)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .ignore(true)
            .parents(true)
            .require_git(false)
            .max_filesize(None);
        // None = unbounded (full subtree); Some(1) = root + its direct children.
        builder.max_depth(max_depth);
        let walker = builder.build();

        for result in walker {
            let Ok(dent) = result else { continue };
            let Ok(rel) = dent.path().strip_prefix(&root) else {
                continue;
            };
            let is_dir = dent.file_type().is_some_and(|t| t.is_dir());
            // CPU/memory backstop: stop the (otherwise unbounded) walk once
            // we've collected enough. Dropping the `walker` iterator here stops
            // further filesystem traversal, so a giant tree can't peg a core.
            if push_indexed(&mut out, &mut seen, rel, is_dir, max_entries) {
                break;
            }
        }
        out
    }

    /// Shallow warm-up scan of the root's direct children, used to show
    /// immediate results on the first `@` before the full-tree walk finishes.
    ///
    /// Delegates to `walk_with_depth(.., Some(1))` so it applies the **exact
    /// same** filtering as the full index — crucially gitignore — rather than
    /// a raw `read_dir`. This prevents gitignored top-level dirs
    /// (`node_modules/`, `target/`, …) from flashing in the popup and then
    /// vanishing once the full walk replaces the cache. Still bounded to one
    /// directory level, so it stays effectively instant.
    fn scan_shallow(root: &Path) -> Vec<Entry> {
        Self::walk_with_depth(root.to_path_buf(), Some(1), MAX_INDEX_ENTRIES)
    }

    /// Re-point the index to a new root directory and clear all cached
    /// entries / in-flight background work. The next `filter()` call will
    /// lazily trigger a fresh shallow scan + background rebuild for the
    /// new root. Called by `apply_cd` when the user switches directories.
    pub fn reset(&self, new_root: PathBuf) {
        // Cancel any in-flight background build by taking the receiver
        // and dropping it — the spawned thread's send will fail silently.
        let _ = self.pending.borrow_mut().take();
        *self.root.borrow_mut() = new_root;
        *self.entries.borrow_mut() = None;
        *self.building.borrow_mut() = false;
        *self.built_at.borrow_mut() = None;
    }

    /// Construct an index with hand-built entries, bypassing the filesystem
    /// walk. A test-support constructor: it is used by tests in DOWNSTREAM
    /// crates (the TUI's `@`-mention and file-viewer tests), which link this
    /// crate as a NON-test rlib — so it must NOT be `#[cfg(test)]` (that would
    /// strip it from the rlib and break those crates' test builds). It stays
    /// `pub` (never dead code) but has no production caller.
    pub fn from_entries(root: PathBuf, entries: Vec<Entry>) -> Self {
        Self {
            root: std::cell::RefCell::new(root),
            entries: RefCell::new(Some(entries)),
            pending: RefCell::new(None),
            building: RefCell::new(false),
            built_at: RefCell::new(None),
        }
    }

    /// Test-only: backdate the cache so the next `filter()` treats it as stale
    /// and triggers a TTL refresh — without sleeping for the real TTL.
    #[cfg(test)]
    fn mark_stale(&self) {
        *self.built_at.borrow_mut() =
            Instant::now().checked_sub(STALE_TTL + Duration::from_secs(1));
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

    #[test]
    fn directory_at_mention_replacement_keeps_token_active() {
        let replacement = format_at_mention_replacement("crates/atomcode-bridge/");

        assert_eq!(replacement, "@crates/atomcode-bridge/");
        assert_eq!(
            detect_at_mention(&replacement, replacement.len()),
            Some("crates/atomcode-bridge/".to_string())
        );
    }

    #[test]
    fn file_at_mention_replacement_finalizes_token() {
        let replacement = format_at_mention_replacement("latest.json");

        assert_eq!(replacement, "@latest.json ");
        assert_eq!(detect_at_mention(&replacement, replacement.len()), None);
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
                Entry {
                    rel_path: "Cargo.toml".into(),
                    is_dir: false,
                    depth: 1,
                },
                Entry {
                    rel_path: "crates/".into(),
                    is_dir: true,
                    depth: 1,
                },
                Entry {
                    rel_path: "docker/".into(),
                    is_dir: true,
                    depth: 1,
                },
                Entry {
                    rel_path: ".atomcode/".into(),
                    is_dir: true,
                    depth: 1,
                },
                Entry {
                    rel_path: "crates/atomcode-cli/".into(),
                    is_dir: true,
                    depth: 2,
                },
                Entry {
                    rel_path: "crates/atomcode-tuix/".into(),
                    is_dir: true,
                    depth: 2,
                },
                Entry {
                    rel_path: "crates/atomcode-tuix/Cargo.toml".into(),
                    is_dir: false,
                    depth: 3,
                },
                Entry {
                    rel_path: "docker/Dockerfile".into(),
                    is_dir: false,
                    depth: 2,
                },
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
        assert!(names.contains(&"crates/atomcode-tuix/"), "got: {:?}", names);
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

    // Feature: the well-known agent-config dirs (`.claude`, `.atomcode`,
    // `.agents`) must be `@`-indexable even when `.gitignore` excludes them,
    // so a project that gitignores `.claude` can still `@`-reference the
    // skills/commands inside. Other gitignored paths stay filtered out.
    #[test]
    fn allowlisted_agent_dirs_bypass_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(&tmp.path().join(".gitignore"), ".claude/\nnode_modules/\n");
        write_file(&tmp.path().join(".claude/skills/wiki/SKILL.md"), "x");
        write_file(&tmp.path().join("node_modules/dep/index.js"), "y");
        write_file(&tmp.path().join("kept.txt"), "z");

        let idx = FileIndex::new(tmp.path().to_path_buf());

        // Top-level view (empty filter = direct children only): the
        // gitignored `.claude/` dir is now indexed; `node_modules/` is not.
        let top: Vec<String> = filter_walk(&idx, "", "")
            .iter()
            .map(|e| e.rel_path.clone())
            .collect();
        assert!(
            top.contains(&".claude/".to_string()),
            "top-level: {:?}",
            top
        );
        assert!(
            top.contains(&"kept.txt".to_string()),
            "top-level: {:?}",
            top
        );
        assert!(
            !top.iter().any(|n| n.starts_with("node_modules")),
            "gitignored node_modules must not surface: {:?}",
            top
        );

        // Cross-level filter proves the whole gitignored `.claude` subtree is
        // in the index (reachable by `@`-drilling or filtering).
        let deep: Vec<String> = filter_walk(&idx, "", "SKILL")
            .iter()
            .map(|e| e.rel_path.clone())
            .collect();
        assert!(
            deep.iter().any(|n| n == ".claude/skills/wiki/SKILL.md"),
            "gitignored .claude contents must be indexed: {:?}",
            deep
        );

        // Other gitignored trees stay out of the index entirely.
        let nm: Vec<String> = filter_walk(&idx, "", "index.js")
            .iter()
            .map(|e| e.rel_path.clone())
            .collect();
        assert!(
            nm.is_empty(),
            "node_modules must stay filtered out: {:?}",
            nm
        );
    }

    // A `.claude` dir that is NOT gitignored must still be indexed exactly
    // once (the allowlist pass and the main walk both see it → dedup).
    #[test]
    fn allowlisted_dir_not_double_indexed_when_tracked() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(&tmp.path().join(".claude/skills/wiki/SKILL.md"), "x");

        let idx = FileIndex::new(tmp.path().to_path_buf());
        let result = filter_walk(&idx, "", "");
        let claude_hits = result.iter().filter(|e| e.rel_path == ".claude/").count();
        assert_eq!(
            claude_hits, 1,
            "`.claude/` must be indexed once, not duplicated"
        );
    }

    // Regression (review): a `.git` nested inside an allowlisted dir (e.g. a
    // vendored skill repo at `.claude/skills/repo/.git/`) must NOT leak into
    // the index — the allowlist pass walks with git_ignore(false), so the
    // per-entry `.git` filter has to match at any depth, not just the root.
    #[test]
    fn nested_git_inside_allowlisted_dir_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(&tmp.path().join(".gitignore"), ".claude/\n");
        write_file(&tmp.path().join(".claude/skills/repo/.git/HEAD"), "ref: x");
        write_file(&tmp.path().join(".claude/skills/repo/SKILL.md"), "y");

        let idx = FileIndex::new(tmp.path().to_path_buf());
        let all: Vec<String> = filter_walk(&idx, "", "git")
            .iter()
            .map(|e| e.rel_path.clone())
            .collect();
        assert!(
            !all.iter()
                .any(|n| n.contains("/.git/") || n.ends_with("/.git")),
            "nested .git must be filtered out: {:?}",
            all
        );
        // The real skill file next to it must still be indexed.
        let skills: Vec<String> = filter_walk(&idx, "", "SKILL")
            .iter()
            .map(|e| e.rel_path.clone())
            .collect();
        assert!(
            skills.iter().any(|n| n == ".claude/skills/repo/SKILL.md"),
            "skill file must still be indexed: {:?}",
            skills
        );
    }

    // Regression (review): a large `.claude` must not starve the user's
    // source files out of the `@` index. The allowlist pass is bounded per
    // dir by `ALLOWLIST_DIR_MAX_ENTRIES`, so the main walk always runs and
    // the tracked source file still shows up.
    #[test]
    fn large_allowlisted_dir_does_not_starve_source_files() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(&tmp.path().join(".gitignore"), ".claude/\n");
        // Overflow the per-dir cap with junk under `.claude`.
        for i in 0..(ALLOWLIST_DIR_MAX_ENTRIES + 50) {
            write_file(&tmp.path().join(format!(".claude/junk/f{i}.txt")), "x");
        }
        write_file(&tmp.path().join("main.rs"), "fn main() {}");

        let idx = FileIndex::new(tmp.path().to_path_buf());

        // The source file survived (main walk still ran after the cap).
        let top: Vec<String> = filter_walk(&idx, "", "")
            .iter()
            .map(|e| e.rel_path.clone())
            .collect();
        assert!(
            top.contains(&"main.rs".to_string()),
            "source file must not be starved by a large .claude: {:?}",
            top
        );
        // Inspect the RAW cache (not `filter`, which truncates to 30): the
        // allowlist contribution for `.claude/junk/` must be bounded by the
        // per-dir cap even though 2050 files exist on disk. Without the cap
        // this count would exceed ALLOWLIST_DIR_MAX_ENTRIES.
        let junk = idx
            .entries
            .borrow()
            .as_ref()
            .map(|v| {
                v.iter()
                    .filter(|e| e.rel_path.starts_with(".claude/junk/"))
                    .count()
            })
            .unwrap_or(0);
        assert!(
            junk > 0 && junk <= ALLOWLIST_DIR_MAX_ENTRIES,
            "allowlist pass must be capped per dir, indexed {junk} junk entries"
        );
    }

    // Regression: the staged warm-up's shallow scan must apply the SAME
    // gitignore filtering as the full walk. Otherwise a gitignored top-level
    // dir would flash in the popup during warm-up and then vanish once the
    // full walk replaces the cache.
    #[test]
    fn scan_shallow_respects_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(&tmp.path().join(".gitignore"), "ignored/\n");
        write_file(&tmp.path().join("ignored/").join("secret.txt"), "x");
        write_file(&tmp.path().join("visible.txt"), "v");

        let shallow = FileIndex::scan_shallow(tmp.path());
        let names: Vec<&str> = shallow.iter().map(|e| e.rel_path.as_str()).collect();
        assert!(
            names.contains(&"visible.txt"),
            "shallow scan should list non-ignored direct children: {:?}",
            names
        );
        assert!(
            !names.iter().any(|n| n.starts_with("ignored")),
            "shallow scan must honour .gitignore (no `ignored/`): {:?}",
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

    // Regression: a file created mid-session must become discoverable via the
    // TTL refresh, without a `/cd` or restart (the v1 "built once per session"
    // limitation). The cache serves stale until the staleness window elapses,
    // then a background re-walk picks up the new file.
    #[test]
    fn stale_cache_refresh_picks_up_new_files() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(&tmp.path().join("first.txt"), "x");
        let idx = FileIndex::new(tmp.path().to_path_buf());

        // Initial full walk sees first.txt only.
        let r1 = filter_walk(&idx, "", "");
        let n1: Vec<&str> = r1.iter().map(|e| e.rel_path.as_str()).collect();
        assert!(n1.contains(&"first.txt"), "got: {:?}", n1);
        assert!(!n1.contains(&"second.txt"), "got: {:?}", n1);

        // Create a file after the index was built.
        write_file(&tmp.path().join("second.txt"), "y");

        // While the cache is still fresh, the new file stays hidden.
        let r2 = idx.filter("", "");
        assert!(
            !r2.iter().any(|e| e.rel_path == "second.txt"),
            "fresh cache should not yet show the new file: {:?}",
            r2.iter().map(|e| e.rel_path.as_str()).collect::<Vec<_>>()
        );

        // Once stale, the next filter() kicks a background re-walk that
        // surfaces the new file.
        idx.mark_stale();
        let r3 = filter_walk(&idx, "", "");
        let n3: Vec<&str> = r3.iter().map(|e| e.rel_path.as_str()).collect();
        assert!(
            n3.contains(&"second.txt"),
            "TTL refresh should surface new file: {:?}",
            n3
        );
        assert!(n3.contains(&"first.txt"), "got: {:?}", n3);
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

    // ---- FileIndex.reset ----

    #[test]
    fn reset_returns_files_from_new_root() {
        let dir_a = tempfile::tempdir().unwrap();
        write_file(&dir_a.path().join("alpha.txt"), "a");
        let dir_b = tempfile::tempdir().unwrap();
        write_file(&dir_b.path().join("beta.txt"), "b");

        let idx = FileIndex::new(dir_a.path().to_path_buf());
        // Wait for full walk of dir_a.
        let result_a = filter_walk(&idx, "", "");
        let names_a: Vec<&str> = result_a.iter().map(|e| e.rel_path.as_str()).collect();
        assert!(names_a.contains(&"alpha.txt"), "got: {:?}", names_a);

        // Reset to dir_b — next filter should see dir_b's files.
        idx.reset(dir_b.path().to_path_buf());
        let result_b = filter_walk(&idx, "", "");
        let names_b: Vec<&str> = result_b.iter().map(|e| e.rel_path.as_str()).collect();
        assert!(
            names_b.contains(&"beta.txt"),
            "after reset should see dir_b files, got: {:?}",
            names_b
        );
        assert!(
            !names_b.contains(&"alpha.txt"),
            "after reset should NOT see dir_a files, got: {:?}",
            names_b
        );
    }

    #[test]
    fn reset_then_filter_triggers_fresh_background_build() {
        let dir_a = tempfile::tempdir().unwrap();
        write_file(&dir_a.path().join("only_a.txt"), "a");
        let dir_b = tempfile::tempdir().unwrap();
        write_file(&dir_b.path().join("only_b.txt"), "b");
        write_file(&dir_b.path().join("sub/").join("nested.txt"), "nested");

        let idx = FileIndex::new(dir_a.path().to_path_buf());
        // Let the background build for dir_a complete.
        let _ = filter_walk(&idx, "", "");

        // Reset to dir_b and immediately check — shallow scan gives us
        // direct children without waiting for the full walk.
        idx.reset(dir_b.path().to_path_buf());
        let shallow = idx.filter("", "");
        let shallow_names: Vec<&str> = shallow.iter().map(|e| e.rel_path.as_str()).collect();
        assert!(
            shallow_names.contains(&"only_b.txt"),
            "shallow after reset should see dir_b direct children, got: {:?}",
            shallow_names
        );

        // Wait for background walk to finish and confirm deeper files appear.
        let full = filter_walk(&idx, "", "");
        let full_names: Vec<&str> = full.iter().map(|e| e.rel_path.as_str()).collect();
        assert!(
            full_names.contains(&"sub/"),
            "full walk after reset should find sub/, got: {:?}",
            full_names
        );
        assert!(
            full_names.contains(&"only_b.txt"),
            "full walk after reset should still have only_b.txt, got: {:?}",
            full_names
        );
    }

    #[test]
    fn reset_cancels_in_flight_build() {
        let dir_a = tempfile::tempdir().unwrap();
        write_file(&dir_a.path().join("a.txt"), "a");
        let dir_b = tempfile::tempdir().unwrap();
        write_file(&dir_b.path().join("b.txt"), "b");

        let idx = FileIndex::new(dir_a.path().to_path_buf());

        // Start the background build but do NOT wait for it to complete.
        assert!(idx.build_async(), "first build_async should spawn");
        // building is true, pending is Some.

        // Mid-flight reset to dir_b.
        idx.reset(dir_b.path().to_path_buf());
        // After reset: building=false, pending=None, entries=None.
        // filter() will trigger a fresh build for dir_b.
        let result = filter_walk(&idx, "", "");
        let names: Vec<&str> = result.iter().map(|e| e.rel_path.as_str()).collect();
        assert!(
            names.contains(&"b.txt"),
            "after reset mid-flight should see dir_b files, got: {:?}",
            names
        );
        assert!(
            !names.contains(&"a.txt"),
            "after reset mid-flight should NOT see dir_a files, got: {:?}",
            names
        );
    }

    // Regression: the gitignore-aware walk is otherwise unbounded, so a giant
    // tree (an accidental run in `~` / `/`, a repo with a huge generated dir)
    // pegged a core at 100% CPU walking millions of files. The walk must stop
    // once it has collected `max_entries`.
    #[test]
    fn walk_with_depth_caps_entry_count() {
        let tmp = tempfile::tempdir().unwrap();
        for i in 0..10 {
            write_file(&tmp.path().join(format!("f{i}.txt")), "x");
        }
        let capped = FileIndex::walk_with_depth(tmp.path().to_path_buf(), None, 3);
        assert!(
            capped.len() <= 3,
            "walk must stop at the cap, got {} entries",
            capped.len()
        );
    }

    // The home directory and filesystem roots must be detected so `walk_inner`
    // serves only the shallow view instead of a full (pathological) recursive
    // walk. A normal project dir must NOT be flagged.
    #[test]
    fn detects_filesystem_root_but_not_a_project_dir() {
        assert!(
            FileIndex::is_home_or_filesystem_root(Path::new(std::path::MAIN_SEPARATOR_STR)),
            "filesystem root must be flagged"
        );
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            !FileIndex::is_home_or_filesystem_root(tmp.path()),
            "a normal project dir must not be flagged"
        );
    }

    // The daemon `/fs/search` endpoint relies on `search_blocking` returning
    // the FULL cross-level match set on a SINGLE call (no shallow warm-up, no
    // background drain) — a per-request index has no later keystroke to finish
    // the walk on. This is the property that makes the webui `@`-mention picker
    // match the CLI popup.
    #[test]
    fn search_blocking_returns_cross_level_matches_on_first_call() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(
            &tmp.path()
                .join("src/main/java/cn/ApplyStockController.java"),
            "x",
        );
        write_file(&tmp.path().join("src/convert/ApplyStockConverter.java"), "y");
        write_file(&tmp.path().join("README.md"), "z");

        // Deep files matched by a cross-level substring, from root scope, on the
        // very first (and only) call — no busy-wait like the async `filter`.
        let hits: Vec<String> = FileIndex::search_blocking(tmp.path(), "", "applystock")
            .iter()
            .map(|e| e.rel_path.clone())
            .collect();
        assert!(
            hits.iter().any(|p| p.ends_with("ApplyStockController.java")),
            "cross-level substring must find the deep controller: {hits:?}"
        );
        assert!(
            hits.iter().any(|p| p.ends_with("ApplyStockConverter.java")),
            "cross-level substring must find the deep converter: {hits:?}"
        );
        assert!(
            !hits.iter().any(|p| p == "README.md"),
            "non-matching file must be excluded: {hits:?}"
        );
    }

    // `search_blocking` shares the gitignore-aware walk, so an ignored tree
    // (e.g. `target/`) stays out of the webui `@` results just like the CLI.
    #[test]
    fn search_blocking_respects_gitignore() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(&tmp.path().join(".gitignore"), "target/\n");
        write_file(&tmp.path().join("target/ApplyStockGenerated.java"), "x");
        write_file(&tmp.path().join("src/ApplyStockReal.java"), "y");

        let hits: Vec<String> = FileIndex::search_blocking(tmp.path(), "", "applystock")
            .iter()
            .map(|e| e.rel_path.clone())
            .collect();
        assert!(
            hits.iter().any(|p| p.ends_with("ApplyStockReal.java")),
            "tracked match must appear: {hits:?}"
        );
        assert!(
            !hits.iter().any(|p| p.contains("target/")),
            "gitignored match must be excluded: {hits:?}"
        );
    }

    #[test]
    fn reset_works_on_fresh_unbuilt_index() {
        let dir = tempfile::tempdir().unwrap();
        write_file(&dir.path().join("fresh.txt"), "fresh");

        // Create index pointing to a non-existent dir, then reset.
        let idx = FileIndex::new(PathBuf::from("/nonexistent/path"));
        idx.reset(dir.path().to_path_buf());

        // filter() should lazily build for the new root.
        let result = filter_walk(&idx, "", "");
        assert!(
            result.iter().any(|e| e.rel_path == "fresh.txt"),
            "after reset on fresh index should see files: {:?}",
            result
        );
    }
}
