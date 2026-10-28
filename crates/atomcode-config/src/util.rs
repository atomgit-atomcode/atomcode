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

/// Read at most `limit + 1` bytes from a regular UTF-8 file. The extra byte
/// detects truncation without trusting metadata (the file may grow after stat).
/// Only an incomplete final UTF-8 character at a truncated boundary is dropped;
/// invalid UTF-8 in the retained content remains an error.
pub fn read_bounded_utf8_file(path: &Path, limit: usize) -> std::io::Result<(String, bool)> {
    use std::io::{self, Read};
    let regular = |metadata: std::fs::Metadata| {
        if metadata.is_file() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a regular file",
            ))
        }
    };
    regular(std::fs::metadata(path)?)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    // A path replaced with a FIFO between stat and open must not hang startup.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    regular(file.metadata()?)?;
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)?;
    let truncated = bytes.len() > limit;
    if truncated {
        bytes.truncate(limit);
    }
    match std::str::from_utf8(&bytes) {
        Ok(_) => {}
        Err(error) if truncated && error.error_len().is_none() => {
            bytes.truncate(error.valid_up_to())
        }
        Err(error) => return Err(io::Error::new(io::ErrorKind::InvalidData, error)),
    }
    let text = String::from_utf8(bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok((text, truncated))
}

#[cfg(test)]
mod bounded_file_tests {
    use super::read_bounded_utf8_file;
    #[test]
    fn exact_limit_utf8_boundary_and_invalid_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instructions");
        std::fs::write(&path, "abcd").unwrap();
        assert_eq!(
            read_bounded_utf8_file(&path, 4).unwrap(),
            ("abcd".into(), false)
        );
        std::fs::write(&path, "abc中文").unwrap();
        assert_eq!(
            read_bounded_utf8_file(&path, 4).unwrap(),
            ("abc".into(), true)
        );
        std::fs::write(&path, [0xff, b'a', b'b', b'c', b'd']).unwrap();
        assert!(read_bounded_utf8_file(&path, 4).is_err());
        assert!(read_bounded_utf8_file(dir.path(), 4).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn huge_sparse_file_does_not_require_reading_the_whole_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(8 * 1024 * 1024 * 1024).unwrap();
        let (text, truncated) = read_bounded_utf8_file(&path, 1024).unwrap();
        assert!(truncated);
        assert_eq!(text.len(), 1024);
    }

    #[cfg(unix)]
    #[test]
    fn fifo_is_rejected_without_opening_it() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pipe");
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: name is NUL terminated and lives through the call.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(read_bounded_utf8_file(&path, 1024).is_err());
    }
}
