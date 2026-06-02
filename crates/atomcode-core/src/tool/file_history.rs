//! File-level snapshot history — backup files before every edit/write.
//!
//! Inspired by Claude Code's file checkpointing: every file is backed up
//! before modification, stored in `$ATOMCODE_HOME/file-history/{session}/`.
//! No git required. Users can rewind to any previous version via `/undo`.
//!
//! Design:
//! - `backup_before_write(path)` → copies the file to backup dir (no-op if new file)
//! - Backup filename: `{sha256_of_path_first16}@v{version}`
//! - Max 50 versions per file per session
//! - `restore(path, version)` → copies backup back to original location
//! - `list_versions(path)` → returns available versions with timestamps

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Per-file version tracker.
struct FileVersions {
    /// Next version number to assign.
    next_version: u32,
    /// version → backup filename
    backups: Vec<(u32, String, std::time::SystemTime)>,
}

/// File history manager for one session.
pub struct FileHistory {
    /// Base directory: $ATOMCODE_HOME/file-history/{session}/
    backup_dir: PathBuf,
    /// Track versions per file path.
    files: HashMap<String, FileVersions>,
}

const MAX_VERSIONS_PER_FILE: usize = 50;

impl FileHistory {
    pub fn new(session_id: &str) -> Self {
        let config_dir = crate::config::Config::config_dir();
        let backup_dir = config_dir.join("file-history").join(session_id);
        Self {
            backup_dir,
            files: HashMap::new(),
        }
    }

    /// Backup a file before it gets modified.
    /// Call this BEFORE writing/editing. No-op if file doesn't exist (new file).
    /// Returns the backup version number, or None if no backup was needed.
    pub async fn backup_before_write(&mut self, file_path: &str) -> Option<u32> {
        let path = Path::new(file_path);
        if !path.exists() {
            return None; // New file, nothing to backup
        }

        // Ensure backup directory exists
        if let Err(e) = tokio::fs::create_dir_all(&self.backup_dir).await {
            tracing::warn!("[file-history] Failed to create backup dir: {}", e);
            return None;
        }

        let versions = self
            .files
            .entry(file_path.to_string())
            .or_insert_with(|| FileVersions {
                next_version: 1,
                backups: Vec::new(),
            });

        let version = versions.next_version;
        let backup_name = backup_filename(file_path, version);
        let backup_path = self.backup_dir.join(&backup_name);

        // Copy file to backup (system-level copy, no memory overhead)
        if let Err(e) = tokio::fs::copy(file_path, &backup_path).await {
            tracing::warn!("[file-history] Failed to backup {}: {}", file_path, e);
            return None;
        }

        versions
            .backups
            .push((version, backup_name, std::time::SystemTime::now()));
        versions.next_version += 1;

        // Evict old versions if over limit
        while versions.backups.len() > MAX_VERSIONS_PER_FILE {
            if let Some((_, old_name, _)) = versions.backups.first() {
                let old_path = self.backup_dir.join(old_name);
                let _ = tokio::fs::remove_file(&old_path).await;
            }
            versions.backups.remove(0);
        }

        Some(version)
    }

    /// Restore a file to a specific version.
    /// Returns Ok(version) on success, Err(message) on failure.
    pub async fn restore(&self, file_path: &str, version: Option<u32>) -> Result<u32, String> {
        let versions = self
            .files
            .get(file_path)
            .ok_or_else(|| format!("No history for {}", file_path))?;

        let (ver, backup_name, _) = if let Some(v) = version {
            versions
                .backups
                .iter()
                .find(|(bv, _, _)| *bv == v)
                .ok_or_else(|| format!("Version {} not found for {}", v, file_path))?
        } else {
            // Latest version before current
            versions
                .backups
                .last()
                .ok_or_else(|| format!("No backups for {}", file_path))?
        };

        let backup_path = self.backup_dir.join(backup_name);
        tokio::fs::copy(&backup_path, file_path)
            .await
            .map_err(|e| format!("Failed to restore {}: {}", file_path, e))?;

        Ok(*ver)
    }

    /// List all backed-up files and their version counts.
    pub fn list_files(&self) -> Vec<(String, usize)> {
        self.files
            .iter()
            .map(|(path, v)| (path.clone(), v.backups.len()))
            .collect()
    }

    /// Get the most recently backed-up version number for a file.
    pub fn latest_version(&self, file_path: &str) -> Option<u32> {
        self.files
            .get(file_path)
            .and_then(|v| v.backups.last())
            .map(|(ver, _, _)| *ver)
    }

    /// Clean up all backups for this session.
    pub async fn cleanup(&self) {
        let _ = tokio::fs::remove_dir_all(&self.backup_dir).await;
    }
}

/// Generate a deterministic backup filename from file path + version.
fn backup_filename(file_path: &str, version: u32) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    file_path.hash(&mut hasher);
    let hash = hasher.finish();
    format!("{:016x}@v{}", hash, version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_backup_and_restore() {
        let dir = std::env::temp_dir().join("atomcode_test_fh_backup");
        let _ = std::fs::create_dir_all(&dir);
        let test_file = dir.join("test.txt");
        std::fs::write(&test_file, "version 1 content").unwrap();

        let mut fh = FileHistory::new("test-session-1");
        let file_str = test_file.to_string_lossy().to_string();

        // Backup before first edit
        let v = fh.backup_before_write(&file_str).await;
        assert_eq!(v, Some(1));

        // Simulate edit
        std::fs::write(&test_file, "version 2 content").unwrap();

        // Backup before second edit
        let v = fh.backup_before_write(&file_str).await;
        assert_eq!(v, Some(2));

        // Simulate another edit
        std::fs::write(&test_file, "version 3 BROKEN").unwrap();

        // Restore to version 1
        let restored = fh.restore(&file_str, Some(1)).await.unwrap();
        assert_eq!(restored, 1);
        assert_eq!(
            std::fs::read_to_string(&test_file).unwrap(),
            "version 1 content"
        );

        // Restore to latest (version 2)
        std::fs::write(&test_file, "broken again").unwrap();
        let restored = fh.restore(&file_str, None).await.unwrap();
        assert_eq!(restored, 2);
        assert_eq!(
            std::fs::read_to_string(&test_file).unwrap(),
            "version 2 content"
        );

        // Cleanup
        fh.cleanup().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_new_file_no_backup() {
        let mut fh = FileHistory::new("test-session-2");
        let v = fh.backup_before_write("/nonexistent/file.txt").await;
        assert_eq!(v, None);
        fh.cleanup().await;
    }

    #[tokio::test]
    async fn test_eviction() {
        let dir = std::env::temp_dir().join("atomcode_test_fh_evict");
        let _ = std::fs::create_dir_all(&dir);
        let test_file = dir.join("evict.txt");
        std::fs::write(&test_file, "content").unwrap();

        let mut fh = FileHistory::new("test-session-3");
        let file_str = test_file.to_string_lossy().to_string();

        // Create MAX + 5 versions
        for _ in 0..(MAX_VERSIONS_PER_FILE + 5) {
            fh.backup_before_write(&file_str).await;
        }

        let versions = &fh.files[&file_str];
        assert_eq!(versions.backups.len(), MAX_VERSIONS_PER_FILE);

        fh.cleanup().await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_backup_filename_deterministic() {
        let a = backup_filename("/path/to/file.ts", 1);
        let b = backup_filename("/path/to/file.ts", 1);
        assert_eq!(a, b);

        let c = backup_filename("/path/to/file.ts", 2);
        assert_ne!(a, c); // Different version
    }
}
