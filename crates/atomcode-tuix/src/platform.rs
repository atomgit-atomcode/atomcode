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
/// On Windows the comparison is case-insensitive because
/// `canonicalize` may normalise the path to a different casing than
/// `dirs::home_dir()` returns (e.g. `C:\Users\…` vs `c:\users\…`).
///
/// Used by the status row + welcome page to keep long paths readable.
pub fn collapse_home(path: &str) -> String {
    collapse_home_with(path, home_dir().as_deref())
}

/// Implementation of [`collapse_home`] that accepts an explicit home
/// directory. This lets unit tests verify Windows-specific logic on any
/// platform without needing `cfg!(windows)`.
fn collapse_home_with(path: &str, home: Option<&std::path::Path>) -> String {
    let path: std::borrow::Cow<'_, str> = if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        std::borrow::Cow::Owned(format!(r"\\{}", rest))
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        std::borrow::Cow::Borrowed(rest)
    } else {
        std::borrow::Cow::Borrowed(path)
    };
    if let Some(home) = home {
        let home_str = home.to_string_lossy();
        if !home_str.is_empty() {
            // On Windows the filesystem is case-insensitive.  `canonicalize`
            // may return a path whose drive-letter / user-directory casing
            // differs from `dirs::home_dir()` (e.g. `C:\USERS\alice\…` vs
            // `C:\users\alice`).  Compare case-insensitively on Windows so
            // the prefix is reliably stripped and the status row shows
            // `~/atomcode` instead of the raw `C:\USERS\alice\atomcode`.
            let rest = if cfg!(windows) {
                // Case-insensitive prefix match on Windows: compare the
                // lowercased forms, but slice the *original* path at
                // `home_str.len()` — that offset is where the remainder
                // starts regardless of casing differences.  Using the
                // lowercase remainder's length (old code: `s.len()`) was
                // wrong because it equals `path.len() - home_str.len()`,
                // so `path[s.len()..]` took the *last* `home_str.len()`
                // characters instead of everything after the home prefix.
                if path.to_lowercase().starts_with(&home_str.to_lowercase()) {
                    Some(path[home_str.len()..].to_string())
                } else {
                    None
                }
            } else {
                path.strip_prefix(&*home_str).map(|s| s.to_string())
            };
            if let Some(rest) = rest {
                if rest.is_empty() {
                    return "~".to_string();
                }
                // Always emit forward slashes after `~` — the `~`
                // shortcut is a Unix shell convention and `~\foo`
                // (the Windows-native form) matches no actual shell:
                // PowerShell / cmd don't expand `~`, Git Bash / WSL
                // use `~/`. Mixed `~\…` reads as a typo. Normalising
                // here keeps every status-row path consistent with
                // the rest of the TUI (skill paths, command help,
                // docs) which all reference `~/.atomcode/...`.
                return format!("~{}", rest.replace('\\', "/"));
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
            assert_eq!(got, "~/project/foo");
        }
    }

    /// Windows `home_str` uses `\`, and the input path strip_prefix
    /// leaves a backslash-prefixed remainder. The collapse output must
    /// still normalise to `~/foo`, not the hybrid `~\foo` form.
    #[test]
    fn collapse_home_emits_forward_slash_on_windows_separators() {
        if let Some(home) = home_dir() {
            let home_str = home.to_string_lossy().to_string();
            // Build the path with the same separator home_str uses, so
            // strip_prefix succeeds on both Unix and Windows test runs.
            let sep = if home_str.contains('\\') { '\\' } else { '/' };
            let nested = format!("{home_str}{sep}atomcode{sep}src");
            let got = collapse_home(&nested);
            assert_eq!(got, "~/atomcode/src");
            assert!(!got.contains('\\'), "must not retain backslashes: {got}");
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

    /// Simulates the exact scenario reported in issue #356: on Windows 10
    /// the user's home directory is `C:\Users\username` (as returned by
    /// `dirs::home_dir()`) but `canonicalize` produces
    /// `\\?\C:\Users\username\atomcode` — note the `\\?\` prefix that
    /// `collapse_home` already strips, **and** a potential case mismatch
    /// between the two paths.  On Windows the filesystem is
    /// case-insensitive so `C:\Users` and `c:\users` refer to the same
    /// directory, but `str::strip_prefix` is case-sensitive.  Without
    /// case-insensitive comparison the prefix is not matched and the raw
    /// path is shown instead of `~/atomcode`.
    #[test]
    fn collapse_home_windows_case_insensitive_match() {
        // Directly call `collapse_home_with` with synthetic paths so the
        // test works on every platform (macOS, Linux, CI).  We simulate
        // the Windows scenario: home_dir returns `C:\Users\username` but
        // canonicalize gives us a different casing.
        let home = std::path::Path::new(r"C:\Users\username");

        // Exact case — should always work
        assert_eq!(
            collapse_home_with(r"C:\Users\username\atomcode", Some(home)),
            "~/atomcode"
        );

        // Home dir uses `C:\Users\username`, canonicalize returns
        // `C:\USERS\username` — must still collapse to `~/atomcode`.
        // On non-Windows cfg!(windows) is false so this test verifies
        // the case-sensitive path; the Windows-specific test below
        // covers the case-insensitive branch.
        if cfg!(windows) {
            assert_eq!(
                collapse_home_with(r"C:\USERS\username\atomcode", Some(home)),
                "~/atomcode"
            );
        }
    }

    /// On Windows, a verbatim-prefixed path whose casing differs from
    /// `dirs::home_dir()` must still be collapsed.  This test only
    /// asserts the verbatim-stripping + case-insensitive match when
    /// actually running on Windows; on other platforms it verifies
    /// that the verbatim prefix is stripped correctly (which is
    /// platform-agnostic).
    #[test]
    fn collapse_home_windows_verbatim_with_different_case() {
        let home = std::path::Path::new(r"C:\Users\username");

        // Verbatim prefix stripped, exact-case home matched
        assert_eq!(
            collapse_home_with(r"\\?\C:\Users\username\atomcode", Some(home)),
            "~/atomcode"
        );

        // On Windows the case-insensitive branch must also handle
        // verbatim paths with different casing.
        if cfg!(windows) {
            assert_eq!(
                collapse_home_with(r"\\?\C:\USERS\username\atomcode", Some(home)),
                "~/atomcode"
            );
        }
    }

    #[test]
    fn collapse_home_windows_exact_home() {
        let home = std::path::Path::new(r"C:\Users\username");
        assert_eq!(
            collapse_home_with(r"C:\Users\username", Some(home)),
            "~"
        );
    }

    /// Regression test for the slice-offset bug in the Windows
    /// case-insensitive branch.  The old code did:
    ///
    ///   path.to_lowercase()
    ///       .strip_prefix(&home_str.to_lowercase())
    ///       .map(|s| path[s.len()..].to_string())
    ///
    /// where `s` was the *lowercase remainder* after stripping the
    /// home prefix.  `s.len()` equals `path.len() - home_str.len()`,
    /// so `path[s.len()..]` took the **last** `home_str.len()` chars
    /// of the original path instead of everything *after* the home
    /// prefix.  For `C:\Users\hao\Documents\WPSDrive\NotLoginPage`
    /// with home `C:\Users\hao`, the result was `~NotLoginPage`
    /// instead of `~/Documents/WPSDrive/NotLoginPage`.
    #[test]
    fn collapse_home_windows_deeply_nested_path() {
        let home = std::path::Path::new(r"C:\Users\hao");
        assert_eq!(
            collapse_home_with(
                r"C:\Users\hao\Documents\WPSDrive\NotLoginPage",
                Some(home),
            ),
            "~/Documents/WPSDrive/NotLoginPage"
        );
    }

    /// Same bug, but exercising the verbatim-prefix path that
    /// `std::fs::canonicalize` returns on Windows.  After the `\\?\`
    /// prefix is stripped the remaining path must still be sliced at
    /// `home_str.len()`, not at the lowercase-remainder length.
    #[test]
    fn collapse_home_windows_verbatim_deeply_nested_path() {
        let home = std::path::Path::new(r"C:\Users\hao");
        assert_eq!(
            collapse_home_with(
                r"\\?\C:\Users\hao\Documents\WPSDrive\NotLoginPage",
                Some(home),
            ),
            "~/Documents/WPSDrive/NotLoginPage"
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
