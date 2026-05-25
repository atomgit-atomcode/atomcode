//! Plugin marketplace + installation. See
//! `docs/superpowers/specs/2026-04-29-plugin-marketplace-design.md`.

// Public API surface (per spec §10): the high-level entry points used by
// the TUI dispatcher and downstream registries.
pub mod bootstrap;
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
    /// Generic failure: `op` is one of "add" / "update" / "install" so the
    /// renderer can produce the same human message as the prior sync path.
    Failed { op: String, msg: String },
}
