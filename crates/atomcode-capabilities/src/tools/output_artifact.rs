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

    /// Byte length of a stored artifact — an O(1) `metadata` stat, so the fetch
    /// pagination hint doesn't re-read the whole (≤4 MiB) blob just for its size.
    /// `Ok(None)` for a missing file or an id that isn't `[0-9a-f]{16}`.
    pub fn size(&self, id: &str) -> std::io::Result<Option<u64>> {
        if !is_valid_id(id) {
            return Ok(None);
        }
        match std::fs::metadata(self.dir.join(id)) {
            Ok(m) => Ok(Some(m.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

pub const THRESHOLD_BYTES: usize = 16 * 1024;
/// Stable prefix embedded in a conversation-visible result when the complete
/// tool output was replaced by an artifact-backed head/tail preview.
pub const ARTIFACT_TRUNCATION_MARKER_PREFIX: &str = "[atomcode: output truncated";
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
    fn size_is_metadata_len_and_none_for_missing_or_bad_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = super::ArtifactStore::new(dir.path());
        let id = store.put(&b"z".repeat(1234)).unwrap();
        assert_eq!(store.size(&id).unwrap(), Some(1234));
        assert_eq!(store.size("0123456789abcdef").unwrap(), None); // absent
        assert_eq!(store.size("../etc/passwd").unwrap(), None); // traversal → rejected
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

pub struct FetchOutputTool {
    store: Arc<ArtifactStore>,
}

impl FetchOutputTool {
    pub fn new(store: Arc<ArtifactStore>) -> Self {
        Self { store }
    }
}

#[derive(serde::Deserialize)]
struct FetchArgs {
    artifact_id: String,
    #[serde(default)]
    offset: usize,
    #[serde(default)]
    limit: Option<usize>,
}

const FETCH_MAX_LIMIT: usize = 64 * 1024;

#[async_trait::async_trait]
impl atomcode_kernel::tool::Tool for FetchOutputTool {
    fn name(&self) -> &str {
        "fetch_output"
    }

    fn description(&self) -> &str {
        "Read more of a large tool output that was truncated. Pass the artifact_id from a \
truncation marker plus a byte offset and limit. Returns the requested byte slice; if the \
artifact is unavailable, re-run the original command instead."
    }

    fn read_only_hint(&self) -> bool {
        true
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "artifact_id": {"type": "string", "description": "id from a truncation marker"},
                "offset": {"type": "integer", "description": "byte offset to start at (default 0)"},
                "limit": {"type": "integer", "description": "max bytes to return (default/max 65536)"}
            },
            "required": ["artifact_id"]
        })
    }

    async fn execute(
        &self,
        args: &str,
        _ctx: &atomcode_kernel::tool::ToolContext,
    ) -> atomcode_kernel::tool::ToolResult {
        let parsed: FetchArgs = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => return super::err(format!("invalid fetch_output args: {e}")),
        };

        let limit = parsed.limit.unwrap_or(FETCH_MAX_LIMIT).min(FETCH_MAX_LIMIT);

        match self.store.get(&parsed.artifact_id, parsed.offset, limit) {
            Ok(Some(bytes)) => {
                // Total via an O(1) metadata stat, not a full re-read.
                let total = self
                    .store
                    .size(&parsed.artifact_id)
                    .ok()
                    .flatten()
                    .unwrap_or(0) as usize;
                // Clamp the reported window to the artifact so an offset past the
                // end yields a coherent "at end" hint (never "5000–5000 of 3000").
                // `start <= total` holds, so `end` lands in `[start, total]`.
                let start = parsed.offset.min(total);
                let end = start.saturating_add(bytes.len()).min(total);
                let body = String::from_utf8_lossy(&bytes);
                let hint = if end < total {
                    format!(
                        "\n\n[showing bytes {start}–{end} of {total}; call fetch_output(artifact_id=\"{}\", offset={end}) for more]",
                        parsed.artifact_id
                    )
                } else {
                    format!("\n\n[showing bytes {start}–{end} of {total} (end)]")
                };
                super::ok(format!("{body}{hint}"))
            }
            Ok(None) => super::err(format!(
                "Artifact {} is no longer available (truncated captures don't survive across machines or after cleanup). \
Re-run the original command to regenerate its output.",
                parsed.artifact_id
            )),
            Err(e) => super::err(format!("fetch_output failed: {e}")),
        }
    }
}

#[cfg(test)]
mod fetch_output_tests {
    use super::*;
    use atomcode_kernel::tool::{Tool, ToolContext};
    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        }
    }

    #[tokio::test]
    async fn fetch_slices_paginates_and_reports_missing() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(ArtifactStore::new(dir.path()));
        let id = store.put(&b"A".repeat(100_000)).unwrap();
        let tool = FetchOutputTool::new(store.clone());
        let test_ctx = ctx(dir.path());

        // slice with explicit offset/limit
        let r = tool
            .execute(
                &format!(r#"{{"artifact_id":"{}","offset":0,"limit":10}}"#, id),
                &test_ctx,
            )
            .await;
        assert!(!r.is_error, "first fetch should succeed: {}", r.content);
        assert!(
            r.content.starts_with("AAAAAAAAAA"),
            "content should start with 10 As: {}",
            r.content
        );
        assert!(
            r.content.contains("of 100000"),
            "pagination hint should mention total: {}",
            r.content
        );

        // limit hard-capped at 64 KiB even if bigger requested
        let r = tool
            .execute(
                &format!(
                    r#"{{"artifact_id":"{}","offset":0,"limit":999999}}"#,
                    id
                ),
                &test_ctx,
            )
            .await;
        assert!(
            !r.is_error,
            "fetch with huge limit should succeed (get capped): {}",
            r.content
        );
        assert!(
            r.content.contains("65536") || r.content.contains("of 100000"),
            "pagination hint should show the hard cap or total: {}",
            r.content
        );

        // missing artifact → terminal, actionable error, no "fetch" retry wording
        let r = tool
            .execute(
                r#"{"artifact_id":"0000000000000000","offset":0,"limit":10}"#,
                &test_ctx,
            )
            .await;
        assert!(r.is_error, "missing artifact should be an error: {}", r.content);
        assert!(
            r.content.to_lowercase().contains("re-run"),
            "error should tell user to re-run: {}",
            r.content
        );
        assert!(
            !r.content.to_lowercase().contains("try fetch again"),
            "error should not suggest fetching again: {}",
            r.content
        );

        // offset PAST the end → coherent "at end" hint, not "N–N of <smaller>".
        let small = std::sync::Arc::new(ArtifactStore::new(dir.path()));
        let sid = small.put(b"abc").unwrap(); // 3 bytes
        let tool2 = FetchOutputTool::new(small);
        let r = tool2
            .execute(
                &format!(r#"{{"artifact_id":"{}","offset":5000,"limit":10}}"#, sid),
                &test_ctx,
            )
            .await;
        assert!(!r.is_error, "past-end fetch is not an error: {}", r.content);
        assert!(
            r.content.contains("3–3 of 3 (end)"),
            "past-end window clamps to total, coherent hint: {}",
            r.content
        );
    }
}
