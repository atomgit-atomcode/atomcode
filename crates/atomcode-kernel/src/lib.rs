//! atomcode-kernel (spike) — a domain-neutral agent driven by a bidirectional,
//! serializable Command/Event handle.
//!
//! Phase A0: internals are minimal/throwaway; the public API *shape* is what
//! Phase A1 carries the proven hot-path code into. The kernel knows nothing
//! about approval, persona, or code-intelligence.

pub mod clock;
pub mod message;
pub mod tool;
pub mod stream;
pub mod provider;
pub mod event;
pub mod request;
pub mod middleware;
pub mod hook;
pub mod agent;
pub mod conformance;
pub mod testkit;
