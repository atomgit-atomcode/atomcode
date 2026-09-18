//! atomcode-kernel (spike) — a domain-neutral agent driven by a bidirectional,
//! serializable Command/Event handle.
//!
//! Phase A0: internals are minimal/throwaway; the public API *shape* is what
//! Phase A1 carries the proven hot-path code into. The kernel knows nothing
//! about approval, persona, or code-intelligence.

pub mod agent;
pub mod checkpoint;
pub mod clock;
pub mod conformance;
pub mod event;
pub use event::{OUTPUT_TRUNCATION_CHECKPOINT_KIND, ROUND_CAP_CHECKPOINT_KIND};
pub mod hook;
// `host` moved out to the `atomcode-host-api` crate on 2026-09-18. The kernel keeps
// the agent's core — the handle protocol, session facts, the neutral value
// types — and nothing that grows with the product surface: host control took
// thirteen new commands in a single day, and the kernel must not move for that
// (`docs/adr/0021`, the 2026-09-18 revision).
pub mod message;
pub mod middleware;
pub mod provider;
pub mod request;
pub mod session;
pub mod stream;
pub mod testkit;
pub mod tool;

/// Test-only `ATOMCODE_HOME` isolation ([`test_support::isolate_home`]), shared by
/// every workspace crate whose tests persist sessions/config/memory. Gated behind the
/// `test-support` feature (enabled via a dev-dependency) so the env-mutating helper
/// never enters a normal build.
#[cfg(feature = "test-support")]
pub mod test_support;
