//! Library surface for the `atomcode` binary.
//!
//! Exists so integration tests (e.g. `tests/script_parity.rs`) and the binary
//! share testable modules. The bulk of the CLI still lives in `main.rs`; only
//! modules that need to be reachable from `tests/` belong here.

// Redirect ATOMCODE_HOME to a temp dir before this lib crate's tests run, so they
// don't pollute the real ~/.atomcode (mirrors the bin's ctor in main.rs).
#[cfg(test)]
#[ctor::ctor]
fn _isolate_atomcode_home() {
    atomcode_kernel::test_support::isolate_home();
}

#[cfg(unix)]
pub mod askpass;
pub mod uninstall;

/// ACP (Agent Client Protocol) stdio server — lets atomcode be driven by Zed /
/// multi-agent orchestrators over stdin/stdout. Wired up by the `atomcode acp`
/// subcommand in `main.rs`; the engine/dispatch/translate/permission internals
/// live here. Does not depend on `atomcode-core` (v2 stack only).
pub mod acp;
