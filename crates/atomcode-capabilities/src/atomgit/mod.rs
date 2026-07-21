//! AtomGit REST tools (L1): an async `AtomgitClient` + serde models backing the
//! `atomgit_repo` / `atomgit_pr` / `atomgit_issue` agent tools.
//!
//! # Layering & auth
//!
//! This is an L1 capability — it depends ONLY on the kernel and cannot reach
//! `atomcode-core::auth` (L2, compile-enforced). The OAuth token is therefore
//! **injected** via [`TokenProvider`], which the embedder implements over its own
//! auth (e.g. `core::auth::get_valid_token`). The token is fetched fresh per request,
//! so a provider that auto-refreshes keeps long sessions working.
//!
//! # API contract
//!
//! Mirrors the `ag-cli` tool: base `https://api.atomgit.com/api/v5`, plain
//! `Authorization: Bearer <token>` on every endpoint. AtomGit's gate rejects the
//! default reqwest UA, so a custom UA (e.g. `atomcode/<ver>`) is required — supplied
//! via [`AtomgitConfig::user_agent`].

pub mod client;
pub mod issue;
pub mod models;
pub mod pr;
pub mod push_label_mw;
pub mod remote;
pub mod repo;

pub use client::AtomgitClient;

use std::sync::Arc;

/// AtomGit's production API base. The embedder may override via [`AtomgitConfig`].
pub const DEFAULT_BASE_URL: &str = "https://api.atomgit.com/api/v5";

/// Supplies a currently-valid OAuth bearer token. Implemented by the embedder (L2),
/// where the real auth store lives. Kept dyn-compatible (object-safe) so the client
/// can hold `Arc<dyn TokenProvider>`.
pub trait TokenProvider: Send + Sync {
    /// Return a valid bearer token, refreshing if needed. `Err` is a user-facing
    /// message (e.g. "not logged in — run `atomcode login`").
    fn token(&self) -> Result<String, String>;
}

/// Construction inputs for [`AtomgitClient`].
pub struct AtomgitConfig {
    /// API base, no trailing slash. Defaults to [`DEFAULT_BASE_URL`].
    pub base_url: String,
    /// User-Agent header (AtomGit rejects the default reqwest UA).
    pub user_agent: String,
    /// Token source (see [`TokenProvider`]).
    pub token: Arc<dyn TokenProvider>,
}

/// Live token provider — reads/refreshes the OAuth token from auth.toml.
/// NOTE: `token()` is sync and (rarely) does network I/O on refresh; callers on
/// the hot async path should prefer prefetching via spawn_blocking + StaticTokenProvider.
pub struct LiveTokenProvider;
impl TokenProvider for LiveTokenProvider {
    fn token(&self) -> Result<String, String> {
        atomcode_auth::oauth::get_valid_token().map_err(|e| format!("{e:#}"))
    }
}

/// A pre-fetched, fixed token (used by the push-label middleware, which fetches
/// once via spawn_blocking so `token()` never blocks the runtime).
pub struct StaticTokenProvider(pub String);
impl TokenProvider for StaticTokenProvider {
    fn token(&self) -> Result<String, String> {
        Ok(self.0.clone())
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::TokenProvider;
    /// A fixed token, for tests.
    pub struct StaticToken(pub &'static str);
    impl TokenProvider for StaticToken {
        fn token(&self) -> Result<String, String> {
            Ok(self.0.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::StaticToken;
    use super::TokenProvider;

    #[test]
    fn static_token_returns_its_value() {
        assert_eq!(StaticToken("abc").token().unwrap(), "abc");
    }
}
