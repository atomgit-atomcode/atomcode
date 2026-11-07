//! Read symbolic Git HEAD for display, without starting a Git process.
use std::path::Path;

fn small_text(path: &Path) -> Option<String> {
    use std::io::Read;
    if !path.metadata().ok()?.is_file() {
        return None;
    }
    let mut text = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(4097)
        .read_to_string(&mut text)
        .ok()?;
    (text.len() <= 4096).then_some(text)
}

pub fn read(cwd: &Path) -> Option<String> {
    if !cwd.is_dir() {
        return None;
    }
    for dir in cwd.ancestors() {
        let dot = dir.join(".git");
        let git = if dot.is_dir() {
            dot
        } else if dot.is_file() {
            let link = small_text(&dot)?;
            let target = link.trim().strip_prefix("gitdir: ")?;
            dir.join(target)
        } else {
            continue;
        };
        let head = small_text(&git.join("HEAD"))?;
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
    fn malformed_oversized_and_missing_heads_are_not_branches() {
        let root = tempfile::tempdir().unwrap();
        let git = root.path().join(".git");
        std::fs::create_dir(&git).unwrap();
        assert_eq!(read(root.path()), None);
        for head in [
            "ref: refs/heads/".to_string(),
            "ref: refs/heads/bad\u{0001}name".to_string(),
            format!("ref: refs/heads/{}", "x".repeat(4096)),
        ] {
            std::fs::write(git.join("HEAD"), head).unwrap();
            assert_eq!(read(root.path()), None);
        }
        assert_eq!(read(&root.path().join("missing")), None);
    }

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
