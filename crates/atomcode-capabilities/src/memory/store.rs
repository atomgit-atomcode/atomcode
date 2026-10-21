//! `MemoryStore` — ported from `atomcode_core::config::memory`. Byte-compatible with
//! production's `memory.md` files: same `- ` bullet format, same 64KB tail-read cap,
//! same merged-prompt header and 4000-char truncation. The three tiers take the
//! directory they live in from the caller ([`crate::ProductDirs`]); nothing here
//! spells the product directory or reads the environment for it.

use super::document::{self, Document};
use super::entry::{MemoryEntry, MemoryPatch, MemoryStatus};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const MAX_MEMORY_FILE_SIZE: u64 = 64 * 1024;
/// Full memory source for a driver, without prompt truncation or presentation parsing.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct MemorySection {
    pub scope: String,
    pub path: PathBuf,
    pub text: String,
    /// Derived diagnostics, separate from the unmodified source text. Older
    /// readers may ignore it; older command payloads omit it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<MemoryEntry>,
}

pub struct MemoryStore {
    path: PathBuf,
    /// Marks the machine-local store: `append` drops a wildcard-only `.gitignore`
    /// sentinel into the store's directory on first write, so machine-specific
    /// entries never reach version control even in repos where `atomcode setup`
    /// never appended the repo-root marker.
    local: bool,
}

/// True when `store_path` is already excluded by some `.gitignore` layer between its
/// directory and the filesystem root. Git precedence: the DEEPEST matching pattern
/// wins, so layers are evaluated shallow → deep and the last non-None match decides.
/// Directory patterns (`.atomcode/local/`) only match the directory itself, hence the
/// any-parents matcher — a file under an excluded directory counts as excluded.
fn path_is_gitignored(store_path: &Path) -> bool {
    // Collect existing layers walking up (deep → shallow), then reverse: shallow → deep.
    let mut layers = Vec::new();
    let mut dir = store_path.parent();
    while let Some(d) = dir {
        let gi = d.join(".gitignore");
        if gi.is_file() {
            layers.push((d.to_path_buf(), ignore::gitignore::Gitignore::new(&gi).0));
        }
        dir = d.parent();
    }
    let mut covered = false;
    for (root, matcher) in layers.iter().rev() {
        let Ok(rel) = store_path.strip_prefix(root) else {
            continue;
        };
        match matcher.matched_path_or_any_parents(rel, false) {
            ignore::Match::None => {}
            ignore::Match::Ignore(_) => covered = true,
            ignore::Match::Whitelist(_) => covered = false,
        }
    }
    covered
}

/// Drop a wildcard-only `.gitignore` next to the store file so the machine-local
/// memory never reaches version control — even in repos where `atomcode setup` never
/// appended the repo-root marker. Never clobbers an existing `.gitignore`. Propagates a
/// genuine write failure: for a local store, "couldn't protect" MUST surface to the
/// caller rather than silently proceeding to write committable memory (the whole point
/// of this scope is not-leaking, so leak-prevention wins over write-at-all-costs).
fn ensure_gitignore_sentinel(store_path: &Path) -> io::Result<()> {
    if let Some(dir) = store_path.parent() {
        let sentinel = dir.join(".gitignore");
        if !sentinel.exists() {
            fs::write(sentinel, "*\n")?;
        }
    }
    Ok(())
}

