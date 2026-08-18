//! Every server address the client talks to, resolved in one place.
//!
//! These addresses used to be `const`s spread across eight crates, so running
//! against anything other than the hosted service meant editing source in eight
//! places. Each one keeps its current value as the default and gains an env
//! override.
//!
//! # Resolution
//!
//! Each address is its own variable holding a **complete URL** — there is
//! deliberately no single "domain" knob that the rest are derived from. A
//! self-hosted deployment is not obliged to mirror the hosted service's
//! subdomain layout: its auth broker might be `sso.corp.example`, its gateway
//! `ai.corp.example/v1`, its mirror an IP and port. Deriving those from one
//! root would only work for a deployment that copied our naming.
//!
//! With nothing set, every address is byte-identical to the const it replaced —
//! [`tests::nothing_configured_keeps_the_hosted_addresses`] pins that, so this
//! is additive and carries no behaviour change.
//!
//! # Scope
//!
//! Addresses, plus the two settings that only make sense beside them:
//! [`relay_enabled`] (a deployment with no relay to reach) and
//! [`codingplan_provider_prefix`] (the name its CodingPlan entries carry).
//!
//! Being one module is the point: a distribution retargets a build by
//! replacing this file, so anything it needs to change belongs here rather
//! than scattered across the crates that consume it.

use std::sync::OnceLock;

/// Address overrides. Each takes a complete URL.
pub const PLATFORM_SERVER_ENV: &str = "ATOMCODE_PLATFORM_SERVER";
pub const CODINGPLAN_API_BASE_ENV: &str = "ATOMCODE_CODINGPLAN_API_BASE";
pub const CODINGPLAN_LLM_BASE_URL_ENV: &str = "ATOMCODE_CODINGPLAN_LLM_BASE_URL";
pub const UPDATE_MANIFEST_URL_ENV: &str = "ATOMCODE_UPDATE_MANIFEST_URL";
pub const UPDATE_DOWNLOAD_BASE_ENV: &str = "ATOMCODE_UPDATE_DOWNLOAD_BASE";
pub const DESKTOP_DOWNLOAD_URL_ENV: &str = "ATOMCODE_DESKTOP_DOWNLOAD_URL";
pub const RELAY_URL_ENV: &str = "ATOMCODE_APP_RELAY";

/// Marketplace git URLs, comma-separated. Replaces the default list; an
/// explicitly empty value registers none.
pub const PLUGIN_MARKETPLACES_ENV: &str = "ATOMCODE_PLUGIN_MARKETPLACES";

/// Which of [`PLUGIN_MARKETPLACES_ENV`] are force-installed, comma-separated.
/// Replaces the default; an explicitly empty value installs none.
pub const PLUGIN_AUTO_INSTALL_ENV: &str = "ATOMCODE_PLUGIN_AUTO_INSTALL";

/// Whether `/app` remote access is offered. Defaults to on.
pub const ENABLE_RELAY_ENV: &str = "ATOMCODE_ENABLE_RELAY";

/// Overrides the prefix CodingPlan provider keys are written with.
pub const CODINGPLAN_PROVIDER_PREFIX_ENV: &str = "ATOMCODE_CODINGPLAN_PROVIDER_PREFIX";

/// Hosts to treat as first-party, comma-separated. **Replaces** the default
/// set rather than adding to it: a deployment that has moved off the hosted
/// service should stop trusting it, not accumulate both.
pub const TRUSTED_HOSTS_ENV: &str = "ATOMCODE_TRUSTED_HOSTS";

// ---------------------------------------------------------------------------
// Hosted-service addresses
//
// The values these call sites used to hardcode. They remain the default, so a
// build with nothing configured behaves exactly as before.
// ---------------------------------------------------------------------------

const HOSTED_PLATFORM_SERVER: &str = "https://acs.atomgit.com";
const HOSTED_CODINGPLAN_API_BASE: &str = "https://api.gitcode.com/api/v5";
const HOSTED_CODINGPLAN_LLM_BASE_URL: &str = "https://llm-api.atomgit.com/v1";
const HOSTED_UPDATE_MANIFEST_URL: &str =
    "https://raw.atomgit.com/atomgit_atomcode/atomcode/raw/main/latest.json";
