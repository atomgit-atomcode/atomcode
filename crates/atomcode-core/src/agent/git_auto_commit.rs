//! Auto-commit edited files after each agent turn.
//!
//! When `config.auto_commit` is enabled and the working directory is a git repo,
//! files edited during the turn are staged and committed with an auto-generated message.

use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoCommitOutcome {
    Committed { sha: String, message: String },
    Skipped { reason: String },
    Failed { reason: String },
}

/// Auto-commit files edited during the agent turn.
pub fn auto_commit_edited_files(working_dir: &Path, edited_files: &[String]) -> AutoCommitOutcome {
    if edited_files.is_empty() {
        return AutoCommitOutcome::Skipped {
            reason: "no edited files".to_string(),
        };
    }

    if !is_git_repo(working_dir) {
        return AutoCommitOutcome::Skipped {
            reason: "not a git repository".to_string(),
        };
    }

    // Do not mix user-staged changes into an automatic commit. `git commit`
    // commits the whole index, so auto-commit is only safe when the index is
    // clean before we stage this turn's edited files.
    if has_staged_changes(working_dir) {
        return AutoCommitOutcome::Skipped {
            reason: "index has pre-existing staged changes".to_string(),
        };
    }

    let file_paths: Vec<String> = edited_files
        .iter()
        .map(|file| {
            if Path::new(file).is_absolute() {
                file.to_string()
            } else {
                working_dir.join(file).to_string_lossy().to_string()
            }
        })
        .collect();

    // Stage only the files that were actually edited.
    let mut add_cmd = Command::new("git");
    add_cmd.arg("add").arg("--").args(&file_paths).current_dir(working_dir);
    let output = match add_cmd.output() {
        Ok(output) => output,
        Err(e) => {
            return AutoCommitOutcome::Failed {
                reason: format!("git add failed to start: {e}"),
            };
        }
    };

    if !output.status.success() {
        return AutoCommitOutcome::Failed {
            reason: format!("git add failed: {}", command_output_message(&output)),
        };
    }

    if file_paths.is_empty() {
        return AutoCommitOutcome::Skipped {
            reason: "no edited files".to_string(),
        };
    }

    // Check if there are staged changes
    let diff_output = Command::new("git")
        .args(["diff", "--cached", "--quiet"])
        .current_dir(working_dir)
        .status();
    if let Ok(status) = diff_output {
        if status.success() {
            // Exit code 0 means no staged changes
            return AutoCommitOutcome::Skipped {
                reason: "no staged changes after git add".to_string(),
            };
        }
    } else if let Err(e) = diff_output {
        return AutoCommitOutcome::Failed {
            reason: format!("git diff --cached failed to start: {e}"),
        };
    }

    let message = generate_commit_message(edited_files);

    let output = match Command::new("git")
        .args(["commit", "-m", &message])
        .current_dir(working_dir)
        .output()
    {
        Ok(output) => output,
        Err(e) => {
            return AutoCommitOutcome::Failed {
                reason: format!("git commit failed to start: {e}"),
            };
        }
    };

    if !output.status.success() {
        return AutoCommitOutcome::Failed {
            reason: format!("git commit failed: {}", command_output_message(&output)),
        };
    }

    // Extract commit SHA
    let sha_output = match Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(working_dir)
        .output()
    {
        Ok(output) => output,
        Err(e) => {
            return AutoCommitOutcome::Failed {
                reason: format!("git rev-parse failed to start: {e}"),
            };
        }
    };

    let sha = String::from_utf8_lossy(&sha_output.stdout)
        .trim()
        .to_string();
    if sha.is_empty() {
        AutoCommitOutcome::Failed {
            reason: "git rev-parse returned an empty sha".to_string(),
        }
    } else {
        AutoCommitOutcome::Committed { sha, message }
    }
}

fn command_output_message(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !stdout.is_empty() {
        return stdout;
    }
    format!("exit status {}", output.status)
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

        let outcome = auto_commit_edited_files(dir.path(), &["edited.txt".to_string()]);
        assert!(matches!(outcome, AutoCommitOutcome::Committed { .. }));

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
        let outcome = auto_commit_edited_files(dir.path(), &["edited.txt".to_string()]);

        assert!(matches!(outcome, AutoCommitOutcome::Skipped { .. }));
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
