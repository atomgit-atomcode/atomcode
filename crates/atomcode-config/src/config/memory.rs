use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const MAX_MEMORY_FILE_SIZE: u64 = 64 * 1024;
const DEFAULT_CHAR_LIMIT: usize = 4000;
/// Per-entry char cap for the injected prompt, so one runaway `remember` can't eat the
/// whole [`DEFAULT_CHAR_LIMIT`] budget and starve every other fact.
const MAX_MEMORY_ENTRY_CHARS: usize = 500;

pub struct MemoryStore {
    path: PathBuf,
    /// Marks the machine-local store: `append` drops a wildcard-only `.gitignore`
    /// sentinel into the store's directory on first write, so machine-specific entries
    /// never reach version control — parity with the capabilities-side store. This copy
    /// stays dependency-free (no `ignore`-crate gitignore-semantics check) to keep this
    /// foundational config leaf light; it may therefore write a harmless redundant
    /// sentinel in a repo whose root `.gitignore` already covers `.atomcode/local/`
    /// (the capabilities store skips that). The daemon only reads/forgets local memory
    /// today, so that divergence is not currently observable — the field exists so a
    /// FUTURE write through this store can never create an unprotected file.
    local: bool,
}

/// Resolve the project-scope memory file. `override_dir` = the value of
/// `ATOMCODE_PROJECT_MEMORY_DIR` (None/empty → default ".atomcode"). A relative value
/// nests under `project_root`; an absolute value is used as-is (std `Path::join`
/// semantics). `memory.md` is appended in either case.
fn project_memory_path(project_root: &Path, override_dir: Option<&str>) -> PathBuf {
    let dir = override_dir
        .filter(|s| !s.is_empty())
        .unwrap_or(".atomcode");
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

/// Drop a wildcard-only `.gitignore` next to a machine-local store so its entries never
/// reach version control — even in repos where `atomcode setup` never appended the
/// repo-root marker. Never clobbers an existing `.gitignore`. Propagates a genuine write
/// failure: for a local store, "couldn't protect" must surface rather than silently leave
/// the memory committable.
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

    pub fn global() -> Self {
        let dir = super::Config::config_dir();
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
            .filter_map(|line| {
                let trimmed = line.trim();
                if trimmed.starts_with("- ") {
                    Some(trimmed[2..].to_string())
                } else {
                    None
                }
            })
            .collect()
    }

    pub fn append(&self, content: &str) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        if self.local {
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
            "=== MEMORY ===\nThe user has asked you to remember these facts and preferences:\n",
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
    fn project_memory_path_resolves_override() {
        use std::path::Path;
        let root = Path::new("/proj");
        assert_eq!(
            super::project_memory_path(root, None),
            Path::new("/proj/.atomcode/memory.md")
        );
        assert_eq!(
            super::project_memory_path(root, Some("")),
            Path::new("/proj/.atomcode/memory.md")
        );
        assert_eq!(
            super::project_memory_path(root, Some(".myapp")),
            Path::new("/proj/.myapp/memory.md")
        );
        assert_eq!(
            super::project_memory_path(root, Some("/opt/brand/mem")),
            Path::new("/opt/brand/mem/memory.md")
        );
    }

    #[test]
    fn local_memory_path_resolves_override() {
        use std::path::Path;
        let root = Path::new("/proj");
        assert_eq!(
            super::local_memory_path(root, None),
            Path::new("/proj/.atomcode/local/memory.md")
        );
        assert_eq!(
            super::local_memory_path(root, Some("")),
            Path::new("/proj/.atomcode/local/memory.md")
        );
        assert_eq!(
            super::local_memory_path(root, Some(".myapp/local")),
            Path::new("/proj/.myapp/local/memory.md")
        );
        assert_eq!(
            super::local_memory_path(root, Some("/opt/brand/mem")),
            Path::new("/opt/brand/mem/memory.md")
        );
    }

    #[test]
    fn local_append_writes_gitignore_sentinel() {
        // Parity with the capabilities store: a local write must protect itself with a
        // wildcard `.gitignore` so machine-specific memory never reaches version control,
        // even if the store is created through the daemon's copy.
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::local(dir.path());
        store.append("machine only").unwrap();
        let sentinel = store.path().parent().unwrap().join(".gitignore");
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "*\n");
        // Idempotent: an existing (possibly user-customized) sentinel is never clobbered.
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
    fn test_merged_for_prompt_caps_entry_and_keeps_newest() {
        let dir = tempfile::tempdir().unwrap();
        // One runaway entry is capped, not injected whole.
        let path = dir.path().join("memory.md");
        fs::write(&path, format!("- {}\n", "x".repeat(5000))).unwrap();
        let store = MemoryStore::new(path);
        let empty = MemoryStore::new(dir.path().join("none.md"));
        let result = MemoryStore::merged_for_prompt(&store, &empty, &empty, "p");
        assert!(result.chars().count() < 1000, "giant entry must be capped");
        assert!(result.contains('…'));

        // Over budget → NEWEST kept, OLDEST dropped (the fix), with an omitted-count marker.
        let path2 = dir.path().join("many.md");
        let mut content = String::new();
        for i in 0..100 {
            content.push_str(&format!("- entry number {i} {}\n", "y".repeat(80)));
        }
        fs::write(&path2, content).unwrap();
        let many = MemoryStore::new(path2);
        let result = MemoryStore::merged_for_prompt(&many, &empty, &empty, "p");
        assert!(result.contains("omitted to fit the budget"));
        assert!(result.contains("entry number 99"), "newest survives");
        assert!(!result.contains("entry number 0 "), "oldest dropped");
    }

    #[test]
    fn test_merged_for_prompt_three_tiers_ordered_and_omitted_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let new_store = |name: &str, entry: Option<&str>| {
            let s = MemoryStore::new(dir.path().join(name));
            if let Some(e) = entry {
                s.append(e).unwrap();
            }
            s
        };
        let g = new_store("g.md", Some("g1"));
        let p = new_store("p.md", Some("p1"));
        let l = new_store("l.md", Some("l1"));

        // All three tiers present, in order.
        let merged = MemoryStore::merged_for_prompt(&g, &p, &l, "myproj");
        let g_pos = merged.find("[Global]\n- g1").unwrap();
        let p_pos = merged.find("[Project: myproj]\n- p1").unwrap();
        let l_pos = merged.find("[Local]\n- l1").unwrap();
        assert!(g_pos < p_pos && p_pos < l_pos);

        // Local empty → `[Local]` section omitted; global/project still present.
        let empty = new_store("empty.md", None);
        let merged2 = MemoryStore::merged_for_prompt(&g, &p, &empty, "myproj");
        assert!(merged2.contains("[Global]") && merged2.contains("[Project: myproj]"));
        assert!(!merged2.contains("[Local]"));

        // All empty → empty string.
        assert!(MemoryStore::merged_for_prompt(&empty, &empty, &empty, "myproj").is_empty());
    }
}
