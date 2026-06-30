//! Library surface for the `atomcode` binary.
//!
//! Exists so integration tests (e.g. `tests/script_parity.rs`) and the binary
//! share testable modules. The bulk of the CLI still lives in `main.rs`; only
//! modules that need to be reachable from `tests/` belong here.

pub mod uninstall;
#[cfg(unix)]
pub mod askpass;
