use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

/// Files with more lines than this return a skeleton (structure overview)
/// instead of full content when read without offset/limit. GLM-5 gets lost
/// in the middle at ~685 lines — 300 is the safe full-content ceiling.
/// Shared with `agent::tool_dispatch` so its first-read heuristic stays aligned.
pub(crate) const SKELETON_LINE_THRESHOLD: usize = 300;

pub struct ReadFileTool;

/// Deserialize a number that may arrive as a float string (weak models often send "50.0" instead of 50).
fn deserialize_lenient_usize<'de, D>(deserializer: D) -> std::result::Result<Option<usize>, D::Error>
where D: serde::Deserializer<'de> {
    use serde::de;
    struct V;
    impl<'de> de::Visitor<'de> for V {
        type Value = Option<usize>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { f.write_str("usize or string") }
        fn visit_none<E: de::Error>(self) -> std::result::Result<Self::Value, E> { Ok(None) }
        fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> { Ok(None) }
        fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> { Ok(Some(v as usize)) }
        fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
            if v >= 0 { Ok(Some(v as usize)) } else { Ok(None) }
        }
        fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<Self::Value, E> { Ok(Some(v as usize)) }
        fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
            // Handle "50.0" → 50
            if let Ok(n) = v.trim().parse::<usize>() { return Ok(Some(n)); }
            if let Ok(f) = v.trim().parse::<f64>() { return Ok(Some(f as usize)); }
            Ok(None)
        }
    }
    deserializer.deserialize_any(V)
}

