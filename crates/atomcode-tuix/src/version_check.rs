//! Passive "new version available" check.
//!
//! At startup, atomcode GETs `latest.json` (the same manifest `/upgrade`
//! consumes) and, if the advertised version is strictly newer than what's
//! compiled in, surfaces a right-aligned hint on the input-box status
//! row. Any error (network, parse, non-matching format) silently returns
//! `None` — this feature must never be noisy.

use atomcode_updater::{manifest_url, Manifest};

/// Compare a `latest.json` body against the current compiled-in version.
///
/// `current` is expected in `vMAJOR.MINOR.PATCH` form; `body` is the raw
/// JSON manifest. Returns `Some(latest)` only when the body deserializes
/// cleanly AND its `version` is strictly greater than `current`. Returns
/// `None` for same-or-older versions, malformed JSON, HTML responses, or
/// any other noise.
pub fn parse_and_compare(current: &str, body: &str) -> Option<String> {
    let manifest: Manifest = serde_json::from_str(body).ok()?;
    let latest = parse_version_line(&manifest.version)?;
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
    // Strip pre-release suffix (e.g. "-beta.1", "-rc.2") so versions
    // like "v4.25.0-beta.1" parse correctly (Issue #596).
    let rest = rest.split('-').next()?;
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

/// Apply the process proxy policy to an async reqwest builder. Self-contained
/// over the config leaf's proxy env machinery — mirrors the identical per-layer
/// helpers in `atomcode-capabilities`/`atomcode-telemetry`.
fn apply_async_proxy_policy(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    atomcode_config::proxy::ensure_runtime_initialized();
    let builder = if std::env::var(atomcode_config::proxy::MODE_ENV)
        .ok()
        .as_deref()
        == Some(atomcode_config::proxy::ProxyMode::NoProxy.as_str())
    {
        builder.no_proxy()
    } else {
        builder
    };
    if atomcode_config::tls::env_forces_tls12() {
        builder.max_tls_version(reqwest::tls::Version::TLS_1_2)
    } else {
        builder
    }
}

/// Fetch `latest.json` and, if newer than `current`, return the advertised
/// version. Short timeout keeps startup snappy; any error (network, HTTP,
/// parse) returns `None` silently.
pub async fn check_latest(current: &str) -> Option<String> {
    let client = apply_async_proxy_policy(reqwest::Client::builder())
        .timeout(std::time::Duration::from_secs(5))
        .user_agent(atomcode_auth::ATOMCODE_USER_AGENT)
        .build()
        .ok()?;
    let resp = client.get(manifest_url()).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.text().await.ok()?;
    parse_and_compare(current, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest_body(version: &str) -> String {
        format!(
            r#"{{"version":"{}","binaries":{{"darwin-arm64":{{"sha256":"abcd","size":1024}}}}}}"#,
            version
        )
    }

    #[test]
    fn newer_patch_version_returns_some() {
        assert_eq!(
            parse_and_compare("v4.15.3", &manifest_body("v4.15.4")),
            Some("v4.15.4".to_string())
        );
    }

    #[test]
    fn newer_minor_version_returns_some() {
        assert_eq!(
            parse_and_compare("v4.15.3", &manifest_body("v4.16.0")),
            Some("v4.16.0".to_string())
        );
    }

    #[test]
    fn newer_major_version_returns_some() {
        assert_eq!(
            parse_and_compare("v4.15.3", &manifest_body("v5.0.6")),
            Some("v5.0.6".to_string())
        );
    }

    #[test]
    fn same_version_returns_none() {
        assert_eq!(
            parse_and_compare("v4.15.3", &manifest_body("v4.15.3")),
            None
        );
    }

    #[test]
    fn older_version_returns_none() {
        assert_eq!(
            parse_and_compare("v4.15.3", &manifest_body("v4.15.2")),
            None
        );
        assert_eq!(
            parse_and_compare("v4.15.3", &manifest_body("v4.14.99")),
            None
        );
        assert_eq!(
            parse_and_compare("v4.15.3", &manifest_body("v3.99.99")),
            None
        );
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
        assert_eq!(parse_and_compare("v4.15.3", &manifest_body("4.15.4")), None);
    }

    #[test]
    fn non_numeric_segment_returns_none() {
        assert_eq!(
            parse_and_compare("v4.15.3", &manifest_body("v4.15.x")),
            None
        );
        assert_eq!(parse_and_compare("v4.15.3", &manifest_body("vX.Y.Z")), None);
    }

    #[test]
    fn too_many_components_returns_none() {
        assert_eq!(
            parse_and_compare("v4.15.3", &manifest_body("v4.15.4.1")),
            None
        );
    }

    #[test]
    fn too_few_components_returns_none() {
        assert_eq!(parse_and_compare("v4.15.3", &manifest_body("v4.15")), None);
    }

    #[test]
    fn missing_version_field_returns_none() {
        let body = r#"{"binaries":{"darwin-arm64":{"sha256":"abcd","size":1024}}}"#;
        assert_eq!(parse_and_compare("v4.15.3", body), None);
    }

    #[test]
    fn missing_binaries_field_returns_none() {
        // Manifest requires `binaries`; missing it fails deserialization.
        let body = r#"{"version":"v4.16.0"}"#;
        assert_eq!(parse_and_compare("v4.15.3", body), None);
    }

    #[test]
    fn trims_whitespace_in_version() {
        let body = r#"{"version":"  v4.16.0\r\n","binaries":{"darwin-arm64":{"sha256":"abcd","size":1024}}}"#;
        assert_eq!(
            parse_and_compare("v4.15.3", body),
            Some("v4.16.0".to_string())
        );
    }

    #[test]
    fn malformed_current_returns_none() {
        assert_eq!(parse_and_compare("bad", &manifest_body("v4.16.0")), None);
    }

    #[test]
    fn prerelease_version_parses_correctly() {
        // Issue #596: pre-release versions must not return None
        assert_eq!(
            parse_and_compare("v4.15.3", &manifest_body("v4.16.0-beta.1")),
            Some("v4.16.0".to_string())
        );
    }

    #[test]
    fn prerelease_current_parses_correctly() {
        // Current version with pre-release suffix should also parse
        assert_eq!(
            parse_and_compare("v4.15.0-beta.2", &manifest_body("v4.16.0")),
            Some("v4.16.0".to_string())
        );
    }

    #[test]
    fn prerelease_same_version_returns_none() {
        // v4.16.0-beta.1 vs v4.16.0 → not newer (pre-release == release)
        assert_eq!(
            parse_and_compare("v4.16.0-beta.1", &manifest_body("v4.16.0")),
            None
        );
    }

    #[test]
    fn ignores_unknown_json_fields() {
        let body = r#"{
            "version": "v4.16.0",
            "released_at": "2026-04-20T00:00:00Z",
            "signature": "future-field",
            "binaries": {"darwin-arm64": {"sha256": "abcd", "size": 1024}}
        }"#;
        assert_eq!(
            parse_and_compare("v4.15.3", body),
            Some("v4.16.0".to_string())
        );
    }
}
