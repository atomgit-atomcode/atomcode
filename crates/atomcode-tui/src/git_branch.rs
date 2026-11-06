//! Read local symbolic HEAD without starting a process in the render loop.
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub(crate) fn current(cwd: &str) -> Option<String> {
    static CACHE: Mutex<Option<(String, Instant, Option<String>)>> = Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((path, at, branch)) = cache.as_ref() {
        if path == cwd && at.elapsed() < Duration::from_secs(1) {
            return branch.clone();
        }
    }
    let expanded = crate::text::expand_home_with(cwd, crate::text::home_dir().as_deref());
    let branch = read(Path::new(&expanded));
    *cache = Some((cwd.into(), Instant::now(), branch.clone()));
    branch
}

fn read(cwd: &Path) -> Option<String> {
    for dir in cwd.ancestors() {
        let dot = dir.join(".git");
        let git = if dot.is_dir() {
            dot
        } else if dot.is_file() {
            let link = std::fs::read_to_string(&dot).ok()?;
            let target = link.trim().strip_prefix("gitdir: ")?;
            dir.join(target)
        } else {
            continue;
        };
        let head = std::fs::read_to_string(git.join("HEAD")).ok()?;
        let branch = head.trim().strip_prefix("ref: refs/heads/")?;
        return (!branch.is_empty() && !branch.chars().any(char::is_control))
            .then(|| branch.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branches_follow_nested_directories_worktrees_and_detached_heads() {
        let root = tempfile::tempdir().unwrap();
        let git = root.path().join(".git");
        std::fs::create_dir(&git).unwrap();
        std::fs::write(git.join("HEAD"), "ref: refs/heads/release/v5.2.7\n").unwrap();
        let child = root.path().join("src");
        std::fs::create_dir(&child).unwrap();
        assert_eq!(read(&child).as_deref(), Some("release/v5.2.7"));
        std::fs::write(git.join("HEAD"), "0123456789abcdef\n").unwrap();
        assert_eq!(read(&child), None);
        std::fs::remove_dir_all(&git).unwrap();
        assert_eq!(read(root.path()), None);
        let metadata = root.path().join("metadata");
        std::fs::create_dir(&metadata).unwrap();
        std::fs::write(metadata.join("HEAD"), "ref: refs/heads/feature/test\n").unwrap();
        std::fs::write(&git, "gitdir: metadata\n").unwrap();
        assert_eq!(read(&child).as_deref(), Some("feature/test"));
    }
}
