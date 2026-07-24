//! Proxy runtime that needs the reqwest HTTP stack. The config types + process-env
//! policy moved to the leaf `atomcode_config::proxy`; this module re-exports them so
//! `atomcode_core::proxy::*` keeps resolving, and adds the two reqwest-applying
//! policy fns (which can't live in the leaf config crate).
//!
//! An explicit `ATOMCODE_TLS_MAX=1.2` override is also process-wide. Automatic
//! AtomGit fallback is applied by endpoint-aware provider/auth clients instead.

pub use atomcode_config::proxy::*;

/// Whether the process proxy mode is the explicit `no_proxy` bypass.
fn is_no_proxy_mode() -> bool {
    std::env::var(atomcode_config::proxy::MODE_ENV)
        .ok()
        .as_deref()
        == Some(atomcode_config::proxy::ProxyMode::NoProxy.as_str())
}

/// Apply the process proxy policy to an async reqwest client builder: honor
/// `no_proxy` mode (otherwise leave reqwest's env-based proxy detection intact),
/// and honor the explicit process-wide TLS ceiling.
pub fn apply_async_proxy_policy(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    atomcode_config::proxy::ensure_runtime_initialized();
    let builder = if is_no_proxy_mode() {
        builder.no_proxy()
    } else {
        builder
    };
    if atomcode_config::tls::env_forces_tls12() {
        builder.max_tls_version(reqwest::tls::Version::TLS_1_2)
    } else {
        builder
    }
}

/// Blocking-client counterpart of [`apply_async_proxy_policy`].
pub fn apply_blocking_proxy_policy(
    builder: reqwest::blocking::ClientBuilder,
) -> reqwest::blocking::ClientBuilder {
    atomcode_config::proxy::ensure_runtime_initialized();
    let builder = if is_no_proxy_mode() {
        builder.no_proxy()
    } else {
        builder
    };
    if atomcode_config::tls::env_forces_tls12() {
        builder.max_tls_version(reqwest::tls::Version::TLS_1_2)
    } else {
        builder
    }
}
