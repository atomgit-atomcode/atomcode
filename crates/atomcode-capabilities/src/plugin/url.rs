use anyhow::{anyhow, bail, Result};
use url::Url;

/// Accept https / http / ssh / git@host: / file scheme. Reject everything else.
pub fn validate_git_url(url: &str) -> Result<()> {
    if url.is_empty() || url != url.trim() {
        bail!("malformed git url: {}", url);
    }

    if is_git_ssh_shorthand(url) {
        return Ok(());
    }

    let parsed =
        Url::parse(url).map_err(|_| anyhow!("unsupported or malformed git url: {}", url))?;
    match parsed.scheme() {
        "http" | "https" | "ssh" => {
            if parsed.host_str().is_none() {
                bail!("git url missing host: {}", url);
            }
            if !has_repo_path(&parsed) {
                bail!("git url missing repository path: {}", url);
            }
            Ok(())
        }
        "file" => {
            if !has_repo_path(&parsed) {
                bail!("git url missing repository path: {}", url);
            }
            Ok(())
        }
        scheme => Err(anyhow!("unsupported git url scheme: {}", scheme)),
    }
}

fn is_git_ssh_shorthand(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("git@") else {
        return false;
    };
    let Some((host, path)) = rest.split_once(':') else {
        return false;
    };
    !host.is_empty() && !host.contains('/') && !path.trim_matches('/').is_empty()
}

fn has_repo_path(url: &Url) -> bool {
    let path = url.path().trim_matches('/');
    !path.is_empty()
}

fn strip_git_suffix_once(url: &str) -> &str {
    url.strip_suffix(".git").unwrap_or(url)
}

fn normalize_name_source(url: &str) -> &str {
    strip_git_suffix_once(url.trim_end_matches('/'))
}

fn last_path_segment(url: &str) -> Option<&str> {
    url.rsplit(['/', ':']).next().filter(|s| !s.is_empty())
}

/// Extract the last path segment from a git URL, stripping `.git` suffix.
/// Examples:
///   https://gitcode.com/u/foo.git → foo
///   git@github.com:o/bar         → bar
///   file:///C:/Users/u/bar       → bar   (Windows)
pub fn infer_marketplace_name_from_url(url: &str) -> Result<String> {
    // Prefer the URL parser: it handles `file:///C:/...` (Windows),
    // `file:///tmp/foo` (Unix) and standard remote URLs correctly.
    // Fall back to text-level splitting for SSH shorthand (`git@host:o/r`).
    if let Ok(parsed) = url::Url::parse(url) {
        let path = parsed.path().trim_end_matches('/');
        let last = path
            .rsplit('/')
            .find(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("cannot infer name from url: {}", url))?;
        return Ok(strip_git_suffix_once(last).to_string());
    }
    // SSH shorthand fallback
    let trimmed = normalize_name_source(url);
    let last =
        last_path_segment(trimmed).ok_or_else(|| anyhow!("cannot infer name from url: {}", url))?;
    Ok(last.to_string())
}

/// True when `url`'s host is an AtomCode-platform git host we may inject the
/// stored login token into. NEVER widen without thought — the token must
/// never be sent to a third-party host. ssh shorthand / malformed → false.
pub(crate) fn host_is_trusted(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    atomcode_config::endpoints::is_trusted_host(host)
}

/// `scheme://host` prefix (no path), for scoping git's per-URL
/// `http.<base>.extraHeader` so an injected credential header can't leak on a
/// cross-host redirect. None for unparseable URLs / hostless (ssh shorthand).
pub(crate) fn scheme_host_prefix(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    Some(format!("{}://{}", parsed.scheme(), host))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_supported_schemes() {
        for u in [
            "https://x.com/r.git",
            "http://x.com/r",
            "ssh://git@x.com/r.git",
            "git@x.com:o/r.git",
            "file:///tmp/r",
        ] {
            assert!(validate_git_url(u).is_ok(), "{}", u);
        }
    }

    #[test]
    fn rejects_unsupported_schemes() {
        for u in ["ftp://x/r", "javascript:alert(1)", "../local"] {
            assert!(validate_git_url(u).is_err(), "{}", u);
        }
    }

    #[test]
    fn rejects_malformed_urls_with_supported_schemes() {
        for u in [
            "",
            " https://x.com/r.git",
            "https://",
            "https://x.com",
            "ssh://git@/r.git",
            "file:///",
        ] {
            assert!(validate_git_url(u).is_err(), "{}", u);
        }
    }

    #[test]
    fn rejects_malformed_ssh_shorthand() {
        for u in [
            "git@",
            "git@github.com",
            "git@github.com:",
            "git@:owner/repo.git",
            "git@github.com:/",
        ] {
            assert!(validate_git_url(u).is_err(), "{}", u);
        }
    }

    #[test]
    fn infers_name_from_https() {
        assert_eq!(
            infer_marketplace_name_from_url("https://gitcode.com/u/foo.git").unwrap(),
            "foo"
        );
    }

    #[test]
    fn infers_name_from_ssh_shorthand() {
        assert_eq!(
            infer_marketplace_name_from_url("git@github.com:o/bar.git").unwrap(),
            "bar"
        );
    }

    #[test]
    fn infers_name_without_dot_git() {
        assert_eq!(
            infer_marketplace_name_from_url("https://x.com/u/baz").unwrap(),
            "baz"
        );
    }

    #[test]
    fn strips_only_one_dot_git_suffix_when_inferring_name() {
        assert_eq!(
            infer_marketplace_name_from_url("https://x.com/u/foo.git.git").unwrap(),
            "foo.git"
        );
    }
}

#[cfg(test)]
mod trusted_host_tests {
    use super::{host_is_trusted, scheme_host_prefix};

    #[test]
    fn trusted_hosts_and_subdomains() {
        assert!(host_is_trusted("https://gitcode.com/o/r"));
        assert!(host_is_trusted("https://atomgit.com/o/r"));
        assert!(host_is_trusted("https://www.gitcode.com/o/r"));
        assert!(host_is_trusted("https://x.atomgit.com/o/r.git"));
    }

    #[test]
    fn untrusted_or_malformed_is_false() {
        assert!(!host_is_trusted("https://github.com/o/r"));
        assert!(!host_is_trusted("https://gitcode.com.evil.com/o/r"));
        assert!(!host_is_trusted("git@gitcode.com:o/r")); // ssh shorthand, not a parseable URL host
        assert!(!host_is_trusted("not a url"));
    }

    #[test]
    fn scheme_host_prefix_strips_path() {
        assert_eq!(
            scheme_host_prefix("https://gitcode.com/owner/repo.git").as_deref(),
            Some("https://gitcode.com")
        );
        assert_eq!(scheme_host_prefix("git@gitcode.com:o/r"), None);
    }
}
