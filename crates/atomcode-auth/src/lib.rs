//! Signing in: the protocol half.
//!
//! Where the credentials themselves live is [`atomcode_credentials`] — this
//! crate depends on it and not the other way round (决策 7 of
//! `docs/plans/2026-09-19-remaining-gaps.md`).

pub mod gateway_crypto;
pub mod oauth;
pub mod openrouter;

pub use oauth::*;

/// User-Agent for this crate's OAuth HTTP requests. Lowercase
/// `atomcode/<version>` is deliberate — the gateway UA filter hijacks
/// capital-A `AtomCode`.
pub const ATOMCODE_USER_AGENT: &str = concat!("atomcode/", env!("CARGO_PKG_VERSION"));
