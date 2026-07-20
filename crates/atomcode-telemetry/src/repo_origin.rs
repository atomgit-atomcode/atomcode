//! Helpers for constructing telemetry envelope context at process start:
//! detect the repository host from `git remote origin`.
//!
//! Lives here (not in a driver) because the [`RepoOrigin`]/[`RepoHost`] it produces
//! are telemetry envelope types; the CLI and daemon both call [`detect_repo_origin`]
//! at startup. Ported out of `atomcode-core` (v1 engine) as part of retiring it.

use crate::{RepoHost, RepoOrigin};
use std::path::Path;
use std::process::Command;

/// Suppress the Windows console-window flash for a short-lived child process.
/// `detect_repo_origin` runs from the console-less daemon on every `/chat` turn,
/// so a bare `git` spawn would pop and flicker a window. Uses only safe std
/// (`CREATE_NO_WINDOW` creation flag) so it holds under this crate's
/// `#![forbid(unsafe_code)]`. No-op off Windows.
#[cfg(target_os = "windows")]
fn suppress_console_window(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    cmd.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
fn suppress_console_window(_cmd: &mut Command) {}

pub fn detect_repo_origin(cwd: &Path) -> RepoOrigin {
    let mut cmd = Command::new("git");
    cmd.args(["-C"])
        .arg(cwd)
        .args(["remote", "get-url", "origin"]);
    suppress_console_window(&mut cmd);
    let output = cmd.output();
    match output {
        Ok(o) if o.status.success() => {
            let url = String::from_utf8_lossy(&o.stdout).trim().to_string();
            RepoOrigin {
                host: classify_host(&url),
                has_git: true,
            }
        }
        Ok(_) => RepoOrigin {
            host: RepoHost::None,
            has_git: has_git_dir(cwd),
        },
        Err(_) => RepoOrigin {
            host: RepoHost::None,
            has_git: has_git_dir(cwd),
        },
    }
}

fn classify_host(url: &str) -> RepoHost {
    let u = url.to_ascii_lowercase();
    if u.contains("gitcode.com") {
        RepoHost::Gitcode
    } else if u.contains("atomgit.com") {
        RepoHost::Atomgit
    } else if u.contains("github.com") {
        RepoHost::Github
    } else if u.contains("gitlab.") {
        RepoHost::Gitlab
    } else if u.is_empty() {
        RepoHost::None
    } else {
        RepoHost::Other
    }
}

fn has_git_dir(cwd: &Path) -> bool {
    cwd.join(".git").exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_hosts() {
        assert!(matches!(
            classify_host("git@gitcode.com:foo/bar.git"),
            RepoHost::Gitcode
        ));
        assert!(matches!(
            classify_host("https://atomgit.com/x/y"),
            RepoHost::Atomgit
        ));
        assert!(matches!(
            classify_host("https://github.com/x/y"),
            RepoHost::Github
        ));
        assert!(matches!(
            classify_host("ssh://git@gitlab.foo/x"),
            RepoHost::Gitlab
        ));
        assert!(matches!(
            classify_host("https://other.net/x"),
            RepoHost::Other
        ));
        assert!(matches!(classify_host(""), RepoHost::None));
    }
}
