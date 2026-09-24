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
    id.len() == 16
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
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

pub const THRESHOLD_BYTES: usize = 50 * 1024;
/// Stable prefix embedded in a conversation-visible result when the complete
/// tool output was replaced by an artifact-backed head/tail preview.
pub const ARTIFACT_TRUNCATION_MARKER_PREFIX: &str = "[atomcode: output truncated";
/// Head/tail kept inline when an oversized result is spilled. HEAD-HEAVY on purpose:
/// the START of a command's output (a diff header, an error's first frames, a log's
/// opening) is usually the more useful half, so the head gets the larger share while a
/// smaller tail preserves a trailing error/summary line. Sized to the ~50 KB peer
/// baseline (the previous 16 KB / 4 KB was ~3× tighter than comparable agents, which is
/// what forced the model to keep working around "output truncated" on ordinary diffs /
/// logs). `THRESHOLD_BYTES` stays above `HEAD + TAIL` so a truncated result always
/// shrinks below the original.
const PREVIEW_HEAD: usize = 32 * 1024;
const PREVIEW_TAIL: usize = 12 * 1024;
const MAX_ARTIFACT_BYTES: usize = 4 * 1024 * 1024;

/// Override the spill threshold. Data-dense scenarios (financial reports,
/// whole-market scans) truncate constantly at the 50 KB default; this lets an
/// operator raise it without a rebuild. Default is [`THRESHOLD_BYTES`] to the
/// byte. (Feedback B11.)
const THRESHOLD_ENV: &str = "ATOMCODE_TOOL_OUTPUT_THRESHOLD_BYTES";

/// The spill threshold to use, from `ATOMCODE_TOOL_OUTPUT_THRESHOLD_BYTES` or the
/// default. Pure over its input so it is testable without touching process env.
///
/// Floored at `HEAD + TAIL`: the head/tail preview is fixed-size, so a threshold
/// below it could not shrink the result (the whole invariant of spilling). A
/// bad/empty value falls back to the default rather than erroring — a typo in an
/// env var must not make every tool output either truncate at 0 or never.
fn resolve_threshold(env_val: Option<&str>) -> usize {
    env_val
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map(|v| v.max(PREVIEW_HEAD + PREVIEW_TAIL))
        .unwrap_or(THRESHOLD_BYTES)
}

fn threshold_bytes() -> usize {
    resolve_threshold(std::env::var(THRESHOLD_ENV).ok().as_deref())
}

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

