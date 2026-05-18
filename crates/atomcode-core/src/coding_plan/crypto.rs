// Public interface for per-request signing of AtomGit LLM gateway calls.
//
// Open-source repo: signer() always returns UnavailableSigner, so any
// AtomGit-bound request fails-fast with a localised "official build
// required" hint. The official build pipeline patches this file at CI
// time to delegate signer() to the real implementation.

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

static UNAVAILABLE_SIGNER: UnavailableSigner = UnavailableSigner;

/// Accessor used by every caller. Always returns `UnavailableSigner`
/// in the public-repo source. The official build pipeline patches this
/// function to delegate to a real implementation.
pub fn signer() -> &'static dyn RequestSigner {
    &UNAVAILABLE_SIGNER
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
}
