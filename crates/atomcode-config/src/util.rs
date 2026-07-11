//! Small self-contained helpers config needs, vendored from `atomcode-core` so this
//! crate stays a leaf (no core dependency). Behavior-identical copies:
//!   - [`real_home_dir`] mirrors `atomcode_core::tool::real_home_dir` (sudo-aware).

use std::path::PathBuf;

/// Resolve the invoking user's real home dir, sudo-aware: under `sudo`, `$HOME`
/// points at root, so consult `SUDO_USER` via `getpwnam` first (avoids creating a
/// root-owned `~/.atomcode`). Falls back to `dirs::home_dir()`.
pub fn real_home_dir() -> Option<PathBuf> {
    if let Ok(sudo_user) = std::env::var("SUDO_USER") {
        if let Some(home) = get_user_home(&sudo_user) {
            return Some(home);
        }
    }
    dirs::home_dir()
}

/// Look up a user's home dir via `getpwnam_r` (Unix). Returns `None` on non-Unix.
#[cfg(unix)]
fn get_user_home(username: &str) -> Option<PathBuf> {
    use std::ffi::CString;
    use std::ptr;

    let username_c = CString::new(username).ok()?;
    // SAFETY: getpwnam_r is the thread-safe passwd lookup; `result` is checked
    // non-null before `pwd.pw_dir` is read, and the buffer outlives the call.
    unsafe {
        let mut pwd: libc::passwd = std::mem::zeroed();
        let mut buf = vec![0u8; 4096];
        let mut result: *mut libc::passwd = ptr::null_mut();

        let ret = libc::getpwnam_r(
            username_c.as_ptr(),
            &mut pwd,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        );

        if ret == 0 && !result.is_null() {
            let home = std::ffi::CStr::from_ptr(pwd.pw_dir)
                .to_string_lossy()
                .into_owned();
            return Some(PathBuf::from(home));
        }
    }
    None
}

#[cfg(not(unix))]
fn get_user_home(_username: &str) -> Option<PathBuf> {
    None
}