#[derive(Deserialize)]
struct ReadFileArgs {
    file_path: String,
    #[serde(default, deserialize_with = "deserialize_lenient_usize")]
    offset: Option<usize>,
    #[serde(default, deserialize_with = "deserialize_lenient_usize")]
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReadFileTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "read_file",
            description: "Read a file. Returns full content with line numbers.\n\
                Large files return a skeleton (structure overview) — use offset/limit to read sections.\n\
                NEVER use bash (cat/head/tail) to read files.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "Absolute path to the file to read" },
                    "offset": { "type": "integer", "description": "Start line (1-based). Omit to read from beginning." },
                    "limit": { "type": "integer", "description": "Max lines to read. Defaults to full file." }
                },
                "required": ["file_path"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    fn approval_with_context(&self, args: &str, ctx: &ToolContext) -> ApprovalRequirement {
        let parsed = match serde_json::from_str::<ReadFileArgs>(args) {
            Ok(parsed) => parsed,
            Err(_) => return self.approval(args),
        };
        let working_dir = match ctx.working_dir.try_read() {
            Ok(wd) => wd.clone(),
            Err(_) => return self.approval(args),
        };
        match super::inspect_path_access(&parsed.file_path, &working_dir) {
            Ok(access) if !access.within_workspace => ApprovalRequirement::RequireApproval(
                format!(
                    "Reading file outside working directory: {} (working dir: {})",
                    parsed.file_path,
                    access.workspace_root.display()
                ),
            ),
            Ok(_) => self.approval(args),
            Err(_) => self.approval(args),
        }
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: ReadFileArgs = serde_json::from_str(args)?;
        let working_dir = ctx.working_dir.read().await.clone();
        let path = match super::inspect_path_access(&parsed.file_path, &working_dir) {
            Ok(access) => access.path,
            Err(err) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: err.to_string(),
                    success: false,
                });
            }
        };
        let path_ref = path.as_path();

        // ── Read cache: performance optimization only, NOT a STUB gate ──
        // Cache stores (mtime, rendered_output). If mtime matches, skip disk read +
        // UTF-8 parse + tree-sitter. Returns full content so the model always gets
        // what it asked for. STUB behavior was tried and reverted — it blocks
        // legitimate re-reads and doesn't prevent short-distance duplicates
        // (model ignores STUB text due to lost-at-the-end attention).
        let cache_key: crate::tool::ReadCacheKey = (
            path.clone(),
            parsed.offset,
            parsed.limit,
        );
        let disk_mtime = tokio::fs::metadata(&path).await.ok()
            .and_then(|m| m.modified().ok());
        if let Some(mtime) = disk_mtime {
            if let Some((cached_mtime, cached_output)) = ctx.read_cache.read().await.get(&cache_key).cloned() {
                if cached_mtime == mtime {
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: cached_output,
                        success: true,
                    });
                }
            }
        }

        // Auto-recover: if the path is a directory, return a listing instead of an error.
        if path_ref.is_dir() {
            let mut entries: Vec<String> = Vec::new();
            if let Ok(mut rd) = tokio::fs::read_dir(path_ref).await {
                while let Ok(Some(entry)) = rd.next_entry().await {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                    entries.push(if is_dir { format!("{}/", name) } else { name });
                }
            }
            entries.sort();
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!(
                    "[NOTE: {} is a directory, not a file. Here are its contents:]\n{}",
                    parsed.file_path,
                    entries.join("\n")
                ),
                success: true,
            });
        }

        // If file doesn't exist, auto-find similar filenames and suggest.
        // Saves 2-3 turns of path guessing (7% of sessions hit this).
        if !path_ref.exists() {
            let filename = path_ref.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if !filename.is_empty() {
                // Quick find: walk up to 5 levels deep for matching filename
                let mut matches: Vec<String> = Vec::new();
                fn find_file(dir: &std::path::Path, target: &str, depth: usize, max_depth: usize, results: &mut Vec<String>) {
                    if depth > max_depth || results.len() >= 5 { return; }
                    if let Ok(entries) = std::fs::read_dir(dir) {
                        for entry in entries.flatten() {
                            let name = entry.file_name().to_string_lossy().to_string();
                            if name.starts_with('.') || name == "node_modules" || name == "target" || name == ".git" { continue; }
                            let p = entry.path();
                            if p.is_dir() {
                                find_file(&p, target, depth + 1, max_depth, results);
                            } else if name == target {
                                results.push(p.to_string_lossy().to_string());
                            }
                        }
                    }
                }
                find_file(&working_dir, &filename, 0, 7, &mut matches);
                if !matches.is_empty() {
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: format!(
                            "Error: No such file: {}\n\nDid you mean:\n{}",
                            parsed.file_path,
                            matches.iter().map(|m| format!("  {}", m)).collect::<Vec<_>>().join("\n")
                        ),
                        success: false,
                    });
                }
            }
        }

        let bytes = tokio::fs::read(&path).await?;

        // Decode: UTF-8 first (the vast majority of text files), then GBK
        // fallback for plain-text extensions (Chinese Windows legacy files
        // that fail UTF-8 validation), then declare binary.
        let content = match String::from_utf8(bytes.clone()) {
            Ok(s) => s,
            Err(_) => match decode_non_utf8_text(path_ref, &bytes) {
                Some(s) => s,
                None => {
                    let output = format!(
                        "Binary file ({} bytes), cannot display as text.{}",
                        bytes.len(),
                        binary_recovery_hint(path_ref, &parsed.file_path),
                    );
                    if let Some(mtime) = disk_mtime {
                        ctx.read_cache.write().await.insert(cache_key.clone(), (mtime, output.clone()));
                    }
                    return Ok(ToolResult { call_id: String::new(), output, success: true });
                }
            },
        };

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        // ── Layer A: full content default, skeleton for large files ──
        // Skeleton is the FALLBACK, not the default. Files at or below the
        // threshold return full content so the model can grep→old_string→edit
        // in 2 steps. Above the threshold we return a skeleton (GLM-5 gets
        // lost in the middle at ~685 lines).
        // With offset/limit: always return exact content (model chose a range).
        let auto_skeleton = total_lines > SKELETON_LINE_THRESHOLD
            && parsed.offset.is_none()
            && parsed.limit.is_none();

        if auto_skeleton {
            let mut searcher = ctx.semantic.lock().await;
            let skeleton = if let Some(symbols) = searcher.list_symbols(path_ref) {
                let fname = path_ref.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
                let mut skel = format!("[File skeleton: {} ({} lines). Each symbol line ends with the exact offset/limit to read it — copy those into read_file, don't recompute.]\n\n",
                    fname, total_lines);
                // Skeleton is fully driven by semantic layer's list_symbols().
                // For Vue/Svelte, list_symbols already includes <template>/<style> sections
                // as pseudo-symbols alongside script functions.
                // Score symbols for auto-expansion: high-interest names get priority
                let interest_keywords = ["handle", "process", "route", "search", "query",
                    "fetch", "execute", "dispatch", "run", "main", "serve"];
                let mut scored: Vec<(usize, &crate::semantic::Symbol)> = symbols.iter()
                    .map(|s| {
                        let name_lower = s.name.to_lowercase();
                        let body_lines = s.end_line.saturating_sub(s.start_line) + 1;
                        let keyword_score = if interest_keywords.iter().any(|k| name_lower.contains(k)) { 100 } else { 0 };
                        (keyword_score + body_lines, s)
                    })
                    .collect();
                scored.sort_by(|a, b| b.0.cmp(&a.0));

                // Pick top 2 functions to auto-expand (5-50 lines each)
                let expand_candidates: Vec<&crate::semantic::Symbol> = scored.iter()
                    .filter(|(_, s)| {
                        let body = s.end_line.saturating_sub(s.start_line) + 1;
                        body >= 5 && body <= 50
                    })
                    .take(2)
                    .map(|(_, s)| *s)
                    .collect();

                for s in &symbols {
                    let sig = lines.get(s.start_line.saturating_sub(1))
                        .map(|l| l.trim())
                        .unwrap_or(&s.name);
                    let sig_short = if sig.chars().count() > 70 {
                        format!("{}...", sig.chars().take(67).collect::<String>())
                    } else {
                        sig.to_string()
                    };

                    let body_len = s.end_line.saturating_sub(s.start_line) + 1;
                    if expand_candidates.iter().any(|c| c.start_line == s.start_line && c.name == s.name) {
                        // Auto-expand: show full body (no read-params needed — already visible)
                        skel.push_str(&format!("{:>4}| {}  (L{}-{}) [auto-expanded]\n",
                            s.start_line, sig_short, s.start_line, s.end_line));
                        let start = s.start_line.saturating_sub(1);
                        let end = s.end_line.min(total_lines);
                        for i in (start + 1)..end {
                            if let Some(line) = lines.get(i) {
                                skel.push_str(&format!("{:>4}| {}\n", i + 1, line));
                            }
                        }
                    } else {
                        skel.push_str(&format!("{:>4}| {}  (L{}-{}, read offset={} limit={})\n",
                            s.start_line, sig_short, s.start_line, s.end_line,
                            s.start_line, body_len));
                    }
                }
                skel
            } else {
                // Unreachable: list_symbols always returns Some via indent fallback.
                // Kept as safety net — produces minimal skeleton.
                let fname = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
                format!("[File skeleton: {} ({} lines) — use grep to find relevant lines, then read with offset/limit.]\n",
                    fname, total_lines)
            };
            if let Some(mtime) = disk_mtime {
                ctx.read_cache.write().await.insert(cache_key.clone(), (mtime, skeleton.clone()));
            }
            return Ok(ToolResult { call_id: String::new(), output: skeleton, success: true });
        }

        let offset = parsed.offset.unwrap_or(1).max(1) - 1;

        // No hardcoded line limit — Layer A (auto_skeleton) is the only gate.
        // If auto_skeleton didn't fire, the file fits in budget → return all lines.
        // Ignore model-supplied limit when reading from start (offset=0): if the
        // file passed Layer A, the model is just creating fragments by passing
        // limit=100. GLM-5 does this despite "do NOT use offset/limit" instruction.
        let limit = match (parsed.offset, parsed.limit) {
            (None, Some(_)) => total_lines, // offset=0 + limit → ignore limit, give full
            (Some(_), Some(l)) => l,         // explicit range → respect it
            _ => total_lines,                // no limit → full
        };

        // If offset > 0 but auto-expand would give the whole file, reset offset to 0
        let offset = if offset > 0 && limit >= total_lines { 0 } else { offset };
        // Clamp offset to file size — caller may pass an offset past EOF
        // (e.g. cached line count stale, or model hallucinates a line number).
        let offset = offset.min(total_lines);

        let end = (offset.saturating_add(limit)).min(total_lines);

        // char_limit branch DELETED — Layer A (auto_skeleton) is the only gate.
        // If we reach here, the file passed the budget check → return full content.
        let returned_all = offset == 0 && end >= total_lines;

        let mut output: String = lines[offset..end]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{:>4}| {}", offset + i + 1, line))
            .collect::<Vec<_>>()
            .join("\n");

        if !returned_all {
            // Append tree-sitter skeleton of the UNSEEN portions.
            // Model reads 51 lines but file has 600 — skeleton shows
            // what functions exist in the other 549 lines with line numbers.
            let mut searcher = ctx.semantic.lock().await;
            let skeleton = if let Some(symbols) = searcher.list_symbols(path_ref) {
                let unseen: Vec<String> = symbols.iter()
                    .filter(|s| s.start_line < offset + 1 || s.start_line > end)
                    .map(|s| {
                        let sig = lines.get(s.start_line.saturating_sub(1))
                            .map(|l| l.trim())
                            .unwrap_or(&s.name);
                        let sig_short: String = sig.chars().take(70).collect();
                        let body_len = s.end_line.saturating_sub(s.start_line) + 1;
                        format!("{:>4}| {}  (L{}-{}, read offset={} limit={})",
                            s.start_line, sig_short, s.start_line, s.end_line,
                            s.start_line, body_len)
                    })
                    .collect();
                if !unseen.is_empty() {
                    format!("\n{}", unseen.join("\n"))
                } else {
                    String::new()
                }
            } else {
                String::new()
            };

            output.push_str(&format!(
                "\n\n[Showing lines {}-{} of {} total. Unseen structure:]{}",
                offset + 1, end, total_lines, skeleton
            ));
        }

        if let Some(mtime) = disk_mtime {
            ctx.read_cache.write().await.insert(cache_key, (mtime, output.clone()));
        }
        Ok(ToolResult { call_id: String::new(), output, success: true })
    }
}

