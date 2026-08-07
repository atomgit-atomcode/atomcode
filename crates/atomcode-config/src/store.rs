//! Cross-process-safe `config.toml` transactions.
//!
//! The TUI and IDE daemons share one config file but live in different processes.
//! A transaction therefore locks a sibling lock file, re-reads the latest snapshot,
//! applies one delta, and atomically replaces the config. Consumers use the content
//! revision to reconcile cached UI/runtime state without treating mtimes as reliable.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use toml_edit::DocumentMut;

use crate::Config;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ConfigRevision(String);

impl ConfigRevision {
    fn from_bytes(bytes: &[u8]) -> Self {
        let digest = Sha256::digest(bytes);
        Self(format!("{digest:x}"))
    }
}

#[derive(Clone, Debug)]
pub struct ConfigSnapshot {
    pub config: Config,
    pub revision: ConfigRevision,
}

#[derive(Clone, Debug)]
pub struct ConfigCommit {
    pub snapshot: ConfigSnapshot,
}

#[derive(Clone, Debug)]
pub struct ConfigStore {
    path: PathBuf,
    lock_path: PathBuf,
}

impl ConfigStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let lock_path = sibling_lock_path(&path);
        Self { path, lock_path }
    }

    pub fn default_store() -> Self {
        Self::new(Config::default_path())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Read one internally-consistent config + revision snapshot.
    pub fn read(&self) -> Result<ConfigSnapshot> {
        read_snapshot_tolerant(&self.path)
    }

    /// Apply one delta to the latest disk snapshot while holding the process-shared lock.
    pub fn update<F>(&self, mutate: F) -> Result<ConfigCommit>
    where
        F: FnOnce(&mut Config) -> Result<()>,
    {
        self.with_lock(false, |disk| {
            let mut next = disk
                .as_ref()
                .map(|snapshot| snapshot.config.clone())
                .unwrap_or_default();
            mutate(&mut next)?;
            let snapshot = self.persist_locked(&next, disk.as_ref().map(|s| &s.config))?;
            Ok(ConfigCommit { snapshot })
        })
    }

    /// Patch the latest TOML document while holding the same process-shared lock
    /// used by typed config transactions.
    ///
    /// Unlike [`Self::update`], this preserves comments, ordering, unknown keys,
    /// and provider sections byte-for-byte wherever `toml_edit` does not touch
    /// them. The resulting document is parsed through the tolerant config loader
    /// before it replaces the file, so a UI editor cannot persist an invalid
    /// non-provider setting or accidentally discard quarantined providers.
    pub fn update_document<F>(&self, mutate: F) -> Result<ConfigCommit>
    where
        F: FnOnce(&mut DocumentMut) -> Result<()>,
    {
        self.with_lock(false, |disk| {
            let source = match std::fs::read_to_string(&self.path) {
                Ok(source) => source,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("Failed to read config document: {}", self.path.display())
                    })
                }
            };
            let mut document = source.parse::<DocumentMut>().with_context(|| {
                format!("Failed to parse config document: {}", self.path.display())
            })?;
            mutate(&mut document)?;
            let content = document.to_string();
            let snapshot = self.persist_document_locked(&content)?;

            // Keep the snapshot observed by `with_lock` alive until after the
            // mutation. This makes it explicit that the document was patched
            // against the same locked generation, not a pre-lock cached copy.
            drop(disk);
            Ok(ConfigCommit { snapshot })
        })
    }

    /// Restore an exact TOML document only if no process has changed the file
    /// since `expected_revision`. Intended for compensating a successful disk
    /// commit whose runtime application subsequently failed.
    pub fn replace_document_if_revision(
        &self,
        expected_revision: &ConfigRevision,
        content: &str,
    ) -> Result<Option<ConfigCommit>> {
        self.with_lock(false, |disk| {
            let Some(disk) = disk else { return Ok(None) };
            if &disk.revision != expected_revision {
                return Ok(None);
            }
            let snapshot = self.persist_document_locked(content)?;
            Ok(Some(ConfigCommit { snapshot }))
        })
    }

    /// Apply a delta only when the file still has `expected_revision`.
    ///
    /// This is intended for compensating writes after a later operation fails:
    /// a rollback must not overwrite a newer commit made by another process.
    /// `Ok(None)` means the snapshot advanced (or disappeared), so the caller
    /// must leave the newer state untouched and reconcile from disk.
    pub fn update_if_revision<F>(
        &self,
        expected_revision: &ConfigRevision,
        mutate: F,
    ) -> Result<Option<ConfigCommit>>
    where
        F: FnOnce(&mut Config) -> Result<()>,
    {
        self.with_lock(false, |disk| {
            let Some(disk) = disk else {
                return Ok(None);
            };
            if &disk.revision != expected_revision {
                return Ok(None);
            }
            let mut next = disk.config.clone();
            mutate(&mut next)?;
            let snapshot = self.persist_locked(&next, Some(&disk.config))?;
            Ok(Some(ConfigCommit { snapshot }))
        })
    }

    /// Compatibility path for callers that still own a complete Config snapshot.
    /// New mutation sites should prefer [`Self::update`] so unrelated concurrent edits
    /// cannot be overwritten.
    pub fn replace(&self, next: &Config) -> Result<ConfigCommit> {
        self.with_lock(true, |disk| {
            let snapshot = self.persist_locked(next, disk.as_ref().map(|s| &s.config))?;
            Ok(ConfigCommit { snapshot })
        })
    }

    fn with_lock<T>(
        &self,
        tolerate_invalid_disk: bool,
        operation: impl FnOnce(Option<ConfigSnapshot>) -> Result<T>,
    ) -> Result<T> {
        ensure_parent(&self.lock_path)?;
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&self.lock_path)
            .with_context(|| format!("Failed to open config lock: {}", self.lock_path.display()))?;
        lock.lock_exclusive()
            .with_context(|| format!("Failed to lock config: {}", self.lock_path.display()))?;

        let disk = if tolerate_invalid_disk {
            read_snapshot_for_replace(&self.path)?
        } else {
            // Tolerant read: a single malformed `[providers.<name>]` must not
            // block an unrelated write (e.g. `/model`). The invalid section is
            // isolated into `quarantined_providers` and re-emitted verbatim by
            // `persist_locked`, so the transaction succeeds without dropping the
            // user's text. Genuine syntax errors / non-UTF-8 still fail here.
            match read_snapshot_tolerant(&self.path) {
                Ok(snapshot) => Some(snapshot),
                Err(error) if is_not_found(&error) => None,
                Err(error) => return Err(error),
            }
        };
        let result = operation(disk);
        let unlock_result = FileExt::unlock(&lock)
            .with_context(|| format!("Failed to unlock config: {}", self.lock_path.display()));
        match (result, unlock_result) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(value), Ok(())) => Ok(value),
        }
    }

    fn persist_locked(&self, next: &Config, disk: Option<&Config>) -> Result<ConfigSnapshot> {
        ensure_parent(&self.path)?;
        let content = next.serialize_for_disk(disk)?;
        atomic_replace(&self.path, content.as_bytes())?;
        // Parse tolerantly: the content we just wrote may legitimately carry a
        // preserved-but-invalid `[providers.<name>]` section (round-tripped from
        // `quarantined_providers`), which a strict parse would reject.
        let (config, _warnings) = Config::parse_disk_content_tolerant(&content, &self.path)?;
        Ok(ConfigSnapshot {
            config,
            revision: ConfigRevision::from_bytes(content.as_bytes()),
        })
    }

    fn persist_document_locked(&self, content: &str) -> Result<ConfigSnapshot> {
        // Validate before replacement. Tolerant parsing intentionally accepts
        // and quarantines malformed provider entries, matching all other store
        // transactions, while syntax errors and invalid ordinary fields fail.
        let (config, _warnings) = Config::parse_disk_content_tolerant(content, &self.path)?;
        ensure_parent(&self.path)?;
        atomic_replace(&self.path, content.as_bytes())?;
        Ok(ConfigSnapshot {
            config,
            revision: ConfigRevision::from_bytes(content.as_bytes()),
        })
    }
}

