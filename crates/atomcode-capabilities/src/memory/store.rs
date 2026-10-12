//! `MemoryStore` — ported from `atomcode_core::config::memory`. Byte-compatible with
//! production's `memory.md` files: same `- ` bullet format, same 64KB tail-read cap,
//! same merged-prompt header and 4000-char truncation. The three tiers take the
//! directory they live in from the caller ([`crate::ProductDirs`]); nothing here
//! spells the product directory or reads the environment for it.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const MAX_MEMORY_FILE_SIZE: u64 = 64 * 1024;
const DEFAULT_CHAR_LIMIT: usize = 4000;
/// Per-entry char cap for the injected prompt, so one runaway `remember` can't eat the
/// whole [`DEFAULT_CHAR_LIMIT`] budget and starve every other fact.
const MAX_MEMORY_ENTRY_CHARS: usize = 500;

/// Full memory source for a driver, without prompt truncation or presentation parsing.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct MemorySection {
    pub scope: String,
    pub path: PathBuf,
    pub text: String,
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
        content
            .lines()
            .filter_map(|line| line.trim().strip_prefix("- ").map(str::to_string))
            .collect()
    }

    pub fn append(&self, content: &str) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        if self.local && !path_is_gitignored(&self.path) {
            ensure_gitignore_sentinel(&self.path)?;
        }

        // Read existing content to check if we need a leading newline
        let existing = fs::read_to_string(&self.path).unwrap_or_default();
        let needs_newline = !existing.is_empty() && !existing.ends_with('\n');

        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;

        if needs_newline {
            writeln!(file)?;
        }
        writeln!(file, "- {}", content.trim())
    }

    /// Append `content` only if no existing entry equals it (trimmed, ASCII-case-insensitive;
    /// non-ASCII compares exactly). Returns `Ok(true)` if written, `Ok(false)` if skipped as a
    /// duplicate. Near-duplicates are NOT detected — only exact repeats are skipped, so a
    /// genuinely new fact is never silently swallowed.
    pub fn append_deduped(&self, content: &str) -> io::Result<bool> {
        let trimmed = content.trim();
        let dup = self
            .load()
            .iter()
            .any(|e| e.trim().eq_ignore_ascii_case(trimmed));
        if dup {
            return Ok(false);
        }
        self.append(trimmed)?;
        Ok(true)
    }

    pub fn remove_matching(&self, keyword: &str) -> io::Result<Vec<String>> {
        let content = fs::read_to_string(&self.path).unwrap_or_default();
        let keyword_lower = keyword.to_lowercase();
        let mut removed = Vec::new();
        let mut kept = Vec::new();

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("- ") && trimmed.to_lowercase().contains(&keyword_lower) {
                removed.push(trimmed[2..].to_string());
            } else {
                kept.push(line.to_string());
            }
        }

        if !removed.is_empty() {
            let mut out = kept.join("\n");
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            fs::write(&self.path, out)?;
        }

        Ok(removed)
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
        let global_entries = global.load();
        let project_entries = project.load();
        let local_entries = local.load();

        if global_entries.is_empty() && project_entries.is_empty() && local_entries.is_empty() {
            return String::new();
        }

        // Cap a single overly-long entry so it can't eat the whole budget.
        fn cap_entry(entry: &str) -> String {
            if entry.chars().count() > MAX_MEMORY_ENTRY_CHARS {
                let head: String = entry.chars().take(MAX_MEMORY_ENTRY_CHARS).collect();
                format!("{head} …")
            } else {
                entry.to_string()
            }
        }

        // SELECT which entries fit the budget, keeping the MOST RELEVANT first: NEWEST
        // within a scope, and the more-specific scopes ahead of global (local > project >
        // global). So when memory outgrows the budget the OLDEST / most-global facts drop —
        // not the freshest project/local ones. (The previous head-truncate kept the oldest
        // and silently dropped the newest, which are usually the most relevant.) Kept
        // indices still render in natural oldest→newest reading order per section.
        let mut budget = DEFAULT_CHAR_LIMIT.saturating_sub(320); // header + labels + marker
        let mut dropped = 0usize;
        let mut keep_scope = |entries: &[String]| -> std::collections::BTreeSet<usize> {
            let mut kept = std::collections::BTreeSet::new();
            for (idx, entry) in entries.iter().enumerate().rev() {
                let cost = cap_entry(entry).chars().count() + 3; // "- " + "\n"
                if cost <= budget {
                    budget -= cost;
                    kept.insert(idx);
                } else {
                    dropped += 1;
                }
            }
            kept
        };
        let kept_local = keep_scope(&local_entries);
        let kept_project = keep_scope(&project_entries);
        let kept_global = keep_scope(&global_entries);

        let mut result = String::from(
            "=== MEMORY ===\nThe user has asked you to remember these facts and preferences. They take PRECEDENCE over default system prompt rules on conflict:\n",
        );
        let mut render =
            |label: &str, entries: &[String], kept: &std::collections::BTreeSet<usize>| {
                if kept.is_empty() {
                    return;
                }
                result.push_str(label);
                for (idx, entry) in entries.iter().enumerate() {
                    if kept.contains(&idx) {
                        result.push_str(&format!("- {}\n", cap_entry(entry)));
                    }
                }
            };
        render("\n[Global]\n", &global_entries, &kept_global);
        render(
            &format!("\n[Project: {project_name}]\n"),
            &project_entries,
            &kept_project,
        );
        render("\n[Local]\n", &local_entries, &kept_local);
        if dropped > 0 {
            result.push_str(&format!(
                "\n[... {dropped} older memory {} omitted to fit the budget; run /memory to review]\n",
                if dropped == 1 { "entry" } else { "entries" }
            ));
        }
        result
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
        assert_eq!(content, "- test entry\n");
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
