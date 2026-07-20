//! NEUTRAL per-request auth seam for OpenAI-compatible providers.
//!
//! Some gateways derive request auth from the request itself rather than from a
//! static bearer key. This trait lets a specialization supply that per-attempt; the
//! neutral provider stays unaware of the scheme. `request_signer = None` (the
//! default) keeps the plain `bearer_auth(api_key)` path unchanged.

/// Per-attempt auth material.
#[derive(Clone, Debug, Default)]
pub struct SignedAuth {
    /// Bearer token to use instead of the config api_key. `None` ⇒ keep the api_key.
    pub bearer: Option<String>,
    /// Extra headers to attach to the request.
    pub headers: Vec<(String, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestSigningError {
    CredentialsUnavailable(String),
    SigningFailed(String),
}

impl RequestSigningError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::CredentialsUnavailable(_) => "authentication_unavailable",
            Self::SigningFailed(_) => "request_signing_failed",
        }
    }
}

impl std::fmt::Display for RequestSigningError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CredentialsUnavailable(message) | Self::SigningFailed(message) => {
                f.write_str(message)
            }
        }
    }
}

impl std::error::Error for RequestSigningError {}

/// Supplies per-attempt auth for a request. See the module docs.
pub trait RequestSigner: Send + Sync {
    /// Produce auth for one attempt over the given request body.
    fn sign(&self, body: &[u8]) -> Result<SignedAuth, RequestSigningError>;
}
