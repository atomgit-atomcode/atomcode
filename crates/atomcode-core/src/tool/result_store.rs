use sha2::{Digest, Sha256};
use std::path::PathBuf;

use crate::tool::ToolResult;

/// A lightweight reference to a tool result whose full output lives on disk.
/// Stored in conversation messages instead of the full output to save memory/tokens.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolResultRef {
    pub call_id: String,
    /// Hex hash of the full output (content-addressed key).
    pub hash: String,
    /// One-line summary for compact context representation.
    pub summary: String,
    /// Byte length of the full output.
    pub byte_size: usize,
    pub success: bool,
}

/// Content-addressed disk cache for tool result outputs.
///
/// Stores full tool outputs on disk so that conversation messages can hold
/// lightweight `ToolResultRef` instead of multi-KB strings.
pub struct ToolResultStore {
    cache_dir: PathBuf,
}

impl ToolResultStore {
    pub fn new(cache_dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&cache_dir);
        Self { cache_dir }
    }

    /// Default cache directory: `$ATOMCODE_HOME/tool_cache/`
    pub fn default_dir() -> PathBuf {
        crate::config::Config::config_dir().join("tool_cache")
    }

    /// Store a tool result on disk and return a lightweight reference.
    pub fn store(&self, result: &ToolResult) -> ToolResultRef {
        let hash = content_hash(&result.output);
        let path = self.cache_dir.join(format!("{}.txt", hash));

        // Write only if not already cached (content-addressed = idempotent).
        if !path.exists() {
            let _ = std::fs::write(&path, &result.output);
        }

        ToolResultRef {
            call_id: result.call_id.clone(),
            hash,
            summary: make_summary(&result.output, result.success),
            byte_size: result.output.len(),
            success: result.success,
        }
    }

    /// Load the full output from disk. Returns None if the cache entry is missing.
    pub fn load(&self, ref_: &ToolResultRef) -> Option<String> {
        let path = self.cache_dir.join(format!("{}.txt", ref_.hash));
        std::fs::read_to_string(&path).ok()
    }

    /// Reconstruct a full `ToolResult` from a ref by loading from disk.
    /// Falls back to the summary if the cache entry is gone.
    pub fn inflate(&self, ref_: &ToolResultRef) -> ToolResult {
        let output = self.load(ref_).unwrap_or_else(|| ref_.summary.clone());
        ToolResult {
            call_id: ref_.call_id.clone(),
            output,
            success: ref_.success,
        }
    }

    /// Remove all cached files. Used for cleanup / testing.
    pub fn clear(&self) {
        if let Ok(entries) = std::fs::read_dir(&self.cache_dir) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// Produce a hex-encoded SHA-256 hash of the content.
///
/// SHA-256 is used instead of `DefaultHasher` because:
/// 1. `DefaultHasher` is not guaranteed to be stable across Rust versions,
///    which would invalidate existing cache entries after a toolchain upgrade.
/// 2. SHA-256 is collision-resistant, making cache key collisions practically
///    impossible even for adversarial inputs.
fn content_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:064x}", hasher.finalize())
}

/// Generate a one-line summary of tool output.
fn make_summary(output: &str, success: bool) -> String {
    let first_line = output
        .lines()
        .next()
        .unwrap_or(if success { "OK" } else { "Error" });
    if success {
        if first_line.chars().count() > 100 {
            format!("{}...", first_line.chars().take(97).collect::<String>())
        } else {
            first_line.to_string()
        }
    } else {
        let trimmed = if first_line.chars().count() > 80 {
            format!("{}...", first_line.chars().take(77).collect::<String>())
        } else {
            first_line.to_string()
        };
        format!("FAILED: {}", trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolResult;

    fn temp_store() -> (ToolResultStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = ToolResultStore::new(dir.path().to_path_buf());
        (store, dir)
    }

    #[test]
    fn test_store_and_load() {
        let (store, _dir) = temp_store();
        let result = ToolResult {
            call_id: "c1".into(),
            output: "hello world\nsecond line".into(),
            success: true,
        };
        let ref_ = store.store(&result);
        assert_eq!(ref_.call_id, "c1");
        assert!(ref_.success);
        assert_eq!(ref_.byte_size, result.output.len());
        assert_eq!(ref_.summary, "hello world");

        let loaded = store.load(&ref_).unwrap();
        assert_eq!(loaded, "hello world\nsecond line");
    }

    #[test]
    fn test_store_idempotent() {
        let (store, _dir) = temp_store();
        let result = ToolResult {
            call_id: "c1".into(),
            output: "same content".into(),
            success: true,
        };
        let ref1 = store.store(&result);
        let ref2 = store.store(&result);
        assert_eq!(ref1.hash, ref2.hash);
    }

    #[test]
    fn test_inflate_fallback_to_summary() {
        let (store, _dir) = temp_store();
        let ref_ = ToolResultRef {
            call_id: "c1".into(),
            hash: "nonexistent_hash".into(),
            summary: "fallback summary".into(),
            byte_size: 100,
            success: true,
        };
        let inflated = store.inflate(&ref_);
        assert_eq!(inflated.output, "fallback summary");
    }

    #[test]
    fn test_failure_summary() {
        let (store, _dir) = temp_store();
        let result = ToolResult {
            call_id: "c1".into(),
            output: "command not found: xyz".into(),
            success: false,
        };
        let ref_ = store.store(&result);
        assert_eq!(ref_.summary, "FAILED: command not found: xyz");
    }

    #[test]
    fn test_long_output_summary_truncation() {
        let (store, _dir) = temp_store();
        let long_line = "x".repeat(200);
        let result = ToolResult {
            call_id: "c1".into(),
            output: long_line,
            success: true,
        };
        let ref_ = store.store(&result);
        assert!(ref_.summary.chars().count() <= 100);
        assert!(ref_.summary.ends_with("..."));
    }

    #[test]
    fn test_clear() {
        let (store, _dir) = temp_store();
        let result = ToolResult {
            call_id: "c1".into(),
            output: "data".into(),
            success: true,
        };
        let ref_ = store.store(&result);
        assert!(store.load(&ref_).is_some());
        store.clear();
        assert!(store.load(&ref_).is_none());
    }

    #[test]
    fn test_content_hash_stability() {
        // Same content must always produce the same hash.
        let h1 = content_hash("test content");
        let h2 = content_hash("test content");
        assert_eq!(h1, h2);
        // SHA-256 produces 32 bytes = 64 hex chars.
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn test_content_hash_different_inputs() {
        let h1 = content_hash("content A");
        let h2 = content_hash("content B");
        assert_ne!(h1, h2);
    }

    #[test]
    fn test_content_hash_known_value() {
        // Verify that the hash matches a known SHA-256 value.
        // SHA-256("hello") = 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824
        let h = content_hash("hello");
        assert_eq!(h, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
    }
}
