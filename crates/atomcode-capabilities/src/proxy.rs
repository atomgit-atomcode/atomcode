//! Proxy policy for L1 outbound HTTP clients.
//!
//! It honors the process-global `ATOMCODE_PROXY_MODE` env var published from
//! `atomcode-config` policy by the CLI/daemon before clients are built. This
//! mirrors the self-contained helper in `atomcode-telemetry` (`sender/http.rs`).
//!
//! We call `.no_proxy()` ONLY when the mode is explicitly `no_proxy`. The app's
//! default is `follow_system` (respect the environment proxy — corporate-proxy
//! users would otherwise time out), so an **unset** env var (standalone embedder,
//! or `cargo test`, where `apply_process_proxy_config` never runs) falls back to
//! honoring the ambient proxy rather than bypassing it. For
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

/// The env override that caps outbound TLS at 1.2 (kept in sync with
/// `atomcode_config::tls::MAX_ENV` — hardcoded here for the same layering reason
/// `MODE_ENV` is: this module must build in config-less feature combos). Only read
/// on the config-less path; the `provider` build defers to `atomcode_config::tls`.
#[cfg(not(feature = "provider"))]
const TLS_MAX_ENV: &str = "ATOMCODE_TLS_MAX";

/// Whether the user explicitly requested a process-wide TLS 1.2 ceiling.
/// Automatic AtomGit fallback is endpoint-aware and lives in the provider.
fn force_tls12() -> bool {
    #[cfg(feature = "provider")]
    {
        atomcode_config::tls::env_forces_tls12()
    }
    #[cfg(not(feature = "provider"))]
    {
        env::var(TLS_MAX_ENV)
            .ok()
            .map(|raw| {
                let v = raw.trim();
                v == "1.2" || v.eq_ignore_ascii_case("tlsv1.2") || v.eq_ignore_ascii_case("tls1.2")
            })
            .unwrap_or(false)
    }
}

/// Apply the process proxy policy to an async reqwest client builder, then cap at
/// TLS 1.2 when [`force_tls12`] is set.
pub(crate) fn apply_async_proxy_policy(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    let builder = if proxy_disabled() {
        builder.no_proxy()
    } else {
        builder
    };
    if force_tls12() {
        builder.max_tls_version(reqwest::tls::Version::TLS_1_2)
    } else {
        builder
    }
}

/// Apply the process proxy policy to a blocking reqwest client builder
/// (the one-shot MCP OAuth login / refresh flow), then cap TLS as above.
#[cfg(feature = "mcp")]
pub(crate) fn apply_blocking_proxy_policy(
    builder: reqwest::blocking::ClientBuilder,
) -> reqwest::blocking::ClientBuilder {
    let builder = if proxy_disabled() {
        builder.no_proxy()
    } else {
        builder
    };
    if force_tls12() {
        builder.max_tls_version(reqwest::tls::Version::TLS_1_2)
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
