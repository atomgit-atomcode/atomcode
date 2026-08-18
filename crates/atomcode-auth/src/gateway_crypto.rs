//! AtomGit LLM gateway identification and request-signing primitives.
//!
//! This lives below core/bridge so every runtime and provider adapter shares one gateway
//! boundary. Official builds enable `codingplan-crypto`; source builds expose the same API but
//! return an unavailable signer.

use thiserror::Error;

pub trait RequestSigner: Send + Sync {
    fn sign(&self, req: SignInput<'_>) -> Result<SignOutput, SignError>;
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

#[cfg(not(feature = "codingplan-crypto"))]
pub fn signer() -> &'static dyn RequestSigner {
    &UNAVAILABLE_SIGNER
}

#[cfg(feature = "codingplan-crypto")]
struct RealSigner;

#[cfg(feature = "codingplan-crypto")]
impl RequestSigner for RealSigner {
    fn sign(&self, req: SignInput<'_>) -> Result<SignOutput, SignError> {
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

#[cfg(feature = "codingplan-crypto")]
pub fn signer_available() -> bool {
    true
}

#[cfg(not(feature = "codingplan-crypto"))]
pub fn signer_available() -> bool {
    false
}

pub fn is_atomgit_gateway(base_url: &str) -> bool {
    atomcode_config::endpoints::is_codingplan_llm_gateway(base_url)
}

pub fn canonical_chat_completions_path(base_url: &str) -> String {
    let path = url::Url::parse(base_url)
        .ok()
        .map(|url| url.path().to_string())
        .unwrap_or_else(|| "/v1/chat/completions".to_string());
    if path.ends_with("/chat/completions") {
        path
    } else {
        format!("{}/chat/completions", path.trim_end_matches('/'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_signer_is_explicit() {
        let error = UnavailableSigner
            .sign(SignInput {
                method: "POST",
                path: "/v1/chat/completions",
                body: b"{}",
                oauth_token: "token",
                user_id: "user",
                timestamp_unix: 1,
                nonce: [0; 16],
            })
            .expect_err("source signer must fail");
        assert!(matches!(error, SignError::Unavailable));
        assert_eq!(UnavailableSigner.algorithm_version(), 0);
    }

    #[test]
    fn gateway_matching_is_host_based() {
        for url in [
            "https://llm-api.atomgit.com/v1",
            "https://pre-llm-api-cce.atomgit.com/v1/chat/completions",
            "https://api-ai.gitcode.com/v1",
        ] {
            assert!(is_atomgit_gateway(url), "expected gateway: {url}");
        }
        for url in [
            "https://api.openai.com/v1",
            "http://llm-api.atomgit.com/v1",
            "http://pre-llm-api-cce.atomgit.com/v1",
            "http://api-ai.gitcode.com/v1",
            "https://pre-llm-api-cce.atomgit.com.evil.example",
            "https://evil.pre-llm-api-cce.atomgit.com",
            "ftp://pre-llm-api-cce.atomgit.com",
            "not a url",
        ] {
            assert!(!is_atomgit_gateway(url), "expected external: {url}");
        }
    }

    #[test]
    fn canonical_path_appends_chat_completions_once() {
        assert_eq!(
            canonical_chat_completions_path("https://llm-api.atomgit.com/v1"),
            "/v1/chat/completions"
        );
        assert_eq!(
            canonical_chat_completions_path("https://llm-api.atomgit.com/v1/chat/completions"),
            "/v1/chat/completions"
        );
    }

    #[cfg(not(feature = "codingplan-crypto"))]
    #[test]
    fn source_build_reports_signer_unavailable() {
        assert!(!signer_available());
    }
}