fn read_snapshot_for_replace(path: &Path) -> Result<Option<ConfigSnapshot>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("Failed to read config: {}", path.display()))
        }
    };
    let Ok(content) = std::str::from_utf8(&bytes) else {
        return Ok(None);
    };
    let Ok(config) = Config::parse_disk_content(content, path) else {
        return Ok(None);
    };
    Ok(Some(ConfigSnapshot {
        config,
        revision: ConfigRevision::from_bytes(&bytes),
    }))
}

fn sibling_lock_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "config.toml".into());
    name.push(".lock");
    path.with_file_name(name)
}

fn ensure_parent(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create config directory: {}", parent.display()))?;
    }
    Ok(())
}

fn read_snapshot_tolerant(path: &Path) -> Result<ConfigSnapshot> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("Failed to read config: {}", path.display()))?;
    let content = std::str::from_utf8(&bytes)
        .with_context(|| format!("Config is not UTF-8: {}", path.display()))?;
    let (config, _warnings) = Config::parse_disk_content_tolerant(content, path)?;
    Ok(ConfigSnapshot {
        config,
        revision: ConfigRevision::from_bytes(&bytes),
    })
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("Failed to create temporary config in {}", parent.display()))?;
    temp.write_all(bytes)
        .with_context(|| format!("Failed to write temporary config for {}", path.display()))?;
    temp.as_file()
        .sync_all()
        .with_context(|| format!("Failed to sync temporary config for {}", path.display()))?;
    temp.persist(path)
        .map_err(|error| error.error)
        .with_context(|| format!("Failed to atomically replace config: {}", path.display()))?;

    // Persist the directory entry as well on platforms that support syncing directories.
    #[cfg(unix)]
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .with_context(|| format!("Failed to sync config directory: {}", parent.display()))?;
    Ok(())
}

