//! Where this machine's credentials live, and nothing about how they were got.
//!
//! Split out of `atomcode-auth` (决策 7 of
//! `docs/plans/2026-09-19-remaining-gaps.md`): signing in is a protocol — URLs,
//! polling, token exchange — and storing what came back is a file with a mode
//! on it. They had been one crate, which meant everything that only wanted to
//! read the current user pulled in an HTTP client and an OAuth flow.
//!
//! The direction is one-way: the protocol half depends on this one, to put away
//! what it got and to read what to refresh. Nothing here knows a server exists.

// `Write::write_all` and `PathBuf` only appear inside `#[cfg(unix)]`
// blocks below (the atomic-rename + chmod-600 path uses them; the
// Windows fallback at line 49 just calls `std::fs::write`). Gate the
// imports so a Windows build doesn't fire unused_imports.
#[cfg(any(unix, target_os = "windows"))]
use std::io::Write;
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

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

    #[cfg(target_os = "windows")]
    {
        let parent = path
            .parent()
            .context("Invalid auth file path — please use /login again")?;
        let mut temp = tempfile::NamedTempFile::new_in(parent).with_context(|| {
            format!("Failed to create temp auth file beside {}", path.display())
        })?;
        temp.write_all(content.as_bytes())
            .context("Failed to write auth content")?;
        temp.as_file()
            .sync_all()
            .context("Failed to sync auth file")?;
        temp.persist(path)
            .map_err(|error| error.error)
            .with_context(|| {
                format!(
                    "Failed to atomically replace auth file at {}",
                    path.display()
                )
            })?;
    }

    #[cfg(not(any(unix, target_os = "windows")))]
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

/// Stored authentication data
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthInfo {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub token_type: String,
    pub expires_in: Option<i64>,
    /// Unix timestamp (seconds) when this token was obtained
    #[serde(default)]
    pub created_at: i64,
    pub user: UserInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserInfo {
    pub id: String,
    pub username: String,
    pub name: Option<String>,
    pub email: Option<String>,
    pub avatar_url: Option<String>,
}

/// Logout - clear stored auth.
///
/// Core-layer function: does the filesystem work and returns. User-facing
/// messaging is the caller's job — this was previously `println!`-ing
/// "Logged out successfully" directly, which bypassed the TUI renderer
/// and bled into the input box area on next repaint, and also produced
/// a duplicate line in CLI mode where `handle_command` prints its own
/// confirmation. No `Err` distinguishes "file absent" from "file removed" —
/// both are success from the user's perspective ("you're logged out").
pub fn logout() -> Result<()> {
    let auth_path = auth_file_path();
    // Absent file ⇒ already logged out. Return before touching the lock so a
    // never-logged-in user's /logout stays a pure no-op — no directory or lock
    // file created, and no failure on a read-only HOME.
    if !auth_path.exists() {
        return Ok(());
    }
    with_auth_lock(|| {
        if auth_path.exists() {
            std::fs::remove_file(&auth_path).context("Failed to remove auth file")?;
        }
        Ok(())
    })
}

/// Get stored auth info
pub fn get_stored_auth() -> Option<AuthInfo> {
    let auth_path = auth_file_path();
    read_stored_auth_at(&auth_path).ok().flatten()
}

/// Read credentials without collapsing transient I/O or parse failures into a
/// confirmed logout. Credential writers replace the file atomically, so this
/// remains non-blocking even while a refresh request holds the writer lock.
pub fn get_stored_auth_checked() -> Result<Option<AuthInfo>> {
    read_stored_auth_at(&auth_file_path())
}

fn read_stored_auth_at(auth_path: &std::path::Path) -> Result<Option<AuthInfo>> {
    let content = match std::fs::read_to_string(auth_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Failed to read auth file at {}", auth_path.display()));
        }
    };
    toml::from_str(&content)
        .map(Some)
        .with_context(|| format!("Failed to parse auth file at {}", auth_path.display()))
}

/// Save auth info to file
pub fn save_auth(auth: &AuthInfo) -> Result<()> {
    with_auth_lock(|| save_auth_unlocked(auth))
}

