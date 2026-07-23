//! Proxy runtime that needs the reqwest HTTP stack. The config types + process-env
//! policy moved to the leaf `atomcode_config::proxy`; this module re-exports them so
//! `atomcode_core::proxy::*` keeps resolving, and adds the two reqwest-applying
//! policy fns (which can't live in the leaf config crate).

pub use atomcode_config::proxy::*;

/// Apply the process proxy policy to an async reqwest client builder: honor
/// `no_proxy` mode, otherwise leave reqwest's env-based proxy detection intact.
pub fn apply_async_proxy_policy(builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    atomcode_config::proxy::ensure_runtime_initialized();
    if std::env::var(atomcode_config::proxy::MODE_ENV)
        .ok()
        .as_deref()
        == Some(atomcode_config::proxy::ProxyMode::NoProxy.as_str())
    {
        builder.no_proxy()
    } else {
        builder
    }
}

/// Blocking-client counterpart of [`apply_async_proxy_policy`].
pub fn apply_blocking_proxy_policy(
    builder: reqwest::blocking::ClientBuilder,
) -> reqwest::blocking::ClientBuilder {
    atomcode_config::proxy::ensure_runtime_initialized();
    if std::env::var(atomcode_config::proxy::MODE_ENV)
        .ok()
        .as_deref()
        == Some(atomcode_config::proxy::ProxyMode::NoProxy.as_str())
    {
        builder.no_proxy()
    } else {
        builder
    }
}
