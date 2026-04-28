//! Auto-commit edited files after each agent turn.
//!
//! When `config.auto_commit` is enabled and the working directory is a git repo,
//! files edited during the turn are staged and committed with an auto-generated message.

use std::path::Path;
use std::process::Command;

/// Auto-commit files edited during the agent turn.
/// Returns the commit SHA on success, or None if nothing to commit / not a git repo.
pub fn auto_commit_edited_files(working_dir: &Path, edited_files: &[String]) -> Option<String> {
    if edited_files.is_empty() {
        return None;
    }

    if !is_git_repo(working_dir) {
        return None;
    }

    // Do not mix user-staged changes into an automatic commit. `git commit`
    // commits the whole index, so auto-commit is only safe when the index is
    // clean before we stage this turn's edited files.
    if has_staged_changes(working_dir) {
        return None;
    }

    // Stage only the files that were actually edited
    let mut added = 0;
    for file in edited_files {
        let file_path = if Path::new(file).is_absolute() {
            file.to_string()
        } else {
            working_dir.join(file).to_string_lossy().to_string()
        };
        if let Ok(output) = Command::new("git")
            .args(["add", &file_path])
            .current_dir(working_dir)
            .output()
        {
            if output.status.success() {
                added += 1;
            }
        }
    }

    if added == 0 {
        return None;
    }

    // Check if there are staged changes
    let diff_output = Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(working_dir)
        .status();
    if let Ok(status) = diff_output {
        if status.success() {
            // Exit code 0 means no staged changes
            return None;
        }
    }

    let message = generate_commit_message(edited_files);

    let output = Command::new("git")
        .args(["commit", "-m", &message])
        .current_dir(working_dir)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    // Extract commit SHA
    let sha_output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(working_dir)
        .output()
        .ok()?;

    let sha = String::from_utf8_lossy(&sha_output.stdout)
        .trim()
        .to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

fn generate_commit_message(files: &[String]) -> String {
    let file_count = files.len();

    // Extract short file names for the message
    let short_names: Vec<&str> = files
        .iter()
        .map(|f| f.rsplit('/').next().unwrap_or(f))
        .collect();

    if file_count == 1 {
        format!("atomcode: edit {}", short_names[0])
    } else if file_count <= 3 {
        format!("atomcode: edit {}", short_names.join(", "))
    } else {
        format!(
            "atomcode: edit {} and {} more",
            short_names[..2].join(", "),
            file_count - 2
        )
    }
}

fn is_git_repo(working_dir: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(working_dir)
        .output()
        .ok()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn has_staged_changes(working_dir: &Path) -> bool {
    Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(working_dir)
        .status()
        .map(|status| !status.success())
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn run_git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run_git(dir.path(), &["init"]);
        run_git(
            dir.path(),
            &["config", "user.email", "atomcode@example.com"],
        );
        run_git(dir.path(), &["config", "user.name", "AtomCode"]);
        dir
    }

    #[test]
    fn auto_commit_commits_only_when_index_is_clean() {
        let dir = init_repo();
        let edited = dir.path().join("edited.txt");
        fs::write(&edited, "hello\n").unwrap();

        let sha = auto_commit_edited_files(dir.path(), &["edited.txt".to_string()]);
        assert!(sha.is_some());

        let log = Command::new("git")
            .args(["log", "--oneline", "-1"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&log.stdout).contains("atomcode: edit edited.txt"));
    }

    #[test]
    fn auto_commit_skips_when_user_has_staged_changes() {
        let dir = init_repo();
        fs::write(dir.path().join("pre_staged.txt"), "user work\n").unwrap();
        run_git(dir.path(), &["add", "pre_staged.txt"]);

        fs::write(dir.path().join("edited.txt"), "agent work\n").unwrap();
        let sha = auto_commit_edited_files(dir.path(), &["edited.txt".to_string()]);

        assert!(sha.is_none());
        let status = Command::new("git")
            .args(["diff", "--cached", "--name-only"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&status.stdout).trim(),
            "pre_staged.txt"
        );
    }
}