/// Execute one authentication-store transaction. Every writer uses this seam so
/// a refresh response cannot overwrite a concurrent login/logout from another
/// thread or process.
///
/// Public because refreshing is a read-modify-write that has to happen inside
/// one: the protocol half reads what expired, asks the server, and puts the
/// answer back, and another process logging out in the middle of that must not
/// be overwritten.
pub fn with_auth_lock<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let auth_path = auth_file_path();
    let parent = auth_path
        .parent()
        .context("Invalid auth file path — please use /login again")?;
    std::fs::create_dir_all(parent).context("Failed to create auth directory")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Best-effort: a directory we can write to but can't chmod (unusual
        // mounts, or a dir owned by another user) must not block login / refresh /
        // logout. The file itself is still written 0600 by write_auth_file_secure.
        if let Err(error) = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
        {
            tracing::warn!(%error, "failed to tighten auth directory permissions");
        }
    }
    with_auth_lock_file(&parent.join("auth-refresh.lock"), operation)
}

fn with_auth_lock_file<T>(
    lock_path: &std::path::Path,
    operation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    use fs2::FileExt;
    use std::fs::OpenOptions;

    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(lock_path)
        .context("Failed to open auth refresh lock")?;
    lock.lock_exclusive()
        .context("Failed to acquire auth refresh lock")?;
    operation()
}

/// Caller must hold `auth-refresh.lock`.
/// Save without taking the lock — for use inside [`with_auth_lock`].
pub fn save_auth_unlocked(auth: &AuthInfo) -> Result<()> {
    let auth_path = auth_file_path();
    let content = toml::to_string_pretty(auth).context("Failed to serialize auth info")?;
    write_auth_file_secure(&auth_path, &content).context("Failed to write auth file")?;

    // Set file permissions to 0o600 (owner read/write only) on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o600))
            .context("Failed to set auth file permissions")?;
    }

    // No stdout output here. `save_auth` is called from CLI flows, TUI
    // slash commands, the daemon, AND the silent in-chat 401 → refresh
    // path. Printing here would corrupt the TUI input box on the silent
    // refresh path (the cursor sits in the prompt and `println!` bypasses
    // the renderer). CLI callers print their own user-facing success
    // message right after calling this.
    Ok(())
}

/// Get path to auth file
pub fn auth_file_path() -> std::path::PathBuf {
    atomcode_config::config::Config::config_dir().join("auth.toml")
}

/// Check if user is logged in
pub fn is_logged_in() -> bool {
    get_stored_auth().is_some()
}

/// Get current user info (if logged in)
pub fn current_user() -> Option<UserInfo> {
    get_stored_auth().map(|auth| auth.user)
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

    #[cfg(target_os = "windows")]
    #[test]
    fn write_auth_file_secure_atomically_replaces_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let auth_path = tmp.path().join("auth.toml");
        std::fs::write(&auth_path, "access_token = \"old\"\n").unwrap();

        write_auth_file_secure(&auth_path, "access_token = \"new\"\n").unwrap();

        assert_eq!(
            std::fs::read_to_string(auth_path).unwrap(),
            "access_token = \"new\"\n"
        );
    }

    #[test]
    fn auth_store_lock_serializes_concurrent_writers() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Barrier};

        let temp = tempfile::tempdir().unwrap();
        let lock_path = temp.path().join("auth-refresh.lock");
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();

        for _ in 0..2 {
            let lock_path = lock_path.clone();
            let active = active.clone();
            let max_active = max_active.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                with_auth_lock_file(&lock_path, || {
                    let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                })
                .unwrap();
            }));
        }
        barrier.wait();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(max_active.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn checked_auth_read_distinguishes_invalid_data_from_logout() {
        let temp = tempfile::tempdir().unwrap();
        let auth_path = temp.path().join("auth.toml");
        assert!(read_stored_auth_at(&auth_path).unwrap().is_none());

        std::fs::write(&auth_path, "").unwrap();
        assert!(read_stored_auth_at(&auth_path).is_err());
        assert!(read_stored_auth_at(&auth_path).ok().flatten().is_none());
    }
}
