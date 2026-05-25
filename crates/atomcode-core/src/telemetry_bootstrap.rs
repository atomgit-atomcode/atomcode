//! Helpers for constructing telemetry envelope context at process start.

use atomcode_telemetry::{RepoHost, RepoOrigin};
use std::path::Path;
use std::process::Command;

pub fn detect_repo_origin(cwd: &Path) -> RepoOrigin {
    let output = Command::new("git")
        .args(["-C"])
        .arg(cwd)
        .args(["remote", "get-url", "origin"])
        .output();
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
