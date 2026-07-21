//! atomcode disk/TOML config system, extracted from the retiring `atomcode-core`.
//!
//! Leaf crate — depends only on `atomcode-telemetry` + serde/toml/anyhow, so every
//! stack layer (drivers, bridge, coding, and core itself during the transition) can
//! read `config.toml` without a dependency on `atomcode-core`. See
//! `docs/superpowers/plans/2026-07-11-extract-atomcode-config.md`.

/// UI language selection (`Config.language`). Self-contained serde enum, moved here
/// from `atomcode-core` so `Config` has no core dependency. `atomcode_core::locale`
/// re-exports this during the transition.
pub mod locale;

/// Localization message tables + `t()`/`Msg` (moved out of `atomcode-core`; it only
/// needs `locale::Locale`). `atomcode_core::i18n` re-exports this during the transition.
pub mod i18n;

/// Vendored leaf helpers (home-dir resolution, vision heuristic) config needs.
pub mod util;

/// `[network.proxy]` config types + process-env proxy policy. The reqwest-applying
/// runtime stays in `atomcode-core::proxy` (needs the HTTP stack) and re-exports these.
pub mod proxy;

/// The `LspServerConfig` config type (`Config.lsp.servers`). The LSP runtime
/// (`LspServerRegistry`, client, manager) stays in `atomcode-core::lsp` and re-exports it.
pub mod lsp_registry;

/// The disk/TOML config system: [`Config`](config::Config) + all sub-configs +
/// load/save/paths. `atomcode_core::config` re-exports this during the transition.
pub mod config;

/// Transactional, cross-process-safe access to `config.toml`.
pub mod store;

/// Pure parsers for OS system-proxy descriptions: Windows ProxyServer/ProxyOverride
/// and macOS `scutil --proxy` output → normalized HTTP(S)_PROXY / NO_PROXY values.
pub mod system_proxy;

pub use config::{provider::ProviderConfig, Config};
pub use store::{ConfigCommit, ConfigRevision, ConfigSnapshot, ConfigStore};