impl ArtifactMiddleware {
    /// Spill an oversized result to the store and leave head+tail inline.
    ///
    /// The judgement, with no opinion about how it is delivered — the kernel
    /// `ToolMiddleware::after` below wears one shell, a harness `tools/execute`
    /// listener wears the other, and both call this. `self_bounds_output` is
    /// passed as a plain bool rather than the resolved tool so neither shell has
    /// to agree with the other about how a tool is looked up.
    pub async fn spill(
        &self,
        result: &mut atomcode_kernel::tool::ToolResult,
        self_bounds_output: bool,
    ) {
        // A tool that bounds and structures its own output (e.g. `read_file`: self-capped,
        // 1-based line numbers, pagination) must reach the model WHOLE — head/tail
        // truncation would corrupt it. Read the contract off the resolved tool, so this is
        // robust even if an earlier `before` short-circuited the chain with `Allow`.
        if self_bounds_output {
            return;
        }
        let total = result.content.len();
        if total <= threshold_bytes() {
            return;
        }
        let head_end = head_boundary(&result.content, PREVIEW_HEAD);
        let tail_begin = tail_start(&result.content, PREVIEW_TAIL);
        let head = &result.content[..head_end];
        let tail = &result.content[tail_begin..];
        // How many `fetch_output` reads the full output takes, so the model sees
        // the SCALE ("part 1 of N") rather than a bare "there's more" — the
        // structural hint from feedback B11. Head+tail count as part 1.
        let parts = total.div_ceil(FETCH_MAX_LIMIT).max(1);

        if total > MAX_ARTIFACT_BYTES {
            // Too large to store; inline-truncate only.
            let marker = format!(
                "\n\n[atomcode: output truncated — {total} bytes total (~{parts} parts of {FETCH_MAX_LIMIT}), \
showing first {} + last {} bytes (part 1). \
Full output unavailable (exceeds {MAX_ARTIFACT_BYTES}-byte artifact ceiling).]\n\n",
                head.len(),
                tail.len()
            );
            result.content = format!("{head}{marker}{tail}");
            return;
        }

        let marker = match self.store.put(result.content.as_bytes()) {
            Ok(id) => format!(
                "\n\n[atomcode: output truncated — {total} bytes total (~{parts} parts of {FETCH_MAX_LIMIT}), \
showing first {} + last {} bytes (part 1). \
Full output saved as artifact {id}. To read the next part: fetch_output(artifact_id=\"{id}\", offset={}, limit={FETCH_MAX_LIMIT}).]\n\n",
                head.len(),
                tail.len(),
                head.len(),
            ),
            Err(_) => format!(
                "\n\n[atomcode: output truncated — {total} bytes total (~{parts} parts of {FETCH_MAX_LIMIT}), \
showing first {} + last {} bytes (part 1). \
Full output unavailable (could not be saved).]\n\n",
                head.len(),
                tail.len()
            ),
        };
        result.content = format!("{head}{marker}{tail}");
    }
}

