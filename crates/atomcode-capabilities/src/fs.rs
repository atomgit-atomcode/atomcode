//! Cross-platform atomic file write: tempfile in same dir → fsync → persist
//! → parent dir fsync. POSIX durability + Windows MoveFileEx semantics.
//! Ported from `atomcode-core`'s `fs_atomic` for the `plugin` feature (trust store).

use anyhow::{Context, Result};
#[cfg(not(windows))]
use std::fs::File;
use std::io::Write;
use std::path::Path;

/// Atomically write `content` to `path`. Uses a temp file in the **same parent
/// directory** (avoids EXDEV on iCloud/Dropbox symlinks), fsyncs the file
/// before persist, then fsyncs the parent directory for POSIX durability.
///
/// `mode` is the Unix file mode (`0o644` normal, `0o600` secrets). Ignored on Windows.
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
    }

    tmp.persist(path)
        .map_err(|e| anyhow::anyhow!("atomic_write: persist({}): {}", path.display(), e.error))?;

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
    #[cfg(unix)]
    dir.sync_all()
        .with_context(|| format!("atomic_write: fsync parent {}", parent.display()))?;
    #[cfg(windows)]
    let _ = dir;

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
        assert_eq!(std::fs::read(&path).unwrap(), b"hello world");
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

    #[test]
    fn atomic_write_overwrite_shorter_leaves_no_trailing_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trailing.txt");
        atomic_write(&path, &vec![b'A'; 1000], 0o644).unwrap();
        atomic_write(&path, b"BBBBBBBBBB", 0o644).unwrap();
        let read = std::fs::read(&path).unwrap();
        assert_eq!(read, b"BBBBBBBBBB");
        assert!(!read.iter().any(|&b| b == b'A'));
    }
}
