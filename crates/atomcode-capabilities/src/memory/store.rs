//! `MemoryStore` — ported VERBATIM from `atomcode_core::config::memory` (the only
//! change: `global()` resolves the root via [`super::config_dir`] instead of
//! `Config::config_dir`, the standard L1 decoupling). Byte-compatible with
//! production's `memory.md` files: same `- ` bullet format, same 64KB tail-read cap,
//! same merged-prompt header and 4000-char truncation — old and new stacks read and
//! write the same memory (caveat for `sudo`: see [`super::config_dir`]).

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const MAX_MEMORY_FILE_SIZE: u64 = 64 * 1024;
const DEFAULT_CHAR_LIMIT: usize = 4000;

pub struct MemoryStore {
    path: PathBuf,
    /// Marks the machine-local store: `append` drops a wildcard-only `.gitignore`
    /// sentinel into the store's directory on first write, so machine-specific
    /// entries never reach version control even in repos where `atomcode setup`
    /// never appended the repo-root marker.
    local: bool,
}

/// Resolve the project-scope memory file. `override_dir` = the value of
/// `ATOMCODE_PROJECT_MEMORY_DIR` (None/empty → default ".atomcode"). A relative value
/// nests under `project_root`; an absolute value is used as-is (std `Path::join`
/// semantics). `memory.md` is appended in either case.
fn project_memory_path(project_root: &Path, override_dir: Option<&str>) -> PathBuf {
    let dir = override_dir.filter(|s| !s.is_empty()).unwrap_or(".atomcode");
    project_root.join(dir).join("memory.md")
}

