//! Device identity. `device_id` persists in `$ATOMCODE_HOME/device_id`.

use anyhow::{Context, Result};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub fn load_or_create(atomcode_dir: &Path) -> Result<Uuid> {
    let path = atomcode_dir.join("device_id");
    match fs::read_to_string(&path) {
        Ok(s) => Uuid::parse_str(s.trim())
            .with_context(|| format!("device_id file corrupt at {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(atomcode_dir)
                .with_context(|| format!("creating {}", atomcode_dir.display()))?;
            let id = Uuid::new_v4();
            fs::write(&path, id.to_string())
                .with_context(|| format!("writing {}", path.display()))?;
            Ok(id)
        }
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Get the real user's home directory, accounting for sudo scenarios.
///
/// When running under sudo, `dirs::home_dir()` returns root's home directory
/// because $HOME is set to /root. This function checks for SUDO_USER and
/// attempts to use that information.
///
/// Note: This crate forbids unsafe code, so we cannot use getpwnam_r.
/// Instead, we use environment variables and construct the path.
pub fn real_home_dir() -> Option<PathBuf> {
    // Check if we're running under sudo
    if let Ok(sudo_user) = env::var("SUDO_USER") {
        // Try to construct the home path from SUDO_USER
        // On Linux: /home/{user} or /root for root
        // On macOS: /Users/{user}
        if cfg!(target_os = "macos") {
            return Some(PathBuf::from("/Users").join(sudo_user));
        } else if cfg!(target_os = "linux") {
            if sudo_user == "root" {
                return Some(PathBuf::from("/root"));
            }
            return Some(PathBuf::from("/home").join(sudo_user));
        }
    }

    // Fall back to the standard home directory
    dirs::home_dir()
}

/// Return the AtomCode data directory, respecting the `ATOMCODE_HOME`
/// environment variable. When `ATOMCODE_HOME` is set, it IS the data root
/// (no `.atomcode` suffix is appended). Otherwise falls back to
/// `$HOME/.atomcode`, or `./.atomcode` when `$HOME` cannot be resolved.
///
/// This mirrors the logic in `atomcode_core::config::Config::config_dir()`
/// but is implemented independently to avoid a circular dependency between
/// the `atomcode-telemetry` and `atomcode-core` crates.
pub fn default_atomcode_dir() -> PathBuf {
    if let Some(p) = env::var("ATOMCODE_HOME").ok().filter(|s| !s.is_empty()) {
        PathBuf::from(p)
    } else {
        real_home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".atomcode")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn creates_on_first_call_then_reads_same_id() {
        let dir = TempDir::new().unwrap();
        let id1 = load_or_create(dir.path()).unwrap();
        let id2 = load_or_create(dir.path()).unwrap();
        assert_eq!(id1, id2);
        assert!(dir.path().join("device_id").exists());
    }

    #[test]
    fn rejects_corrupt_file() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("device_id"), "not-a-uuid").unwrap();
        assert!(load_or_create(dir.path()).is_err());
    }

    #[test]
    fn trims_whitespace_in_file() {
        let dir = TempDir::new().unwrap();
        let id = Uuid::new_v4();
        std::fs::write(dir.path().join("device_id"), format!("{}\n\n", id)).unwrap();
        let got = load_or_create(dir.path()).unwrap();
        assert_eq!(id, got);
    }
}
