//! Test-only isolation of `ATOMCODE_HOME`.
//!
//! atomcode persists sessions / config / memory under `ATOMCODE_HOME` (default
//! `~/.atomcode`). Tests that construct a `SessionManager`, run the agent, or
//! otherwise persist without setting `ATOMCODE_HOME` write into the developer's
//! REAL home — a full `cargo test` run leaves dozens of junk `sessions/<hash>/`
//! buckets (working dirs that are throwaway `tempfile` paths).
//!
//! [`isolate_home`] redirects `ATOMCODE_HOME` to a throwaway temp dir the FIRST
//! time it runs, only when the var isn't already set. It's idempotent (guarded by
//! a `Once`), so calling it from a `#[ctor]` in each test binary sets one stable
//! value before libtest spawns any thread — no `set_var` race (unlike per-test
//! `set_var`, which races under the parallel harness). Tests that set their own
//! `ATOMCODE_HOME` still win.
//!
//! Gated behind the `test-support` cargo feature so the env-mutating helper never
//! enters a normal (non-test) build. Consuming crates enable it via a
//! dev-dependency and call it from a `#[ctor]` in their own `#[cfg(test)]` module
//! (and every `tests/*.rs` integration binary):
//!
//! ```ignore
//! // Cargo.toml
//! [dev-dependencies]
//! atomcode-kernel = { path = "../atomcode-kernel", features = ["test-support"] }
//! ctor = "0.2"
//! ```
//! ```ignore
//! #[cfg(test)]
//! #[ctor::ctor]
//! fn _isolate_atomcode_home() {
//!     atomcode_kernel::test_support::isolate_home();
//! }
//! ```
//!
//! Putting the `#[ctor]` in the CONSUMING crate (and referencing this fn) is what
//! forces the linker to keep it — a bare `use … as _` on a ctor-only crate gets
//! dropped and never fires.

use std::sync::Once;

static INIT: Once = Once::new();

/// Redirect `ATOMCODE_HOME` to a per-process temp dir if unset. Idempotent and
/// race-free (runs once). Call from a `#[ctor]` so it lands before any test.
pub fn isolate_home() {
    INIT.call_once(|| {
        if std::env::var_os("ATOMCODE_HOME").is_some() {
            return; // explicit override (real run, or a test that set its own) wins
        }
        let dir = std::env::temp_dir().join(format!("atomcode-test-home-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::env::set_var("ATOMCODE_HOME", &dir);
    });
}