/// Extensions that are plain text in practice but routinely arrive in GBK /
/// GB18030 on Chinese Windows systems. We *only* try GBK for these — for
/// genuine binary formats (.doc/.pdf/etc) the decode would succeed by luck
/// (GBK accepts most byte sequences) and dump random ideographs into the
/// model's context.
const GBK_CANDIDATE_EXTENSIONS: &[&str] = &[
    "txt", "md", "markdown", "csv", "tsv", "log", "sql",
    "ini", "conf", "cfg", "toml", "yaml", "yml",
    "html", "htm", "xml", "json", "js", "ts", "css",
    "py", "rb", "go", "rs", "c", "h", "cpp", "hpp", "java", "kt",
    "sh", "bat", "ps1",
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

/// Attempt to decode a file that failed UTF-8 validation. Today this tries
/// GB18030 (superset of GBK/GB2312) only, and only for text-ish extensions —
/// that's ~100% of the real-world miss we've seen on Chinese Windows `.txt`.
/// Returns `None` for binary files so the caller can emit the recovery hint.
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

/// Build a recovery hint for a file that couldn't be decoded as text. Lets
/// the model pivot to an external converter (pandoc / pdftotext / unzip
/// for .docx) on the first failure instead of cycling through offset/limit
/// values for 30 turns.
fn binary_recovery_hint(path: &std::path::Path, full_path_str: &str) -> String {
    let ext = path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let quoted = shell_quote(full_path_str);
    match ext.as_str() {
        "doc" => format!(
            "\n\n[Recovery] This is a legacy Word (.doc) binary. Run one of:\n\
             - bash: `antiword {q}`\n\
             - bash: `pandoc {q} -t plain`\n\
             - bash: `catdoc {q}`",
            q = quoted,
        ),
        "docx" => format!(
            "\n\n[Recovery] This is a modern Word (.docx) — a zip containing XML. Run:\n\
             - bash: `unzip -p {q} word/document.xml | sed 's/<[^>]*>//g'`\n\
             - or: `pandoc {q} -t plain`",
            q = quoted,
        ),
        "xls" => format!(
            "\n\n[Recovery] Legacy Excel (.xls). Run:\n\
             - bash: `libreoffice --headless --convert-to csv --outdir /tmp {q} && cat /tmp/*.csv`",
            q = quoted,
        ),
        "xlsx" => format!(
            "\n\n[Recovery] Modern Excel (.xlsx). Run:\n\
             - bash: `libreoffice --headless --convert-to csv --outdir /tmp {q} && cat /tmp/*.csv`\n\
             - or: `unzip -p {q} xl/sharedStrings.xml` (raw string table)",
            q = quoted,
        ),
        "ppt" | "pptx" => format!(
            "\n\n[Recovery] PowerPoint. Run:\n\
             - bash: `pandoc {q} -t plain`",
            q = quoted,
        ),
        "pdf" => format!(
            "\n\n[Recovery] PDF. Run:\n\
             - bash: `pdftotext {q} -` (poppler)\n\
             - or: `mutool draw -F txt {q}`",
            q = quoted,
        ),
        "rtf" => format!(
            "\n\n[Recovery] RTF. Run:\n\
             - bash: `pandoc {q} -t plain`\n\
             - or: `unrtf --text {q}`",
            q = quoted,
        ),
        _ => format!(
            "\n\n[Hint] The file is not UTF-8 and not a recognised text extension. \
             If it's text in another encoding, ask the user; if it's a packaged format \
             (archive, installer, media), there is no point reading it as text.",
        ),
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
    use tempfile::TempDir;

    /// Cache hit returns full content (performance cache, not STUB).
    #[tokio::test]
    async fn read_cache_hits_returns_full_content() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn main() {}\n").unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = ReadFileTool;
        let args = format!(r#"{{"file_path":"{}"}}"#, path.display());

        let r1 = tool.execute(&args, &ctx).await.unwrap();
        assert!(r1.success);
        assert!(r1.output.contains("fn main"), "first read should return content");

        let r2 = tool.execute(&args, &ctx).await.unwrap();
        assert!(r2.success);
        assert!(r2.output.contains("fn main"), "cache hit should return same content");
    }

    /// Cache miss after file content changes — mtime shifts, cached entry is ignored.
    #[tokio::test]
    async fn read_cache_misses_when_mtime_changes() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("b.rs");
        std::fs::write(&path, "fn main() {}\n").unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = ReadFileTool;
        let args = format!(r#"{{"file_path":"{}"}}"#, path.display());

        let r1 = tool.execute(&args, &ctx).await.unwrap();
        let out1 = r1.output.clone();

        // Touch the file with new content + force a visible mtime change.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&path, "fn main() { println!(\"hi\"); }\n").unwrap();

        let r2 = tool.execute(&args, &ctx).await.unwrap();
        assert_ne!(r2.output, out1, "2nd read must re-read from disk when mtime changed");
        assert!(r2.output.contains("println"));
    }

    /// GBK-encoded .txt should decode via the fallback path, not be reported
    /// as binary. This is the hot path for Chinese Windows legacy text files.
    #[tokio::test]
    async fn read_decodes_gbk_text_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("notes.txt");
        // "你好世界" in GB18030 (hex: C4 E3 BA C3 CA C0 BD E7). Using Vec
        // defeats the compile-time invalid-UTF-8 literal lint.
        let gbk_bytes: Vec<u8> = vec![0xC4, 0xE3, 0xBA, 0xC3, 0xCA, 0xC0, 0xBD, 0xE7, 0x0A];
        std::fs::write(&path, &gbk_bytes).unwrap();
        // Sanity: these bytes must not be valid UTF-8, otherwise the test
        // wouldn't exercise the fallback.
        assert!(std::str::from_utf8(&gbk_bytes).is_err());

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = ReadFileTool;
        let args = format!(r#"{{"file_path":"{}"}}"#, path.display());

        let r = tool.execute(&args, &ctx).await.unwrap();
        assert!(r.success, "GBK text should decode, got: {}", r.output);
        assert!(r.output.contains("你好世界"), "expected decoded text, got: {}", r.output);
        assert!(!r.output.contains("Binary file"));
    }

    /// Binary formats (Office, PDF) should NOT trigger GBK decode (that would
    /// dump random ideographs into context). Instead the hint path fires.
    #[tokio::test]
    async fn read_docx_returns_recovery_hint_not_garbage() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("spec.docx");
        // Docx is a zip — "PK\x03\x04" + random bytes that aren't valid UTF-8.
        let docx_bytes: Vec<u8> = [b'P', b'K', 0x03, 0x04].iter().copied()
            .chain((0..200).map(|i| (i as u8).wrapping_mul(31).wrapping_add(0x80)))
            .collect();
        // Ensure non-UTF-8 (our mul trick usually produces invalid sequences,
        // but belt-and-braces: append a clearly invalid byte).
        let mut docx_bytes = docx_bytes;
        docx_bytes.extend_from_slice(&[0xFE, 0xFF, 0xC0]);
        std::fs::write(&path, &docx_bytes).unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = ReadFileTool;
        let args = format!(r#"{{"file_path":"{}"}}"#, path.display());

        let r = tool.execute(&args, &ctx).await.unwrap();
        assert!(r.output.contains("Binary file"));
        assert!(r.output.contains("Recovery"), "should give recovery hint: {}", r.output);
        assert!(r.output.contains("unzip") || r.output.contains("pandoc"));
    }

    #[tokio::test]
    async fn read_pdf_returns_pdftotext_hint() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("doc.pdf");
        // %PDF-1.4 header + junk that fails UTF-8.
        let mut bytes: Vec<u8> = b"%PDF-1.4\n".to_vec();
        bytes.extend_from_slice(&[0xFF, 0xFE, 0xC0, 0x80, 0xFE]);
        std::fs::write(&path, &bytes).unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = ReadFileTool;
        let args = format!(r#"{{"file_path":"{}"}}"#, path.display());

        let r = tool.execute(&args, &ctx).await.unwrap();
        assert!(r.output.contains("Binary file"));
        assert!(r.output.contains("pdftotext"), "should suggest pdftotext: {}", r.output);
    }

    #[test]
    fn shell_quote_escapes_single_quote() {
        assert_eq!(shell_quote("abc"), "'abc'");
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
        assert_eq!(shell_quote("/tmp/file with spaces.doc"), "'/tmp/file with spaces.doc'");
    }

    /// Skeleton symbol lines carry ready-to-copy offset/limit values so the
    /// model doesn't have to compute body length from the L{start}-{end} span.
    #[tokio::test]
    async fn skeleton_includes_read_offset_limit_hints() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("big.rs");

        // Build >SKELETON_LINE_THRESHOLD lines of Rust with one recognizable
        // fn that is long enough to survive the auto-expand filter (>50 body
        // lines → stays collapsed → should get the read-params hint).
        let mut content = String::new();
        content.push_str("pub fn save_session(id: &str) -> Result<()> {\n");
        for i in 0..80 {
            content.push_str(&format!("    let _x{} = {};\n", i, i));
        }
        content.push_str("    Ok(())\n");
        content.push_str("}\n");
        for i in 0..(SKELETON_LINE_THRESHOLD + 20) {
            content.push_str(&format!("// filler {}\n", i));
        }
        std::fs::write(&path, &content).unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = ReadFileTool;
        let args = format!(r#"{{"file_path":"{}"}}"#, path.display());

        let r = tool.execute(&args, &ctx).await.unwrap();
        assert!(r.success);
        assert!(r.output.contains("[File skeleton:"), "expected skeleton output, got:\n{}", r.output);
        // A collapsed symbol line must carry the pre-computed read params.
        assert!(
            r.output.contains("read offset=1 limit="),
            "skeleton should expose offset=1 limit=<body_len> for save_session\nGot:\n{}",
            r.output
        );
    }
}
