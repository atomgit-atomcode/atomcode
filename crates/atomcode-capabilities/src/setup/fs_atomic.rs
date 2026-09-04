//! Cross-platform atomic file write: tempfile in same dir → fsync → persist
//! → parent dir fsync. POSIX durability + Windows MoveFileEx semantics.

use anyhow::{Context, Result};
#[cfg(not(windows))]
use std::fs::File;
use std::io::Write;
use std::path::Path;

/// Atomically write `content` to `path`. Uses a temp file in the **same parent
/// directory** (avoids EXDEV on iCloud/Dropbox symlinks), fsyncs the file
/// before persist, then fsyncs the parent directory for POSIX durability.
///
/// On Windows, `tempfile::NamedTempFile::persist` uses `MoveFileExW` with
/// `MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH` so the rename is
/// atomic even when the target exists.
///
/// `mode` is the Unix file mode. Use `0o644` for normal config files and
/// `0o600` for secrets (tokens, OAuth credentials). On Windows, `0o600`
/// maps to an owner-only ACL via `icacls` (strip inherited ACEs, grant the
/// current user full control); other modes keep the default inherited ACL.
pub fn atomic_write(path: &Path, content: &[u8], mode: u32) -> Result<()> {
    let parent = path.parent().context("atomic_write: path has no parent")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("atomic_write: create_dir_all({})", parent.display()))?;

    let mut tmp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("atomic_write: NamedTempFile::new_in({})", parent.display()))?;
    tmp.write_all(content)
        .with_context(|| format!("atomic_write: write({})", tmp.path().display()))?;
    tmp.as_file_mut()
        .sync_all()
        .with_context(|| format!("atomic_write: fsync({})", tmp.path().display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(mode))
            .with_context(|| format!("atomic_write: chmod({:o})", mode))?;
    }
    #[cfg(windows)]
    {
        // Windows has no POSIX permission bits. For secret files (mode
        // 0o600) we mirror the POSIX semantics with `icacls`: strip all
        // inherited ACEs and grant full control to the current user only.
        // Non-secret files (0o644) keep the default inherited ACL.
        if mode == 0o600 {
            restrict_windows_acl(tmp.path())?;
        }
    }

    tmp.persist(path)
        .map_err(|e| anyhow::anyhow!("atomic_write: persist({}): {}", path.display(), e.error))?;

    // POSIX durability: fsync the directory entry so dirent survives crash.
    // Windows requires FILE_FLAG_BACKUP_SEMANTICS to open a directory handle.
    #[cfg(not(windows))]
    let dir = File::open(parent)
        .with_context(|| format!("atomic_write: open parent dir {}", parent.display()))?;
    #[cfg(windows)]
    let dir = {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(0x02000000) // FILE_FLAG_BACKUP_SEMANTICS
            .open(parent)
            .with_context(|| format!("atomic_write: open parent dir {}", parent.display()))?
    };
    // POSIX durability: fsync the directory entry so dirent survives crash.
    // Windows FlushFileBuffers does not support directory handles (ERROR_INVALID_HANDLE).
    #[cfg(unix)]
    dir.sync_all()
        .with_context(|| format!("atomic_write: fsync parent {}", parent.display()))?;

    Ok(())
}