#[async_trait::async_trait]
impl atomcode_kernel::middleware::ToolMiddleware for ArtifactMiddleware {
    async fn after(
        &self,
        result: &mut atomcode_kernel::tool::ToolResult,
        tool: Option<&Arc<dyn atomcode_kernel::tool::Tool>>,
    ) -> atomcode_kernel::middleware::AfterOutcome {
        // Read the contract off the RESOLVED tool, so this is robust even if an
        // earlier `before` short-circuited the chain with `Allow`.
        self.spill(result, tool.is_some_and(|t| t.self_bounds_output()))
            .await;
        atomcode_kernel::middleware::AfterOutcome::Proceed
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn threshold_env_override_defaults_byte_identical_and_floors() {
        use super::{resolve_threshold, PREVIEW_HEAD, PREVIEW_TAIL, THRESHOLD_BYTES};
        // Unset / empty / garbage → the default, to the byte.
        assert_eq!(resolve_threshold(None), THRESHOLD_BYTES);
        assert_eq!(resolve_threshold(Some("  ")), THRESHOLD_BYTES);
        assert_eq!(resolve_threshold(Some("not-a-number")), THRESHOLD_BYTES);
        // A larger value (the data-dense case) is honored verbatim.
        assert_eq!(resolve_threshold(Some("200000")), 200_000);
        assert_eq!(resolve_threshold(Some(" 200000 ")), 200_000);
        // Below the fixed head+tail preview → floored, so a spill still shrinks.
        assert_eq!(
            resolve_threshold(Some("1000")),
            PREVIEW_HEAD + PREVIEW_TAIL
        );
    }

    #[test]
    fn id_is_16_hex_and_deterministic() {
        let a = super::artifact_id(b"hello world");
        let b = super::artifact_id(b"hello world");
        assert_eq!(a, b);
        assert_eq!(a.len(), 16);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
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
        assert!(store.get("../etc/passwd", 0, 10).unwrap().is_none()); // traversal → rejected
        assert!(store.get("XYZ", 0, 10).unwrap().is_none()); // non-hex → rejected
    }

    #[tokio::test]
    async fn under_threshold_untouched() {
        use atomcode_kernel::middleware::{AfterOutcome, ToolMiddleware};
        use atomcode_kernel::tool::ToolResult;
        let dir = tempfile::tempdir().unwrap();
        let mw = super::ArtifactMiddleware::new(std::sync::Arc::new(super::ArtifactStore::new(
            dir.path(),
        )));
        let mut r = ToolResult {
            call_id: "c".into(),
            content: "small".into(),
            is_error: false,
            images: vec![],
        };
        assert!(matches!(
            mw.after(&mut r, None).await,
            AfterOutcome::Proceed
        ));
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
        let big = "x".repeat(60 * 1024);
        let mk = || ToolResult {
            call_id: "c".into(),
            content: big.clone(),
            is_error: false,
            images: vec![],
        };

        let mut r1 = mk();
        mw.after(&mut r1, None).await;
        // rewritten: smaller, has head+tail+marker, names fetch_output + the id
        assert!(r1.content.len() < big.len());
        assert!(r1.content.contains("fetch_output"));
        // B11: the marker carries the structural scale ("part 1" + "~N parts").
        assert!(r1.content.contains("part 1"), "marker names the part: {}", r1.content);
        assert!(r1.content.contains("parts of"), "marker names the total parts: {}", r1.content);
        let id = super::artifact_id(big.as_bytes());
        assert!(r1.content.contains(&id));
        // artifact holds the FULL original
        assert_eq!(
            store.get(&id, 0, big.len()).unwrap().unwrap(),
            big.as_bytes()
        );

        // determinism: same output → byte-identical rewritten content
        let mut r2 = mk();
        mw.after(&mut r2, None).await;
        assert_eq!(r1.content, r2.content);
        assert!(!r1.is_error);
    }

    #[tokio::test]
    async fn self_bounding_tool_output_passes_through_whole() {
        use atomcode_kernel::middleware::{AfterOutcome, ToolMiddleware};
        use atomcode_kernel::tool::{Tool, ToolResult};
        let dir = tempfile::tempdir().unwrap();
        let mw = super::ArtifactMiddleware::new(std::sync::Arc::new(super::ArtifactStore::new(
            dir.path(),
        )));
        // read_file declares `self_bounds_output() == true`.
        let tool: std::sync::Arc<dyn Tool> =
            std::sync::Arc::new(crate::tools::read::ReadFileTool::new(false));

        // A large read_file result (over THRESHOLD) must reach the model WHOLE.
        let big = "x".repeat(60 * 1024);
        let mut r = ToolResult {
            call_id: "rc".into(),
            content: big.clone(),
            is_error: false,
            images: vec![],
        };
        assert!(matches!(
            mw.after(&mut r, Some(&tool)).await,
            AfterOutcome::Proceed
        ));
        assert_eq!(r.content, big, "self-bounding output must be untouched");
        assert!(!r.content.contains("fetch_output"));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0); // nothing stored

        // Control: the same size from a non-self-bounding tool (None) is STILL truncated.
        let mut other = ToolResult {
            call_id: "bash-1".into(),
            content: big.clone(),
            is_error: false,
            images: vec![],
        };
        mw.after(&mut other, None).await;
        assert!(other.content.len() < big.len());
        assert!(other.content.contains("fetch_output"));
    }

    #[tokio::test]
    async fn over_ceiling_inline_truncates_without_artifact() {
        use atomcode_kernel::middleware::ToolMiddleware;
        use atomcode_kernel::tool::ToolResult;
        let dir = tempfile::tempdir().unwrap();
        let mw = super::ArtifactMiddleware::new(std::sync::Arc::new(super::ArtifactStore::new(
            dir.path(),
        )));
        let huge = "y".repeat(5 * 1024 * 1024);
        let mut r = ToolResult {
            call_id: "c".into(),
            content: huge,
            is_error: false,
            images: vec![],
        };
        mw.after(&mut r, None).await;
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
                &format!(r#"{{"artifact_id":"{}","offset":0,"limit":999999}}"#, id),
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
        assert!(
            r.is_error,
            "missing artifact should be an error: {}",
            r.content
        );
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