const HOSTED_UPDATE_DOWNLOAD_BASE: &str =
    "https://atomgit.com/atomgit_atomcode/atomcode/releases/download";
const HOSTED_DESKTOP_DOWNLOAD_URL: &str =
    "https://atomgit.com/atomgit_atomcode/atomCode-air-releases/releases";
const HOSTED_RELAY_URL: &str = "https://relay-atomcode.atomgit.com";
const HOSTED_MARKETPLACES: &[&str] = &[
    "https://atomgit.com/atomgit_atomcode/atomcode-plugins-official.git",
    "https://atomgit.com/atomgit_atomcode/atomcode-skills.git",
];
const HOSTED_AUTO_INSTALL: &[&str] = &["https://atomgit.com/atomgit_atomcode/atomcode-skills.git"];
const HOSTED_TRUSTED_DOMAINS: &[&str] = &["atomgit.com", "gitcode.com"];
/// Whether `/app` remote access is offered when nothing overrides it.
const HOSTED_RELAY_ENABLED: bool = true;
/// Prefix for CodingPlan provider keys. User-visible: it is the selection id in
/// the model picker's left column and the account label in its right one, so a
/// build serving its own gateway wants it to say something else.
const HOSTED_CODINGPLAN_PROVIDER_PREFIX: &str = "AtomGit";
/// Narrower than [`HOSTED_TRUSTED_DOMAINS`], and deliberately so: the TLS-1.2
/// fallback has only ever applied to `api.gitcode.com`, not to gitcode.com at
/// large. Listing the full host here matches that exactly — it has no
/// subdomains of its own for the suffix rule to widen.
const HOSTED_TLS_FALLBACK_DOMAINS: &[&str] = &["atomgit.com", "api.gitcode.com"];

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Read `key`, treating whitespace-only as unset and stripping a trailing `/`
/// so callers can concatenate `"{base}/path"` without doubling the separator.
fn env_url(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().trim_end_matches('/').to_string())
        .filter(|v| !v.is_empty())
}

/// Resolve one address: the override if set, else the hosted default.
///
/// Every accessor caches its result, so within one process all URL-derived work
/// targets the same host even if the environment mutates mid-flight — a login
/// flow that resolved a host at step 1 must not land on a different box at
/// step 3.
fn resolve(key: &str, hosted: &str) -> String {
    env_url(key).unwrap_or_else(|| hosted.to_string())
}

/// Parse a boolean switch. Anything unrecognised is `None` so the caller keeps
/// its own default rather than guessing at a typo's intent.
fn env_bool(key: &str) -> Option<bool> {
    let raw = std::env::var(key).ok()?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Split a comma-separated env list, trimming entries and dropping blanks.
fn split_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// A list-valued setting: the env value replaces the default when the variable
/// is present, including when it is empty.
fn resolve_list(key: &str, hosted: &[&str]) -> Vec<String> {
    match std::env::var(key) {
        Ok(raw) => split_list(&raw),
        Err(_) => hosted.iter().map(|s| s.to_string()).collect(),
    }
}

// ---------------------------------------------------------------------------
// Addresses
// ---------------------------------------------------------------------------

/// Auth broker base — serves `/auth/login`, `/auth/check`, `/auth/token` and
/// `/oauth/refresh`. The client secret stays on the broker.
pub fn platform_server() -> &'static str {
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| resolve(PLATFORM_SERVER_ENV, HOSTED_PLATFORM_SERVER))
}

/// CodingPlan REST control plane, including the API version segment.
pub fn codingplan_api_base() -> &'static str {
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| resolve(CODINGPLAN_API_BASE_ENV, HOSTED_CODINGPLAN_API_BASE))
}

/// OpenAI-compatible LLM gateway for CodingPlan-managed providers. The
/// `models-v2` payload may override this per model.
pub fn codingplan_llm_base_url() -> &'static str {
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| resolve(CODINGPLAN_LLM_BASE_URL_ENV, HOSTED_CODINGPLAN_LLM_BASE_URL))
}

