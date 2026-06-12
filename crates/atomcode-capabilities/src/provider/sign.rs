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

/// Supplies per-attempt auth for a request. See the module docs.
pub trait RequestSigner: Send + Sync {
    /// Produce auth for one attempt over the given request body.
    fn sign(&self, body: &[u8]) -> SignedAuth;
}
