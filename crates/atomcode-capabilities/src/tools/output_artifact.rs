//! Content-addressed storage for full tool outputs — large blobs indexed by
//! sha256 hash, one directory per session. Conversation carries only previews;
//! full outputs live on disk, deduplicated by content.

use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;

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

pub const THRESHOLD_BYTES: usize = 16 * 1024;
const PREVIEW_HALF: usize = 4 * 1024;
const MAX_ARTIFACT_BYTES: usize = 4 * 1024 * 1024;

/// Largest char-boundary index ≤ n.
fn head_boundary(s: &str, n: usize) -> usize {
    let mut i = n.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smallest char-boundary index ≥ (len - n).
fn tail_start(s: &str, n: usize) -> usize {
    let mut i = s.len().saturating_sub(n);
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

pub struct ArtifactMiddleware {
    store: Arc<ArtifactStore>,
}

impl ArtifactMiddleware {
    pub fn new(store: Arc<ArtifactStore>) -> Self {
        Self { store }
    }
}

#[async_trait::async_trait]
impl atomcode_kernel::middleware::ToolMiddleware for ArtifactMiddleware {
    async fn after(
        &self,
        result: &mut atomcode_kernel::tool::ToolResult,
    ) -> atomcode_kernel::middleware::AfterOutcome {
        let total = result.content.len();
        if total <= THRESHOLD_BYTES {
            return atomcode_kernel::middleware::AfterOutcome::Proceed;
        }
        let head_end = head_boundary(&result.content, PREVIEW_HALF);
        let tail_begin = tail_start(&result.content, PREVIEW_HALF);
        let head = &result.content[..head_end];
        let tail = &result.content[tail_begin..];

        if total > MAX_ARTIFACT_BYTES {
            // Too large to store; inline-truncate only.
            let marker = format!(
                "\n\n[atomcode: output truncated — {total} bytes total, showing first {} + last {} bytes. \
Full output unavailable (exceeds {MAX_ARTIFACT_BYTES}-byte artifact ceiling).]\n\n",
                head.len(),
                tail.len()
            );
            result.content = format!("{head}{marker}{tail}");
            return atomcode_kernel::middleware::AfterOutcome::Proceed;
        }

        let marker = match self.store.put(result.content.as_bytes()) {
            Ok(id) => format!(
                "\n\n[atomcode: output truncated — {total} bytes total, showing first {} + last {} bytes. \
Full output saved as artifact {id}. To read more: fetch_output(artifact_id=\"{id}\", offset, limit).]\n\n",
                head.len(),
                tail.len()
            ),
            Err(_) => format!(
                "\n\n[atomcode: output truncated — {total} bytes total, showing first {} + last {} bytes. \
Full output unavailable (could not be saved).]\n\n",
                head.len(),
                tail.len()
            ),
        };
        result.content = format!("{head}{marker}{tail}");
        atomcode_kernel::middleware::AfterOutcome::Proceed
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

    #[tokio::test]
    async fn under_threshold_untouched() {
        use atomcode_kernel::middleware::{AfterOutcome, ToolMiddleware};
        use atomcode_kernel::tool::ToolResult;
        let dir = tempfile::tempdir().unwrap();
        let mw = super::ArtifactMiddleware::new(std::sync::Arc::new(super::ArtifactStore::new(dir.path())));
        let mut r = ToolResult { call_id: "c".into(), content: "small".into(), is_error: false, images: vec![] };
        assert!(matches!(mw.after(&mut r).await, AfterOutcome::Proceed));
        assert_eq!(r.content, "small");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0); // nothing stored
    }

    #[tokio::test]
    async fn over_threshold_stores_and_rewrites_deterministically() {
        use atomcode_kernel::middleware::ToolMiddleware;
        use atomcode_kernel::tool::ToolResult;
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(super::ArtifactStore::new(dir.path()));
        let mw = super::ArtifactMiddleware::new(store.clone());
        let big = "x".repeat(20 * 1024);
        let mk = || ToolResult { call_id: "c".into(), content: big.clone(), is_error: false, images: vec![] };

        let mut r1 = mk();
        mw.after(&mut r1).await;
        // rewritten: smaller, has head+tail+marker, names fetch_output + the id
        assert!(r1.content.len() < big.len());
        assert!(r1.content.contains("fetch_output"));
        let id = super::artifact_id(big.as_bytes());
        assert!(r1.content.contains(&id));
        // artifact holds the FULL original
        assert_eq!(store.get(&id, 0, big.len()).unwrap().unwrap(), big.as_bytes());

        // determinism: same output → byte-identical rewritten content
        let mut r2 = mk();
        mw.after(&mut r2).await;
        assert_eq!(r1.content, r2.content);
        assert!(!r1.is_error);
    }

    #[tokio::test]
    async fn over_ceiling_inline_truncates_without_artifact() {
        use atomcode_kernel::middleware::ToolMiddleware;
        use atomcode_kernel::tool::ToolResult;
        let dir = tempfile::tempdir().unwrap();
        let mw = super::ArtifactMiddleware::new(std::sync::Arc::new(super::ArtifactStore::new(dir.path())));
        let huge = "y".repeat(5 * 1024 * 1024);
        let mut r = ToolResult { call_id: "c".into(), content: huge, is_error: false, images: vec![] };
        mw.after(&mut r).await;
        assert!(r.content.contains("Full output unavailable"));
        assert!(!r.content.contains("fetch_output"));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
