//! Observe the installation without executing it or replacing the running agent.
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    identity: (u64, u64),
}
fn stamp(path: &Path) -> Option<Stamp> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    Some(Stamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
        #[cfg(unix)]
        identity: {
            use std::os::unix::fs::MetadataExt;
            (metadata.dev(), metadata.ino())
        },
    })
}

pub(super) struct InstalledBinary {
    path: PathBuf,
    original: Stamp,
}
impl InstalledBinary {
    pub(super) fn capture() -> Option<std::sync::Arc<Self>> {
        // The running image does not change on a provider/session remount.
        // Never reset the comparison baseline to an already replaced file.
        static ORIGINAL: std::sync::OnceLock<Option<std::sync::Arc<InstalledBinary>>> =
            std::sync::OnceLock::new();
        ORIGINAL
            .get_or_init(|| Self::detect().map(std::sync::Arc::new))
            .clone()
    }
    fn detect() -> Option<Self> {
        let executable = std::env::current_exe().ok()?;
        let arg = PathBuf::from(std::env::args_os().next()?);
        let candidates = if arg.components().count() > 1 || arg.is_absolute() {
            vec![arg]
        } else {
            std::env::var_os("PATH")
                .map(|paths| {
                    std::env::split_paths(&paths)
                        .map(|dir| dir.join(&arg))
                        .collect()
                })
                .unwrap_or_default()
        };
        // Keep the invoked symlink (e.g. Homebrew's bin link), rather than its
        // versioned Cellar target. Only select a path proven to be this process.
        let actual = std::fs::canonicalize(&executable).ok()?;
        let path = candidates
            .into_iter()
            .find(|path| std::fs::canonicalize(path).ok().as_ref() == Some(&actual))
            .unwrap_or(executable);
        let path = if path.is_absolute() {
            path
        } else {
            std::env::current_dir().ok()?.join(path)
        };
        Self::at(path)
    }
    fn at(path: PathBuf) -> Option<Self> {
        Some(Self {
            original: stamp(&path)?,
            path,
        })
    }
    pub(super) fn changed(&self) -> bool {
        stamp(&self.path).is_some_and(|now| now != self.original)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replacement_is_detected_but_missing_files_are_not_an_update() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("atomcode");
        std::fs::write(&path, "old").unwrap();
        let watch = InstalledBinary::at(path.clone()).unwrap();
        assert!(!watch.changed());
        std::fs::remove_file(&path).unwrap();
        assert!(!watch.changed());
        std::fs::write(&path, "new binary").unwrap();
        assert!(watch.changed());
    }
    #[cfg(unix)]
    #[test]
    fn a_versioned_install_is_watched_through_its_stable_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        let link = dir.path().join("atomcode");
        std::fs::write(&a, "same").unwrap();
        std::fs::write(&b, "same").unwrap();
        std::os::unix::fs::symlink(&a, &link).unwrap();
        let watch = InstalledBinary::at(link.clone()).unwrap();
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&b, &link).unwrap();
        assert!(watch.changed());
    }
}
