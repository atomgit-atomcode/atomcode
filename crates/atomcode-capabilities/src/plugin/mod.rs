//! Plugin marketplace + installation. See
//! `docs/superpowers/specs/2026-04-29-plugin-marketplace-design.md`.

// Public API surface (per spec §10): the high-level entry points used by
// the TUI dispatcher and downstream registries.
pub mod bootstrap;
pub mod hook_trust;
pub mod installer;
pub mod loader;
pub mod marketplace;

// Internal modules: state schema, manifest types, path helpers, URL
// helpers. Not part of the public API; if a downstream consumer needs one
// of these symbols, re-export it explicitly above.
pub(crate) mod manifest;
pub(crate) mod paths;
pub(crate) mod state;
pub(crate) mod url;

// Re-export types needed by downstream crates (TUI, CLI).
pub use loader::{installed_plugin_cc_hooks, installed_plugin_hook_trust_status, PluginHookTrust};
pub use manifest::{
    load_marketplace_manifest, load_plugin_manifest, MarketplaceManifest, PluginEntry,
    PluginManifest, PluginSource,
};
pub use paths::{marketplaces_root, plugins_root, project_plugins_root};
pub use state::InstallScope;

#[cfg(test)]
pub(crate) mod test_support;

/// Result of a long-running plugin operation that the TUI runs off the event
/// loop (clone / pull / install). The dispatcher fires-and-forgets a
/// `tokio::task::spawn_blocking` and the main `select!` consumes one of these
/// once the worker finishes, so the input thread never sees the git latency.
#[derive(Debug)]
pub enum PluginJobEvent {
    MarketplaceAdded(marketplace::MarketplaceInfo),
    MarketplaceUpdated(marketplace::MarketplaceInfo),
    PluginInstalled(installer::InstalledPluginInfo),
    PluginUpdated(installer::InstalledPluginInfo),
    /// The plugin is already installed; carries the canonical id so the
    /// renderer can show a friendly reinstall hint with the right commands.
    PluginAlreadyInstalled {
        id: String,
    },
    /// Generic failure: `op` is one of "add" / "update" / "install" so the
    /// renderer can produce the same human message as the prior sync path.
    Failed {
        op: String,
        msg: String,
    },
    /// Git is not installed or not on PATH. This is a pre-check failure,
    /// not an operational error — the renderer should show a friendly hint
    /// (not an error) to guide the user to install git.
    GitNotFound,
}
