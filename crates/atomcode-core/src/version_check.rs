//! Passive "new version available" check.
//!
//! At startup, atomcode GETs a single-line `latest.txt` from the release
//! repo and, if the advertised version is strictly newer than what's
//! compiled in, surfaces a right-aligned hint on the input-box status
//! row. Any error (network, parse, non-matching format) silently returns
//! `None` — this feature must never be noisy.

/// Compare a `latest.txt` body against the current compiled-in version.
///
/// `current` and the body are both expected in `vMAJOR.MINOR.PATCH` form.
/// Returns `Some(latest)` only when the body parses cleanly AND is
/// strictly greater than `current`. Returns `None` for same-or-older
/// versions, malformed bodies, HTML responses, or any other noise.
pub fn parse_and_compare(current: &str, body: &str) -> Option<String> {
    let latest = parse_version_line(body)?;
    let current = parse_version_line(current)?;
    if latest > current {
        Some(format_version(latest))
    } else {
        None
    }
}

fn parse_version_line(s: &str) -> Option<(u64, u64, u64)> {
    let trimmed = s.trim();
    if trimmed.len() > 32 {
        return None;
    }
    let rest = trimmed.strip_prefix('v')?;
    let mut parts = rest.split('.');
    let major = parts.next()?.parse::<u64>().ok()?;
    let minor = parts.next()?.parse::<u64>().ok()?;
    let patch = parts.next()?.parse::<u64>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn format_version(v: (u64, u64, u64)) -> String {
    format!("v{}.{}.{}", v.0, v.1, v.2)
}

/// Fetch `latest.txt` from the release repo and, if newer than `current`,
/// return the advertised version. Any error (network, HTTP, parse)
/// returns `None` silently.
pub async fn check_latest(current: &str) -> Option<String> {
    const URL: &str =
        "https://raw.gitcode.com/atomgit_atomcode/atomcode/raw/main/latest.txt";
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .user_agent(format!("atomcode-version-check/{}", current))
        .build()
        .ok()?;
    let resp = client.get(URL).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.text().await.ok()?;
    parse_and_compare(current, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_patch_version_returns_some() {
        assert_eq!(
            parse_and_compare("v4.15.3", "v4.15.4\n"),
            Some("v4.15.4".to_string())
        );
    }

    #[test]
    fn newer_minor_version_returns_some() {
        assert_eq!(
            parse_and_compare("v4.15.3", "v4.16.0"),
            Some("v4.16.0".to_string())
        );
    }

    #[test]
    fn newer_major_version_returns_some() {
        assert_eq!(
            parse_and_compare("v4.15.3", "v5.0.0"),
            Some("v5.0.0".to_string())
        );
    }

    #[test]
    fn same_version_returns_none() {
        assert_eq!(parse_and_compare("v4.15.3", "v4.15.3"), None);
    }

    #[test]
    fn older_version_returns_none() {
        assert_eq!(parse_and_compare("v4.15.3", "v4.15.2"), None);
        assert_eq!(parse_and_compare("v4.15.3", "v4.14.99"), None);
        assert_eq!(parse_and_compare("v4.15.3", "v3.99.99"), None);
    }

    #[test]
    fn html_body_returns_none() {
        assert_eq!(parse_and_compare("v4.15.3", "<html>404</html>"), None);
    }

    #[test]
    fn empty_body_returns_none() {
        assert_eq!(parse_and_compare("v4.15.3", ""), None);
        assert_eq!(parse_and_compare("v4.15.3", "   \n"), None);
    }

    #[test]
    fn missing_v_prefix_returns_none() {
        assert_eq!(parse_and_compare("v4.15.3", "4.15.4"), None);
    }

    #[test]
    fn non_numeric_segment_returns_none() {
        assert_eq!(parse_and_compare("v4.15.3", "v4.15.x"), None);
        assert_eq!(parse_and_compare("v4.15.3", "vX.Y.Z"), None);
    }

    #[test]
    fn too_many_components_returns_none() {
        assert_eq!(parse_and_compare("v4.15.3", "v4.15.4.1"), None);
    }

    #[test]
    fn too_few_components_returns_none() {
        assert_eq!(parse_and_compare("v4.15.3", "v4.15"), None);
    }

    #[test]
    fn overly_long_body_returns_none() {
        let long_body = "v".to_string() + &"1".repeat(40);
        assert_eq!(parse_and_compare("v4.15.3", &long_body), None);
    }

    #[test]
    fn trims_whitespace() {
        assert_eq!(
            parse_and_compare("v4.15.3", "  v4.16.0\r\n"),
            Some("v4.16.0".to_string())
        );
    }

    #[test]
    fn malformed_current_returns_none() {
        assert_eq!(parse_and_compare("bad", "v4.16.0"), None);
    }
}
