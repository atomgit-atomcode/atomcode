//! Self-contained configuration helpers. Home resolution honors a sudo caller
//! only for a root process; ordinary users retain their own home directory.

use std::path::{Path, PathBuf};

/// Stable bucket key shared by session storage and project-scoped trust data.
/// The `PathBuf` hash is part of the existing on-disk format and must not be
/// replaced with a plain string hash.
pub fn stable_project_hash(path: &Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut normalized = path.to_string_lossy().replace('\\', "/");
    if normalized.len() > 1 && normalized.ends_with('/') {
        normalized.pop();
    }
    #[cfg(windows)]
    let normalized = normalized.to_lowercase();

    let mut hasher = DefaultHasher::new();
    PathBuf::from(normalized).hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Use the invoking user's home only for a root process launched through sudo.
/// Non-root processes must not inherit a stale SUDO_USER override in containers.
pub fn real_home_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no arguments, pointers or failure mode.
        let effective_uid = unsafe { libc::geteuid() };
        let sudo_user = std::env::var("SUDO_USER").ok();
        invoking_home(
            effective_uid,
            sudo_user.as_deref(),
            get_user_home,
            dirs::home_dir,
        )
    }
    #[cfg(not(unix))]
    {
        dirs::home_dir()
    }
}

#[cfg(any(unix, test))]
fn invoking_home(
    effective_uid: u32,
    sudo_user: Option<&str>,
    lookup: impl FnOnce(&str) -> Option<PathBuf>,
    fallback: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    if effective_uid == 0 {
        if let Some(user) = sudo_user.filter(|user| !user.is_empty() && *user != "root") {
            if let Some(home) = lookup(user) {
                return Some(home);
            }
        }
    }
    fallback()
}

/// Look up a non-root user's home directory via `getpwnam_r`.
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

        if ret == 0 && !result.is_null() && pwd.pw_uid != 0 {
            let home = std::ffi::CStr::from_ptr(pwd.pw_dir)
                .to_string_lossy()
                .into_owned();
            return Some(PathBuf::from(home));
        }
    }
    None
}

/// Heuristic: does this model name look vision-capable? Same rules as
/// `atomcode_capabilities::provider::model_suggests_vision`, which is the copy
/// the coding/provider side arms `supports_vision` from. This one backs
/// `ProviderConfig::accepts_images` and the config's own active-model check.
/// MUST stay in sync with that copy.
pub fn model_name_suggests_vision(name: &str) -> bool {
    let lowered = name.to_lowercase();
    // A gateway that fronts many vendors qualifies the model with its vendor:
    // OpenRouter ids are `anthropic/claude-opus-4.1`, `openai/gpt-4o`,
    // `google/gemini-2.5-pro`, and Vertex's are a whole resource path ending in
    // the model name. Every `starts_with` rule below looks at the *front* of
    // the string, so on a prefixed id not one of them fired — a vision model
    // behind `anthropic/…` was classified blind, and the image someone pasted
    // for it was degraded to a caption with nothing saying so. The rules are
    // about the model, so they run against the model: the last path segment,
    // which is the whole name when there is no prefix.
    let n = lowered.rsplit('/').next().unwrap_or(lowered.as_str());
    n.contains("vision")
        || n.contains("-vl")
        || n.contains("vl-")
        || n.contains("ocr")
        || n.contains("-4v")
        || n.contains("-4.1v")
        || n.starts_with("gpt-4o")
        || n.starts_with("claude-3")
        || n.starts_with("claude-4")
        || n.starts_with("claude-5")
        || n.starts_with("claude-6")
        || n.starts_with("claude-7")
        || n.starts_with("claude-sonnet")
        || n.starts_with("claude-opus")
        || n.starts_with("claude-haiku")
        || n.starts_with("gemini")
        || n.starts_with("pixtral")
        || n.contains("llava")
        || n.contains("qvq")
}

#[cfg(test)]
mod tests {
    #[test]
    fn ordinary_users_ignore_inherited_sudo_identity() {
        let home = invoking_home(
            1000,
            Some("root"),
            |_| panic!("must not query sudo identity"),
            || Some(PathBuf::from("/home/user")),
        );
        assert_eq!(home, Some(PathBuf::from("/home/user")));
        let home = invoking_home(
            1000,
            Some("other"),
            |_| panic!("must not query sudo identity"),
            || Some(PathBuf::from("/home/user")),
        );
        assert_eq!(home, Some(PathBuf::from("/home/user")));
    }

    #[test]
    fn root_uses_a_nonroot_sudo_callers_home() {
        let home = invoking_home(
            0,
            Some("user"),
            |name| {
                assert_eq!(name, "user");
                Some(PathBuf::from("/home/user"))
            },
            || panic!("caller home resolved"),
        );
        assert_eq!(home, Some(PathBuf::from("/home/user")));
    }

    #[test]
    fn root_without_a_valid_sudo_caller_falls_back() {
        for user in [None, Some(""), Some("root")] {
            assert_eq!(
                invoking_home(
                    0,
                    user,
                    |_| panic!("must not resolve root as sudo caller"),
                    || Some(PathBuf::from("/root"))
                ),
                Some(PathBuf::from("/root"))
            );
        }
        assert_eq!(
            invoking_home(
                0,
                Some("missing"),
                |_| None,
                || Some(PathBuf::from("/root"))
            ),
            Some(PathBuf::from("/root"))
        );
    }

    use super::*;

    #[cfg(unix)]
    #[test]
    fn stable_project_hash_keeps_existing_disk_key() {
        assert_eq!(
            stable_project_hash(Path::new("/tmp/atomcode-trust-golden")),
            "8b6a67e0b2c06dae"
        );
    }
}
