//! Auto-commit edited files after each agent turn.
//!
//! When `config.auto_commit` is enabled and the working directory is a git repo,
//! files edited during the turn are staged and committed with an auto-generated message.

use std::path::Path;
use std::process::Command;

/// Auto-commit files edited during the agent turn.
/// Returns the commit SHA on success, or None if nothing to commit / not a git repo.
pub fn auto_commit_edited_files(
    working_dir: &Path,
    edited_files: &[String],
) -> Option<String> {
    if edited_files.is_empty() {
        return None;
    }

    if !is_git_repo(working_dir) {
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
        let status = Command::new("git")
            .args(["add", &file_path])
            .current_dir(working_dir)
            .output();
        if status.is_ok() {
            added += 1;
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

    let sha = String::from_utf8_lossy(&sha_output.stdout).trim().to_string();
    if sha.is_empty() { None } else { Some(sha) }
}

fn generate_commit_message(files: &[String]) -> String {
    let file_count = files.len();

    // Extract short file names for the message
    let short_names: Vec<&str> = files
        .iter()
        .map(|f| {
            f.rsplit('/').next().unwrap_or(f)
        })
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
