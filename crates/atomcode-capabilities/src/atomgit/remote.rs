//! Parse a git remote URL into an AtomGit/GitCode API push target. Only
//! gitcode.com / atomgit.com are recognised (they share one v5 API + token);
//! any other host yields `None`. See issue: auto-label after push.

use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushTarget {
    /// v5 REST API base for the host (no trailing slash).
    pub base_url: &'static str,
    pub owner: String,
    pub repo: String,
}

/// Host → v5 API base. `None` for unsupported hosts.
fn base_url_for(url_lower: &str) -> Option<&'static str> {
    if url_lower.contains("atomgit.com") {
        Some("https://api.atomgit.com/api/v5")
    } else if url_lower.contains("gitcode.com") {
        Some("https://api.gitcode.com/api/v5")
    } else {
        None
    }
}

/// Parse `owner`/`repo` from a remote URL (ssh scp-form, ssh://, or https),
/// paired with the API base. `None` unless the host is atomgit/gitcode AND both
/// owner and repo are present.
pub fn parse_push_target(remote_url: &str) -> Option<PushTarget> {
    let lower = remote_url.to_ascii_lowercase();
    let base_url = base_url_for(&lower)?;
    // Locate the host, then take the path after it. The separator is ':' for
    // scp-form (git@host:owner/repo) or '/' for URL-form (…host/owner/repo).
    let host = if lower.contains("atomgit.com") {
        "atomgit.com"
    } else {
        "gitcode.com"
    };
    let idx = lower.find(host)?;
    let rest = &remote_url[idx + host.len()..];
    let rest = rest.trim_start_matches([':', '/']);
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/').filter(|s| !s.is_empty());
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(PushTarget {
        base_url,
        owner,
        repo,
    })
}

/// Run `git -C <cwd> remote get-url origin` and parse it. `None` when there's no
/// origin or it isn't an atomgit/gitcode remote.
pub fn detect_push_target(cwd: &Path) -> Option<PushTarget> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout);
    parse_push_target(url.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ssh_and_https_for_both_hosts() {
        let cases = [
            (
                "git@atomgit.com:acme/widget.git",
                "https://api.atomgit.com/api/v5",
                "acme",
                "widget",
            ),
            (
                "https://atomgit.com/acme/widget.git",
                "https://api.atomgit.com/api/v5",
                "acme",
                "widget",
            ),
            (
                "https://atomgit.com/acme/widget",
                "https://api.atomgit.com/api/v5",
                "acme",
                "widget",
            ),
            (
                "ssh://git@gitcode.com/acme/widget.git",
                "https://api.gitcode.com/api/v5",
                "acme",
                "widget",
            ),
            (
                "https://gitcode.com/acme/widget/",
                "https://api.gitcode.com/api/v5",
                "acme",
                "widget",
            ),
        ];
        for (url, base, owner, repo) in cases {
            let t = parse_push_target(url).unwrap_or_else(|| panic!("expected Some for {url}"));
            assert_eq!(t.base_url, base, "base for {url}");
            assert_eq!(t.owner, owner, "owner for {url}");
            assert_eq!(t.repo, repo, "repo for {url}");
        }
    }

    #[test]
    fn rejects_other_hosts_and_garbage() {
        assert!(parse_push_target("git@github.com:a/b.git").is_none());
        assert!(parse_push_target("https://gitlab.com/a/b.git").is_none());
        assert!(parse_push_target("https://atomgit.com/onlyowner").is_none());
        assert!(parse_push_target("").is_none());
    }

    // `detect_push_target` shells out to real `git` on a temp repo (no network): the gate the
    // post-push label middleware relies on to decide whether to touch the remote at all.
    fn git(dir: &std::path::Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git available")
            .status
            .success();
        assert!(ok, "git {args:?} failed");
    }

    #[test]
    fn detect_push_target_reads_origin_and_gates_on_host() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q"]);

        // No origin yet → None (not a crash).
        assert!(detect_push_target(p).is_none(), "no origin → None");

        // A gitcode origin → Some, parsed to owner/repo.
        git(
            p,
            &[
                "remote",
                "add",
                "origin",
                "https://gitcode.com/saulcy/order_a_meal.git",
            ],
        );
        let t = detect_push_target(p).expect("gitcode origin → Some");
        assert_eq!(
            (t.owner.as_str(), t.repo.as_str()),
            ("saulcy", "order_a_meal")
        );

        // A non-atomgit origin (github) → None: the middleware skips labelling.
        git(
            p,
            &[
                "remote",
                "set-url",
                "origin",
                "git@github.com:acme/widget.git",
            ],
        );
        assert!(detect_push_target(p).is_none(), "github origin → None");
    }
}
