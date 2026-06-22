// Public interface for per-request signing of AtomGit LLM gateway calls.
//
// Open-source build (`codingplan-crypto` feature off — the default):
// `signer()` returns `UnavailableSigner`, so any AtomGit-bound request
// fails-fast with a localised "official build required" hint.
//
// Official build (`codingplan-crypto` feature on): `signer()` returns
// `RealSigner`, a thin pass-through to `atomcode_codingplan_crypto::sign_v1`.
// The closed-source crate owns all wire-format details (body hashing,
// header names, hex encoding, canonical-message construction, master
// secret, HMAC primitive); the wrapper here only marshals trait inputs
// into primitive-typed arguments and returns the resulting headers
// unchanged. The public source tree carries a stub crate at
// `crates/atomcode-codingplan-crypto/` with the same API surface; the
// official build pipeline replaces that directory with the private
// overlay before turning the feature on.

use thiserror::Error;

/// Sign a single outbound request. The body stays plaintext; the impl
/// returns the headers the caller must merge onto the outbound
/// `reqwest::RequestBuilder`.
pub trait RequestSigner: Send + Sync {
    fn sign(&self, req: SignInput<'_>) -> Result<SignOutput, SignError>;
    /// One-byte selector identifying which signing scheme the impl
    /// emits. `0` is reserved for `UnavailableSigner`; real algorithms
    /// start at `1`.
    fn algorithm_version(&self) -> u8;
}

pub struct SignInput<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub body: &'a [u8],
    pub oauth_token: &'a str,
    pub user_id: &'a str,
    pub timestamp_unix: u64,
    pub nonce: [u8; 16],
}

#[derive(Debug)]
pub struct SignOutput {
    pub headers: Vec<(&'static str, String)>,
}

#[derive(Debug, Error)]
pub enum SignError {
    #[error("signer unavailable in this build")]
    Unavailable,
    #[error("signing-key derivation failed: {0}")]
    Derive(String),
}

/// Zero-sized stub. Always errors with `Unavailable`.
pub struct UnavailableSigner;

impl RequestSigner for UnavailableSigner {
    fn sign(&self, _req: SignInput<'_>) -> Result<SignOutput, SignError> {
        Err(SignError::Unavailable)
    }
    fn algorithm_version(&self) -> u8 {
        0
    }
}

#[cfg(not(feature = "codingplan-crypto"))]
static UNAVAILABLE_SIGNER: UnavailableSigner = UnavailableSigner;

/// Accessor used by every caller. Returns `UnavailableSigner` in the
/// open-source build; with `codingplan-crypto` on, returns `RealSigner`
/// which forwards into the closed-source `atomcode-codingplan-crypto`
/// crate.
#[cfg(not(feature = "codingplan-crypto"))]
pub fn signer() -> &'static dyn RequestSigner {
    &UNAVAILABLE_SIGNER
}

#[cfg(feature = "codingplan-crypto")]
struct RealSigner;

#[cfg(feature = "codingplan-crypto")]
impl RequestSigner for RealSigner {
    fn sign(&self, req: SignInput<'_>) -> Result<SignOutput, SignError> {
        // env!() expands at compile time to atomcode-core's package
        // version (which inherits version.workspace = true, so it
        // equals the atomcode binary's version). Threading it in here
        // — rather than through SignInput — keeps the call sites in
        // provider/openai.rs unaware of the version-binding mechanism.
        Ok(SignOutput {
            headers: atomcode_codingplan_crypto::sign_v1(
                req.method,
                req.path,
                req.body,
                req.oauth_token,
                req.user_id,
                req.timestamp_unix,
                &req.nonce,
                env!("CARGO_PKG_VERSION"),
            ),
        })
    }

    fn algorithm_version(&self) -> u8 {
        atomcode_codingplan_crypto::ALGORITHM_VERSION
    }
}

#[cfg(feature = "codingplan-crypto")]
static REAL_SIGNER: RealSigner = RealSigner;

#[cfg(feature = "codingplan-crypto")]
pub fn signer() -> &'static dyn RequestSigner {
    &REAL_SIGNER
}

/// True iff this build can actually produce a signature. Lets callers
/// fail-fast with `CpOfficialBuildRequired` BEFORE walking auth state
/// or hitting the network, instead of either (a) discovering it only
/// after `sign()` returns `Unavailable` (current `build_codingplan_headers`
/// — but its error is then swallowed by `resign`'s `unwrap_or_default`)
/// or (b) surfacing the misleading `CpAuthRequired` to an open-source
/// user whose auth is empty.
#[cfg(feature = "codingplan-crypto")]
pub fn signer_available() -> bool {
    true
}

