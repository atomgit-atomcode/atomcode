//! `read_file` — read a file (or list a directory) with line numbers and optional
//! slicing. Non-destructive ⇒ always `Safe`. Neutral core ported from the production
//! reader, minus the coding enrichments (semantic skeleton, read_cache, file_store).

use super::{err, looks_binary, not_found_hint, ok, ok_with_images, resolve_path};
use async_trait::async_trait;
use atomcode_kernel::message::ImageContent;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use base64::Engine;
use serde::Deserialize;
use serde_json::json;

/// Hard safety ceiling for the current in-memory decoder. Default pagination
/// controls model-visible output, but decoding still needs the complete file for
/// UTF-8/GB18030 detection and codeintel. Refuse pathological inputs uniformly,
/// including callers that supplied an offset/limit.
const MAX_IN_MEMORY_BYTES: u64 = 64 * 1024 * 1024;
/// Default page size when the caller omits `limit`. Sized SO THE 50 KiB byte budget
/// (`MAX_READ_OUTPUT_BYTES`) — not the line count — is what usually bounds a page: a
/// default read returns a substantial window in ONE call instead of truncating early at a
/// small line cap and forcing pagination round-trips (each re-sends the whole context).
/// Large CODE files read bare still return a symbol skeleton first (`SKELETON_THRESHOLD`),
/// so this mainly governs offset reads and large non-code files (logs / JSON). Aligned to
/// the ~1.5–3k peer baseline (opencode 2000 / oh-my-pi 3000).
const DEFAULT_READ_LIMIT: usize = 1500;
/// Per-call output budget for a read. `read_file` is EXEMPT from the artifact
/// head/tail truncation middleware (`ArtifactMiddleware`), so THIS budget — not the
/// 16 KiB artifact threshold — is what bounds a read, and it may exceed that threshold
/// while keeping full line-based pagination intact. Sized to the ~50 KiB peer baseline
/// (opencode/oh-my-pi) so a normal window — a symbol, or the default page —
/// comes back in ONE call instead of forcing pagination round-trips, each of which
/// re-sends the whole growing context for nothing. Pathologically long single lines are
/// still capped by `MAX_LINE_LEN`.
const MAX_READ_OUTPUT_BYTES: usize = 50 * 1024;
/// Per-line display cap (very long minified lines are truncated with a marker).
const MAX_LINE_LEN: usize = 2000;
/// Above this line count, an un-sliced read of a CODE file returns a symbol skeleton
/// (when the `codeintel` capability is enabled) instead of the full dump.
#[cfg(feature = "codeintel")]
const SKELETON_THRESHOLD: usize = 300;

/// `vision` = the active model can SEE images. When true, reading an image file
/// returns the picture itself (base64) for the model instead of the "binary,
/// cannot display" text dead-end. The capability is decided at the coding layer
/// and passed in as a plain flag — this crate stays model-agnostic (and core-free).
/// Default `false` (text-only).
#[derive(Default)]
pub struct ReadFileTool {
    vision: bool,
}

impl ReadFileTool {
    pub fn new(vision: bool) -> Self {
        Self { vision }
    }
}

fn continuation_footer(
    file_path: &str,
    start: usize,
    end: usize,
    total: usize,
    page_limit: usize,
) -> String {
    let continuation = json!({
        "file_path": file_path,
        "limit": page_limit,
        "offset": end + 1,
    });
    let detailed = format!(
        "[Showing lines {start}-{end} of {total}. Continue with read_file({continuation})]"
    );
    // Reserve enough room for the one line we always return, even when it is a
    // maximum-length four-byte UTF-8 line. This keeps the total budget real,
    // rather than only bounding the body in ordinary short-path cases.
    let detailed_footer_budget = MAX_READ_OUTPUT_BYTES
        .saturating_sub(MAX_LINE_LEN.saturating_mul(4))
        .saturating_sub(128);
    if detailed.len() <= detailed_footer_budget {
        detailed
    } else {
        format!(
            "[Showing lines {start}-{end} of {total}. Continue reading the same file with offset={} and limit={page_limit}.]",
            end + 1
        )
    }
}

/// Render one 1-based numbered line for the model (`<n>\t<content>\n`), truncating a
/// pathologically long line to `MAX_LINE_LEN` chars. Shared by the single-range and
/// multi-range paths so both prefix line numbers and truncate identically.
fn render_line(n: usize, line: &str) -> String {
    if line.chars().count() > MAX_LINE_LEN {
        let head: String = line.chars().take(MAX_LINE_LEN).collect();
        format!("{n}\t{head}... (line truncated to {MAX_LINE_LEN} chars)\n")
    } else {
        format!("{n}\t{line}\n")
    }
}

