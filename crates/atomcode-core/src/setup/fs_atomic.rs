//! Cross-platform atomic file write: tempfile in same dir → fsync → persist
//! → parent dir fsync. POSIX durability + Windows MoveFileEx semantics.

use anyhow::{Context, Result};
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
/// `0o600` for secrets (tokens, OAuth credentials). Ignored on Windows
/// for now; ACL handling is a P8 follow-up.
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
    #[cfg(not(unix))]
    {
        let _ = mode;
        // TODO(P8-v2): Windows ACL for mode 0o600 files (mcp.json with tokens).
        // Use `windows-acl` crate or `icacls` command to restrict to owner-only.
        // Currently Windows files inherit parent directory ACL, which is acceptable
        // for most dev environments but not for shared/enterprise machines.
        // Tracked in spec Section 5 / "Windows ACL" follow-up.
    }

    tmp.persist(path)
        .map_err(|e| anyhow::anyhow!("atomic_write: persist({}): {}", path.display(), e.error))?;

    // POSIX durability: fsync the directory entry so dirent survives crash.
    let dir = File::open(parent)
        .with_context(|| format!("atomic_write: open parent dir {}", parent.display()))?;
    dir.sync_all()
        .with_context(|| format!("atomic_write: fsync parent {}", parent.display()))?;

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

    #[test]
    fn atomic_write_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a/b/c/file.txt");
        atomic_write(&path, b"deep", 0o644).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"deep");
    }
}