/// Whether `base_url` is one of the authenticated CodingPlan LLM gateways.
///
/// Keep this classification in the leaf config crate so config projection,
/// TUI management and the auth signer cannot disagree about which accounts are
/// product-managed. Only HTTPS is accepted because these hosts carry OAuth
/// credentials and request signatures.
pub fn is_codingplan_llm_gateway(base_url: &str) -> bool {
    let Ok(url) = url::Url::parse(base_url) else {
        return false;
    };
    if url.scheme() != "https" {
        return false;
    }
    if matches!(
        url.host_str(),
        Some("llm-api.atomgit.com")
            | Some("pre-llm-api-cce.atomgit.com")
            | Some("api-ai.gitcode.com")
    ) {
        return true;
    }

    // A distribution may retarget the official gateway through the endpoint
    // override. Match its HTTPS origin too, without widening trust to sibling
    // hosts or a plaintext variant.
    url::Url::parse(codingplan_llm_base_url())
        .ok()
        .filter(|configured| configured.scheme() == "https")
        .is_some_and(|configured| {
            url.host_str() == configured.host_str()
                && url.port_or_known_default() == configured.port_or_known_default()
        })
}

/// Version manifest (`latest.json`) for self-update.
pub fn update_manifest_url() -> &'static str {
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| resolve(UPDATE_MANIFEST_URL_ENV, HOSTED_UPDATE_MANIFEST_URL))
}

/// Base for release binary downloads; the updater appends `/<version>/<asset>`.
pub fn update_download_base() -> &'static str {
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| resolve(UPDATE_DOWNLOAD_BASE_ENV, HOSTED_UPDATE_DOWNLOAD_BASE))
}

/// Landing page the `/desktop` command points users at.
pub fn desktop_download_url() -> &'static str {
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| resolve(DESKTOP_DOWNLOAD_URL_ENV, HOSTED_DESKTOP_DOWNLOAD_URL))
}

/// Relay base URL. Only meaningful when [`relay_enabled`] is true.
pub fn relay_url() -> &'static str {
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| resolve(RELAY_URL_ENV, HOSTED_RELAY_URL))
}

/// Whether `/app` remote access is offered.
///
/// On by default, so this changes nothing unless asked. A deployment with no
/// relay of its own sets [`ENABLE_RELAY_ENV`] to `0`, which is better than
/// letting `/app` reach an unrelated relay and download a client binary from a
/// host the operator does not run.
pub fn relay_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| env_bool(ENABLE_RELAY_ENV).unwrap_or(HOSTED_RELAY_ENABLED))
}

/// Prefix that new CodingPlan entries are written with.
///
/// Recognition of already-written keys is a separate question owned by
/// `config::is_codingplan_provider_name`, which keeps accepting the historical
/// prefix whatever this returns — so changing it does not orphan an existing
/// `config.toml`.
pub fn codingplan_provider_prefix() -> &'static str {
    static PREFIX: OnceLock<String> = OnceLock::new();
    PREFIX.get_or_init(|| {
        std::env::var(CODINGPLAN_PROVIDER_PREFIX_ENV)
            .ok()
            .and_then(|raw| normalize_codingplan_prefix(&raw))
            .unwrap_or_else(|| HOSTED_CODINGPLAN_PROVIDER_PREFIX.to_string())
    })
}

/// Accept only what survives as a TOML bare key, since the prefix becomes one.
/// A rejected value falls back to the default rather than producing a config
/// file that cannot be re-read.
fn normalize_codingplan_prefix(raw: &str) -> Option<String> {
    let prefix = raw.trim();
    let is_bare_key = !prefix.is_empty()
        && prefix
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    is_bare_key.then(|| prefix.to_string())
}

/// Git URLs of the marketplaces registered on first run, in order.
pub fn plugin_marketplaces() -> &'static [String] {
    static URLS: OnceLock<Vec<String>> = OnceLock::new();
    URLS.get_or_init(|| resolve_list(PLUGIN_MARKETPLACES_ENV, HOSTED_MARKETPLACES))
}