/// Read several disjoint line windows in ONE call — collapses what would otherwise be
/// N sequential paginated reads. Ranges render in the order given (no merge/dedup: the
/// caller asked for exactly these), each under a `[Lines a-b]` header, ALL sharing the
/// single `MAX_READ_OUTPUT_BYTES` budget so the combined result stays as bounded as a
/// normal page (keeps the per-turn output small — important for prompt-cache/cold-start).
/// When the budget runs out, the still-unshown ranges are reported so the model can
/// re-request them.
fn render_multi_range(file_path: &str, text: &str, total: usize, ranges: &[RangeArg]) -> String {
    // Keep content under a cap that reserves room for the trailing "budget reached" line,
    // so the whole result (content + summary) stays within MAX_READ_OUTPUT_BYTES.
    const SUMMARY_RESERVE: usize = 160;
    let content_cap = MAX_READ_OUTPUT_BYTES.saturating_sub(SUMMARY_RESERVE);

    let lines: Vec<&str> = text.lines().collect();
    let mut out = format!("[Multi-range read of {file_path} ({total} lines total).]\n");
    let mut shown = 0usize;
    let mut budget_hit = false;
    for r in ranges {
        let start = r.offset.unwrap_or(1).max(1);
        let start_idx = start - 1;
        if start_idx >= total {
            out.push_str(&format!(
                "\n[Lines {start}-… : beyond end of file ({total} lines total)]\n"
            ));
            shown += 1;
            continue;
        }
        let limit = r.limit.unwrap_or(DEFAULT_READ_LIMIT).max(1);
        let end_idx = start_idx.saturating_add(limit).min(total);
        let header = format!("\n[Lines {start}-{end_idx}]\n");

        // Build the window body first; commit header + body together only if at least one
        // line fits, so the budget never leaves a bare header with no content behind (and a
        // content-less window is never counted as "shown").
        let mut body = String::new();
        for (i, line) in lines.iter().enumerate().take(end_idx).skip(start_idx) {
            let rendered = render_line(i + 1, line);
            if out.len() + header.len() + body.len() + rendered.len() > content_cap {
                budget_hit = true;
                break;
            }
            body.push_str(&rendered);
        }
        if body.is_empty() {
            // Not even the first line fit → report overflow instead of an empty section.
            budget_hit = true;
            break;
        }
        out.push_str(&header);
        out.push_str(&body);
        shown += 1;
        if budget_hit {
            break;
        }
    }
    if budget_hit && shown < ranges.len() {
        out.push_str(&format!(
            "\n[Output budget reached; {shown} of {} ranges shown. Re-request the remaining ranges.]\n",
            ranges.len()
        ));
    }
    out
}

/// Cap on an image read back to a vision model: base64 inflates ~33% and every image
/// costs ~1600 tokens, so refuse an oversized one (it would blow the result-size cap /
/// context) and fall back to the binary-text hint. Generous enough for book covers,
/// screenshots, diagrams (the real use cases).
const MAX_IMAGE_BYTES: u64 = 4 * 1024 * 1024;