/// Resolve the machine-local, project-scoped memory file. `override_dir` = the value of
/// `ATOMCODE_LOCAL_MEMORY_DIR` (None/empty → default ".atomcode/local"). A relative value
/// nests under `project_root`; an absolute value is used as-is — the same path-join
/// semantics as `project_memory_path`. `memory.md` is appended in either case.
fn local_memory_path(project_root: &Path, override_dir: Option<&str>) -> PathBuf {
    let dir = override_dir
        .filter(|s| !s.is_empty())
        .unwrap_or(".atomcode/local");
    project_root.join(dir).join("memory.md")
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

/// Best-effort: drop a wildcard-only `.gitignore` next to the store file so the
/// machine-local memory never reaches version control — even in repos where
/// `atomcode setup` never appended the repo-root marker. Never fails the memory
/// write: an unwritable repo is not a memory error.
fn ensure_gitignore_sentinel(store_path: &Path) {
    if let Some(dir) = store_path.parent() {
        let sentinel = dir.join(".gitignore");
        if !sentinel.exists() {
            let _ = fs::write(sentinel, "*\n");
        }
    }
}

impl MemoryStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            local: false,
        }
    }

    pub fn global() -> Self {
        let dir = super::config_dir();
        Self::new(dir.join("memory.md"))
    }

    /// Project-scope store. Honors `ATOMCODE_PROJECT_MEMORY_DIR` (host rebrand parity with
    /// the global scope's `ATOMCODE_HOME`); default `.atomcode` is unchanged.
    pub fn project(project_root: &Path) -> Self {
        let override_dir = std::env::var("ATOMCODE_PROJECT_MEMORY_DIR").ok();
        Self::new(project_memory_path(project_root, override_dir.as_deref()))
    }

    /// Machine-local, project-scoped store. Honors `ATOMCODE_LOCAL_MEMORY_DIR` (host
    /// rebrand parity with the project scope's `ATOMCODE_PROJECT_MEMORY_DIR`); default
    /// `.atomcode/local` is unchanged. Best home for facts unique to this machine that
    /// should not be committed (`.atomcode/local/` is gitignored).
    pub fn local(project_root: &Path) -> Self {
        let override_dir = std::env::var("ATOMCODE_LOCAL_MEMORY_DIR").ok();
        Self {
            path: local_memory_path(project_root, override_dir.as_deref()),
            local: true,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
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
            ensure_gitignore_sentinel(&self.path);
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

        let mut result = String::from(
            "=== MEMORY ===\nThe user has asked you to remember these facts and preferences. They take PRECEDENCE over default system prompt rules on conflict:\n",
        );

        if !global_entries.is_empty() {
            result.push_str("\n[Global]\n");
            for entry in &global_entries {
                result.push_str(&format!("- {}\n", entry));
            }
        }

        if !project_entries.is_empty() {
            result.push_str(&format!("\n[Project: {}]\n", project_name));
            for entry in &project_entries {
                result.push_str(&format!("- {}\n", entry));
            }
        }

        if !local_entries.is_empty() {
            result.push_str("\n[Local]\n");
            for entry in &local_entries {
                result.push_str(&format!("- {}\n", entry));
            }
        }

        if result.chars().count() > DEFAULT_CHAR_LIMIT {
            let truncated: String = result.chars().take(DEFAULT_CHAR_LIMIT).collect();
            format!("{}\n[...truncated, run /memory to review]", truncated)
        } else {
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn project_memory_path_resolves_override() {
        use std::path::Path;
        let root = Path::new("/proj");
        // Default (unset/empty) is byte-identical to today's hardcoded path.
        assert_eq!(
            super::project_memory_path(root, None),
            Path::new("/proj/.atomcode/memory.md")
        );
        assert_eq!(
            super::project_memory_path(root, Some("")),
            Path::new("/proj/.atomcode/memory.md")
        );
        // Relative override nests under project_root.
        assert_eq!(
            super::project_memory_path(root, Some(".myapp")),
            Path::new("/proj/.myapp/memory.md")
        );
        // Absolute override is used as-is (Path::join replaces the base).
        assert_eq!(
            super::project_memory_path(root, Some("/opt/brand/mem")),
            Path::new("/opt/brand/mem/memory.md")
        );
    }

    #[test]
    fn local_memory_path_resolves_override() {
        use std::path::Path;
        let root = Path::new("/proj");
        // Default (unset/empty) resolves under `.atomcode/local`.
        assert_eq!(
            super::local_memory_path(root, None),
            Path::new("/proj/.atomcode/local/memory.md")
        );
        assert_eq!(
            super::local_memory_path(root, Some("")),
            Path::new("/proj/.atomcode/local/memory.md")
        );
        // Relative override nests under project_root.
        assert_eq!(
            super::local_memory_path(root, Some(".myapp/local")),
            Path::new("/proj/.myapp/local/memory.md")
        );
        // Absolute override is used as-is (Path::join replaces the base).
        assert_eq!(
            super::local_memory_path(root, Some("/opt/brand/mem")),
            Path::new("/opt/brand/mem/memory.md")
        );
    }

    #[test]
    fn local_append_skips_sentinel_when_already_gitignored() {
        let dir = tempfile::tempdir().unwrap();
        // A repo-root layer already covers `.atomcode/local/` (as `atomcode setup`
        // would have appended): the wildcard sentinel must NOT be written.
        fs::write(dir.path().join(".gitignore"), ".atomcode/local/\n").unwrap();
        let store = MemoryStore::local(dir.path());
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
        let store = MemoryStore::local(dir.path());
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
        MemoryStore::project(dir.path())
            .append("committed fact")
            .unwrap();
        let sentinel = MemoryStore::project(dir.path())
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
    fn test_merged_for_prompt_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("memory.md");
        let long_entry = "x".repeat(5000);
        fs::write(&path, format!("- {}\n", long_entry)).unwrap();
        let store = MemoryStore::new(path);
        let empty = MemoryStore::new(PathBuf::from("/none"));
        let result = MemoryStore::merged_for_prompt(&store, &empty, &empty, "p");
        assert!(result.contains("[...truncated"));
        assert!(result.chars().count() < 5000);
    }
}