/// Subset of [`plugin_marketplaces`] whose plugins are force-installed.
pub fn plugin_auto_install() -> &'static [String] {
    static URLS: OnceLock<Vec<String>> = OnceLock::new();
    URLS.get_or_init(|| resolve_list(PLUGIN_AUTO_INSTALL_ENV, HOSTED_AUTO_INSTALL))
}

// ---------------------------------------------------------------------------
// Host trust
// ---------------------------------------------------------------------------

/// A domain list, lowercased with blanks dropped.
fn normalized_list(key: &str, hosted: &[&str]) -> Vec<String> {
    resolve_list(key, hosted)
        .into_iter()
        .map(|d| d.trim_matches('.').to_ascii_lowercase())
        .filter(|d| !d.is_empty())
        .collect()
}

/// Domains whose hosts may receive an OAuth token. Each matches itself and its
/// subdomains.
pub fn trusted_domains() -> &'static [String] {
    static DOMAINS: OnceLock<Vec<String>> = OnceLock::new();
    DOMAINS.get_or_init(|| normalized_list(TRUSTED_HOSTS_ENV, HOSTED_TRUSTED_DOMAINS))
}

/// Whether `host` is `domain` itself or a subdomain of it.
///
/// Label-aware on purpose: the `.` in the suffix check rejects
/// `evilatomgit.com`, and requiring `domain` to be a *suffix* rejects
/// `atomgit.com.attacker.test`.
pub fn host_matches_domain(host: &str, domain: &str) -> bool {
    // No trailing-dot normalization: `url` already lowercases a parsed host but
    // keeps `example.com.` distinct, and upstream treated that as untrusted.
    // Widening a trust check is not a change to make in passing.
    let host = host.to_ascii_lowercase();
    let domain = domain.to_ascii_lowercase();
    host == domain || host.ends_with(&format!(".{}", domain))
}

/// Whether `host` belongs to the deployment. Gates OAuth-token injection into
/// git remotes — never widen without weighing token leakage.
pub fn is_trusted_host(host: &str) -> bool {
    trusted_domains()
        .iter()
        .any(|d| host_matches_domain(host, d))
}

/// Hosts eligible for the automatic TLS-1.2 downgrade retry. Follows
/// [`TRUSTED_HOSTS_ENV`] when set; otherwise the narrower hosted set.
pub fn tls_fallback_domains() -> &'static [String] {
    static DOMAINS: OnceLock<Vec<String>> = OnceLock::new();
    DOMAINS.get_or_init(|| normalized_list(TRUSTED_HOSTS_ENV, HOSTED_TLS_FALLBACK_DOMAINS))
}

