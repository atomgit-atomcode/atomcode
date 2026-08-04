//! `read_file` — read a file (or list a directory) with line numbers and optional
//! slicing. Non-destructive ⇒ always `Safe`. Neutral core ported from the production
//! reader, minus the coding enrichments (semantic skeleton, read_cache, file_store).

use super::{err, looks_binary, ok, ok_with_images, resolve_path};
use async_trait::async_trait;
use atomcode_kernel::message::ImageContent;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use base64::Engine;
use serde::Deserialize;
use serde_json::json;

/// Refuse a full (un-sliced) read above this size; the model must slice instead.
const MAX_FULL_BYTES: u64 = 5 * 1024 * 1024;
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

#[derive(Deserialize)]
struct Args {
    file_path: String,
    #[serde(default, deserialize_with = "lenient_usize")]
    offset: Option<usize>,
    #[serde(default, deserialize_with = "lenient_usize")]
    limit: Option<usize>,
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
        "Read a file from the filesystem. Returns the contents prefixed with 1-based \
         line numbers (`<n>\\t<content>`). For a large file, read a slice with `offset` \
         (1-based start line) and `limit` (max lines). If the path is a directory its \
         entries are listed instead. Relative paths resolve against the working directory."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "file_path": { "type": "string", "description": "Path to read (absolute, or relative to the working directory)" },
                "offset": { "type": "integer", "description": "Start line, 1-based. Omit to read from the beginning." },
                "limit": { "type": "integer", "description": "Max number of lines to read. Omit to read to the end." }
            },
            "required": ["file_path"]
        })
    }
    /// No side effects — a pure read. Makes it `parallel_safe` (concurrent
    /// execution) and allowed in plan mode.
    fn read_only_hint(&self) -> bool {
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
        let path = resolve_path(&a.file_path, &ctx.working_dir);

        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(_) => {
                return err(format!(
                    "Error: no such file: {} (resolved to {})",
                    a.file_path,
                    crate::pathnorm::to_display(&path)
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

        if a.offset.is_none() && a.limit.is_none() && meta.len() > MAX_FULL_BYTES {
            return err(format!(
                "File too large to read in full: {} bytes ({:.1} MB). Read a slice with \
                 offset/limit (e.g. read_file({{\"file_path\":\"{}\",\"offset\":1,\"limit\":200}})), \
                 or use bash (wc -l / sed -n).",
                meta.len(),
                meta.len() as f64 / 1_048_576.0,
                a.file_path
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
            Err(_) => match decode_non_utf8_text(&path, &bytes) {
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
        let lines: Vec<&str> = text.lines().collect();
        let total = lines.len();

        // codeintel enrichment: outline a large CODE file as a symbol skeleton instead of
        // dumping it (cross-capability composition; only when codeintel is compiled in).
        // A given offset/limit means the model wants a specific range, so skip it.
        #[cfg(feature = "codeintel")]
        if a.offset.is_none() && a.limit.is_none() && total > SKELETON_THRESHOLD {
            if let Some(skel) = crate::codeintel::skeleton(&path, text.as_ref()) {
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
        let end_idx = match a.limit {
            Some(l) => start_idx.saturating_add(l).min(total),
            None => total,
        };

        let mut out = String::new();
        for (i, line) in lines[start_idx..end_idx].iter().enumerate() {
            let n = start + i;
            if line.chars().count() > MAX_LINE_LEN {
                let head: String = line.chars().take(MAX_LINE_LEN).collect();
                out.push_str(&format!(
                    "{n}\t{head}... (line truncated to {MAX_LINE_LEN} chars)\n"
                ));
            } else {
                out.push_str(&format!("{n}\t{line}\n"));
            }
        }
        if start > 1 || end_idx < total {
            out.push_str(&format!("[Showing lines {start}-{end_idx} of {total}]"));
        }
        ok(out)
    }
}

/// Text-ish extensions worth trying a GBK/GB18030 decode for when UTF-8 fails.
/// A binary file with one of these would already have tripped `looks_binary`,
/// so this gate just avoids feeding genuine binary blobs to the decoder.
const GBK_CANDIDATE_EXTENSIONS: &[&str] = &[
    "txt", "md", "markdown", "csv", "tsv", "log", "sql", "ini", "conf", "cfg", "toml", "yaml",
    "yml", "html", "htm", "xml", "json", "js", "ts", "css", "py", "rb", "go", "rs", "c", "h",
    "cpp", "hpp", "java", "kt", "sh", "bat", "ps1",
];

fn has_text_extension(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let e = e.to_ascii_lowercase();
            GBK_CANDIDATE_EXTENSIONS.iter().any(|t| *t == e)
        })
        .unwrap_or(false)
}

/// Attempt to decode a file that failed UTF-8 validation. Tries GB18030 (superset of
/// GBK/GB2312) only, and only for text-ish extensions — that's ~100% of the real-world
/// miss we've seen on Chinese Windows `.txt`. Returns `None` for everything else so the
/// caller emits the recovery hint instead of mojibake.
fn decode_non_utf8_text(path: &std::path::Path, bytes: &[u8]) -> Option<String> {
    if !has_text_extension(path) {
        return None;
    }
    let (decoded, _, had_errors) = encoding_rs::GB18030.decode(bytes);
    if had_errors {
        return None;
    }
    Some(decoded.into_owned())
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
            r.content.contains("[Showing lines 2-3 of 5]"),
            "{}",
            r.content
        );
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
    async fn large_symbolless_code_file_reads_fully() {
        // a >300-line .rs with NO symbols (only comments) → skeleton() None → full read.
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
    }

    #[cfg(feature = "codeintel")]
    #[tokio::test]
    async fn large_non_code_file_reads_fully() {
        // .txt has no tree-sitter language → skeleton() returns None → normal full read.
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