/// MIME type for an image file by extension, or `None` if not a recognized raster image.
/// Gates which binaries `read_file` hands to a vision model — only true images, never a
/// PDF / archive / executable (those keep the text recovery hint). The set matches what
/// the providers actually accept AND the user-paste path (png/jpg/jpeg/gif/webp); BMP is
/// deliberately EXCLUDED — neither the OpenAI nor Anthropic vision wire format accepts it,
/// so handing one over would be a hard gateway rejection, strictly worse than the
/// binary-text dead-end it would replace.
fn image_media_type(path: &std::path::Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

/// One window in a multi-range read. Same lenient numeric handling as the top-level
/// `offset`/`limit` (weak models emit `"10"` / `10.0`).
#[derive(Deserialize)]
struct RangeArg {
    #[serde(default, deserialize_with = "lenient_usize")]
    offset: Option<usize>,
    #[serde(default, deserialize_with = "lenient_usize")]
    limit: Option<usize>,
}

#[derive(Deserialize)]
struct Args {
    file_path: String,
    #[serde(default, deserialize_with = "lenient_usize")]
    offset: Option<usize>,
    #[serde(default, deserialize_with = "lenient_usize")]
    limit: Option<usize>,
    /// Optional multiple windows in one call; when present and non-empty it takes
    /// precedence over `offset`/`limit` and over the large-file skeleton.
    #[serde(default)]
    ranges: Option<Vec<RangeArg>>,
}

/// Deserialize a usize that weak models may send as a float or a string (`50`, `"50"`,
/// `50.0`, `"50.0"`) instead of an integer. Absent / null / empty → `None`.
/// Shared with `grep` (max_results/context) — keep this the single source.
pub(crate) fn lenient_usize<'de, D>(d: D) -> Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Num {
        U(u64),
        F(f64),
        S(String),
    }
    // Be lenient about the representation, not the value: weak models commonly
    // emit `50.0`/`"50.0"` for an integer field, but a true fraction has no
    // unambiguous offset/limit meaning. Reject invalid and out-of-range values
    // instead of letting Rust's float cast silently truncate or saturate them.
    fn checked(f: f64) -> Result<usize, &'static str> {
        // Near the IEEE-754 safe-integer edge, the serde_json + untagged path
        // can already map adjacent decimal spellings to the same f64. Keep a
        // conservative guard below that edge; callers can still send an exact
        // JSON integer or integer string, both handled without f64.
        const FLOAT_INTEGER_EXACT_LIMIT: f64 = 4_503_599_627_370_496.0; // 2^52
        if !f.is_finite() || f < 0.0 {
            return Err("negative or non-finite value not allowed");
        }
        if f.fract() != 0.0 {
            return Err("fractional value not allowed");
        }
        if f >= FLOAT_INTEGER_EXACT_LIMIT {
            return Err("floating-point integer exceeds exact range");
        }
        let n = f as u128;
        usize::try_from(n).map_err(|_| "value exceeds usize range")
    }
    Ok(match Option::<Num>::deserialize(d)? {
        None => None,
        Some(Num::U(n)) => Some(
            usize::try_from(n)
                .map_err(|_| serde::de::Error::custom("value exceeds usize range"))?,
        ),
        Some(Num::F(f)) => Some(checked(f).map_err(serde::de::Error::custom)?),
        Some(Num::S(s)) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else if let Ok(n) = t.parse::<usize>() {
                Some(n)
            } else {
                let f = t.parse::<f64>().map_err(serde::de::Error::custom)?;
                Some(checked(f).map_err(serde::de::Error::custom)?)
            }
        }
    })
}

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read a file — or any slice of one — from the filesystem. This is the way to read \
         or slice files: prefer it over `bash cat`/`head`/`tail` or a `python`/`awk` script, \
         which return partial content and bypass paging, caching, and history retention. \
         Returns the contents prefixed with 1-based line numbers (`<n>\\t<content>`). By \
         default returns up to 1500 lines; a ~50 KiB output budget may return fewer. When a result \
         shows a continuation offset, continue from that offset instead of rereading line 1. \
         Use `offset` (1-based start line) and `limit` (max lines) when a larger relevant \
         window is needed; avoid many tiny overlapping reads. To grab several disjoint \
         windows at once (e.g. multiple symbols listed in a skeleton), pass `ranges` instead \
         of paginating or splitting the file yourself. If the path is a directory its entries \
         are listed instead. Relative paths resolve against the working directory."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "Path to read (absolute, or relative to the working directory)" },
                "offset": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Start line, 1-based. Omit to start at line 1; after a partial result, use the next offset shown."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "default": DEFAULT_READ_LIMIT,
                    "description": "Maximum lines to read. Defaults to 1500; the ~50 KiB output byte budget may return fewer."
                },
                "ranges": {
                    "type": "array",
                    "description": "Read several disjoint line windows in ONE call instead of paginating. Each item is {offset, limit}. Takes precedence over top-level offset/limit and over the large-file skeleton. Prefer this when you already know the ranges (e.g. from a skeleton's per-symbol offset/limit). All windows share one output budget.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "offset": { "type": "integer", "minimum": 1, "description": "Start line, 1-based" },
                            "limit": { "type": "integer", "minimum": 1, "description": "Max lines for this window" }
                        },
                        "required": ["offset"]
                    }
                }
            },
            "required": ["file_path"]
        })
    }
    /// No side effects — a pure read. Makes it `parallel_safe` (concurrent
    /// execution) and allowed in plan mode.
    fn read_only_hint(&self) -> bool {
        true
    }
    /// Self-capped at `MAX_READ_OUTPUT_BYTES` with 1-based line numbers and offset/limit
    /// pagination — the artifact head/tail middleware must pass it through WHOLE, or the
    /// line numbering breaks. This is why the read budget may exceed the artifact threshold.
    fn self_bounds_output(&self) -> bool {
        true
    }
    // read is non-destructive → risk() defaults to Safe.
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => {
                return err(format!(
                    "read_file: invalid arguments: {e}. Expected {{\"file_path\": \"<path>\"}}."
                ))
            }
        };
        // Only enforce the top-level `limit` when it actually governs the read — a
        // multi-range request ignores top-level offset/limit (per-window limits are
        // coerced to ≥1 in render_multi_range), so it must not be rejected here.
        let has_ranges = a.ranges.as_deref().is_some_and(|r| !r.is_empty());
        if !has_ranges && a.limit == Some(0) {
            return err("read_file: `limit` must be at least 1.");
        }
        let path = resolve_path(&a.file_path, &ctx.working_dir);

        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(_) => {
                return err(format!(
                    "Error: no such file: {} (resolved to {}){}",
                    a.file_path,
                    crate::pathnorm::to_display(&path),
                    not_found_hint(&path, &ctx.working_dir).await
                ))
            }
        };

        if meta.is_dir() {
            let mut entries = Vec::new();
            if let Ok(mut rd) = tokio::fs::read_dir(&path).await {
                while let Ok(Some(e)) = rd.next_entry().await {
                    let is_dir = e.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                    let name = e.file_name().to_string_lossy().to_string();
                    entries.push(if is_dir { format!("{name}/") } else { name });
                }
            }
            entries.sort();
            return ok(format!(
                "[NOTE: {} is a directory. Its contents:]\n{}",
                crate::pathnorm::to_display(&path),
                entries.join("\n")
            ));
        }

        if meta.len() > MAX_IN_MEMORY_BYTES {
            return err(format!(
                "File too large for read_file's in-memory decoder: {} bytes ({:.1} MB; \
                 limit is {:.0} MB). Use grep/list_symbols to locate the relevant content \
                 first; ONLY for a file this oversized is `bash sed -n`/`rg` an acceptable \
                 way to read a bounded range (for normal-sized files always use read_file \
                 with offset/limit).",
                meta.len(),
                meta.len() as f64 / 1_048_576.0,
                MAX_IN_MEMORY_BYTES as f64 / 1_048_576.0,
            ));
        }

        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) => {
                return err(format!(
                    "read_file: failed to read {}: {e}",
                    crate::pathnorm::to_display(&path)
                ))
            }
        };
        if looks_binary(&bytes) {
            // VISION path: an image file read by a model that can SEE → hand back the
            // picture itself (base64) so it reaches the model on a follow-up user
            // message, instead of the "cannot display" text dead-end. Gated on
            // `self.vision` (model capability) AND a recognized image type AND a sane
            // size; anything else keeps the existing binary-text + recovery hint.
            if self.vision && meta.len() <= MAX_IMAGE_BYTES {
                if let Some(media_type) = image_media_type(&path) {
                    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    return ok_with_images(
                        format!(
                            "[Image: {} ({} bytes) — attached below for the vision model]",
                            a.file_path,
                            bytes.len()
                        ),
                        vec![ImageContent {
                            media_type: media_type.to_string(),
                            data,
                        }],
                    );
                }
            }
            return ok(format!(
                "Binary file ({} bytes), cannot display as text.{}",
                bytes.len(),
                binary_recovery_hint(&path, &a.file_path),
            ));
        }

        // Decode: prefer UTF-8; fall back to GB18030 (GBK/GB2312 superset) for text-ish
        // extensions. Chinese Windows editors write .txt/.md/.csv as GBK, which a lossy
        // UTF-8 decode would mangle into replacement chars (mojibake). If neither decodes,
        // treat it as binary and hand back a recovery hint.
        let text: std::borrow::Cow<str> = match std::str::from_utf8(&bytes) {
            Ok(s) => std::borrow::Cow::Borrowed(s),
            Err(_) => match crate::tools::encoding::decode_non_utf8_text(&path, &bytes) {
                Some(s) => std::borrow::Cow::Owned(s),
                None => {
                    return ok(format!(
                        "Binary file ({} bytes), cannot display as text.{}",
                        bytes.len(),
                        binary_recovery_hint(&path, &a.file_path),
                    ))
                }
            },
        };
        // Count and page with iterators instead of collecting `Vec<&str>`: a file
        // containing millions of tiny lines would otherwise spend far more memory
        // on line pointers than on the bounded source bytes themselves.
        let total = text.lines().count();

        // Multi-range: several disjoint windows in one call (collapses N paginated reads).
        // Checked BEFORE the skeleton branch so an explicit ranges request always reads
        // content, and before the single-range slice so it takes precedence.
        if let Some(ranges) = a.ranges.as_deref().filter(|r| !r.is_empty()) {
            return ok(render_multi_range(&a.file_path, &text, total, ranges));
        }

        // codeintel enrichment: outline a large CODE file as a symbol skeleton instead of
        // dumping it (cross-capability composition; only when codeintel is compiled in).
        // A given offset/limit means the model wants a specific range, so skip it.
        #[cfg(feature = "codeintel")]
        if a.offset.is_none() && a.limit.is_none() && total > SKELETON_THRESHOLD {
            if let Some(skel) = crate::codeintel::skeleton(&path, text.as_ref(), &a.file_path) {
                return ok(skel);
            }
        }

        let start = a.offset.unwrap_or(1).max(1); // 1-based
        let start_idx = start - 1;
        if start_idx >= total {
            return ok(format!(
                "[no lines in requested range (start={start}, total={total})]"
            ));
        }
        let page_limit = a.limit.unwrap_or(DEFAULT_READ_LIMIT);
        let requested_end_idx = start_idx.saturating_add(page_limit).min(total);

        let mut out = String::new();
        let mut end_idx = start_idx;
        for (i, line) in text
            .lines()
            .skip(start_idx)
            .take(requested_end_idx - start_idx)
            .enumerate()
        {
            let n = start + i;
            let rendered = render_line(n, line);
            let candidate_end = start_idx + i + 1;
            let footer_len = if candidate_end < total {
                continuation_footer(&a.file_path, start, candidate_end, total, page_limit).len()
            } else if start > 1 {
                format!("[Showing lines {start}-{candidate_end} of {total} (end)]").len()
            } else {
                0
            };
            if !out.is_empty()
                && out
                    .len()
                    .saturating_add(rendered.len())
                    .saturating_add(footer_len)
                    > MAX_READ_OUTPUT_BYTES
            {
                break;
            }
            out.push_str(&rendered);
            end_idx = candidate_end;
        }
        if end_idx < total {
            out.push_str(&continuation_footer(
                &a.file_path,
                start,
                end_idx,
                total,
                page_limit,
            ));
        } else if start > 1 {
            out.push_str(&format!(
                "[Showing lines {start}-{end_idx} of {total} (end)]"
            ));
        }
        ok(out)
    }
}