/// Whether `raw` is an HTTPS URL on a host we operate.
///
/// HTTPS is mandatory: these URLs carry bearer credentials, so classifying a
/// plaintext URL as first-party would let a misconfigured base URL leak a token.
pub fn is_managed_https_url(raw: &str) -> bool {
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };
    if url.scheme() != "https" {
        return false;
    }
    url.host_str().is_some_and(|host| {
        tls_fallback_domains()
            .iter()
            .any(|d| host_matches_domain(host, d))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The accessors read process env once and cache, so the env-dependent paths
    // are covered through the pure helpers rather than by mutating a variable a
    // parallel test may already have observed.

    /// Skipped when the process has an override set — the claim is about the
    /// unconfigured default, and a harness that sets one is testing something
    /// else. A build that replaces the `HOSTED_*` values still runs it, and
    /// still passes: both sides of each assertion move together.
    #[test]
    fn nothing_configured_keeps_the_hosted_addresses() {
        if std::env::var(PLATFORM_SERVER_ENV).is_ok() {
            return;
        }
        // The central promise of this module: it is additive. With no variable
        // set, every address is byte-identical to the const it replaced.
        assert_eq!(platform_server(), HOSTED_PLATFORM_SERVER);
        assert_eq!(codingplan_api_base(), HOSTED_CODINGPLAN_API_BASE);
        assert_eq!(codingplan_llm_base_url(), HOSTED_CODINGPLAN_LLM_BASE_URL);
        assert_eq!(update_manifest_url(), HOSTED_UPDATE_MANIFEST_URL);
        assert_eq!(update_download_base(), HOSTED_UPDATE_DOWNLOAD_BASE);
        assert_eq!(desktop_download_url(), HOSTED_DESKTOP_DOWNLOAD_URL);
        assert_eq!(relay_url(), HOSTED_RELAY_URL);
        assert_eq!(plugin_marketplaces(), HOSTED_MARKETPLACES);
        assert_eq!(plugin_auto_install(), HOSTED_AUTO_INSTALL);
        assert_eq!(
            codingplan_provider_prefix(),
            HOSTED_CODINGPLAN_PROVIDER_PREFIX
        );
        assert_eq!(relay_enabled(), HOSTED_RELAY_ENABLED);
    }

    #[test]
    fn codingplan_gateway_matching_is_https_host_based() {
        for url in [
            "https://llm-api.atomgit.com/v1",
            "https://pre-llm-api-cce.atomgit.com/v1/chat/completions",
            "https://api-ai.gitcode.com/v1",
        ] {
            assert!(is_codingplan_llm_gateway(url), "expected gateway: {url}");
        }
        for url in [
            "http://llm-api.atomgit.com/v1",
            "https://llm-api.atomgit.com.evil.example/v1",
            "https://api.openai.com/v1",
            "not a url",
        ] {
            assert!(!is_codingplan_llm_gateway(url), "expected external: {url}");
        }
    }

    #[test]
    fn an_override_is_taken_whole_not_derived() {
        // A deployment need not mirror the hosted subdomain layout.
        std::env::set_var(
            "ATOMCODE_TEST_EP_WHOLE",
            "https://sso.corp.example:8443/auth",
        );
        assert_eq!(
            resolve("ATOMCODE_TEST_EP_WHOLE", "https://hosted.test"),
            "https://sso.corp.example:8443/auth"
        );
        std::env::remove_var("ATOMCODE_TEST_EP_WHOLE");
    }

    #[test]
    fn an_unset_override_falls_back_to_the_hosted_address() {
        assert_eq!(
            resolve("ATOMCODE_TEST_EP_ABSENT", "https://hosted.test"),
            "https://hosted.test"
        );
    }

    #[test]
    fn env_url_strips_trailing_slash_and_treats_blank_as_unset() {
        std::env::set_var("ATOMCODE_TEST_EP_URL", "https://x.test/api/v4/");
        assert_eq!(
            env_url("ATOMCODE_TEST_EP_URL").as_deref(),
            Some("https://x.test/api/v4")
        );
        std::env::set_var("ATOMCODE_TEST_EP_URL", "   ");
        assert_eq!(env_url("ATOMCODE_TEST_EP_URL"), None);
        std::env::remove_var("ATOMCODE_TEST_EP_URL");
    }

    #[test]
    fn a_present_list_replaces_the_default_and_may_be_empty() {
        std::env::set_var("ATOMCODE_TEST_EP_LIST", "a.git, b.git");
        assert_eq!(
            resolve_list("ATOMCODE_TEST_EP_LIST", &["hosted.git"]),
            vec!["a.git".to_string(), "b.git".to_string()]
        );
        // Present-but-empty means "none", not "fall back to the default" —
        // this is how a deployment opts out of marketplaces entirely.
        std::env::set_var("ATOMCODE_TEST_EP_LIST", "");
        assert!(resolve_list("ATOMCODE_TEST_EP_LIST", &["hosted.git"]).is_empty());
        std::env::remove_var("ATOMCODE_TEST_EP_LIST");
        assert_eq!(
            resolve_list("ATOMCODE_TEST_EP_LIST", &["hosted.git"]),
            vec!["hosted.git".to_string()]
        );
    }

    #[test]
    fn bool_switch_accepts_common_spellings_and_ignores_junk() {
        std::env::set_var("ATOMCODE_TEST_EP_BOOL", "ON");
        assert_eq!(env_bool("ATOMCODE_TEST_EP_BOOL"), Some(true));
        std::env::set_var("ATOMCODE_TEST_EP_BOOL", "0");
        assert_eq!(env_bool("ATOMCODE_TEST_EP_BOOL"), Some(false));
        std::env::set_var("ATOMCODE_TEST_EP_BOOL", "maybe");
        assert_eq!(env_bool("ATOMCODE_TEST_EP_BOOL"), None);
        std::env::remove_var("ATOMCODE_TEST_EP_BOOL");
    }

    #[test]
    fn list_split_trims_and_drops_blanks() {
        assert_eq!(
            split_list(" a.git , , b.git ,"),
            vec!["a.git".to_string(), "b.git".to_string()]
        );
        assert!(split_list("").is_empty());
    }

    #[test]
    fn a_prefix_must_survive_as_a_toml_bare_key() {
        assert_eq!(
            normalize_codingplan_prefix("Longyuan").as_deref(),
            Some("Longyuan")
        );
        assert_eq!(
            normalize_codingplan_prefix("  Longyuan  ").as_deref(),
            Some("Longyuan")
        );
        assert_eq!(
            normalize_codingplan_prefix("ly_gw-1").as_deref(),
            Some("ly_gw-1")
        );
    }

    #[test]
    fn a_prefix_that_would_break_the_config_file_is_rejected() {
        // Each would produce a key TOML cannot round-trip, leaving a config
        // that no longer parses — fall back to the default instead.
        for bad in ["", "   ", "Long Yuan", "long.yuan", "[ly]", "ly=1", "ly\"q"] {
            assert_eq!(normalize_codingplan_prefix(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn host_matching_is_label_aware() {
        assert!(host_matches_domain("atomgit.com", "atomgit.com"));
        assert!(host_matches_domain("acs.atomgit.com", "atomgit.com"));
        assert!(host_matches_domain("a.b.corp.example", "corp.example"));
        // `url` lowercases a parsed host; matching is case-insensitive anyway.
        assert!(host_matches_domain("ACS.AtomGit.Com", "atomgit.com"));
        // A trailing root dot is a distinct host string and stays untrusted,
        // exactly as before this module existed.
        assert!(!host_matches_domain("atomgit.com.", "atomgit.com"));
        // Lookalike prefix — the '.' in the suffix check rejects it.
        assert!(!host_matches_domain("evilatomgit.com", "atomgit.com"));
        // Suffix-position attack.
        assert!(!host_matches_domain(
            "atomgit.com.attacker.test",
            "atomgit.com"
        ));
    }

    // Host assertions derive from the configured sets rather than naming a
    // vendor, so they hold in a build whose `HOSTED_*` values were replaced.

    #[test]
    fn a_host_outside_the_configured_sets_is_never_managed() {
        // Holds for any configuration, including an empty one.
        assert!(!is_managed_https_url("https://api.openai.com/v1"));
        assert!(!is_managed_https_url("not a url"));
        assert!(!is_trusted_host("api.openai.com"));
    }

    #[test]
    fn a_configured_fallback_host_is_managed_only_over_https() {
        let Some(domain) = tls_fallback_domains().first() else {
            return; // nothing configured — nothing to assert
        };
        assert!(is_managed_https_url(&format!("https://{domain}/some/path")));
        // Plaintext never qualifies: these URLs carry a bearer token.
        assert!(!is_managed_https_url(&format!("http://{domain}")));
        // Lookalikes on either side of the host are rejected.
        assert!(!is_managed_https_url(&format!("https://evil{domain}")));
        assert!(!is_managed_https_url(&format!(
            "https://{domain}.attacker.test"
        )));
    }

    #[test]
    fn every_configured_trusted_domain_is_trusted() {
        for domain in trusted_domains() {
            assert!(is_trusted_host(domain), "{domain}");
            assert!(is_trusted_host(&format!("sub.{domain}")), "{domain}");
            assert!(!is_trusted_host(&format!("evil{domain}")), "{domain}");
        }
    }

    #[test]
    fn tls_fallback_is_never_wider_than_token_trust() {
        // The retry set is scoped more tightly than token trust on purpose;
        // sharing one list between them would silently widen it.
        for domain in tls_fallback_domains() {
            assert!(
                is_trusted_host(domain),
                "{domain} takes the TLS retry but is not trusted"
            );
        }
    }
}
