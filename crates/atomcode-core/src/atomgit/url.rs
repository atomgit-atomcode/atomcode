use anyhow::{anyhow, Result};

/// Parsed coordinates of an AtomGit/GitCode issue URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueRef {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

/// `(owner, repo)` without the issue number — produced either from an
/// issue URL (via `IssueRef`) or from a git remote URL (via
/// `parse_repo_url`). Case-insensitive equality for comparison.
#[derive(Debug, Clone)]
pub struct RepoRef {
    pub owner: String,
    pub repo: String,
}

impl RepoRef {
    pub fn matches(&self, other: &RepoRef) -> bool {
        self.owner.eq_ignore_ascii_case(&other.owner) && self.repo.eq_ignore_ascii_case(&other.repo)
    }
}

impl From<&IssueRef> for RepoRef {
    fn from(r: &IssueRef) -> Self {
        Self {
            owner: r.owner.clone(),
            repo: r.repo.clone(),
        }
    }
}

/// Hosts we accept as "this is an atomgit.com repo". `gitcode.com` is
/// the same physical origin mirrored onto a second domain — the owner /
/// repo slug is identical and every API call still targets
/// `api.atomgit.com`, so a checkout whose `origin` points to either host
/// should be treated as the same repo for /issue / /fixissue purposes.
///
/// If more mirror hosts appear later (e.g. a private enterprise mirror),
/// extend this list. Matching is case-insensitive.
const ATOMGIT_MIRROR_HOSTS: &[&str] = &["atomgit.com", "gitcode.com"];

fn is_atomgit_host(host: &str) -> bool {
    ATOMGIT_MIRROR_HOSTS
        .iter()
        .any(|h| host.eq_ignore_ascii_case(h))
}

/// Parse a git remote URL into `(owner, repo)`. Supports the four
/// common forms:
///   * `https://atomgit.com/owner/repo.git`
///   * `https://atomgit.com/owner/repo`
///   * `git@atomgit.com:owner/repo.git`
///   * `ssh://git@atomgit.com/owner/repo.git`
///
/// Also accepts `gitcode.com` in any of those forms — atomgit mirrors
/// every repo onto gitcode with the same slug, and the API always
/// resolves against `api.atomgit.com`, so the host is effectively
/// informational here.
///
/// Returns `None` when the URL isn't an atomgit mirror host — used to
/// detect "cwd is in a git repo but it's a different host (e.g. GitHub)"
/// and skip validation rather than false-positive.
pub fn parse_repo_url(url: &str) -> Option<RepoRef> {
    let trimmed = url.trim();

    // SSH shorthand: `git@host:owner/repo.git`
    if let Some(rest) = trimmed.strip_prefix("git@") {
        let (host_with_port, path) = rest.split_once(':')?;
        let host = strip_port(host_with_port);
        if !is_atomgit_host(host) {
            return None;
        }
        return split_owner_repo(path);
    }

    // URL forms (https://, http://, ssh://)
    let without_scheme = if let Some(rest) = trimmed.strip_prefix("https://") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("ssh://") {
        rest
    } else {
        return None;
    };

    // Strip optional userinfo (`user[:password]@`). Git for Windows
    // credential helpers rewrite origin to embed the OAuth token, e.g.
    // `https://oauth2:TOKEN@atomgit.com/owner/repo.git`; without this,
    // the host check sees `oauth2:TOKEN@atomgit.com` and /issue /
    // /fixissue both fail with "needs cwd to be a clone of an
    // atomgit.com repo" even though the underlying repo IS atomgit.
    // Same path also handles ssh://git@host/... (the leading `git@`
    // is just userinfo here, no need for a separate strip).
    //
    // The `userinfo.contains('/')` guard ensures we only strip when
    // the `@` appears in authority — a defensive parse for the rare
    // case of `@` inside a path segment.
    let after_userinfo = match without_scheme.split_once('@') {
        Some((userinfo, rest)) if !userinfo.contains('/') => rest,
        _ => without_scheme,
    };

    let (host_with_port, path) = after_userinfo.split_once('/')?;
    let host = strip_port(host_with_port);
    if !is_atomgit_host(host) {
        return None;
    }
    split_owner_repo(path)
}

/// Strip an optional `:port` suffix from an authority component.
/// `atomgit.com:443` → `atomgit.com`; `atomgit.com` → `atomgit.com`.
fn strip_port(host: &str) -> &str {
    host.split_once(':').map(|(h, _)| h).unwrap_or(host)
}

fn split_owner_repo(path: &str) -> Option<RepoRef> {
    let mut parts = path
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty());
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    let repo = repo.strip_suffix(".git").unwrap_or(&repo).to_string();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(RepoRef { owner, repo })
}

