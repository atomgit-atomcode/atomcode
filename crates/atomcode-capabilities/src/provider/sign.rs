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
    /// Stable account identity used to reject an accidental account switch while
    /// recovering a failed request. Plain API-key signers leave this unset.
    pub account_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RequestSigningError {
    CredentialsUnavailable(String),
    SigningFailed(String),
    RecoveryTransient(String),
    ReauthenticationRequired(String),
}

impl RequestSigningError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::CredentialsUnavailable(_) => "authentication_unavailable",
            Self::SigningFailed(_) => "request_signing_failed",
            Self::RecoveryTransient(_) => "authentication_refresh_transient",
            Self::ReauthenticationRequired(_) => "authentication_expired",
        }
    }
}

impl std::fmt::Display for RequestSigningError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CredentialsUnavailable(message)
            | Self::SigningFailed(message)
            | Self::RecoveryTransient(message)
            | Self::ReauthenticationRequired(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for RequestSigningError {}

/// Supplies per-attempt auth for a request. See the module docs.
#[async_trait::async_trait]
pub trait RequestSigner: Send + Sync {
    /// Produce auth for one attempt over the given request body.
    fn sign(&self, body: &[u8]) -> Result<SignedAuth, RequestSigningError>;

    /// Give a refreshable signer one bounded chance to recover after the server
    /// rejects auth. `false` means this signer does not support recovery.
    async fn recover_unauthorized(
        &self,
        _rejected: &SignedAuth,
    ) -> Result<bool, RequestSigningError> {
        Ok(false)
    }
}
