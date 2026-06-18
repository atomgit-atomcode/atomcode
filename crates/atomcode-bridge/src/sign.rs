//! Gateway request auth for the bridge.
//!
//! No secret lives here: the scheme itself is in the closed-source
//! `atomcode-codingplan-crypto`, reached only through core's `crypto::signer()`.
//! Whether real auth is produced is gated by core's `codingplan-crypto` feature —
//! off (open-source) ⇒ the caller ([`crate::runtime::build_provider`]) attaches
//! nothing; on (the official build) ⇒ the closed crate handles it.

use std::sync::Arc;

use atomcode_capabilities::provider::{RequestSigner, SignedAuth};
use atomcode_core::auth::oauth::get_stored_auth;
use atomcode_core::coding_plan::crypto::{self, SignInput};

struct GatewaySigner {
    path: String,
    user_id: String,
    token: String,
}

impl RequestSigner for GatewaySigner {
    fn sign(&self, body: &[u8]) -> SignedAuth {
        let mut nonce = [0u8; 16];
        if getrandom::getrandom(&mut nonce).is_err() {
            return SignedAuth { bearer: Some(self.token.clone()), headers: Vec::new() };
        }
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let headers = match crypto::signer().sign(SignInput {
            method: "POST",
            path: &self.path,
            body,
            oauth_token: &self.token,
            user_id: &self.user_id,
            timestamp_unix: ts,
            nonce,
        }) {
            Ok(out) => out.headers.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            Err(_) => Vec::new(),
        };
        SignedAuth { bearer: Some(self.token.clone()), headers }
    }
}

/// Build the gateway signer for `base_url`, reading the stored identity. `Err` if the
/// user is not logged in. The caller MUST only invoke this when
/// [`crypto::signer_available`] is true.
pub fn atomgit_signer(base_url: &str) -> anyhow::Result<Arc<dyn RequestSigner>> {
    let auth = get_stored_auth()
        .ok_or_else(|| anyhow::anyhow!("AtomGit gateway requires login — run `/login` first"))?;
    if auth.user.id.is_empty() || auth.access_token.is_empty() {
        anyhow::bail!("AtomGit gateway requires login — run `/login` first");
    }
    Ok(Arc::new(GatewaySigner {
        path: canonical_path(base_url),
        user_id: auth.user.id,
        token: auth.access_token,
    }))
}

fn canonical_path(base_url: &str) -> String {
    let path = url::Url::parse(base_url)
        .ok()
        .map(|u| u.path().to_string())
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
    fn canonical_path_appends_chat_completions() {
        assert_eq!(canonical_path("https://llm-api.atomgit.com/v1"), "/v1/chat/completions");
        assert_eq!(
            canonical_path("https://llm-api.atomgit.com/v1/chat/completions"),
            "/v1/chat/completions"
        );
        assert_eq!(canonical_path("https://llm-api.atomgit.com"), "/chat/completions");
    }
}
