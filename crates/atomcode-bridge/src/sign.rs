//! Gateway request auth for the bridge.
//!
//! No secret lives here: the scheme itself is in the closed-source
//! `atomcode-codingplan-crypto`, reached only through core's `crypto::signer()`.
//! Whether real auth is produced is gated by core's `codingplan-crypto` feature —
//! off (open-source) ⇒ the caller ([`crate::runtime::build_provider`]) attaches
//! nothing; on (the official build) ⇒ the closed crate handles it.

use std::sync::Arc;

use atomcode_capabilities::provider::{RequestSigner, SignedAuth};
use atomcode_core::auth::oauth::{get_stored_auth, get_valid_token};
use atomcode_core::coding_plan::crypto::{self, SignInput};

struct GatewaySigner {
    path: String,
    user_id: String,
    /// Resolves the OAuth token FRESH on every request rather than snapshotting it at
    /// build time. In production this is `get_valid_token` (auto-refresh near expiry,
    /// and picks up a token a later `/login` wrote to disk). Boxed so it stays testable
    /// without touching the real auth file. See [`atomgit_signer`].
    token: Box<dyn Fn() -> String + Send + Sync>,
}

impl RequestSigner for GatewaySigner {
    fn sign(&self, body: &[u8]) -> SignedAuth {
        let token = (self.token)();
        let mut nonce = [0u8; 16];
        if getrandom::getrandom(&mut nonce).is_err() {
            return SignedAuth { bearer: Some(token), headers: Vec::new() };
        }
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let headers = match crypto::signer().sign(SignInput {
            method: "POST",
            path: &self.path,
            body,
            oauth_token: &token,
            user_id: &self.user_id,
            timestamp_unix: ts,
            nonce,
        }) {
            Ok(out) => out.headers.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
            Err(_) => Vec::new(),
        };
        SignedAuth { bearer: Some(token), headers }
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
    // Resolve the token per request via `get_valid_token` (auto-refreshing near expiry).
    // This is what lets a long-lived signer recover after `/login` without a rebuild —
    // the daemon LiveSession bridge never gets `/login`'s ReloadConfig, so a snapshotted
    // token would stay stale forever and every synced chat would 401. Fall back to the
    // build-time token if a later lookup transiently fails, so signing never hard-fails.
    //
    // Deliberate tradeoff: `get_valid_token` is disk-only in the common case, but within
    // 300s of expiry it does ONE blocking HTTP refresh on the request thread (≈once per
    // token cycle; repeated only if the OAuth broker is down). We accept that brief stall
    // — it matches the codebase's existing "get_valid_token on every authenticated API
    // call" pattern, and it keeps the chat path SELF-HEALING. A disk-only `get_stored_auth`
    // would avoid the stall but, if nothing else ever refreshes the token, would let an
    // idle session's token expire with no recovery — re-introducing this very bug.
    let fallback = auth.access_token;
    Ok(Arc::new(GatewaySigner {
        path: canonical_path(base_url),
        user_id: auth.user.id,
        token: Box::new(move || get_valid_token().unwrap_or_else(|_| fallback.clone())),
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

    // The signer must resolve the token FRESH on every `sign()`, not snapshot it at
    // build time. Otherwise a long-lived signer (the daemon LiveSession bridge, which
    // `/login`'s ReloadConfig never reaches) keeps signing with a stale/expired token →
    // gateway 401 → the user is told to `/login` but it never helps.
    #[test]
    fn sign_resolves_token_fresh_per_request_not_snapshot() {
        use std::sync::{Arc, Mutex};
        let tok = Arc::new(Mutex::new("old-token".to_string()));
        let tok_for_signer = tok.clone();
        let signer = GatewaySigner {
            path: "/v1/chat/completions".to_string(),
            user_id: "u1".to_string(),
            token: Box::new(move || tok_for_signer.lock().unwrap().clone()),
        };
        assert_eq!(signer.sign(b"body").bearer.as_deref(), Some("old-token"));
        // Simulate /login (or an auto-refresh) writing a fresh token AFTER the signer
        // was built — the next request must pick it up.
        *tok.lock().unwrap() = "new-token".to_string();
        assert_eq!(
            signer.sign(b"body").bearer.as_deref(),
            Some("new-token"),
            "sign() must read the token fresh each call, not the build-time snapshot"
        );
    }
}
