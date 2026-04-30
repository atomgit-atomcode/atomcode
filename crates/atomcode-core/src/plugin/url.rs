use anyhow::{anyhow, Result};

/// Accept https / http / ssh / git@host: / file scheme. Reject everything else.
pub fn validate_git_url(url: &str) -> Result<()> {
    let ok = url.starts_with("https://")
        || url.starts_with("http://")
        || url.starts_with("ssh://")
        || url.starts_with("file://")
        || url.starts_with("git@");
    if ok {
        Ok(())
    } else {
        Err(anyhow!("unsupported git url scheme: {}", url))
    }
}

/// Extract the last path segment from a git URL, stripping `.git` suffix.
/// Examples:
///   https://gitcode.com/u/foo.git → foo
///   git@github.com:o/bar         → bar
pub fn infer_marketplace_name_from_url(url: &str) -> Result<String> {
    let trimmed = url.trim_end_matches('/').trim_end_matches(".git");
    let last = trimmed
        .rsplit(|c: char| c == '/' || c == ':')
        .next()
        .filter(|s| !s.is_empty())
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
}
