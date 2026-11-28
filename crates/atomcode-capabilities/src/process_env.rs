//! Extra environment for child processes, kept out of the daemon's own.
//!
//! `std::env::set_var` is unsound in a multithreaded process: the daemon is
//! `axum` on a thread pool, so while one request writes the environment another
//! thread may be reading it — `Command::spawn` reads it to build the child's
//! environment. The value that needs setting also changes at runtime: a data
//! package installed later points `ZIG_LIB_DIR` at its own `lib/`.
//!
//! So the value lives in a registry here, and is applied to each child at
//! spawn time instead of being written into the process. The daemon populates
//! it (it knows what a package declared); the tool layer applies it (it is the
//! only place that spawns shells), so neither has to know the other.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::sync::{OnceLock, RwLock};

fn table() -> &'static RwLock<BTreeMap<String, OsString>> {
    static TABLE: OnceLock<RwLock<BTreeMap<String, OsString>>> = OnceLock::new();
    TABLE.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// Set (or replace) a variable every child process receives.
pub fn set(key: impl Into<String>, value: impl Into<OsString>) {
    if let Ok(mut map) = table().write() {
        map.insert(key.into(), value.into());
    }
}

/// Drop a variable, so children started afterwards no longer inherit it.
pub fn remove(key: &str) {
    if let Ok(mut map) = table().write() {
        map.remove(key);
    }
}

/// Apply the registry to a `std` child command.
pub fn apply(cmd: &mut std::process::Command) {
    if let Ok(map) = table().read() {
        for (key, value) in map.iter() {
            cmd.env(key, value);
        }
    }
}

/// Apply the registry to a `tokio` child command.
pub fn apply_async(cmd: &mut tokio::process::Command) {
    let Ok(map) = table().read() else {
        return;
    };
    for (key, value) in map.iter() {
        cmd.env(key, value);
    }
}
