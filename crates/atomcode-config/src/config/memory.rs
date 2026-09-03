use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

const MAX_MEMORY_FILE_SIZE: u64 = 64 * 1024;
const DEFAULT_CHAR_LIMIT: usize = 4000;

pub struct MemoryStore {
    path: PathBuf,
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

impl MemoryStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
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
        Self::new(local_memory_path(project_root, override_dir.as_deref()))
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

        let mut result = String::from(
            "=== MEMORY ===\nThe user has asked you to remember these facts and preferences:\n",
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
        assert!(
            MemoryStore::merged_for_prompt(&empty, &empty, &empty, "myproj").is_empty()
        );
    }
}
