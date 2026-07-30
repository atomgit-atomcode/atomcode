//! Atomcode disk/TOML config system.
//!
//! Leaf crate — depends only on `atomcode-telemetry` + serde/toml/anyhow, so every
//! stack layer can read `config.toml` without depending on a runtime/driver crate. See
//! `docs/superpowers/plans/2026-07-11-extract-atomcode-config.md`.

/// UI language selection (`Config.language`).
pub mod locale;

/// Localization message tables + `t()`/`Msg`.
pub mod i18n;

/// Vendored leaf helpers (home-dir resolution, vision heuristic) config needs.
pub mod util;

/// `[network.proxy]` config types + process-env proxy policy. HTTP-owning crates
/// apply this policy to their own reqwest builders.
pub mod proxy;

/// TLS-version policy for the explicit process-wide env ceiling and the
/// endpoint-scoped AtomGit fallback latch. Pure URL/env/atomic logic; HTTP
/// clients remain in their owning leaf/provider crates.
pub mod tls;

/// The `LspServerConfig` config type (`Config.lsp.servers`). The LSP runtime is
/// owned by `atomcode-capabilities::codeintel::lsp`.
pub mod lsp_registry;

/// The disk/TOML config system: [`Config`](config::Config) + all sub-configs,
/// load/save and paths.
pub mod config;

/// Transactional, cross-process-safe access to `config.toml`.
pub mod store;

/// Pure parsers for OS system-proxy descriptions: Windows ProxyServer/ProxyOverride
/// and macOS `scutil --proxy` output → normalized HTTP(S)_PROXY / NO_PROXY values.
pub mod system_proxy;

pub use config::{provider::ProviderConfig, Config};
pub use store::{ConfigCommit, ConfigRevision, ConfigSnapshot, ConfigStore};
