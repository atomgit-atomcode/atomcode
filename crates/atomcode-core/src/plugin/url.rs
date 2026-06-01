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

    let parsed = Url::parse(url)
        .map_err(|_| anyhow!("unsupported or malformed git url: {}", url))?;
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
    url.rsplit(|c: char| c == '/' || c == ':')
        .next()
        .filter(|s| !s.is_empty())
}

/// Extract the last path segment from a git URL, stripping `.git` suffix.
/// Examples:
///   https://gitcode.com/u/foo.git → foo
///   git@github.com:o/bar         → bar
pub fn infer_marketplace_name_from_url(url: &str) -> Result<String> {
    let trimmed = normalize_name_source(url);
    let last = last_path_segment(trimmed)
        .ok_or_else(|| anyhow!("cannot infer name from url: {}", url))?;
    Ok(last.to_string())
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
