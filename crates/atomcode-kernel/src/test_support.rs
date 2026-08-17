//! Test-only isolation of `ATOMCODE_HOME`.
//!
//! atomcode persists sessions / config / memory under `ATOMCODE_HOME` (default
//! `~/.atomcode`). Tests that construct a `SessionManager`, run the agent, or
//! otherwise persist without setting `ATOMCODE_HOME` write into the developer's
//! REAL home — a full `cargo test` run leaves dozens of junk `sessions/<hash>/`
//! buckets (working dirs that are throwaway `tempfile` paths).
//!
//! [`isolate_home`] redirects `ATOMCODE_HOME` to a throwaway temp dir the FIRST
//! time it runs, replacing any value inherited from the developer's shell. It's
//! idempotent (guarded by a `Once`), so calling it from a `#[ctor]` in each test
//! binary sets one stable value before libtest spawns any thread — no `set_var`
//! race (unlike per-test `set_var`, which races under the parallel harness).
//! Tests that need a dedicated home may still replace the isolated value inside
//! their test, using the crate's process-global environment lock where required.
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

/// Redirect `ATOMCODE_HOME` to a per-process temp dir. Any inherited value is
/// deliberately replaced: a test process must never interpret a developer's
/// real configured data directory as disposable fixture state.
///
/// Idempotent and race-free (runs once). Call from a `#[ctor]` so it lands before
/// any test.
pub fn isolate_home() {
    INIT.call_once(|| {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock must be after Unix epoch")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("atomcode-test-home-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap_or_else(|error| {
            panic!(
                "failed to create isolated ATOMCODE_HOME at {}: {error}",
                dir.display()
            )
        });
        std::env::set_var("ATOMCODE_HOME", &dir);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherited_atomcode_home_is_never_reused_as_test_storage() {
        let inherited = std::env::temp_dir().join(format!(
            "atomcode-real-home-sentinel-{}",
            std::process::id()
        ));
        std::env::set_var("ATOMCODE_HOME", &inherited);

        isolate_home();

        let isolated = std::env::var_os("ATOMCODE_HOME").unwrap();
        assert_ne!(std::path::PathBuf::from(&isolated), inherited);
        assert!(std::path::Path::new(&isolated).is_dir());
    }
}