#[cfg(not(feature = "codingplan-crypto"))]
pub fn signer_available() -> bool {
    false
}

/// True iff the given base URL points at an AtomGit-operated LLM
/// gateway that REQUIRES per-request signing.
///
/// Host-based — does NOT trust provider config keys. Rejects non-
/// HTTP(S) schemes and subdomain spoofs.
///
/// Both production hostnames sign:
///   * `pre-llm-api-cce.atomgit.com` — current dedicated host.
///   * `api-ai.gitcode.com`  — pre-P3 host. Kept signing-enforced
///     so any legacy provider config silently upgraded to signing
///     after the P3 cutover; users with `api-ai.gitcode.com` in their
///     config get the same protection as `pre-llm-api-cce.atomgit.com` users
///     and don't need to edit their config to migrate.
pub fn is_atomgit_gateway(base_url: &str) -> bool {
    let url = match url::Url::parse(base_url) {
        Ok(u) => u,
        Err(_) => return false,
    };
    match url.scheme() {
        "https" | "http" => {}
        _ => return false,
    }
    matches!(
        url.host_str(),
        Some("llm-api.atomgit.com") | Some("pre-llm-api-cce.atomgit.com") | Some("api-ai.gitcode.com")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_signer_returns_unavailable_error() {
        let s = UnavailableSigner;
        let input = SignInput {
            method: "POST",
            path: "/v1/chat/completions",
            body: b"{}",
            oauth_token: "any-token",
            user_id: "user-1",
            timestamp_unix: 1_700_000_000,
            nonce: [0u8; 16],
        };
        let err = s.sign(input).expect_err("UnavailableSigner must error");
        assert!(matches!(err, SignError::Unavailable));
    }

    #[test]
    fn unavailable_signer_reports_algorithm_version_zero() {
        let s = UnavailableSigner;
        assert_eq!(s.algorithm_version(), 0);
    }

    #[cfg(not(feature = "codingplan-crypto"))]
    #[test]
    fn signer_available_reports_false_in_open_source_build() {
        assert!(!signer_available());
    }

    #[cfg(feature = "codingplan-crypto")]
    #[test]
    fn signer_available_reports_true_in_official_build() {
        assert!(signer_available());
    }

    #[cfg(not(feature = "codingplan-crypto"))]
    #[test]
    fn default_signer_in_open_source_build_is_unavailable() {
        let input = SignInput {
            method: "POST",
            path: "/v1/chat/completions",
            body: b"{}",
            oauth_token: "any-token",
            user_id: "user-1",
            timestamp_unix: 1_700_000_000,
            nonce: [0u8; 16],
        };
        let err = signer().sign(input).expect_err("open-source must error");
        assert!(matches!(err, SignError::Unavailable));
    }

    #[test]
    fn is_atomgit_gateway_matches_official_host() {
        assert!(is_atomgit_gateway("https://llm-api.atomgit.com/v1"));
        assert!(is_atomgit_gateway("https://llm-api.atomgit.com/v1/chat/completions"));
        assert!(is_atomgit_gateway("https://pre-llm-api-cce.atomgit.com/v1"));
        assert!(is_atomgit_gateway("https://pre-llm-api-cce.atomgit.com/v1/chat/completions"));
    }

    #[test]
    fn is_atomgit_gateway_matches_legacy_codingplan_host() {
        // Post-P3 cutover: `api-ai.gitcode.com` is now also a signing-
        // enforced gateway. Previously this test asserted the opposite
        // (legacy host plaintext until P3) — the inversion is the
        // contract change.
        assert!(is_atomgit_gateway("https://api-ai.gitcode.com/v1"));
        assert!(is_atomgit_gateway("https://api-ai.gitcode.com/v1/chat/completions"));
    }

    #[test]
    fn is_atomgit_gateway_rejects_third_party_hosts() {
        assert!(!is_atomgit_gateway("https://api.anthropic.com"));
        assert!(!is_atomgit_gateway("https://api.openai.com/v1"));
        assert!(!is_atomgit_gateway("http://localhost:11434"));
    }

    #[test]
    fn is_atomgit_gateway_rejects_subdomains_and_lookalikes() {
        assert!(!is_atomgit_gateway("https://pre-llm-api-cce.atomgit.com.evil.example"));
        assert!(!is_atomgit_gateway("https://evil.pre-llm-api-cce.atomgit.com"));
        assert!(!is_atomgit_gateway("https://atomgit.com"));
    }

    #[test]
    fn is_atomgit_gateway_rejects_malformed_input() {
        assert!(!is_atomgit_gateway(""));
        assert!(!is_atomgit_gateway("not a url"));
        assert!(!is_atomgit_gateway("ftp://pre-llm-api-cce.atomgit.com"));
    }
}
