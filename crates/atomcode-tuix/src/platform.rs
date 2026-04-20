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
/// Prefer this over `std::env::var("HOME")` — the latter returns `None`
/// on stock Windows and sends us down a fallback path that then hits
/// `/tmp` (also nonexistent on Windows).
pub fn home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

/// Replace a leading `$HOME` in `path` with `~`. Returns `path`
/// unchanged if it doesn't start under home, or if home isn't known.
///
/// Used by the status row + welcome page to keep long paths readable.
pub fn collapse_home(path: &str) -> String {
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
    path.to_string()
}

/// Path for the per-user input history file.
/// `~/.atomcode/history` when home is known, `<tempdir>/atomcode-history`
/// as a last-resort fallback (beats writing to a hardcoded `/tmp` that
/// doesn't exist on Windows).
pub fn history_path() -> PathBuf {
    if let Some(home) = home_dir() {
        return home.join(".atomcode").join("history");
    }
    std::env::temp_dir().join("atomcode-history")
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
    fn history_path_never_panics() {
        let _ = history_path();
    }
}