/// Build a recovery hint for a file that couldn't be decoded as text. Lets the model
/// pivot to an external converter (pandoc / pdftotext / unzip for .docx) on the first
/// failure instead of cycling through offset/limit values for 30 turns.
fn binary_recovery_hint(path: &std::path::Path, full_path_str: &str) -> String {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let q = shell_quote(full_path_str);
    match ext.as_str() {
        "doc" => format!(
            "\n\n[Recovery] This is a legacy Word (.doc) binary. Run one of:\n\
             - bash: `antiword {q}`\n\
             - bash: `pandoc {q} -t plain`\n\
             - bash: `catdoc {q}`"
        ),
        "docx" => format!(
            "\n\n[Recovery] This is a modern Word (.docx) — a zip containing XML. Run:\n\
             - bash: `unzip -p {q} word/document.xml | sed 's/<[^>]*>//g'`\n\
             - or: `pandoc {q} -t plain`"
        ),
        "xls" => format!(
            "\n\n[Recovery] Legacy Excel (.xls). Run:\n\
             - bash: `libreoffice --headless --convert-to csv --outdir /tmp {q} && cat /tmp/*.csv`"
        ),
        "xlsx" => format!(
            "\n\n[Recovery] Modern Excel (.xlsx). Run:\n\
             - bash: `libreoffice --headless --convert-to csv --outdir /tmp {q} && cat /tmp/*.csv`\n\
             - or: `unzip -p {q} xl/sharedStrings.xml` (raw string table)"
        ),
        "ppt" | "pptx" => format!(
            "\n\n[Recovery] PowerPoint. Run:\n\
             - bash: `pandoc {q} -t plain`"
        ),
        "pdf" => format!(
            "\n\n[Recovery] PDF. Run:\n\
             - bash: `pdftotext {q} -` (poppler)\n\
             - or: `mutool draw -F txt {q}`"
        ),
        "rtf" => format!(
            "\n\n[Recovery] RTF. Run:\n\
             - bash: `pandoc {q} -t plain`\n\
             - or: `unrtf --text {q}`"
        ),
        _ => "\n\n[Hint] The file is not UTF-8 and not a recognised text extension. \
             If it's text in another encoding, ask the user; if it's a packaged format \
             (archive, installer, media), there is no point reading it as text."
            .to_string(),
    }
}

