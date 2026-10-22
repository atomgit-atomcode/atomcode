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

/// Resolve a host-owned optional store when a call/schema is built, not at mount.
pub type ArtifactStoreProvider = Arc<dyn Fn() -> Option<Arc<ArtifactStore>> + Send + Sync>;

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

pub(crate) fn shell_preview_lines(
    requested: Option<u64>,
    available: bool,
) -> Result<Option<usize>, String> {
    match requested {
        None => Ok(None),
        Some(_) if !available => {
            Err("output_tail_lines requires a configured output artifact store".into())
        }
        Some(lines @ 1..=2000) => Ok(Some(lines as usize)),
        Some(_) => Err("output_tail_lines must be between 1 and 2000".into()),
    }
}

pub(crate) fn add_shell_preview_schema(schema: &mut serde_json::Value) {
    schema["properties"]["output_tail_lines"] = serde_json::json!({
        "type": "integer", "minimum": 1, "maximum": 2000,
        "description": "Optional final output lines to show (1–2000, also bounded to a 12KiB UTF-8 preview). Execution is unchanged; process status is retained separately. Omitted captured text is saved as an artifact when within storage limits. Use this instead of shell output filters."
    });
}
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

/// The spill threshold to use: `ATOMCODE_TOOL_OUTPUT_THRESHOLD_BYTES`, else the
/// configured value (`[tools.output] threshold_bytes`), else the default. The
/// environment wins so an operator can override a file without editing it; the
/// file exists for a process that cannot be given an environment, such as a
/// daemon an editor extension starts. Pure over its inputs so it is testable
/// without touching process env.
///
/// Clamped to `[HEAD + TAIL + marker, MAX_ARTIFACT_BYTES]`:
/// - **Floor** — the preview is head + tail + a ~few-hundred-byte marker, so a
///   threshold below that could make a "truncated" result *larger* than the
///   original (breaking the whole point of spilling); the 1 KiB slack covers the
///   marker with room to spare.
/// - **Ceiling** — never above the artifact ceiling, or a result between the
///   ceiling and an over-large threshold would reach the model *whole* and blow
///   the context (the exact failure `MAX_ARTIFACT_BYTES` exists to prevent).
///
/// A bad/empty value falls back to the default rather than erroring — a typo in
/// an env var must not make every tool output either truncate at 0 or never.
fn resolve_threshold(env_val: Option<&str>, configured: Option<usize>) -> usize {
    const FLOOR: usize = PREVIEW_HEAD + PREVIEW_TAIL + 1024;
    env_val
        .and_then(|v| v.trim().parse::<usize>().ok())
        .or(configured)
        .map(|v| v.clamp(FLOOR, MAX_ARTIFACT_BYTES))
        .unwrap_or(THRESHOLD_BYTES)
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

/// The opening of a truncation marker: what is shown, what is NOT, and that
/// nothing in the missing range may be used.
///
/// Said as a byte range rather than left for the model to work out from "first
/// N + last M bytes". A preview of structured output (JSON, a table) cut in two
/// reads as one broken document, and a model that does not see it is missing a
/// stretch fills the stretch in: in a financial deployment on 5.1.0 an answer
/// quoted three figures, credited to the tool, that appeared in none of its
/// results. The marker ends at the first `]` after its prefix (see
/// `next_prompt_suggestion`), so none may appear before the end.
fn missing_range(total: usize, head_end: usize, tail_begin: usize) -> String {
    format!(
        "{ARTIFACT_TRUNCATION_MARKER_PREFIX} — {total} bytes total. Shown: bytes 0–{head_end} and \
{tail_begin}–{total}. NOT shown: bytes {head_end}–{tail_begin} ({} bytes) — do not quote, total or infer \
any value from that range until you have read it.",
        tail_begin - head_end
    )
}

pub struct ArtifactMiddleware {
    store: Arc<ArtifactStore>,
    /// `[tools.output] threshold_bytes`, when the deployment set one.
    threshold: Option<usize>,
}

impl ArtifactMiddleware {
    /// A caller-requested shell preview. Only presentation changes; the caller
    /// supplies the status from its process result, never from output text.
    pub(crate) fn shell_tail(
        &self,
        result: &mut atomcode_kernel::tool::ToolResult,
        lines: usize,
        status: &str,
        status_in_body: bool,
        notice: &str,
    ) {
        let captured = result.content.trim_end_matches('\n');
        let body = if status_in_body {
            captured
                .strip_suffix(status)
                .map(|s| s.trim_end_matches('\n'))
                .unwrap_or(captured)
        } else {
            captured
        };
        let line_start = body
            .rmatch_indices('\n')
            .nth(lines - 1)
            .map_or(0, |(i, _)| i + 1);
        let start = line_start.max(tail_start(body, PREVIEW_TAIL));
        if start > 0 {
            let capture = if result.content.len() > MAX_ARTIFACT_BYTES {
                format!("Full captured result unavailable (exceeds {MAX_ARTIFACT_BYTES}-byte artifact ceiling).")
            } else {
                match self.store.put(result.content.as_bytes()) {
                    Ok(id) => format!("Full captured result saved as artifact {id}; read with fetch_output(artifact_id=\"{id}\", offset=0, limit={FETCH_MAX_LIMIT})."),
                    Err(_) => "Full captured result unavailable (could not be saved).".into(),
                }
            };
            result.content = format!(
                "{notice}{ARTIFACT_TRUNCATION_MARKER_PREFIX} — tail preview; earlier bytes 0–{start} omitted. {capture}]\n{}",
                &body[start..],
            );
        }
        if !result.content.ends_with(status) {
            if !result.content.ends_with('\n') {
                result.content.push('\n');
            }
            result.content.push_str(status);
        }
    }

    pub fn new(store: Arc<ArtifactStore>) -> Self {
        Self {
            store,
            threshold: None,
        }
    }

    /// Spill above `threshold` bytes rather than the default; see
    /// [`resolve_threshold`] for how it ranks against the environment.
    pub fn with_threshold(mut self, threshold: Option<usize>) -> Self {
        self.threshold = threshold;
        self
    }

    fn threshold_bytes(&self) -> usize {
        resolve_threshold(std::env::var(THRESHOLD_ENV).ok().as_deref(), self.threshold)
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
        if total <= self.threshold_bytes() {
            return;
        }
        let head_end = head_boundary(&result.content, PREVIEW_HEAD);
        let tail_begin = tail_start(&result.content, PREVIEW_TAIL);
        let head = &result.content[..head_end];
        let tail = &result.content[tail_begin..];
        let missing = missing_range(total, head_end, tail_begin);

        if total > MAX_ARTIFACT_BYTES {
            // Too large to store; inline-truncate only.
            let marker = format!(
                "\n\n{missing} Full output unavailable (exceeds {MAX_ARTIFACT_BYTES}-byte artifact ceiling); \
re-run the tool call with narrower output to see that range.]\n\n"
            );
            result.content = format!("{head}{marker}{tail}");
            return;
        }

        let marker = match self.store.put(result.content.as_bytes()) {
            Ok(id) => format!(
                "\n\n{missing} Full output saved as artifact {id}; the missing range is {} part(s) of up to \
{FETCH_MAX_LIMIT} bytes. Read part 1: fetch_output(artifact_id=\"{id}\", offset={head_end}, limit={FETCH_MAX_LIMIT}).]\n\n",
                (tail_begin - head_end).div_ceil(FETCH_MAX_LIMIT).max(1),
            ),
            Err(_) => format!(
                "\n\n{missing} Full output unavailable (could not be saved); re-run the tool call with narrower \
output to see that range.]\n\n"
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
    fn requested_shell_tail_keeps_status_and_saves_utf8_capture() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(super::ArtifactStore::new(dir.path()));
        let preview = super::ArtifactMiddleware::new(store.clone());
        let full = "首行错误\n继续\n末行\n[exit code 23]";
        let mut result = atomcode_kernel::tool::ToolResult {
            content: full.into(),
            ..Default::default()
        };
        preview.shell_tail(&mut result, 2, "[exit code 23]", true, "");
        assert!(!result.content.contains("首行错误"));
        assert!(result.content.ends_with("末行\n[exit code 23]"));
        let id = super::artifact_id(full.as_bytes());
        assert_eq!(
            store.get(&id, 0, full.len()).unwrap().unwrap(),
            full.as_bytes()
        );
        assert!(result.content.contains(&id));
        assert!(!result.is_error);
    }

    #[test]
    fn shell_tail_storage_failure_and_ceiling_do_not_claim_saved_output() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, "not a directory").unwrap();
        for (path, full, reason) in [
            (blocker, "first\nlast".to_string(), "could not be saved"),
            (
                dir.path().join("ceiling"),
                format!("{}\nlast", "x".repeat(super::MAX_ARTIFACT_BYTES)),
                "artifact ceiling",
            ),
        ] {
            let store = std::sync::Arc::new(super::ArtifactStore::new(&path));
            let preview = super::ArtifactMiddleware::new(store);
            let mut result = atomcode_kernel::tool::ToolResult {
                content: full,
                ..Default::default()
            };
            preview.shell_tail(
                &mut result,
                1,
                "[exit code 23]",
                false,
                "[earlier output dropped]\n",
            );
            assert!(result.content.contains(reason));
            assert!(!result.content.contains("saved as artifact"));
            assert!(result.content.starts_with("[earlier output dropped]"));
            assert!(result.content.ends_with("last\n[exit code 23]"));
        }
    }

    #[test]
    fn shell_preview_validation_is_explicit() {
        assert_eq!(super::shell_preview_lines(None, false).unwrap(), None);
        assert_eq!(super::shell_preview_lines(Some(1), true).unwrap(), Some(1));
        assert_eq!(
            super::shell_preview_lines(Some(2000), true).unwrap(),
            Some(2000)
        );
        for n in [0, 2001, u64::MAX] {
            assert!(super::shell_preview_lines(Some(n), true).is_err());
        }
        assert!(super::shell_preview_lines(Some(12), false).is_err());
    }

    #[test]
    fn a_single_huge_utf8_line_is_bounded_without_losing_status() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(super::ArtifactStore::new(dir.path()));
        let preview = super::ArtifactMiddleware::new(store.clone());
        let full = format!("{}\n[exit code 0]", "🦀".repeat(super::PREVIEW_TAIL));
        let mut result = atomcode_kernel::tool::ToolResult {
            content: full.clone(),
            ..Default::default()
        };
        preview.shell_tail(&mut result, 1, "[exit code 0]", true, "");
        assert!(result.content.len() < super::PREVIEW_TAIL + 1024);
        assert!(result.content.ends_with("[exit code 0]"));
        assert!(!result.content.contains('�'));
        assert_eq!(
            store
                .get(&super::artifact_id(full.as_bytes()), 0, full.len())
                .unwrap()
                .unwrap(),
            full.as_bytes()
        );
    }
    #[test]
    fn threshold_env_override_defaults_byte_identical_and_clamps() {
        use super::{
            resolve_threshold, MAX_ARTIFACT_BYTES, PREVIEW_HEAD, PREVIEW_TAIL, THRESHOLD_BYTES,
        };
        let floor = PREVIEW_HEAD + PREVIEW_TAIL + 1024;
        // Unset / empty / garbage → the default, to the byte.
        assert_eq!(resolve_threshold(None, None), THRESHOLD_BYTES);
        assert_eq!(resolve_threshold(Some("  "), None), THRESHOLD_BYTES);
        assert_eq!(
            resolve_threshold(Some("not-a-number"), None),
            THRESHOLD_BYTES
        );
        // A larger value (the data-dense case) is honored verbatim.
        assert_eq!(resolve_threshold(Some("200000"), None), 200_000);
        assert_eq!(resolve_threshold(Some(" 200000 "), None), 200_000);
        // Below head+tail+marker → floored, so a spill still SHRINKS the result.
        assert_eq!(resolve_threshold(Some("1000"), None), floor);
        // Above the artifact ceiling → clamped, so the hard context cap holds.
        assert_eq!(
            resolve_threshold(Some("8388608"), None),
            MAX_ARTIFACT_BYTES,
            "an over-large threshold must not defeat the {MAX_ARTIFACT_BYTES}-byte ceiling"
        );
    }

    /// `[tools.output] threshold_bytes` is used when the environment says
    /// nothing usable, is clamped the same way, and gives way to the
    /// environment when both are set.
    #[test]
    fn a_configured_threshold_is_used_clamped_and_yields_to_the_environment() {
        use super::{
            resolve_threshold, MAX_ARTIFACT_BYTES, PREVIEW_HEAD, PREVIEW_TAIL, THRESHOLD_BYTES,
        };
        let floor = PREVIEW_HEAD + PREVIEW_TAIL + 1024;
        assert_eq!(resolve_threshold(None, Some(204_800)), 204_800);
        assert_eq!(resolve_threshold(Some("junk"), Some(204_800)), 204_800);
        assert_eq!(resolve_threshold(None, Some(1)), floor);
        assert_eq!(
            resolve_threshold(None, Some(usize::MAX)),
            MAX_ARTIFACT_BYTES
        );
        assert_eq!(resolve_threshold(Some("100000"), Some(204_800)), 100_000);
        assert_eq!(resolve_threshold(None, None), THRESHOLD_BYTES);
    }

    /// A middleware given a threshold spills by it: a result between the default
    /// and the configured value reaches the model whole.
    #[tokio::test]
    async fn a_configured_threshold_decides_what_is_spilled() {
        use atomcode_kernel::tool::ToolResult;
        // Only when the environment is silent; the ranking itself is pinned above.
        if std::env::var_os(super::THRESHOLD_ENV).is_some() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(super::ArtifactStore::new(dir.path()));
        let big = "x".repeat(super::THRESHOLD_BYTES + 10_000);
        let mut result = ToolResult {
            call_id: "c".into(),
            content: big.clone(),
            is_error: false,
            images: vec![],
        };
        super::ArtifactMiddleware::new(store.clone())
            .with_threshold(Some(super::THRESHOLD_BYTES + 20_000))
            .spill(&mut result, false)
            .await;
        assert_eq!(
            result.content, big,
            "under the configured threshold: untouched"
        );

        super::ArtifactMiddleware::new(store)
            .spill(&mut result, false)
            .await;
        assert!(
            result
                .content
                .contains(super::ARTIFACT_TRUNCATION_MARKER_PREFIX),
            "and the default still spills it"
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
        // The marker names what is missing — the byte range that is NOT shown and
        // that it is not to be used — and how many reads cover it, from where.
        let (head_end, tail_begin) = (super::PREVIEW_HEAD, big.len() - super::PREVIEW_TAIL);
        assert!(
            r1.content
                .contains(&format!("NOT shown: bytes {head_end}–{tail_begin}")),
            "marker names the missing range: {}",
            r1.content
        );
        assert!(r1.content.contains("do not quote"), "{}", r1.content);
        assert!(
            r1.content.contains("1 part(s)") && r1.content.contains(&format!("offset={head_end}")),
            "marker says how many reads cover it, and where the first starts: {}",
            r1.content
        );
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

    /// A page is at most `FETCH_MAX_LIMIT` bytes and says where the next one
    /// starts. Spilled again, a default-sized page lost its middle to a fresh
    /// artifact, and no page size the marker asked for could be read whole.
    fn self_bounds_output(&self) -> bool {
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

        // `0` is "no preference": an empty page whose hint names the same offset
        // again is a page the model can only ask for forever.
        let limit = parsed
            .limit
            .filter(|&limit| limit > 0)
            .unwrap_or(FETCH_MAX_LIMIT)
            .min(FETCH_MAX_LIMIT);

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
                let (skip, keep) = on_char_boundaries(&bytes, parsed.offset.saturating_add(bytes.len()) < total);
                let start = parsed.offset.saturating_add(skip).min(total);
                let end = start.saturating_add(keep.len()).min(total);
                let body = String::from_utf8_lossy(keep);
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

/// A page's bytes, cut back to whole UTF-8 characters: the continuation bytes an
/// offset landed among are skipped (returned as the count), and — when more of the
/// capture follows (`more`) — a character cut off at the page's end is left for the
/// next page. Byte offsets are what the paging speaks, so a page edge fell inside a
/// character wherever one straddled it: `�` at the end of one page, `��` at the start
/// of the next, on nearly every edge of Chinese output.
///
/// Only ever at most 3 bytes either side, and never down to nothing: a capture that
/// is not UTF-8 (or a limit smaller than one character) is returned as it came.
fn on_char_boundaries(bytes: &[u8], more: bool) -> (usize, &[u8]) {
    let is_continuation = |b: &u8| b & 0b1100_0000 == 0b1000_0000;
    let skip = bytes
        .iter()
        .take(3)
        .take_while(|b| is_continuation(b))
        .count();
    let rest = &bytes[skip..];
    let rest = match std::str::from_utf8(rest) {
        Err(error)
            if more && error.error_len().is_none() && rest.len() - error.valid_up_to() <= 3 =>
        {
            &rest[..error.valid_up_to()]
        }
        _ => rest,
    };
    if rest.is_empty() {
        (0, bytes)
    } else {
        (skip, rest)
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

    /// A page `fetch_output` returns reaches the model whole.
    ///
    /// The page is already bounded (at most `FETCH_MAX_LIMIT` bytes plus a hint
    /// naming the next offset), but it came back through the same spill as any
    /// other result, and a default-sized page is over the spill threshold: it
    /// was cut to head + tail again, into a fresh artifact, and the middle of
    /// every page was out of reach at the size the marker told the model to ask
    /// for. On 5.1.0 (threshold 16 KiB, 4 KiB each side) a 64 KiB page kept 8.
    ///
    /// Negative control: drop `self_bounds_output` from `FetchOutputTool` and
    /// the page carries a truncation marker.
    #[tokio::test]
    async fn a_fetched_page_reaches_the_model_whole() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(ArtifactStore::new(dir.path()));
        let full: String = (0..200_000u32)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let id = store.put(full.as_bytes()).unwrap();
        let tool = FetchOutputTool::new(store.clone());

        let mut page = tool
            .execute(
                &format!(r#"{{"artifact_id":"{id}","offset":0}}"#),
                &ctx(dir.path()),
            )
            .await;
        assert!(!page.is_error, "{}", page.content);
        ArtifactMiddleware::new(store.clone())
            .spill(&mut page, tool.self_bounds_output())
            .await;

        assert!(
            !page.content.contains(ARTIFACT_TRUNCATION_MARKER_PREFIX),
            "the page was truncated again on its way to the model"
        );
        assert!(page.content.starts_with(&full[..FETCH_MAX_LIMIT]));
    }

    /// The page after the page after the page, read back to back, is the capture —
    /// not a capture with a `�` wherever a character straddled a page edge. Chinese
    /// output (3 bytes a character) put one on nearly every edge.
    #[tokio::test]
    async fn pages_split_between_characters_not_inside_them() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(ArtifactStore::new(dir.path()));
        let full = "收入同比增长百分之十二。".repeat(10_000); // 3 bytes a char, 360 000 bytes
        let id = store.put(full.as_bytes()).unwrap();
        let tool = FetchOutputTool::new(store);
        let test_ctx = ctx(dir.path());

        let mut read = String::new();
        let mut offset = 0usize;
        for _ in 0..20 {
            let r = tool
                .execute(
                    &format!(r#"{{"artifact_id":"{id}","offset":{offset}}}"#),
                    &test_ctx,
                )
                .await;
            assert!(!r.is_error, "{}", r.content);
            let (body, hint) = r.content.rsplit_once("\n\n[showing bytes ").unwrap();
            assert!(
                !body.contains('\u{FFFD}'),
                "a character was cut at {offset}"
            );
            read.push_str(body);
            match hint.split_once("offset=") {
                Some((_, next)) => offset = next.trim_end_matches(") for more]").parse().unwrap(),
                None => break,
            }
        }
        assert_eq!(read, full);

        // An offset that lands inside a character starts at the next one.
        let r = tool
            .execute(
                &format!(r#"{{"artifact_id":"{id}","offset":1,"limit":9}}"#),
                &test_ctx,
            )
            .await;
        assert!(
            r.content
                .starts_with("入同\n\n[showing bytes 3–9 of 360000;"),
            "{}",
            r.content
        );
    }

    /// `limit: 0` is "no preference", not an empty page pointing at the same offset.
    #[tokio::test]
    async fn a_zero_limit_reads_a_default_page() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(ArtifactStore::new(dir.path()));
        let id = store.put(&b"B".repeat(100_000)).unwrap();
        let r = FetchOutputTool::new(store)
            .execute(
                &format!(r#"{{"artifact_id":"{id}","offset":0,"limit":0}}"#),
                &ctx(dir.path()),
            )
            .await;
        assert!(
            r.content.contains("[showing bytes 0–65536 of 100000;"),
            "{}",
            &r.content[65530..]
        );
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
            r.content.starts_with(&format!(
                "{}\n\n[showing bytes 0–65536 of 100000;",
                "A".repeat(65536)
            )),
            "the page is exactly the 64 KiB cap, then the hint: {}",
            &r.content[65530..]
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