impl MemoryStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path, local: false }
    }

    /// The global tier: `<user tree>/memory.md`.
    pub fn global(user_dir: &Path) -> Self {
        Self::new(user_dir.join("memory.md"))
    }

    /// The project tier: `<project dir>/memory.md`, committed with the project.
    pub fn project(project_dir: &Path) -> Self {
        Self::new(project_dir.join("memory.md"))
    }

    /// The machine-local, project-scoped tier: `<project dir>/local/memory.md`.
    /// Best home for facts unique to this machine that should not be committed
    /// (`local/` is gitignored by `setup`, and [`Self::append`] drops a sentinel
    /// `.gitignore` beside it when nothing else covers it).
    pub fn local(project_dir: &Path) -> Self {
        Self {
            path: project_dir.join("local").join("memory.md"),
            local: true,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Full source for a person reviewing memory, independent of prompt budgets.
    pub fn full_text(&self) -> io::Result<String> {
        match fs::read_to_string(&self.path) {
            Ok(text) => Ok(text),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(error),
        }
    }

    pub fn for_display(stores: &[(&str, &MemoryStore)]) -> io::Result<String> {
        let mut sections = Vec::new();
        for (label, store) in stores {
            let text = store.full_text().map_err(|error| {
                io::Error::new(error.kind(), format!("{}: {error}", store.path().display()))
            })?;
            if !text.trim().is_empty() {
                sections.push(format!(
                    "## {}\n\n[{label}]\n{text}",
                    store.path().display()
                ));
            }
        }
        Ok(sections.join("\n\n"))
    }

    pub fn load(&self) -> Vec<String> {
        let content = match fs::metadata(&self.path) {
            Ok(meta) => {
                if meta.len() > MAX_MEMORY_FILE_SIZE {
                    let bytes = fs::read(&self.path).unwrap_or_default();
                    let start = bytes.len().saturating_sub(MAX_MEMORY_FILE_SIZE as usize);
                    // Scan forward to the next newline to avoid splitting UTF-8 chars
                    let safe_start = bytes[start..]
                        .iter()
                        .position(|&b| b == b'\n')
                        .map(|pos| start + pos + 1)
                        .unwrap_or(start);
                    String::from_utf8_lossy(&bytes[safe_start..]).to_string()
                } else {
                    fs::read_to_string(&self.path).unwrap_or_default()
                }
            }
            Err(_) => return Vec::new(),
        };
        Document::parse(&content)
            .map(|doc| {
                doc.entries()
                    .into_iter()
                    .filter(|entry| entry.eligible_at(chrono::Utc::now()))
                    .map(|entry| entry.content)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Full typed records. Reading legacy Markdown never writes metadata.
    pub fn entries(&self) -> io::Result<Vec<MemoryEntry>> {
        Ok(Document::parse(&self.full_text()?)?.entries())
    }

    fn transaction<R>(&self, change: impl FnOnce(&mut Document) -> io::Result<R>) -> io::Result<R> {
        if self.local && !path_is_gitignored(&self.path) {
            if let Some(parent) = self.path.parent() {
                fs::create_dir_all(parent)?;
            }
            ensure_gitignore_sentinel(&self.path)?;
        }
        document::edit(&self.path, change)
    }

    pub fn append(&self, text: &str) -> io::Result<()> {
        let entry = MemoryEntry::new(text, &MemoryPatch::default())?;
        self.transaction(|document| document.push(entry))
    }

    pub fn remember(&self, text: &str, patch: &MemoryPatch) -> io::Result<(MemoryEntry, bool)> {
        let entry = MemoryEntry::new(text, patch)?;
        self.transaction(|document| {
            if let Some(existing) = document.entries().into_iter().find(|old| {
                old.metadata.status == MemoryStatus::Active
                    && old.content.eq_ignore_ascii_case(&entry.content)
            }) {
                if patch
                    .topic
                    .as_ref()
                    .is_some_and(|_| existing.metadata.topic != entry.metadata.topic)
                    || patch
                        .source
                        .as_ref()
                        .is_some_and(|_| existing.metadata.source != entry.metadata.source)
                    || patch.pinned.is_some_and(|v| v != existing.metadata.pinned)
                    || patch
                        .expires_at
                        .as_ref()
                        .is_some_and(|v| v != &existing.metadata.expires_at)
                {
                    return Err(super::entry::invalid(format!(
                        "memory already exists as {}; use update with that ID to change metadata",
                        existing.metadata.id
                    )));
                }
                return Ok((existing, false));
            }
            document.push(entry.clone())?;
            Ok((entry, true))
        })
    }

    pub fn append_deduped(&self, text: &str) -> io::Result<bool> {
        self.remember(text, &MemoryPatch::default())
            .map(|(_, added)| added)
    }

    pub fn update(&self, id: &str, patch: &MemoryPatch) -> io::Result<MemoryEntry> {
        self.transaction(|document| {
            let matches: Vec<_> = document.lines.iter().enumerate().filter(|(_, line)| line.entry.as_ref().is_some_and(|entry| entry.metadata.id == id)).map(|(i, _)| i).collect();
            let index = match matches.as_slice() {
                [index] => *index,
                [] => return Err(io::Error::new(io::ErrorKind::NotFound, format!("memory ID not found in this scope: {id}"))),
                _ => return Err(super::entry::invalid("ambiguous legacy ID: identical unannotated entries must be disambiguated before exact updates")),
            };
            let mut entry = document.lines[index].entry.clone().unwrap();
            entry.apply(patch)?;
            // Adoption records the actual metadata write, never a fictitious
            // creation date for an old fact.
            if entry.metadata.updated_at.is_none() {
                entry.metadata.updated_at = Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
            }
            document.replace(index, entry.clone())?;
            Ok(entry)
        })
    }

    /// Explicit keyword deletion retains the old surface, matching content
    /// only: a topic/source/ID must never accidentally match and erase a fact.
    pub fn remove_matching(&self, keyword: &str) -> io::Result<Vec<String>> {
        if keyword.trim().is_empty() {
            return Err(super::entry::invalid("empty forget keyword"));
        }
        self.transaction(|document| {
            let mut removed = Vec::new();
            document.lines.retain(|line| {
                if let Some(entry) = &line.entry {
                    if entry
                        .content
                        .to_lowercase()
                        .contains(&keyword.to_lowercase())
                    {
                        removed.push(entry.content.clone());
                        return false;
                    }
                }
                true
            });
            Ok(removed)
        })
    }

    pub fn find_matching(&self, keyword: &str) -> Vec<String> {
        let keyword_lower = keyword.to_lowercase();
        self.load()
            .into_iter()
            .filter(|entry| entry.to_lowercase().contains(&keyword_lower))
            .collect()
    }

    pub fn merged_for_prompt(
        global: &MemoryStore,
        project: &MemoryStore,
        local: &MemoryStore,
        project_name: &str,
    ) -> String {
        let index = project
            .path()
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("knowledge/INDEX.md");
        super::context::MemoryContext::load(global, project, local, project_name, &index).prompt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_review_keeps_workflows_beyond_prompt_and_file_budgets() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path().join("memory.md"));
        let workflow = format!(
            "- BEGIN-WORKFLOW {} END-WORKFLOW\n  continuation\n",
            "流程".repeat(20_000)
        );
        fs::write(store.path(), &workflow).unwrap();
        let review = MemoryStore::for_display(&[("Project", &store)]).unwrap();
        assert!(review.contains(&workflow));
        assert!(review.contains(&store.path().display().to_string()));
        let empty = MemoryStore::new(tmp.path().join("missing.md"));
        assert!(empty.full_text().unwrap().is_empty());
        fs::write(store.path(), "- ".to_string() + &"x".repeat(1_000)).unwrap();
        let prompt = MemoryStore::merged_for_prompt(&empty, &store, &empty, "project");
        assert!(
            !prompt.contains(&"x".repeat(501)),
            "injection still caps long entries"
        );
        let directory = MemoryStore::new(tmp.path().to_path_buf());
        assert!(MemoryStore::for_display(&[("Project", &directory)]).is_err());
    }

    #[test]
    fn test_append_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(dir.path().join("sub").join("memory.md"));
        store.append("test entry").unwrap();
        let content = fs::read_to_string(store.path()).unwrap();
        assert!(content.starts_with("- test entry "));
        let entry = store.entries().unwrap().remove(0);
        assert_eq!(entry.content, "test entry");
        assert!(entry.metadata.created_at.is_some());
        assert!(entry.metadata.id.starts_with("m-"));
    }

    #[test]
    fn test_append_to_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.md");
        fs::write(&path, "- first\n").unwrap();
        let store = MemoryStore::new(path);
        store.append("second").unwrap();
        let entries = store.load();
        assert_eq!(entries, vec!["first", "second"]);
    }

    #[test]
    fn test_load_skips_non_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.md");
        fs::write(&path, "# Header\n\n- real entry\nnot an entry\n- another\n").unwrap();
        let store = MemoryStore::new(path);
        assert_eq!(store.load(), vec!["real entry", "another"]);
    }

    #[test]
    fn test_load_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.md");
        fs::write(&path, "").unwrap();
        let store = MemoryStore::new(path);
        assert!(store.load().is_empty());
    }

    #[test]
    fn test_load_nonexistent() {
        let store = MemoryStore::new(PathBuf::from("/nonexistent/memory.md"));
        assert!(store.load().is_empty());
    }

    #[test]
    fn test_remove_matching_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.md");
        fs::write(&path, "- Use tabs\n- use spaces\n- pnpm only\n").unwrap();
        let store = MemoryStore::new(path);
        let removed = store.remove_matching("use").unwrap();
        assert_eq!(removed, vec!["Use tabs", "use spaces"]);
        assert_eq!(store.load(), vec!["pnpm only"]);
    }

    #[test]
    fn test_remove_matching_no_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.md");
        fs::write(&path, "- keep this\n").unwrap();
        let store = MemoryStore::new(path.clone());
        let removed = store.remove_matching("nonexistent").unwrap();
        assert!(removed.is_empty());
        assert_eq!(fs::read_to_string(&path).unwrap(), "- keep this\n");
    }

    #[test]
    fn append_deduped_skips_exact_case_insensitive_duplicate() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(tmp.path().join("memory.md"));
        assert_eq!(store.append_deduped("Uses tabs").unwrap(), true);
        assert_eq!(store.append_deduped("uses tabs").unwrap(), false); // 大小写不敏感完全重复 → 跳
        assert_eq!(store.append_deduped("uses spaces").unwrap(), true); // 不同内容 → 写
        assert_eq!(store.load().len(), 2);
    }

    #[test]
    fn each_tier_lives_in_the_dir_it_is_given() {
        use std::path::Path;
        let project_dir = Path::new("/proj/.fork");
        assert_eq!(
            MemoryStore::global(Path::new("/elsewhere/tree")).path(),
            Path::new("/elsewhere/tree/memory.md")
        );
        assert_eq!(
            MemoryStore::project(project_dir).path(),
            Path::new("/proj/.fork/memory.md")
        );
        assert_eq!(
            MemoryStore::local(project_dir).path(),
            Path::new("/proj/.fork/local/memory.md")
        );
    }

    #[test]
    fn local_append_skips_sentinel_when_already_gitignored() {
        let dir = tempfile::tempdir().unwrap();
        // A repo-root layer already covers `.atomcode/local/` (as `atomcode setup`
        // would have appended): the wildcard sentinel must NOT be written.
        fs::write(dir.path().join(".gitignore"), ".ours/local/\n").unwrap();
        let store = MemoryStore::local(&dir.path().join(".ours"));
        store.append("machine only").unwrap();
        let sentinel = store.path().parent().unwrap().join(".gitignore");
        assert!(
            !sentinel.exists(),
            "covered path must not grow a redundant sentinel"
        );
    }

    #[test]
    fn local_append_writes_gitignore_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::local(&dir.path().join(".ours"));
        store.append("machine only").unwrap();
        // Sentinel lands NEXT TO the store file (env-neutral: derive from the
        // resolved path, not from a hardcoded `.atomcode/local` guess).
        let sentinel = store.path().parent().unwrap().join(".gitignore");
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "*\n");
        // Idempotent: an existing sentinel (user-customized or prior run) is never
        // clobbered by a later append.
        fs::write(&sentinel, "# custom\n").unwrap();
        store.append("more").unwrap();
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "# custom\n");
    }

    #[test]
    fn non_local_append_writes_no_sentinel() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join(".ours");
        MemoryStore::project(&project_dir)
            .append("committed fact")
            .unwrap();
        let sentinel = MemoryStore::project(&project_dir)
            .path()
            .parent()
            .unwrap()
            .join(".gitignore");
        assert!(
            !sentinel.exists(),
            "project/global stores must not grow a gitignore sentinel"
        );
    }

    #[test]
    fn ensure_gitignore_sentinel_propagates_write_failure() {
        // A failed sentinel write must NOT be swallowed: for a machine-local store,
        // "couldn't protect" must surface rather than silently leave memory writable.
        // Make the store's parent a FILE so writing `<parent>/.gitignore` fails with
        // ENOTDIR — deterministic across OS/user/permissions (root included).
        let dir = tempfile::tempdir().unwrap();
        let parent_as_file = dir.path().join("local");
        fs::write(&parent_as_file, b"x").unwrap();
        let store_path = parent_as_file.join("memory.md");
        assert!(
            super::ensure_gitignore_sentinel(&store_path).is_err(),
            "a failed sentinel write must be propagated, not swallowed"
        );
    }

    #[test]
    fn merged_caps_a_single_giant_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.md");
        fs::write(&path, format!("- {}\n", "x".repeat(5000))).unwrap();
        let store = MemoryStore::new(path);
        let empty = MemoryStore::new(dir.path().join("none.md"));
        let result = MemoryStore::merged_for_prompt(&store, &empty, &empty, "p");
        // One runaway entry is capped, not injected whole.
        assert!(
            result.chars().count() < 1000,
            "giant entry must be capped: {} chars",
            result.chars().count()
        );
        assert!(result.contains('…'), "a capped entry carries the ellipsis");
    }

    #[test]
    fn merged_keeps_newest_and_drops_oldest_over_budget() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.md");
        // ~100 entries of ~100 chars ≈ 10K chars, well over the 4000-char budget.
        let mut content = String::new();
        for i in 0..100 {
            content.push_str(&format!("- entry number {i} {}\n", "y".repeat(80)));
        }
        fs::write(&path, content).unwrap();
        let store = MemoryStore::new(path);
        let empty = MemoryStore::new(dir.path().join("none.md"));
        let result = MemoryStore::merged_for_prompt(&store, &empty, &empty, "p");
        // Over budget → the omitted-count marker appears.
        assert!(
            result.contains("omitted to fit the budget"),
            "dropped entries must be reported: {result:.120}"
        );
        // The FIX: the NEWEST entry survives and the OLDEST is dropped (was reversed).
        assert!(
            result.contains("entry number 99"),
            "newest entry must survive"
        );
        assert!(
            !result.contains("entry number 0 "),
            "oldest entry must be the one dropped"
        );
    }

    #[test]
    fn merged_prioritizes_local_over_global_when_tight() {
        let dir = tempfile::tempdir().unwrap();
        let g = MemoryStore::new(dir.path().join("g.md"));
        let l = MemoryStore::new(dir.path().join("l.md"));
        let empty = MemoryStore::new(dir.path().join("none.md"));
        // Global overflows the budget on its own; a lone local entry must still survive.
        let mut gc = String::new();
        for i in 0..100 {
            gc.push_str(&format!("- global {i} {}\n", "z".repeat(80)));
        }
        fs::write(dir.path().join("g.md"), gc).unwrap();
        fs::write(dir.path().join("l.md"), "- LOCAL_KEEPER\n").unwrap();
        let result = MemoryStore::merged_for_prompt(&g, &empty, &l, "proj");
        assert!(
            result.contains("LOCAL_KEEPER"),
            "the local entry must survive even when global overflows: {result:.200}"
        );
        assert!(
            result.contains("omitted"),
            "global overflow must be reported"
        );
    }
}
