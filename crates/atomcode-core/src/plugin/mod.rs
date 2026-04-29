//! Plugin marketplace + installation. See
//! `docs/superpowers/specs/2026-04-29-plugin-marketplace-design.md`.

pub mod installer;
pub mod loader;
pub mod manifest;
pub mod marketplace;
pub mod paths;
pub mod state;
pub mod url;

#[cfg(test)]
pub(crate) mod test_support;
