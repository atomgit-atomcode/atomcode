//! Process-wide TLS-version policy.
//!
//! Some networks run a middlebox that resets TLS 1.3 handshakes at the connection
//! layer (`os error 10054` / "connection reset" on Windows) while allowing TLS 1.2 —
//! observed in the wild against `*.atomgit.com`. rustls (our TLS backend) negotiates
//! TLS 1.3 by default, so the login / codingplan / provider clients get RST before any
//! HTTP is exchanged. Capping those clients at TLS 1.2 gets the handshake through.
//!
//! This module is pure (no reqwest): an explicit [`MAX_ENV`] override is global,
//! while automatic fallback is scoped to first-party endpoints and is only
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
///
/// `warrants_fallback` is the caller's judgement that THIS failure class is one
/// a TLS-1.2 downgrade could actually cure. Two shapes qualify, both owned by
/// the caller (this fn only adds the managed-and-uncapped gate):
///   - a connection-establishment failure (`is_connect()`) — a TLS-1.3-hostile
///     middlebox resetting the handshake;
///   - a post-handshake TLS record corruption (`BadRecordMac`/`DecryptError`) —
///     the same middlebox mangling records once the connection has run a while.
///     (Note this one has no `is_connect` requirement — it lands after the
///     handshake succeeds.)
pub fn should_try_fallback(url: &str, was_capped: bool, warrants_fallback: bool) -> bool {
    warrants_fallback && !was_capped && is_managed_https_url(url)
}

/// Match only HTTPS service hosts we operate. Delegates to the deployment's
/// trusted-domain set so a self-hosted backend gets the same fallback coverage
/// the vendor domain used to get; see [`crate::endpoints::is_managed_https_url`]
/// for the label-aware matching that rejects lookalike hosts.
pub fn is_managed_https_url(raw: &str) -> bool {
    crate::endpoints::is_managed_https_url(raw)
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
        // Derived from the configured set rather than naming a vendor: a build
        // whose `HOSTED_*` values were replaced must still hold this.
        let Some(domain) = crate::endpoints::tls_fallback_domains().first() else {
            return; // nothing configured — nothing to match
        };
        assert!(is_managed_https_url(&format!("https://{domain}")));
        assert!(is_managed_https_url(&format!(
            "https://sub.{domain}/some/path"
        )));
        assert!(!is_managed_https_url(&format!("https://evil{domain}")));
        assert!(!is_managed_https_url(&format!(
            "https://{domain}.evil.example"
        )));
    }

    #[test]
    fn non_https_and_custom_hosts_are_not_managed() {
        if let Some(domain) = crate::endpoints::tls_fallback_domains().first() {
            assert!(!is_managed_https_url(&format!("http://{domain}")));
        }
        assert!(!is_managed_https_url("https://api.openai.com/v1"));
        assert!(!is_managed_https_url("not a url"));
    }

    #[test]
    fn fallback_requires_managed_uncapped_connect_failure() {
        let Some(domain) = crate::endpoints::tls_fallback_domains().first() else {
            return; // nothing configured — the gate can never open
        };
        let managed = format!("https://{domain}/v1/chat/completions");
        let managed = managed.as_str();
        assert!(should_try_fallback(managed, false, true));
        assert!(!should_try_fallback(managed, true, true));
        assert!(!should_try_fallback(managed, false, false));
        assert!(!should_try_fallback(
            "https://api.openai.com/v1/chat/completions",
            false,
            true
        ));
    }

    #[test]
    fn fallback_gate_is_independent_of_which_failure_class_warrants_it() {
        // The managed-and-uncapped gate is the same whether the trigger is a
        // connect failure or a post-handshake corruption — the caller decides
        // the class, this fn only gates on endpoint + cap. A corruption trigger
        // (warrants_fallback=true) on a managed uncapped endpoint qualifies with
        // NO is_connect involved.
        let Some(domain) = crate::endpoints::tls_fallback_domains().first() else {
            return; // nothing configured — the gate can never open
        };
        let managed = format!("https://{domain}/v1/chat/completions");
        let managed = managed.as_str();
        assert!(should_try_fallback(managed, false, true));
        // Already capped at 1.2 → nothing lower to escalate to.
        assert!(!should_try_fallback(managed, true, true));
        // Never auto-downgrade a third-party endpoint we don't operate.
        assert!(!should_try_fallback(
            "https://api.openai.com/v1/chat/completions",
            false,
            true
        ));
    }
}
