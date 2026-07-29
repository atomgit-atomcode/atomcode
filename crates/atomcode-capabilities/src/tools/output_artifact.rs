//! Content-addressed storage for full tool outputs — large blobs indexed by
//! sha256 hash, one directory per session. Conversation carries only previews;
//! full outputs live on disk, deduplicated by content.

use sha2::{Digest, Sha256};
use std::path::PathBuf;

/// 16 lowercase hex chars of sha256 — deterministic content id (dedup + cache-safe).
pub fn artifact_id(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut s = String::with_capacity(16);
    for b in &digest[..8] {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn is_valid_id(id: &str) -> bool {
    id.len() == 16 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Content-addressed store for full tool outputs, one directory per session.
pub struct ArtifactStore {
    dir: PathBuf,
}

impl ArtifactStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn put(&self, bytes: &[u8]) -> std::io::Result<String> {
        let id = artifact_id(bytes);
        let path = self.dir.join(&id);
        if !path.exists() {
            std::fs::create_dir_all(&self.dir)?;
            // Write to a temp sibling then rename → readers never see a partial file.
            let tmp = self.dir.join(format!("{id}.tmp"));
            std::fs::write(&tmp, bytes)?;
            std::fs::rename(&tmp, &path)?;
        }
        Ok(id)
    }

    pub fn get(&self, id: &str, offset: usize, limit: usize) -> std::io::Result<Option<Vec<u8>>> {
        if !is_valid_id(id) {
            return Ok(None);
        }
        let path = self.dir.join(id);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let start = offset.min(bytes.len());
        let end = start.saturating_add(limit).min(bytes.len());
        Ok(Some(bytes[start..end].to_vec()))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn id_is_16_hex_and_deterministic() {
        let a = super::artifact_id(b"hello world");
        let b = super::artifact_id(b"hello world");
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(a, super::artifact_id(b"hello worlD"));
    }

    #[test]
    fn put_get_roundtrip_and_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let store = super::ArtifactStore::new(dir.path());
        let id = store.put(b"0123456789abcdef").unwrap();
        // dedup: same bytes → same id, one file
        assert_eq!(store.put(b"0123456789abcdef").unwrap(), id);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
        // slice
        assert_eq!(store.get(&id, 2, 4).unwrap().unwrap(), b"2345");
        // offset past end → empty
        assert_eq!(store.get(&id, 100, 4).unwrap().unwrap(), b"");
        // limit past end → clamped
        assert_eq!(store.get(&id, 14, 999).unwrap().unwrap(), b"ef");
    }

    #[test]
    fn get_missing_or_bad_id_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = super::ArtifactStore::new(dir.path());
        assert!(store.get("0123456789abcdef", 0, 10).unwrap().is_none()); // absent
        assert!(store.get("../etc/passwd", 0, 10).unwrap().is_none());    // traversal → rejected
        assert!(store.get("XYZ", 0, 10).unwrap().is_none());              // non-hex → rejected
    }
}