fn is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_update_preserves_comments_unknown_keys_and_provider_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let source = r#"# keep this heading
auto_update = true # keep inline
mystery_flag = "untouched"

[telemetry]
enabled = true

[providers.Custom]
type = "openai"
api_key = "secret"
model = "demo"
base_url = "https://example.invalid/v1"
custom_provider_key = 42
"#;
        std::fs::write(&path, source).unwrap();

        ConfigStore::new(&path)
            .update_document(|document| {
                document["telemetry"]["enabled"] = toml_edit::value(false);
                Ok(())
            })
            .unwrap();

        let updated = std::fs::read_to_string(path).unwrap();
        assert!(updated.contains("# keep this heading"));
        assert!(updated.contains("auto_update = true # keep inline"));
        assert!(updated.contains("mystery_flag = \"untouched\""));
        assert!(updated.contains("custom_provider_key = 42"));
        assert!(updated.contains("enabled = false"));
    }

    #[test]
    fn invalid_document_update_does_not_replace_disk_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let source = "auto_update = true\n";
        std::fs::write(&path, source).unwrap();

        let result = ConfigStore::new(&path).update_document(|document| {
            document["auto_update"] = toml_edit::value("not-a-boolean");
            Ok(())
        });

        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), source);
    }

    #[test]
    fn document_rollback_is_revision_guarded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "auto_update = true # original\n";
        std::fs::write(&path, original).unwrap();
        let store = ConfigStore::new(&path);
        let changed = store
            .update_document(|document| {
                document["auto_update"] = toml_edit::value(false);
                Ok(())
            })
            .unwrap();

        let rolled_back = store
            .replace_document_if_revision(&changed.snapshot.revision, original)
            .unwrap();
        assert!(rolled_back.is_some());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

        let stale = store
            .replace_document_if_revision(&changed.snapshot.revision, "auto_update = false\n")
            .unwrap();
        assert!(stale.is_none());
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }
}
