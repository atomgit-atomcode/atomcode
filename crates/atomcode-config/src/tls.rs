//! Process-wide TLS-version policy.
//!
//! Some networks run a middlebox that resets TLS 1.3 handshakes at the connection
//! layer (`os error 10054` / "connection reset" on Windows) while allowing TLS 1.2 —
//! observed in the wild against `*.atomgit.com`. rustls (our TLS backend) negotiates
//! TLS 1.3 by default, so the login / codingplan / provider clients get RST before any
//! HTTP is exchanged. Capping those clients at TLS 1.2 gets the handshake through.
//!
//! This module is pure (no reqwest): an explicit [`MAX_ENV`] override is global,
//! while automatic fallback is scoped to AtomGit-managed endpoints and is only
//! latched after a TLS-1.2 retry succeeds.

use std::sync::atomic::{AtomicBool, Ordering};

/// Env override: `ATOMCODE_TLS_MAX=1.2` caps outbound TLS at 1.2 from process start.
/// An escape hatch for users on a TLS-1.3-hostile network (works before any request)
/// and a way to skip the first-request reset+retry the auto-fallback would otherwise do.
pub const MAX_ENV: &str = "ATOMCODE_TLS_MAX";

/// Latched once [`latch_managed_tls12`] fires; never cleared for the process lifetime
/// (a TLS-1.3-hostile path does not heal mid-session, and re-probing 1.3 on every
/// managed-service client would re-incur the reset).
static MANAGED_TLS12: AtomicBool = AtomicBool::new(false);

/// Latch a TLS 1.2 ceiling for AtomGit-managed endpoints for the rest of the
/// process. Call only after a TLS-1.2 fallback request has succeeded.
pub fn latch_managed_tls12() {
    MANAGED_TLS12.store(true, Ordering::Relaxed);
}

/// Whether the user explicitly requested a process-wide TLS 1.2 ceiling.
pub fn env_forces_tls12() -> bool {
    std::env::var(MAX_ENV)
        .ok()
        .as_deref()
        .map(value_requests_tls12)
        .unwrap_or(false)
}

/// Whether a managed endpoint has already proven that TLS 1.2 is required.
pub fn managed_tls12_latched() -> bool {
    MANAGED_TLS12.load(Ordering::Relaxed)
}

/// Whether a client for `url` should start capped at TLS 1.2.
///
/// The explicit env override is intentionally global. Automatic state applies
/// only to HTTPS endpoints owned by the managed AtomGit/CodingPlan service.
pub fn should_cap_url(url: &str) -> bool {
    env_forces_tls12() || (managed_tls12_latched() && is_managed_https_url(url))
}

/// Whether a failed request is eligible for one TLS-1.2 fallback attempt.
pub fn should_try_fallback(url: &str, was_capped: bool, is_connect: bool) -> bool {
    is_connect && !was_capped && is_managed_https_url(url)
}

/// Match only HTTPS service hosts we operate. The label-aware AtomGit check
/// deliberately rejects lookalikes such as `evilatomgit.com`.
pub fn is_managed_https_url(raw: &str) -> bool {
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };
    if url.scheme() != "https" {
        return false;
    }
    matches!(url.host_str(), Some("api.gitcode.com"))
        || url
            .host_str()
            .is_some_and(|host| host == "atomgit.com" || host.ends_with(".atomgit.com"))
}

/// Whether an `ATOMCODE_TLS_MAX` value asks for a TLS 1.2 ceiling. Forgiving of
/// surrounding whitespace and the common spellings (`1.2`, `TLSv1.2`, `TLS1.2`).
fn value_requests_tls12(raw: &str) -> bool {
    let v = raw.trim();
    v == "1.2" || v.eq_ignore_ascii_case("tlsv1.2") || v.eq_ignore_ascii_case("tls1.2")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_1_2_requests_the_cap() {
        assert!(value_requests_tls12("1.2"));
    }

    #[test]
    fn value_is_trimmed() {
        assert!(value_requests_tls12("  1.2 "));
    }

    #[test]
    fn common_tls_spellings_accepted() {
        assert!(value_requests_tls12("TLSv1.2"));
        assert!(value_requests_tls12("tls1.2"));
    }

    #[test]
    fn other_values_do_not_request_the_cap() {
        assert!(!value_requests_tls12("1.3"));
        assert!(!value_requests_tls12(""));
        assert!(!value_requests_tls12("1.20"));
        assert!(!value_requests_tls12("on"));
    }

    #[test]
    fn managed_hosts_are_matched_label_safely() {
        assert!(is_managed_https_url("https://atomgit.com"));
        assert!(is_managed_https_url("https://acs.atomgit.com/auth/login"));
        assert!(is_managed_https_url("https://api.gitcode.com/api/v5"));
        assert!(!is_managed_https_url("https://evilatomgit.com"));
        assert!(!is_managed_https_url(
            "https://acs.atomgit.com.evil.example"
        ));
    }

    #[test]
    fn non_https_and_custom_hosts_are_not_managed() {
        assert!(!is_managed_https_url("http://acs.atomgit.com"));
        assert!(!is_managed_https_url("https://api.openai.com/v1"));
        assert!(!is_managed_https_url("not a url"));
    }

    #[test]
    fn fallback_requires_managed_uncapped_connect_failure() {
        let managed = "https://llm-api.atomgit.com/v1/chat/completions";
        assert!(should_try_fallback(managed, false, true));
        assert!(!should_try_fallback(managed, true, true));
        assert!(!should_try_fallback(managed, false, false));
        assert!(!should_try_fallback(
            "https://api.openai.com/v1/chat/completions",
            false,
            true
        ));
    }
}