/// Restrict a file to the current user on Windows, mirroring POSIX `0o600`.
///
/// Uses `icacls` to strip all inherited ACEs (`/inheritance:r`) and grant
/// the current user full control (`/grant:r`). Runs on the temp file before
/// `persist` so the ACL travels with the atomic rename.
#[cfg(windows)]
fn restrict_windows_acl(path: &Path) -> Result<()> {
    use std::process::Command;

    let user = std::env::var("USERNAME")
        .map_err(|_| anyhow::anyhow!("restrict_windows_acl: USERNAME not set"))?;
    // Quote the account when it contains spaces so icacls parses the grant
    // as a single `<account>:(perm)` argument.
    let grant = if user.contains(' ') {
        format!("\"{user}\":(F)")
    } else {
        format!("{user}:(F)")
    };
    let status = Command::new("icacls")
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(grant)
        .status()
        .with_context(|| format!("restrict_windows_acl: spawn icacls for {}", path.display()))?;
    if !status.success() {
        anyhow::bail!(
            "restrict_windows_acl: icacls exited with {:?} for {}",
            status.code(),
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_write_creates_file_with_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hello.txt");
        atomic_write(&path, b"hello world", 0o644).unwrap();
        let read = std::fs::read(&path).unwrap();
        assert_eq!(read, b"hello world");
    }

    #[test]
    fn atomic_write_overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("file.txt");
        std::fs::write(&path, b"original").unwrap();
        atomic_write(&path, b"updated", 0o644).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"updated");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_sets_mode_0600_for_secrets() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.json");
        atomic_write(&path, b"{}", 0o600).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(windows)]
    #[test]
    fn atomic_write_0600_restricts_acl_on_windows() {
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.json");
        atomic_write(&path, b"{}", 0o600).unwrap();

        // icacls output: `file NT AUTHORITY\SYSTEM:(I)(F) ... Lenovo\Lenovo:(F)`
        // After /inheritance:r + /grant:r, inherited ACEs (marked (I)) must be gone
        // and only the current user retains full control.
        let output = Command::new("icacls")
            .arg(&path)
            .output()
            .expect("icacls query should run");
        assert!(output.status.success(), "icacls query failed");
        let text = String::from_utf8_lossy(&output.stdout);

        // No inherited ACEs left (the "(I)" marker is appended to inherited entries).
        assert!(
            !text.contains("(I)"),
            "0o600 file must not keep inherited ACEs, got:\n{text}"
        );
        // Owner full control must be granted.
        assert!(
            text.contains("(F)"),
            "0o600 file must grant owner full control, got:\n{text}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn atomic_write_0644_keeps_inherited_acl_on_windows() {
        use std::process::Command;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("normal.json");
        atomic_write(&path, b"{}", 0o644).unwrap();

        let output = Command::new("icacls")
            .arg(&path)
            .output()
            .expect("icacls query should run");
        let text = String::from_utf8_lossy(&output.stdout);
        // 0o644 files keep the default (inherited) ACL — the query must still succeed.
        assert!(output.status.success());
        assert!(!text.is_empty());
    }

    #[test]
    fn atomic_write_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/c/file.txt");
        atomic_write(&path, b"deep", 0o644).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"deep");
    }

    #[test]
    fn atomic_write_overwrite_shorter_content_leaves_no_trailing_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trailing.txt");

        // Write 1000 bytes of 'A'
        let long = vec![b'A'; 1000];
        atomic_write(&path, &long, 0o644).unwrap();
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), 1000);

        // Overwrite with 10 bytes of 'B' — shorter than old content
        let short = b"BBBBBBBBBB";
        atomic_write(&path, short, 0o644).unwrap();

        // File must be exactly 10 bytes, no trailing 'A' bytes
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), 10, "file size must match new content exactly");
        let read = std::fs::read(&path).unwrap();
        assert_eq!(&read, short);
        // Explicitly no old bytes
        assert!(!read.iter().any(|&b| b == b'A'), "no trailing old bytes");
    }

    #[test]
    fn atomic_write_overwrite_longer_content_leaves_no_stale_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stale.txt");

        // Write 10 bytes
        atomic_write(&path, b"0123456789", 0o644).unwrap();

        // Overwrite with 1000 bytes of 'X'
        let long = vec![b'X'; 1000];
        atomic_write(&path, &long, 0o644).unwrap();

        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), 1000);
        let read = std::fs::read(&path).unwrap();
        assert_eq!(&read, &long[..]);
        assert!(!read.windows(10).any(|w| w == b"0123456789"));
    }
}
