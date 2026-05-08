//! Install-location detection mirroring scripts/install.sh and install.ps1.

use std::path::{Path, PathBuf};

/// Return `~/.atomcode/` (or override via `ATOMCODE_HOME_OVERRIDE` env var,
/// used by tests).
pub fn atomcode_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("ATOMCODE_HOME_OVERRIDE") {
        return PathBuf::from(p);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".atomcode")
}

/// Filenames inside `~/.atomcode/` that the uninstaller knows about, grouped.
pub struct Manifest {
    pub credential_files: &'static [&'static str],
    pub state_files: &'static [&'static str],
    pub state_dirs: &'static [&'static str],
    pub state_prefixes: &'static [&'static str],
}

pub fn manifest() -> Manifest {
    Manifest {
        credential_files: &["auth.toml", "mcp.json", "config.toml", "ATOMCODE.md"],
        state_files: &[
            "history",
            "input_history.txt",
            "recent_dirs.txt",
            "codingplan_sync.json",
            "device_id",
        ],
        state_dirs: &["staged", "telemetry", "plugins", "commands", "skills"],
        state_prefixes: &["notice."],
    }
}

#[cfg(unix)]
pub struct UnixRcPaths {
    pub zshrc: PathBuf,
    pub bashrc: PathBuf,
}

#[cfg(unix)]
pub fn unix_rc_paths() -> UnixRcPaths {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    unix_rc_paths_for_home(&home)
}

#[cfg(unix)]
pub fn unix_rc_paths_for_home(home: &Path) -> UnixRcPaths {
    UnixRcPaths { zshrc: home.join(".zshrc"), bashrc: home.join(".bashrc") }
}

// Test-only counterpart so the test compiles cross-platform.
#[cfg(all(not(unix), test))]
pub struct UnixRcPaths {
    pub zshrc: PathBuf,
    pub bashrc: PathBuf,
}
#[cfg(all(not(unix), test))]
pub fn unix_rc_paths_for_home(home: &Path) -> UnixRcPaths {
    UnixRcPaths { zshrc: home.join(".zshrc"), bashrc: home.join(".bashrc") }
}

/// Default Windows install-dir candidates (matches install.ps1).
#[cfg(windows)]
pub fn windows_install_dir_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = std::env::var_os("ATOMCODE_PREFIX") {
        out.push(PathBuf::from(p));
    }
    if let Some(p) = std::env::var_os("LOCALAPPDATA") {
        out.push(PathBuf::from(p).join("AtomCode"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomcode_dir_under_home() {
        // We don't override $HOME here — just sanity-check the suffix.
        let p = atomcode_dir();
        assert!(p.ends_with(".atomcode"), "got {:?}", p);
    }

    #[test]
    fn atomcode_home_override_wins() {
        std::env::set_var("ATOMCODE_HOME_OVERRIDE", "/tmp/override");
        assert_eq!(atomcode_dir(), std::path::PathBuf::from("/tmp/override"));
        std::env::remove_var("ATOMCODE_HOME_OVERRIDE");
    }

    #[test]
    fn manifest_groups_credentials_correctly() {
        let m = manifest();
        for f in ["auth.toml", "mcp.json", "config.toml", "ATOMCODE.md"] {
            assert!(m.credential_files.contains(&f), "missing {f}");
        }
    }

    #[test]
    fn manifest_groups_state_correctly() {
        let m = manifest();
        for f in ["history", "input_history.txt", "recent_dirs.txt",
                  "codingplan_sync.json", "device_id"] {
            assert!(m.state_files.contains(&f), "missing {f}");
        }
        for d in ["staged", "telemetry", "plugins", "commands", "skills"] {
            assert!(m.state_dirs.contains(&d), "missing {d}");
        }
    }

    #[test]
    fn rc_files_includes_zshrc_and_bashrc() {
        let rc = unix_rc_paths_for_home(std::path::Path::new("/Users/test"));
        assert_eq!(rc.zshrc, std::path::Path::new("/Users/test/.zshrc"));
        assert_eq!(rc.bashrc, std::path::Path::new("/Users/test/.bashrc"));
    }
}
