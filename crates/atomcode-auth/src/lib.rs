// `Write::write_all` and `PathBuf` only appear inside `#[cfg(unix)]`
// blocks below (the atomic-rename + chmod-600 path uses them; the
// Windows fallback at line 49 just calls `std::fs::write`). Gate the
// imports so a Windows build doesn't fire unused_imports.
#[cfg(unix)]
use std::io::Write;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

use anyhow::{Context, Result};

pub mod gateway_crypto;
pub mod oauth;

pub use oauth::*;

/// User-Agent for this crate's OAuth HTTP requests. Kept identical to
/// `atomcode_core::ATOMCODE_USER_AGENT` (both derive from the unified workspace
/// `CARGO_PKG_VERSION`, so the string matches): lowercase `atomcode/<version>` is
/// deliberate — the gateway UA filter hijacks capital-A `AtomCode`. Held locally so this
/// leaf crate needs no `atomcode-core` dependency.
pub const ATOMCODE_USER_AGENT: &str = concat!("atomcode/", env!("CARGO_PKG_VERSION"));

pub fn write_auth_file_secure(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }

    #[cfg(unix)]
    {
        use std::fs::OpenOptions;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let tmp_path = temp_auth_path(path);
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp_path)
            .with_context(|| {
                format!("Failed to create temp auth file at {}", tmp_path.display())
            })?;

        file.write_all(content.as_bytes())
            .context("Failed to write auth content")?;
        file.sync_all().context("Failed to sync auth file")?;
        drop(file);

        std::fs::rename(&tmp_path, path).with_context(|| {
            format!(
                "Failed to atomically replace auth file from {} to {}",
                tmp_path.display(),
                path.display()
            )
        })?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("Failed to chmod 600 {}", path.display()))?;
    }

    #[cfg(not(unix))]
    {
        std::fs::write(path, content)
            .with_context(|| format!("Failed to write auth file at {}", path.display()))?;
    }

    Ok(())
}

#[cfg(unix)]
fn ensure_private_dir(path: &Path) -> Result<()> {
    use std::fs::DirBuilder;
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::fs::PermissionsExt;

    if path.is_dir() {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("Failed to chmod 700 {}", path.display()))?;
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create parent directory for {}", path.display())
            })?;
        }
    }

    let mut builder = DirBuilder::new();
    builder.mode(0o700);
    builder
        .create(path)
        .with_context(|| format!("Failed to create auth directory {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("Failed to chmod 700 {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn ensure_private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)
        .with_context(|| format!("Failed to create auth directory {}", path.display()))?;
    Ok(())
}

#[cfg(unix)]
fn temp_auth_path(path: &Path) -> PathBuf {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("auth.toml");
    path.with_file_name(format!(".{}.{}.{}.tmp", file_name, pid, nanos))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn write_auth_file_secure_sets_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let auth_path = tmp.path().join("nested").join("auth.toml");

        write_auth_file_secure(&auth_path, "access_token = \"secret\"\n").unwrap();

        let dir_mode = std::fs::metadata(auth_path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let file_mode = std::fs::metadata(&auth_path).unwrap().permissions().mode() & 0o777;

        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn write_auth_file_secure_tightens_existing_file_permissions() {
        use std::fs::OpenOptions;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let tmp = tempfile::tempdir().unwrap();
        let auth_dir = tmp.path().join("auth-home");
        ensure_private_dir(&auth_dir).unwrap();
        let auth_path = auth_dir.join("auth.toml");

        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o644)
            .open(&auth_path)
            .unwrap();
        file.write_all(b"old").unwrap();
        drop(file);

        write_auth_file_secure(&auth_path, "access_token = \"new\"\n").unwrap();

        let file_mode = std::fs::metadata(&auth_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600);
    }
}
