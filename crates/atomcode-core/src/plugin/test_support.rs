//! Shared test plumbing for the plugin module. All plugin tests that
//! mutate `ATOMCODE_HOME` use [`isolated_home`] to obtain an [`IsolatedHome`]
//! guard whose `Drop` removes the env var, preventing cross-test leakage when
//! tempdirs clean up out of order.
//!
//! Tests that share this guard MUST also be marked
//! `#[serial_test::serial]` (default unnamed lock) so they serialize against
//! each other across modules.

use std::path::{Path, PathBuf};

pub struct IsolatedHome {
    _tmp: tempfile::TempDir,
    path: PathBuf,
    prev: Option<std::ffi::OsString>,
}

impl IsolatedHome {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for IsolatedHome {
    fn drop(&mut self) {
        // Restore the prior value instead of unsetting. Under the test-binary
        // `#[ctor]` that installs an isolated temp `ATOMCODE_HOME` baseline,
        // `prev` is always `Some`, so this restores that temp baseline and
        // never leaves the var pointing at the real `~/.atomcode`.
        match &self.prev {
            Some(v) => std::env::set_var("ATOMCODE_HOME", v),
            None => std::env::remove_var("ATOMCODE_HOME"),
        }
    }
}

/// Create a fresh tempdir, point `ATOMCODE_HOME` at it, and return a guard
/// that restores the previous value and cleans up the dir on drop. Caller must
/// keep the returned value alive for the duration of the test.
pub fn isolated_home() -> IsolatedHome {
    let prev = std::env::var_os("ATOMCODE_HOME");
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().to_path_buf();
    std::env::set_var("ATOMCODE_HOME", &path);
    IsolatedHome {
        _tmp: tmp,
        path,
        prev,
    }
}
