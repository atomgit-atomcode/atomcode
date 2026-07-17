use std::sync::Arc;

use atomcode_auth::gateway_crypto::{self, SignInput};
use atomcode_auth::oauth::{get_stored_auth, get_valid_token};

use super::{RequestSigner, SignedAuth};

struct AtomGitRequestSigner {
    path: String,
    user_id: String,
    fallback_token: String,
}

impl RequestSigner for AtomGitRequestSigner {
    fn sign(&self, body: &[u8]) -> SignedAuth {
        let token = get_valid_token().unwrap_or_else(|_| self.fallback_token.clone());
        let mut nonce = [0u8; 16];
        if getrandom::getrandom(&mut nonce).is_err() {
            return SignedAuth {
                bearer: Some(token),
                headers: Vec::new(),
            };
        }
        let timestamp_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        let headers = match gateway_crypto::signer().sign(SignInput {
            method: "POST",
            path: &self.path,
            body,
            oauth_token: &token,
            user_id: &self.user_id,
            timestamp_unix,
            nonce,
        }) {
            Ok(output) => output
                .headers
                .into_iter()
                .map(|(name, value)| (name.to_string(), value))
                .collect(),
            Err(error) => {
                tracing::warn!(%error, "AtomGit request signing failed");
                Vec::new()
            }
        };
        SignedAuth {
            bearer: Some(token),
            headers,
        }
    }
}

pub fn atomgit_request_signer(base_url: &str) -> Result<Arc<dyn RequestSigner>, String> {
    let auth = get_stored_auth()
        .ok_or_else(|| "AtomGit gateway requires login — run `/login` first".to_string())?;
    if auth.user.id.is_empty() || auth.access_token.is_empty() {
        return Err("AtomGit gateway requires login — run `/login` first".to_string());
    }
    Ok(Arc::new(AtomGitRequestSigner {
        path: gateway_crypto::canonical_chat_completions_path(base_url),
        user_id: auth.user.id,
        fallback_token: auth.access_token,
    }))
}

pub use gateway_crypto::{is_atomgit_gateway, signer_available};
