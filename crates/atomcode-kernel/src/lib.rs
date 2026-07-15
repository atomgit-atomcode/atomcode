//! atomcode-kernel (spike) — a domain-neutral agent driven by a bidirectional,
//! serializable Command/Event handle.
//!
//! Phase A0: internals are minimal/throwaway; the public API *shape* is what
//! Phase A1 carries the proven hot-path code into. The kernel knows nothing
//! about approval, persona, or code-intelligence.

pub mod clock;
pub mod checkpoint;
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

/// Test-only `ATOMCODE_HOME` isolation ([`test_support::isolate_home`]), shared by
/// every workspace crate whose tests persist sessions/config/memory. Gated behind the
/// `test-support` feature (enabled via a dev-dependency) so the env-mutating helper
/// never enters a normal build.
#[cfg(feature = "test-support")]
pub mod test_support;
