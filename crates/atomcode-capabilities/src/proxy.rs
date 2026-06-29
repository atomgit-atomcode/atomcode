//! Proxy policy for L1 outbound HTTP clients.
//!
//! `atomcode-capabilities` must not depend on `atomcode-core` (L1 layering is
//! compile-enforced — see Cargo.toml), so it cannot call core's proxy module.
//! Instead it honors the process-global `ATOMCODE_PROXY_MODE` env var that the
//! CLI / daemon publish at startup via
//! `atomcode_core::proxy::apply_process_proxy_config` (which runs before any v2
//! client is built). This mirrors the identical self-contained helper in
//! `atomcode-telemetry` (`sender/http.rs`), which sits below core for the same
//! layering reason.
//!
//! Default = bypass. `no_proxy` is the app's documented default, so when the
//! mode is `no_proxy` OR the env var is **unset** we call `.no_proxy()` on the
//! builder — the unset case matters because a host that never published the var
//! (a standalone embedder, or `cargo test`, where `apply_process_proxy_config`
//! never runs) must still fall back to the NoProxy default rather than silently
//! honoring the developer's ambient `HTTP_PROXY`/`ALL_PROXY`. This matches v1's
//! `apply_async_proxy_policy`, which lazily applies the same NoProxy default via
//! `ensure_runtime_initialized()` on an unset var. For the explicit
//! `follow_system` / `default_proxy` modes the builder is returned untouched and
//! reqwest's default env-based proxy detection picks up the published vars.

use std::env;

/// Process proxy mode env var, published by `apply_process_proxy_config` at startup.
const MODE_ENV: &str = "ATOMCODE_PROXY_MODE";
/// The one explicit mode value that means "bypass all proxies".
const NO_PROXY_MODE: &str = "no_proxy";

/// Whether outbound clients must bypass all proxies, given the published proxy
/// mode (`None` = env var unset). Pure so it is testable without mutating the
/// process environment. Bypass when the mode is explicitly `no_proxy` OR unset
/// (the unset case falls back to the app's documented NoProxy default).
fn mode_disables_proxy(mode: Option<&str>) -> bool {
    matches!(mode, None | Some(NO_PROXY_MODE))
}

/// Whether outbound clients must bypass all proxies for the current process.
fn proxy_disabled() -> bool {
    mode_disables_proxy(env::var(MODE_ENV).ok().as_deref())
}

/// Apply the process proxy policy to an async reqwest client builder.
pub(crate) fn apply_async_proxy_policy(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    if proxy_disabled() {
        builder.no_proxy()
    } else {
        builder
    }
}

/// Apply the process proxy policy to a blocking reqwest client builder
/// (the one-shot MCP OAuth login / refresh flow).
#[cfg(feature = "mcp")]
pub(crate) fn apply_blocking_proxy_policy(
    builder: reqwest::blocking::ClientBuilder,
) -> reqwest::blocking::ClientBuilder {
    if proxy_disabled() {
        builder.no_proxy()
    } else {
        builder
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Pure predicate test — no process-env mutation, so it cannot race or leak
    // state into the sibling tests that build reqwest clients in this binary.
    #[test]
    fn mode_disables_proxy_for_no_proxy_and_unset() {
        assert!(
            mode_disables_proxy(Some(NO_PROXY_MODE)),
            "explicit no_proxy bypasses"
        );
        assert!(
            mode_disables_proxy(None),
            "unset falls back to the NoProxy default (bypass)"
        );
        assert!(
            !mode_disables_proxy(Some("follow_system")),
            "follow_system keeps proxy"
        );
        assert!(
            !mode_disables_proxy(Some("default_proxy")),
            "default_proxy keeps proxy"
        );
    }
}
