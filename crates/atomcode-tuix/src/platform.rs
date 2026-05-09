// crates/atomcode-tuix/src/platform.rs
//
// Small cross-platform helpers. Every `$HOME`, `/tmp`, and shell-command
// decision in tuix routes through this module so Windows doesn't have
// to special-case each caller. Keeps `event_loop` and friends free of
// `#[cfg(unix)]` clutter.

use std::path::PathBuf;

/// User home directory, or `None` if it can't be determined.
///
/// - macOS / Linux: `$HOME`, falling back to `getpwuid_r`
/// - Windows: `%USERPROFILE%` (via `dirs`)
///
/// This function accounts for sudo scenarios where $HOME might be /root
/// but we want the actual user's home directory.
///
/// Prefer this over `std::env::var("HOME")` — the latter returns `None`
/// on stock Windows and sends us down a fallback path that then hits
/// `/tmp` (also nonexistent on Windows).
pub fn home_dir() -> Option<PathBuf> {
    atomcode_core::tool::real_home_dir()
}

/// Replace a leading `$HOME` in `path` with `~`. Returns `path`
/// unchanged if it doesn't start under home, or if home isn't known.
///
/// On Windows, `std::fs::canonicalize` returns paths with the verbatim
/// (`\\?\`) or UNC-verbatim (`\\?\UNC\`) prefix; both are stripped here
/// before any other processing so the status row never shows the raw
/// extended-length form (e.g. `\\?\D:\wwwroot\xingyu-api`).
///
/// Used by the status row + welcome page to keep long paths readable.
pub fn collapse_home(path: &str) -> String {
    let path: std::borrow::Cow<'_, str> = if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        std::borrow::Cow::Owned(format!(r"\\{}", rest))
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        std::borrow::Cow::Borrowed(rest)
    } else {
        std::borrow::Cow::Borrowed(path)
    };
    if let Some(home) = home_dir() {
        let home_str = home.to_string_lossy();
        if !home_str.is_empty() {
            if let Some(rest) = path.strip_prefix(&*home_str) {
                if rest.is_empty() {
                    return "~".to_string();
                }
                // Keep the separator after `~` — on Unix that's `/`,
                // on Windows it's `\`. Either way `rest` starts with
                // it (unless home_str had a trailing slash, in which
                // case tack one on).
                return format!("~{}", rest);
            }
        }
    }
    path.into_owned()
}

/// Path for the per-user input history file.
/// Uses ATOMCODE_HOME if set, otherwise falls back to home directory.
pub fn history_path() -> PathBuf {
    atomcode_core::config::Config::config_dir().join("history")
}

/// Path for the per-user image attachment cache. Sibling to `history_path()`.
/// Used by the History image-attachment feature to persist pasted bytes so
/// up-arrow recall can rehydrate them on a future submit.
pub fn image_cache_dir() -> PathBuf {
    atomcode_core::config::Config::config_dir().join("image-cache")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapse_home_rewrites_prefix() {
        if let Some(home) = home_dir() {
            let home_str = home.to_string_lossy().to_string();
            let nested = format!("{}/project/foo", home_str);
            let got = collapse_home(&nested);
            // Accept both POSIX and Windows separators.
            assert!(
                got == "~/project/foo" || got == "~\\project\\foo",
                "unexpected collapse: {}",
                got
            );
        }
    }

    #[test]
    fn collapse_home_returns_unchanged_for_unrelated_path() {
        assert_eq!(collapse_home("/opt/tool/bar"), "/opt/tool/bar");
    }

    #[test]
    fn collapse_home_strips_windows_verbatim_prefix() {
        // The Windows extended-length / verbatim prefix is never
        // user-facing; strip it before display.
        assert_eq!(
            collapse_home(r"\\?\D:\wwwroot\xingyu-api"),
            r"D:\wwwroot\xingyu-api"
        );
    }

    #[test]
    fn collapse_home_strips_windows_verbatim_unc_prefix() {
        assert_eq!(
            collapse_home(r"\\?\UNC\server\share\proj"),
            r"\\server\share\proj"
        );
    }

    #[test]
    fn history_path_never_panics() {
        let _ = history_path();
    }

    #[test]
    fn image_cache_dir_lives_under_config_dir() {
        let p = image_cache_dir();
        let cfg = atomcode_core::config::Config::config_dir();
        assert!(p.starts_with(&cfg), "{:?} should be under {:?}", p, cfg);
        assert_eq!(p.file_name().and_then(|s| s.to_str()), Some("image-cache"));
    }
}
