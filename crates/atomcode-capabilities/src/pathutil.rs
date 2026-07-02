//! Path helpers shared by the `tools` and `codeintel` tool families. Kept OUTSIDE
//! `tools/` and free of any feature `cfg` because `codeintel` is deliberately
//! independent of the `tools` feature (see `codeintel/mod.rs`) yet must resolve
//! model-supplied paths the SAME way — including leading-`~` expansion, so
//! `read_file("~/x")` and `read_symbol("~/x")` and `glob("~/x/**")` agree with the
//! shell (which the `bash` tool relies on).

use std::path::{Path, PathBuf};

/// The user's home directory, dependency-free (`HOME`, or `USERPROFILE` on Windows).
pub(crate) fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let var = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let var = std::env::var_os("HOME");
    var.map(PathBuf::from).filter(|p| !p.as_os_str().is_empty())
}

/// Expand a leading `~`/`~/` (and, on Windows, `~\`) against `home`; returns `None`
/// when `raw` is not a home-relative tilde form so the caller's absolute/relative
/// handling runs unchanged. Deliberately does NOT expand `~user/…` (needs a passwd
/// lookup), Windows 8.3 short names (`PROGRA~1`), or Office temp/lock files
/// (`~$doc.docx`).
pub(crate) fn expand_tilde_with_home(raw: &str, home: Option<&Path>) -> Option<PathBuf> {
    let rest = tilde_rest(raw)?;
    let home = home?;
    if rest.is_empty() {
        return Some(home.to_path_buf());
    }
    // `Path::join` REPLACES the whole path when its argument is absolute, so a `rest`
    // beginning with a separator (`~//etc` → rest `/etc`) would escape home entirely.
    // Strip leading separators so `rest` is ALWAYS joined as home-relative (the shell
    // keeps `~//etc` under `$HOME` too).
    let rest = rest.trim_start_matches(|c| c == '/' || (cfg!(windows) && c == '\\'));
    Some(home.join(rest))
}

/// [`expand_tilde_with_home`] using the process home. Checks the (cheap, alloc-free)
/// tilde shape BEFORE reading the environment, so a non-tilde path pays no env read.
pub(crate) fn expand_tilde(raw: &str) -> Option<PathBuf> {
    tilde_rest(raw)?; // fast-path: skip the env read entirely unless there's a `~`
    expand_tilde_with_home(raw, home_dir().as_deref())
}

/// The part of `raw` AFTER a leading `~` separator (empty for a bare `~`), or `None`
/// if `raw` is not a home-relative tilde form. Matches only `~`, `~/…`, and — on
/// Windows, where `\` is a separator — `~\…`.
fn tilde_rest(raw: &str) -> Option<&str> {
    if raw == "~" {
        return Some("");
    }
    raw.strip_prefix("~/")
        .or_else(|| if cfg!(windows) { raw.strip_prefix(r"~\") } else { None })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_leading_tilde_to_home() {
        let home = Path::new("/Users/csdn");
        assert_eq!(
            expand_tilde_with_home("~/.atomcode/x", Some(home)),
            Some(PathBuf::from("/Users/csdn/.atomcode/x"))
        );
        // A bare `~` (and `~/`) is the home dir itself.
        assert_eq!(expand_tilde_with_home("~", Some(home)), Some(PathBuf::from("/Users/csdn")));
        assert_eq!(expand_tilde_with_home("~/", Some(home)), Some(PathBuf::from("/Users/csdn")));
    }

    #[test]
    fn tilde_rest_with_leading_separators_stays_under_home() {
        // `Path::join` REPLACES on an absolute arg, so a `rest` beginning with a
        // separator (`~//etc/passwd` → rest `/etc/passwd`) must NOT escape home.
        let home = Path::new("/Users/csdn");
        assert_eq!(
            expand_tilde_with_home("~//etc/passwd", Some(home)),
            Some(PathBuf::from("/Users/csdn/etc/passwd"))
        );
        assert_eq!(expand_tilde_with_home("~//", Some(home)), Some(PathBuf::from("/Users/csdn")));
    }

    #[test]
    fn non_home_forms_are_not_expanded() {
        let home = Path::new("/Users/csdn");
        // `~user/…`, 8.3 short names, Office temp files: not home-relative.
        assert_eq!(expand_tilde_with_home("~bob/notes.txt", Some(home)), None);
        assert_eq!(expand_tilde_with_home("PROGRA~1", Some(home)), None);
        assert_eq!(expand_tilde_with_home("~$report.docx", Some(home)), None);
        // Ordinary relative / absolute paths are not tilde forms.
        assert_eq!(expand_tilde_with_home("src/main.rs", Some(home)), None);
        assert_eq!(expand_tilde_with_home("/etc/hosts", Some(home)), None);
    }

    #[test]
    fn no_home_declines_expansion() {
        // No resolvable home → decline, so the caller degrades to its relative-join.
        assert_eq!(expand_tilde_with_home("~/.atomcode/x", None), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn backslash_is_literal_on_unix() {
        // On Unix `\` is an ordinary filename char, so `~\foo` is not a tilde form.
        assert_eq!(expand_tilde_with_home(r"~\foo", Some(Path::new("/Users/csdn"))), None);
    }

    #[cfg(windows)]
    #[test]
    fn backslash_expands_on_windows() {
        let home = Path::new(r"C:\Users\csdn");
        assert_eq!(
            expand_tilde_with_home(r"~\.atomcode\x", Some(home)),
            Some(PathBuf::from(r"C:\Users\csdn\.atomcode\x"))
        );
        // Doubled separator must not escape to the drive root.
        assert_eq!(
            expand_tilde_with_home(r"~\\Windows", Some(home)),
            Some(PathBuf::from(r"C:\Users\csdn\Windows"))
        );
    }
}
