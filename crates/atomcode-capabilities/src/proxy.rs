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
//! We call `.no_proxy()` ONLY when the mode is explicitly `no_proxy`. The app's
//! default is `follow_system` (respect the environment proxy — corporate-proxy
//! users would otherwise time out), so an **unset** env var (standalone embedder,
//! or `cargo test`, where `apply_process_proxy_config` never runs) falls back to
//! honoring the ambient proxy rather than bypassing it — matching core's
//! `apply_async_proxy_policy`, which lazily installs the same follow_system
//! default via `ensure_runtime_initialized()` on an unset var. For
//! `follow_system` / `default_proxy` the builder is returned untouched and
//! reqwest's default env-based proxy detection picks up the published vars.

use std::env;

/// Process proxy mode env var, published by `apply_process_proxy_config` at startup.
const MODE_ENV: &str = "ATOMCODE_PROXY_MODE";
/// The one explicit mode value that means "bypass all proxies".
const NO_PROXY_MODE: &str = "no_proxy";

/// Whether outbound clients must bypass all proxies, given the published proxy
/// mode (`None` = env var unset). Pure so it is testable without mutating the
/// process environment. Bypass ONLY when the mode is explicitly `no_proxy`; an
/// unset var falls back to the app's `follow_system` default (honor env proxy).
fn mode_disables_proxy(mode: Option<&str>) -> bool {
    matches!(mode, Some(NO_PROXY_MODE))
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
    fn mode_disables_proxy_only_for_explicit_no_proxy() {
        assert!(
            mode_disables_proxy(Some(NO_PROXY_MODE)),
            "explicit no_proxy bypasses"
        );
        assert!(
            !mode_disables_proxy(None),
            "unset falls back to follow_system (honor env proxy), not bypass"
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
