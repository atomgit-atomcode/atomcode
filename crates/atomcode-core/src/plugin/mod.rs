//! Plugin marketplace + installation. See
//! `docs/superpowers/specs/2026-04-29-plugin-marketplace-design.md`.

// Public API surface (per spec §10): the high-level entry points used by
// the TUI dispatcher and downstream registries.
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
