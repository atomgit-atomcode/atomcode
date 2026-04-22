use anyhow::{anyhow, Result};

/// Parsed coordinates of an AtomGit issue URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueRef {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl IssueRef {
    /// Parse `https://atomgit.com/{owner}/{repo}/issues/{number}`.
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
        if !host.eq_ignore_ascii_case("atomgit.com") {
            return Err(anyhow!(
                "only atomgit.com issue URLs are supported (got host {})",
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
}
