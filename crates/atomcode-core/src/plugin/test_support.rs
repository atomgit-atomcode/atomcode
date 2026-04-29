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
}

impl IsolatedHome {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for IsolatedHome {
    fn drop(&mut self) {
        // Unset before the tempdir is removed so subsequent tests cannot
        // observe a stale value pointing at a now-deleted directory.
        std::env::remove_var("ATOMCODE_HOME");
    }
}

/// Create a fresh tempdir, point `ATOMCODE_HOME` at it, and return a guard
/// that cleans up both the env var and the dir on drop. Caller must keep the
/// returned value alive for the duration of the test.
pub fn isolated_home() -> IsolatedHome {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().to_path_buf();
    std::env::set_var("ATOMCODE_HOME", &path);
    IsolatedHome { _tmp: tmp, path }
}