/// Detect the atomgit.com (owner, repo) of a local git checkout by
/// running `git remote get-url origin` in `cwd`. Returns:
///   * `Ok(Some(RepoRef))` — found an atomgit.com origin.
///   * `Ok(None)` — not a git repo, or origin points elsewhere / missing.
///     Callers treat this as "can't validate, proceed with a warning".
///   * `Err(...)` — the git command itself failed unexpectedly.
pub fn detect_cwd_atomgit_repo(cwd: &std::path::Path) -> std::io::Result<Option<RepoRef>> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(["remote", "get-url", "origin"])
        .current_dir(cwd);
    crate::process_utils::suppress_console_window_sync(&mut cmd);
    let output = cmd.output()?;
    if !output.status.success() {
        // `git` returned non-zero — either not a repo, or no `origin`.
        // Both are "can't validate"; don't bubble the raw stderr up.
        return Ok(None);
    }
    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if url.is_empty() {
        return Ok(None);
    }
    Ok(parse_repo_url(&url))
}

impl IssueRef {
    /// Parse `https://atomgit.com/{owner}/{repo}/issues/{number}` or the
    /// equivalent `gitcode.com` mirror URL.
    /// Trailing slash and `?query`/`#fragment` are tolerated.
    pub fn parse(url: &str) -> Result<Self> {
        let trimmed = url.trim();
        let without_scheme = trimmed
            .strip_prefix("https://")
            .or_else(|| trimmed.strip_prefix("http://"))
            .ok_or_else(|| anyhow!("issue URL must start with http(s)://"))?;

        // Drop query + fragment before splitting path segments.
        let path_only = without_scheme
            .split(['?', '#'])
            .next()
            .unwrap_or(without_scheme);

        let mut parts = path_only.split('/').filter(|s| !s.is_empty());
        let host = parts
            .next()
            .ok_or_else(|| anyhow!("missing host in issue URL"))?;
        let host = strip_port(host);
        if !is_atomgit_host(host) {
            return Err(anyhow!(
                "only atomgit.com/gitcode.com issue URLs are supported (got host {})",
                host
            ));
        }

        let owner = parts
            .next()
            .ok_or_else(|| anyhow!("missing owner in issue URL"))?
            .to_string();
        let repo = parts
            .next()
            .ok_or_else(|| anyhow!("missing repo in issue URL"))?
            .to_string();
        let issues_seg = parts
            .next()
            .ok_or_else(|| anyhow!("missing 'issues' segment in URL"))?;
        if issues_seg != "issues" {
            return Err(anyhow!(
                "expected '/issues/' in URL, got '/{}/'",
                issues_seg
            ));
        }
        let number_str = parts
            .next()
            .ok_or_else(|| anyhow!("missing issue number in URL"))?;
        let number = number_str
            .parse::<u64>()
            .map_err(|_| anyhow!("issue number '{}' is not a positive integer", number_str))?;

        Ok(Self {
            owner,
            repo,
            number,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_canonical_url() {
        let r = IssueRef::parse("https://atomgit.com/atomgit_atomcode/atomcode/issues/42").unwrap();
        assert_eq!(r.owner, "atomgit_atomcode");
        assert_eq!(r.repo, "atomcode");
        assert_eq!(r.number, 42);
    }

    #[test]
    fn parses_with_trailing_slash() {
        let r = IssueRef::parse("https://atomgit.com/a/b/issues/1/").unwrap();
        assert_eq!(r.number, 1);
    }

    #[test]
    fn parses_with_query_and_fragment() {
        let r = IssueRef::parse("https://atomgit.com/a/b/issues/7?x=1#comment").unwrap();
        assert_eq!(r.number, 7);
    }

    #[test]
    fn parses_gitcode_mirror_issue_url() {
        let r = IssueRef::parse("https://gitcode.com/atomgit_atomcode/atomcode/issues/340")
            .unwrap();
        assert_eq!(r.owner, "atomgit_atomcode");
        assert_eq!(r.repo, "atomcode");
        assert_eq!(r.number, 340);
    }

    #[test]
    fn parses_issue_url_with_host_port() {
        let r = IssueRef::parse("https://gitcode.com:443/a/b/issues/8").unwrap();
        assert_eq!(r.owner, "a");
        assert_eq!(r.repo, "b");
        assert_eq!(r.number, 8);
    }

    #[test]
    fn rejects_non_atomgit_host() {
        assert!(IssueRef::parse("https://github.com/a/b/issues/1").is_err());
    }

    #[test]
    fn rejects_missing_number() {
        assert!(IssueRef::parse("https://atomgit.com/a/b/issues").is_err());
    }

    #[test]
    fn rejects_non_numeric() {
        assert!(IssueRef::parse("https://atomgit.com/a/b/issues/abc").is_err());
    }

    #[test]
    fn rejects_wrong_path() {
        assert!(IssueRef::parse("https://atomgit.com/a/b/pulls/1").is_err());
    }

    #[test]
    fn parse_repo_url_https() {
        let r = parse_repo_url("https://atomgit.com/owner/repo.git").unwrap();
        assert_eq!(r.owner, "owner");
        assert_eq!(r.repo, "repo");
    }

    #[test]
    fn parse_repo_url_https_no_git_suffix() {
        let r = parse_repo_url("https://atomgit.com/owner/repo").unwrap();
        assert_eq!(r.repo, "repo");
    }

    #[test]
    fn parse_repo_url_ssh_shorthand() {
        let r = parse_repo_url("git@atomgit.com:owner/repo.git").unwrap();
        assert_eq!(r.owner, "owner");
        assert_eq!(r.repo, "repo");
    }

    #[test]
    fn parse_repo_url_ssh_full() {
        let r = parse_repo_url("ssh://git@atomgit.com/owner/repo.git").unwrap();
        assert_eq!(r.owner, "owner");
        assert_eq!(r.repo, "repo");
    }

    #[test]
    fn parse_repo_url_rejects_non_atomgit() {
        assert!(parse_repo_url("https://github.com/foo/bar.git").is_none());
        assert!(parse_repo_url("git@github.com:foo/bar.git").is_none());
    }

    #[test]
    fn parse_repo_url_strips_oauth_userinfo() {
        // Git for Windows credential helper rewrites origin URLs to
        // include the OAuth token in the userinfo segment after the
        // user runs `git push` once. Before the fix, /issue / /fixissue
        // both failed with "needs cwd to be a clone of an atomgit.com
        // repo" because the host check saw `oauth2:TOKEN@atomgit.com`.
        let r = parse_repo_url("https://oauth2:abc123token@atomgit.com/owner/repo.git").unwrap();
        assert_eq!(r.owner, "owner");
        assert_eq!(r.repo, "repo");
    }

    #[test]
    fn parse_repo_url_strips_basic_auth_userinfo() {
        // `https://user:password@host/...` — basic auth, also rewritten
        // by some helpers. Same fix should cover it.
        let r = parse_repo_url("https://user:password@atomgit.com/owner/repo.git").unwrap();
        assert_eq!(r.owner, "owner");
        assert_eq!(r.repo, "repo");
    }

    #[test]
    fn parse_repo_url_strips_user_only_userinfo() {
        // `https://user@host/...` — username with no password.
        let r = parse_repo_url("https://alice@gitcode.com/atomgit_atomcode/atomcode").unwrap();
        assert_eq!(r.owner, "atomgit_atomcode");
        assert_eq!(r.repo, "atomcode");
    }

    #[test]
    fn parse_repo_url_strips_host_port() {
        // `https://atomgit.com:443/...` — explicit port, legal but rare.
        let r = parse_repo_url("https://atomgit.com:443/owner/repo.git").unwrap();
        assert_eq!(r.owner, "owner");
        assert_eq!(r.repo, "repo");
    }

    #[test]
    fn parse_repo_url_ssh_with_port() {
        // `ssh://git@atomgit.com:22/...`.
        let r = parse_repo_url("ssh://git@atomgit.com:22/owner/repo.git").unwrap();
        assert_eq!(r.owner, "owner");
        assert_eq!(r.repo, "repo");
    }

    #[test]
    fn parse_repo_url_oauth_token_does_not_match_non_atomgit() {
        // Defensive: an oauth-rewritten URL pointing at github.com is
        // still not atomgit. The fix must not introduce a false positive.
        assert!(parse_repo_url("https://oauth2:TOKEN@github.com/foo/bar.git").is_none());
    }

    #[test]
    fn parse_repo_url_accepts_gitcode_mirror() {
        // gitcode.com is the second domain atomgit mirrors every repo
        // onto. Plenty of users' local `origin` is set to the gitcode
        // remote (e.g. `git@gitcode.com:atomgit_atomcode/atomcode.git`)
        // while the issue / API still lives on atomgit.com. The slug is
        // the same, so `/issue` / `/fixissue` must treat it as the same
        // repo — otherwise cwd validation fails with "needs cwd to be a
        // clone of an atomgit.com repo" and the user gets stuck.
        for url in [
            "git@gitcode.com:atomgit_atomcode/atomcode.git",
            "https://gitcode.com/atomgit_atomcode/atomcode.git",
            "https://gitcode.com/atomgit_atomcode/atomcode",
            "ssh://git@gitcode.com/atomgit_atomcode/atomcode.git",
        ] {
            let r = parse_repo_url(url)
                .unwrap_or_else(|| panic!("should parse gitcode mirror URL: {}", url));
            assert_eq!(r.owner, "atomgit_atomcode");
            assert_eq!(r.repo, "atomcode");
        }
    }

    #[test]
    fn repo_ref_matches_case_insensitive() {
        let a = RepoRef {
            owner: "Atomgit_Atomcode".into(),
            repo: "AtomCode".into(),
        };
        let b = RepoRef {
            owner: "atomgit_atomcode".into(),
            repo: "atomcode".into(),
        };
        assert!(a.matches(&b));
    }

    #[test]
    fn issue_ref_to_repo_ref() {
        let r = IssueRef::parse("https://atomgit.com/o/r/issues/1").unwrap();
        let rr: RepoRef = (&r).into();
        assert_eq!(rr.owner, "o");
        assert_eq!(rr.repo, "r");
    }
}
