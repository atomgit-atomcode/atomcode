//! Install-location detection mirroring scripts/install.sh and install.ps1.

// `Path` only appears in `unix_rc_paths_for_home`, which is gated to
// `cfg(unix)` for the production path and `cfg(all(not(unix), test))`
// for the cross-platform test stub. A Windows non-test build sees
// neither, so the import would be flagged unused.
use std::path::PathBuf;
#[cfg(any(unix, test))]
use std::path::Path;

/// Return the atomcode data root (`$ATOMCODE_HOME` when set, or
/// `~/.atomcode/` by default). Routes through [`atomcode_core::config::Config::config_dir`]
/// so install/setup/skill/plugin/uninstall all agree on a single root —
/// previously this module looked at a separate `ATOMCODE_HOME_OVERRIDE`
/// variable, which let users customise their data dir but then lose track
/// of it at uninstall time. One variable, one semantics.
pub fn atomcode_dir() -> PathBuf {
    atomcode_core::config::Config::config_dir()
}

/// Filenames inside `$ATOMCODE_HOME/` that the uninstaller knows about, grouped.
pub struct UninstallManifest {
    pub credential_files: &'static [&'static str],
    pub state_files: &'static [&'static str],
    pub state_dirs: &'static [&'static str],
    pub state_prefixes: &'static [&'static str],
}

pub fn uninstall_manifest() -> UninstallManifest {
    UninstallManifest {
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
    UnixRcPaths {
        zshrc: home.join(".zshrc"),
        bashrc: home.join(".bashrc"),
    }
}

// Test-only counterpart so the test compiles cross-platform.
#[cfg(all(not(unix), test))]
pub struct UnixRcPaths {
    pub zshrc: PathBuf,
    pub bashrc: PathBuf,
}
#[cfg(all(not(unix), test))]
pub fn unix_rc_paths_for_home(home: &Path) -> UnixRcPaths {
    UnixRcPaths {
        zshrc: home.join(".zshrc"),
        bashrc: home.join(".bashrc"),
    }
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
    use serial_test::serial;

    #[test]
    #[serial]
    fn atomcode_dir_under_home() {
        // Note: with no $HOME this returns "./.atomcode" — test passes vacuously then.
        let p = atomcode_dir();
        assert!(p.ends_with(".atomcode"), "got {:?}", p);
    }

    #[test]
    #[serial]
    fn atomcode_home_env_wins() {
        // Under unified semantics, ATOMCODE_HOME IS the data root.
        // The legacy ATOMCODE_HOME_OVERRIDE variable is gone.
        std::env::set_var("ATOMCODE_HOME", "/tmp/override");
        assert_eq!(atomcode_dir(), std::path::PathBuf::from("/tmp/override"));
        std::env::remove_var("ATOMCODE_HOME");
    }

    #[test]
    fn manifest_groups_credentials_correctly() {
        let m = uninstall_manifest();
        for f in ["auth.toml", "mcp.json", "config.toml", "ATOMCODE.md"] {
            assert!(m.credential_files.contains(&f), "missing {f}");
        }
    }

    #[test]
    fn manifest_groups_state_correctly() {
        let m = uninstall_manifest();
        for f in [
            "history",
            "input_history.txt",
            "recent_dirs.txt",
            "codingplan_sync.json",
            "device_id",
        ] {
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