/// Minimal shell-quoter for embedding a path in a bash command suggestion.
/// POSIX single-quoted form: wraps in `'`, escapes any existing `'` as `'\''`.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str(r"'\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::tool::ToolContext;
    use tokio_util::sync::CancellationToken;

    fn ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext {
            working_dir: dir.to_path_buf(),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
            requester: None,
        }
    }

    #[test]
    fn lenient_usize_rejects_out_of_domain_values() {
        // Leniency covers integer representations only. It must not guess a
        // fractional value, accept a non-finite value, or saturate overflow.
        for bad in [
            r#"{"file_path":"x","offset":-5.0}"#,  // negative float
            r#"{"file_path":"x","offset":-5}"#,    // bare negative int (untagged → f64)
            r#"{"file_path":"x","limit":"-5"}"#,   // negative as string
            r#"{"file_path":"x","offset":"NaN"}"#, // NaN as string
            r#"{"file_path":"x","offset":"Infinity"}"#,
            r#"{"file_path":"x","offset":3.9}"#,
            r#"{"file_path":"x","limit":"3.9"}"#,
            r#"{"file_path":"x","offset":"340282366920938463463374607431768211455"}"#,
        ] {
            assert!(
                serde_json::from_str::<Args>(bad).is_err(),
                "should reject out-of-domain numeric: {bad}"
            );
        }
        let args: Args =
            serde_json::from_str(r#"{"file_path":"x","offset":2.0,"limit":"3.0"}"#).unwrap();
        assert_eq!(args.offset, Some(2));
        assert_eq!(args.limit, Some(3));
    }

    #[test]
    fn lenient_usize_checks_platform_range_without_saturation() {
        let max = usize::MAX.to_string();
        let args: Args =
            serde_json::from_str(&format!(r#"{{"file_path":"x","offset":"{max}"}}"#)).unwrap();
        assert_eq!(args.offset, Some(usize::MAX));

        let overflow = (usize::MAX as u128 + 1).to_string();
        for input in [
            format!(r#"{{"file_path":"x","offset":{overflow}}}"#),
            format!(r#"{{"file_path":"x","offset":"{overflow}"}}"#),
        ] {
            assert!(
                serde_json::from_str::<Args>(&input).is_err(),
                "must reject a value above this platform's usize range: {input}"
            );
        }
    }

    #[test]
    fn lenient_usize_rejects_ambiguous_f64_integers() {
        // The first value is exact, but must be rejected conservatively because
        // the second distinct decimal input decodes to the same f64 value.
        for input in [
            r#"{"file_path":"x","offset":4503599627370496.0}"#,
            r#"{"file_path":"x","offset":9007199254740992.0}"#,
            r#"{"file_path":"x","offset":9007199254740993.0}"#,
            r#"{"file_path":"x","offset":"9007199254740993.0"}"#,
        ] {
            assert!(
                serde_json::from_str::<Args>(input).is_err(),
                "ambiguous f64 integer must be rejected: {input}"
            );
        }

        let args: Args =
            serde_json::from_str(r#"{"file_path":"x","offset":4503599627370495.0}"#).unwrap();
        assert_eq!(args.offset, Some(4_503_599_627_370_495));
    }

    fn ten_lines() -> String {
        (1..=10)
            .map(|n| format!("l{n}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    }

    #[test]
    fn multi_range_renders_disjoint_windows_in_order() {
        let text = ten_lines();
        let total = text.lines().count();
        let ranges = vec![
            RangeArg { offset: Some(2), limit: Some(2) },
            RangeArg { offset: Some(7), limit: Some(1) },
        ];
        let out = render_multi_range("a.txt", &text, total, &ranges);
        assert!(out.contains("[Lines 2-3]"), "{out}");
        assert!(out.contains("2\tl2") && out.contains("3\tl3"), "{out}");
        assert!(out.contains("[Lines 7-7]") && out.contains("7\tl7"), "{out}");
        // only the requested windows — unrequested lines are absent
        assert!(!out.contains("1\tl1") && !out.contains("5\tl5"), "{out}");
        // rendered in request order
        assert!(out.find("[Lines 2-3]") < out.find("[Lines 7-7]"), "{out}");
    }

    #[test]
    fn multi_range_reports_out_of_bounds_window_gracefully() {
        let text = ten_lines();
        let out = render_multi_range(
            "a.txt",
            &text,
            10,
            &[RangeArg { offset: Some(99), limit: Some(3) }],
        );
        assert!(out.contains("beyond end of file"), "{out}");
    }

    #[test]
    fn multi_range_shares_one_output_budget() {
        // Many wide ranges must not exceed the single-page byte cap; overflow is reported.
        let text = (1..=5000)
            .map(|n| format!("{n}: {}", "x".repeat(200)))
            .collect::<Vec<_>>()
            .join("\n");
        let total = text.lines().count();
        let ranges: Vec<RangeArg> = (1..=50)
            .map(|k| RangeArg { offset: Some(k * 50), limit: Some(20) })
            .collect();
        let out = render_multi_range("big.txt", &text, total, &ranges);
        assert!(out.len() <= MAX_READ_OUTPUT_BYTES, "exceeded budget: {}", out.len());
        assert!(out.contains("Output budget reached"), "{out}");
    }

    #[tokio::test]
    async fn ranges_take_precedence_over_a_zero_top_level_limit() {
        // A multi-range read ignores top-level offset/limit, so `limit:0` must NOT be
        // rejected when `ranges` is present (precedence honored).
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let r = ReadFileTool::default()
            .execute(
                r#"{"file_path":"a.txt","limit":0,"ranges":[{"offset":2,"limit":1}]}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("2\ttwo"), "{}", r.content);
        // top-level limit:0 alone still errors (no ranges)
        let r0 = ReadFileTool::default()
            .execute(r#"{"file_path":"a.txt","limit":0}"#, &ctx(d.path()))
            .await;
        assert!(r0.is_error && r0.content.contains("at least 1"), "{}", r0.content);
    }

    #[tokio::test]
    async fn ranges_param_reads_multiple_windows_and_bypasses_skeleton() {
        let d = tempfile::tempdir().unwrap();
        // >300 code symbols → a plain read would return a skeleton; ranges must bypass it.
        let body = (1..=400)
            .map(|n| format!("fn f{n}() {{}}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(d.path().join("a.rs"), &body).unwrap();
        let r = ReadFileTool::default()
            .execute(
                r#"{"file_path":"a.rs","ranges":[{"offset":10,"limit":2},{"offset":300,"limit":1}]}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("10\tfn f10()"), "{}", r.content);
        assert!(r.content.contains("300\tfn f300()"), "{}", r.content);
        assert!(!r.content.contains("File skeleton"), "ranges must bypass skeleton: {}", r.content);
        // an unrequested line must not appear
        assert!(!r.content.contains("200\tfn f200()"), "{}", r.content);
    }

    #[tokio::test]
    async fn reads_with_line_numbers() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "first\nsecond\nthird\n").unwrap();
        let r = ReadFileTool::default()
            .execute(r#"{"file_path":"a.txt"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error);
        assert!(r.content.contains("1\tfirst"), "{}", r.content);
        assert!(r.content.contains("3\tthird"), "{}", r.content);
    }

    #[tokio::test]
    async fn image_file_returns_base64_for_vision_model() {
        // A vision-capable model must SEE the image: read_file base64-encodes the
        // bytes into the result's `images` instead of the "Binary file" text dead-end.
        let d = tempfile::tempdir().unwrap();
        // Minimal JPEG-ish blob (SOI marker + a NUL so `looks_binary` flags it).
        let bytes: &[u8] = &[
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00,
        ];
        std::fs::write(d.path().join("cover.jpg"), bytes).unwrap();
        let r = ReadFileTool::new(true)
            .execute(r#"{"file_path":"cover.jpg"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(r.images.len(), 1, "vision model must receive the image");
        assert_eq!(r.images[0].media_type, "image/jpeg");
        assert_eq!(
            r.images[0].data,
            base64::engine::general_purpose::STANDARD.encode(bytes),
            "image bytes must be base64-encoded losslessly"
        );
        assert!(!r.content.starts_with("Binary file"), "{}", r.content);
        assert!(r.content.contains("cover.jpg"), "{}", r.content);
    }

    #[tokio::test]
    async fn image_file_stays_binary_text_for_text_only_model() {
        // A text-only model would reject a base64 image / waste tokens → keep the
        // existing "Binary file" text and attach NO image.
        let d = tempfile::tempdir().unwrap();
        let bytes: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        std::fs::write(d.path().join("cover.jpg"), bytes).unwrap();
        let r = ReadFileTool::new(false)
            .execute(r#"{"file_path":"cover.jpg"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error);
        assert!(
            r.images.is_empty(),
            "text-only model must NOT receive an image"
        );
        assert!(r.content.starts_with("Binary file"), "{}", r.content);
    }

    #[tokio::test]
    async fn non_image_binary_stays_text_even_for_vision_model() {
        // A vision model reading a NON-image binary (e.g. a PDF) still gets the text
        // dead-end + recovery hint — only true images become `images`.
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("report.pdf"), b"%PDF-1.4\0\0\0binary blob").unwrap();
        let r = ReadFileTool::new(true)
            .execute(r#"{"file_path":"report.pdf"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(
            r.images.is_empty(),
            "non-image binary must not be sent as an image"
        );
        assert!(r.content.starts_with("Binary file"), "{}", r.content);
    }

    #[tokio::test]
    async fn offset_and_limit_slice() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "l1\nl2\nl3\nl4\nl5\n").unwrap();
        let r = ReadFileTool::default()
            .execute(
                r#"{"file_path":"a.txt","offset":2,"limit":2}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(r.content.contains("2\tl2"), "{}", r.content);
        assert!(r.content.contains("3\tl3"), "{}", r.content);
        assert!(!r.content.contains("\tl1"), "{}", r.content);
        assert!(!r.content.contains("\tl4"), "{}", r.content);
        assert!(
            r.content.contains(
                r#"[Showing lines 2-3 of 5. Continue with read_file({"file_path":"a.txt","limit":2,"offset":4})]"#
            ),
            "{}",
            r.content
        );
    }

    #[tokio::test]
    async fn omitted_limit_uses_a_bounded_page_with_an_actionable_continuation() {
        let d = tempfile::tempdir().unwrap();
        // Just over the 1500-line default page (short lines, so the LINE cap binds before
        // the 50 KiB byte budget) → one bounded page + an actionable continuation.
        let text = (1..=1600)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(d.path().join("notes.txt"), text).unwrap();

        let r = ReadFileTool::default()
            .execute(r#"{"file_path":"notes.txt"}"#, &ctx(d.path()))
            .await;

        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("1500\tline 1500"), "{}", r.content);
        assert!(!r.content.contains("1501\tline 1501"), "{}", r.content);
        assert!(
            r.content.contains(
                r#"Continue with read_file({"file_path":"notes.txt","limit":1500,"offset":1501})"#
            ),
            "{}",
            r.content
        );
    }

    #[tokio::test]
    async fn read_page_is_bounded_by_its_own_budget_not_the_artifact_threshold() {
        let d = tempfile::tempdir().unwrap();
        // ~200 wide lines (~100 KiB) so the page overflows the 50 KiB read budget and must
        // paginate — proving the bound is the read budget, not the artifact threshold.
        let text = (1..=200)
            .map(|n| format!("line {n} {}", "x".repeat(500)))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(d.path().join("wide.txt"), text).unwrap();

        let r = ReadFileTool::default()
            .execute(r#"{"file_path":"wide.txt"}"#, &ctx(d.path()))
            .await;

        assert!(!r.is_error, "{}", r.content);
        // read_file is EXEMPT from the artifact head/tail middleware, so a page is bounded
        // by its OWN budget and may exceed the generic 16 KiB artifact threshold while
        // keeping full line-based pagination.
        assert!(
            r.content.len() <= MAX_READ_OUTPUT_BYTES,
            "page must stay within the read budget: {} bytes",
            r.content.len()
        );
        assert!(
            r.content.len() > crate::tools::output_artifact::THRESHOLD_BYTES,
            "a wide page now legitimately exceeds the artifact threshold: {} bytes",
            r.content.len()
        );
        assert!(
            r.content.contains("Continue with read_file("),
            "{}",
            r.content
        );
    }

    #[tokio::test]
    async fn files_above_the_legacy_five_mib_cutoff_use_default_pagination() {
        let d = tempfile::tempdir().unwrap();
        let line = format!("{}\n", "x".repeat(1023));
        std::fs::write(d.path().join("large.txt"), line.repeat(5 * 1024 + 1)).unwrap();

        let r = ReadFileTool::default()
            .execute(r#"{"file_path":"large.txt"}"#, &ctx(d.path()))
            .await;

        assert!(!r.is_error, "{}", r.content);
        assert!(
            r.content.contains("Continue with read_file("),
            "{}",
            r.content
        );
        assert!(
            r.content.len() <= MAX_READ_OUTPUT_BYTES,
            "{} bytes",
            r.content.len()
        );
    }

    #[test]
    fn oversized_continuation_path_uses_a_compact_footer() {
        // Path long enough that the detailed footer exceeds the read budget (now 50 KiB),
        // forcing the compact, path-free continuation line.
        let footer = continuation_footer(&"x".repeat(60_000), 1, 10, 100, 300);
        assert!(!footer.contains("file_path"), "{footer}");
        assert!(footer.contains("offset=11"), "{footer}");
        assert!(footer.len() < MAX_READ_OUTPUT_BYTES, "{}", footer.len());
    }

    #[tokio::test]
    async fn hard_in_memory_ceiling_applies_even_to_explicit_slices() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("huge.txt");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_IN_MEMORY_BYTES + 1).unwrap();

        let r = ReadFileTool::default()
            .execute(
                r#"{"file_path":"huge.txt","offset":1,"limit":1}"#,
                &ctx(d.path()),
            )
            .await;

        assert!(r.is_error);
        assert!(r.content.contains("in-memory decoder"), "{}", r.content);
    }

    #[tokio::test]
    async fn explicit_limit_can_read_beyond_the_default_line_page() {
        let d = tempfile::tempdir().unwrap();
        let text = (1..=350)
            .map(|n| format!("l{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(d.path().join("short-lines.txt"), text).unwrap();

        let r = ReadFileTool::default()
            .execute(
                r#"{"file_path":"short-lines.txt","offset":1,"limit":350}"#,
                &ctx(d.path()),
            )
            .await;

        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("350\tl350"), "{}", r.content);
        assert!(
            !r.content.contains("Continue with read_file("),
            "{}",
            r.content
        );
    }

    #[tokio::test]
    async fn zero_limit_is_rejected() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("notes.txt"), "hello").unwrap();

        let r = ReadFileTool::default()
            .execute(r#"{"file_path":"notes.txt","limit":0}"#, &ctx(d.path()))
            .await;

        assert!(r.is_error);
        assert!(r.content.contains("must be at least 1"), "{}", r.content);
    }

    #[tokio::test]
    async fn binary_file_is_reported() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("b.bin"), [0u8, 1, 2, 3, 0, 255]).unwrap();
        let r = ReadFileTool::default()
            .execute(r#"{"file_path":"b.bin"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error);
        assert!(r.content.starts_with("Binary file"), "{}", r.content);
    }

    #[tokio::test]
    async fn decodes_gbk_text_file() {
        // Chinese Windows editors write .txt/.md as GBK/GB18030, not UTF-8.
        // from_utf8_lossy would mangle these into replacement chars (mojibake);
        // a GB18030 fallback must recover the original text.
        let d = tempfile::tempdir().unwrap();
        let (gbk, _, had_err) = encoding_rs::GB18030.encode("你好，世界");
        assert!(!had_err);
        std::fs::write(d.path().join("notes.txt"), &gbk).unwrap();
        let r = ReadFileTool::default()
            .execute(r#"{"file_path":"notes.txt"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(
            r.content.contains("你好，世界"),
            "GBK should decode, got: {}",
            r.content
        );
    }

    #[tokio::test]
    async fn binary_file_includes_recovery_hint() {
        // A binary with a recognised document extension should pivot the model to an
        // external converter on the first failure, not leave it cycling offset/limit.
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("report.pdf"), b"%PDF-1.4\0\0\0binary blob").unwrap();
        let r = ReadFileTool::default()
            .execute(r#"{"file_path":"report.pdf"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.starts_with("Binary file"), "{}", r.content);
        assert!(
            r.content.contains("pdftotext"),
            "pdf recovery hint, got: {}",
            r.content
        );
    }

    #[tokio::test]
    async fn directory_lists_contents() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("x.txt"), "hi").unwrap();
        std::fs::create_dir(d.path().join("sub")).unwrap();
        let r = ReadFileTool::default()
            .execute(r#"{"file_path":"."}"#, &ctx(d.path()))
            .await;
        assert!(r.content.contains("is a directory"), "{}", r.content);
        assert!(r.content.contains("sub/"), "{}", r.content);
        assert!(r.content.contains("x.txt"), "{}", r.content);
    }

    #[tokio::test]
    async fn missing_file_errors() {
        let d = tempfile::tempdir().unwrap();
        let r = ReadFileTool::default()
            .execute(r#"{"file_path":"nope.txt"}"#, &ctx(d.path()))
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("no such file"), "{}", r.content);
    }

    /// `read_file` is the single biggest source of not-found failures in the field (234 in 14
    /// days), for the same reason as grep/glob: a guessed path with no clue about where the
    /// tree actually stops.
    #[tokio::test]
    async fn missing_file_error_carries_the_nearest_existing_ancestor() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("app")).unwrap();
        std::fs::write(d.path().join("app/build.gradle"), "").unwrap();
        let r = ReadFileTool::default()
            .execute(
                r#"{"file_path":"app/src/main/AndroidManifest.xml"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(r.is_error, "{}", r.content);
        assert!(
            r.content.contains("Nearest existing directory"),
            "{}",
            r.content
        );
        assert!(r.content.contains("build.gradle"), "{}", r.content);
    }

    #[tokio::test]
    async fn lenient_offset_limit_accepts_float_strings() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "l1\nl2\nl3\nl4\n").unwrap();
        // Weak models send "2.0" / "2.0" instead of integers.
        let r = ReadFileTool::default()
            .execute(
                r#"{"file_path":"a.txt","offset":"2.0","limit":"2.0"}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("2\tl2"), "{}", r.content);
        assert!(r.content.contains("3\tl3"), "{}", r.content);
        assert!(!r.content.contains("\tl4"), "{}", r.content);
    }

    #[cfg(feature = "codeintel")]
    #[tokio::test]
    async fn large_code_file_returns_skeleton() {
        let d = tempfile::tempdir().unwrap();
        let mut src = String::from("fn alpha() {\n");
        for _ in 0..350 {
            src.push_str("    let _ = 1;\n");
        }
        src.push_str("}\nfn beta() {}\n");
        std::fs::write(d.path().join("big.rs"), &src).unwrap();
        let r = ReadFileTool::default()
            .execute(r#"{"file_path":"big.rs"}"#, &ctx(d.path()))
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("File skeleton"), "{}", r.content);
        assert!(
            r.content.contains("alpha") && r.content.contains("beta"),
            "{}",
            r.content
        );
        assert!(
            !r.content.contains("let _ = 1;"),
            "skeleton must not dump bodies: {}",
            r.content
        );
    }

    #[cfg(feature = "codeintel")]
    #[tokio::test]
    async fn offset_bypasses_skeleton() {
        let d = tempfile::tempdir().unwrap();
        let mut src = String::from("fn f() {}\n");
        for i in 0..400 {
            src.push_str(&format!("// line {i}\n"));
        }
        std::fs::write(d.path().join("big.rs"), &src).unwrap();
        let r = ReadFileTool::default()
            .execute(
                r#"{"file_path":"big.rs","offset":1,"limit":3}"#,
                &ctx(d.path()),
            )
            .await;
        assert!(
            !r.content.contains("File skeleton"),
            "offset/limit must bypass skeleton: {}",
            r.content
        );
        assert!(r.content.contains("1\tfn f"), "{}", r.content);
    }

    #[cfg(feature = "codeintel")]
    #[tokio::test]
    async fn skeleton_threshold_boundary() {
        let d = tempfile::tempdir().unwrap();
        // exactly 300 lines (fn + 299 fillers) → total > 300 is false → full read
        let mut at = String::from("fn f() {}\n");
        for _ in 0..299 {
            at.push_str("// x\n");
        }
        std::fs::write(d.path().join("at.rs"), &at).unwrap();
        let r = ReadFileTool::default()
            .execute(r#"{"file_path":"at.rs"}"#, &ctx(d.path()))
            .await;
        assert!(
            !r.content.contains("File skeleton"),
            "300 lines must NOT skeleton: {}",
            r.content
        );
        // 301 lines → skeleton
        let mut over = String::from("fn f() {}\n");
        for _ in 0..300 {
            over.push_str("// x\n");
        }
        std::fs::write(d.path().join("over.rs"), &over).unwrap();
        let r2 = ReadFileTool::default()
            .execute(r#"{"file_path":"over.rs"}"#, &ctx(d.path()))
            .await;
        assert!(
            r2.content.contains("File skeleton"),
            "301 lines must skeleton: {}",
            r2.content
        );
    }

    #[cfg(feature = "codeintel")]
    #[tokio::test]
    async fn large_symbolless_code_file_falls_back_to_a_bounded_page() {
        // A >300-line .rs with NO symbols (only comments) has no skeleton, so it
        // falls back to the same bounded page as other text files.
        let d = tempfile::tempdir().unwrap();
        let mut src = String::new();
        for i in 0..400 {
            src.push_str(&format!("// comment {i}\n"));
        }
        std::fs::write(d.path().join("c.rs"), &src).unwrap();
        let r = ReadFileTool::default()
            .execute(r#"{"file_path":"c.rs"}"#, &ctx(d.path()))
            .await;
        assert!(!r.content.contains("File skeleton"), "{}", r.content);
        assert!(r.content.contains("comment 0"), "{}", r.content);
        assert!(!r.content.contains("comment 300"), "{}", r.content);
        assert!(
            r.content.contains("Continue with read_file("),
            "{}",
            r.content
        );
    }

    #[cfg(feature = "codeintel")]
    #[tokio::test]
    async fn large_non_code_file_uses_a_bounded_page() {
        // .txt has no tree-sitter language, so it uses normal bounded pagination.
        let d = tempfile::tempdir().unwrap();
        let mut src = String::new();
        for i in 0..400 {
            src.push_str(&format!("line {i}\n"));
        }
        std::fs::write(d.path().join("big.txt"), &src).unwrap();
        let r = ReadFileTool::default()
            .execute(r#"{"file_path":"big.txt"}"#, &ctx(d.path()))
            .await;
        assert!(!r.content.contains("File skeleton"), "{}", r.content);
        assert!(r.content.contains("line 0"), "{}", r.content);
        assert!(!r.content.contains("line 300"), "{}", r.content);
        assert!(
            r.content.contains("Continue with read_file("),
            "{}",
            r.content
        );
    }

    #[tokio::test]
    async fn long_line_is_truncated() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("long.txt"), "x".repeat(5000)).unwrap();
        let r = ReadFileTool::default()
            .execute(r#"{"file_path":"long.txt"}"#, &ctx(d.path()))
            .await;
        assert!(
            r.content.contains("line truncated to 2000 chars"),
            "{}",
            r.content
        );
    }

    #[test]
    fn read_file_is_parallel_safe() {
        let t = ReadFileTool::new(false); // vision flag irrelevant here
        assert!(t.read_only_hint(), "read_file has no side effects");
        assert!(t.parallel_safe("{}"), "read_file may run concurrently");
    }
}
