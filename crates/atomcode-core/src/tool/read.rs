use anyhow::{Context, Result};
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
fn deserialize_lenient_usize<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;
    struct V;
    impl<'de> de::Visitor<'de> for V {
        type Value = Option<usize>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("usize or string")
        }
        fn visit_none<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
            Ok(Some(v as usize))
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
            if v >= 0 {
                Ok(Some(v as usize))
            } else {
                Ok(None)
            }
        }
        fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<Self::Value, E> {
            Ok(Some(v as usize))
        }
        fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
            // Handle "50.0" → 50
            if let Ok(n) = v.trim().parse::<usize>() {
                return Ok(Some(n));
            }
            if let Ok(f) = v.trim().parse::<f64>() {
                return Ok(Some(f as usize));
            }
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
        match super::approval_for_path(
            &parsed.file_path,
            &working_dir,
            super::ExternalPathAction::Read,
        ) {
            Ok(approval) => approval,
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

        // ── Read cache: performance optimization + redundant-read signaller ──
        // Cache stores (mtime, rendered_output, hit_count). If mtime matches, skip
        // disk read + UTF-8 parse + tree-sitter and return the cached output. On
        // the 2nd+ identical hit (same path/offset/limit/mtime), prepend a
        // count-aware NOTE telling the model that the content hasn't changed and
        // re-issuing this call will keep returning the same bytes — replaces the
        // old runner-side BLOCKED guard, which was a soft-text error the model
        // could (and did) ignore. By returning the answer + a stop signal instead
        // of refusing the call, models that were stuck in a "retry on error"
        // reflex see useful content + a clear hint to switch tactics.
        let cache_key: crate::tool::ReadCacheKey = (path.clone(), parsed.offset, parsed.limit);
        let disk_mtime = tokio::fs::metadata(&path)
            .await
            .ok()
            .and_then(|m| m.modified().ok());
        if let Some(mtime) = disk_mtime {
            let cached = ctx.read_cache.read().await.get(&cache_key).cloned();
            if let Some((cached_mtime, cached_output, prior_count)) = cached {
                if cached_mtime == mtime {
                    let new_count = prior_count + 1;
                    // Persist the bumped count so the next hit's note keeps escalating.
                    ctx.read_cache.write().await.insert(
                        cache_key.clone(),
                        (mtime, cached_output.clone(), new_count),
                    );
                    let display_name = std::path::Path::new(&parsed.file_path)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| parsed.file_path.clone());
                    let output = if new_count >= 2 {
                        format!(
                            "[NOTE: this region of `{}` has been read {} times this session — \
                             the file hasn't changed since the previous read so the content \
                             below is byte-identical. Re-issuing this same call will keep \
                             returning these same bytes. If you need different information, \
                             jump to a different offset/limit, switch to a different file, \
                             or stop reading and proceed with the answer you already have.]\n\n{}",
                            display_name, new_count, cached_output
                        )
                    } else {
                        cached_output
                    };
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output,
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
        //
        // 2026-04-22: collect up to 20 candidates then rank by path-prefix
        // similarity to what the agent asked for, show top 5. Without the
        // prefix ranking, a random match in an unrelated subtree (e.g. the
        // first `index.html` the walk hit) could outrank the correct one in
        // the requested project — agent ignored the suggestion and started
        // manual `ls` (see 426-atom 2026-04-21 session).
        if !path_ref.exists() {
            // Always return a clean NotFound message (with resolved path
            // surfaced) — never fall through to `tokio::fs::read` on a
            // missing file. Falling through used to leak a bare
            // `"No such file or directory (os error 2)"` from the OS,
            // which (a) didn't say WHICH path was tried, and (b) was
            // indistinguishable from EACCES leaks. The agent often
            // misread it as a permission issue and looped on read_file
            // hitting the call-loop cap (see runner.rs detect_call_loop)
            // instead of correcting the path.
            let filename = path_ref
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let mut matches: Vec<String> = Vec::new();
            if !filename.is_empty() {
                fn find_file(
                    dir: &std::path::Path,
                    target: &str,
                    depth: usize,
                    max_depth: usize,
                    results: &mut Vec<String>,
                ) {
                    if depth > max_depth || results.len() >= 20 {
                        return;
                    }
                    if let Ok(entries) = std::fs::read_dir(dir) {
                        for entry in entries.flatten() {
                            let name = entry.file_name().to_string_lossy().to_string();
                            if name.starts_with('.')
                                || name == "node_modules"
                                || name == "target"
                                || name == ".git"
                            {
                                continue;
                            }
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
                // Rank by shared-path-prefix length with the requested
                // path. The correct match almost always shares the most
                // segments with what the agent asked for.
                matches.sort_by_key(|m| {
                    std::cmp::Reverse(super::shared_prefix_len(&parsed.file_path, m))
                });
            }

            // Build the message. Always include the resolved path so the
            // agent sees what was actually attempted (raw input might be
            // relative — the resolved path is what hit the filesystem).
            let mut output = format!(
                "Error: No such file: {} (resolved to {})",
                parsed.file_path,
                path_ref.display()
            );
            if !matches.is_empty() {
                let shown: Vec<String> =
                    matches.iter().take(5).map(|m| format!("  {}", m)).collect();
                output.push_str("\n\nDid you mean:\n");
                output.push_str(&shown.join("\n"));
            }
            // Nudge the agent toward absolute paths when it passed a
            // relative one. The most common cause of this branch is the
            // agent ignoring an absolute path the user mentioned in
            // their message and passing a bare basename instead.
            if !std::path::Path::new(&parsed.file_path).is_absolute()
                && !parsed.file_path.starts_with('~')
            {
                output.push_str(&format!(
                    "\n\nHint: file_path was relative and resolved against working dir {}. \
                     If the user mentioned a different location (e.g. ~/some/path), retry \
                     with the absolute path.",
                    working_dir.display()
                ));
            }
            return Ok(ToolResult {
                call_id: String::new(),
                output,
                success: false,
            });
        }

        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("Failed to read {}", path.display()))?;

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
                        ctx.read_cache
                            .write()
                            .await
                            .insert(cache_key.clone(), (mtime, output.clone(), 1));
                    }
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output,
                        success: true,
                    });
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
                let fname = path_ref
                    .file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default();
                let mut skel = format!("[File skeleton: {} ({} lines). Each symbol line ends with the exact offset/limit to read it — copy those into read_file, don't recompute.]\n\n",
                    fname, total_lines);
                // Skeleton is fully driven by semantic layer's list_symbols().
                // For Vue/Svelte, list_symbols already includes <template>/<style> sections
                // as pseudo-symbols alongside script functions.
                // Score symbols for auto-expansion: high-interest names get priority
                let interest_keywords = [
                    "handle", "process", "route", "search", "query", "fetch", "execute",
                    "dispatch", "run", "main", "serve",
                ];
                let mut scored: Vec<(usize, &crate::semantic::Symbol)> = symbols
                    .iter()
                    .map(|s| {
                        let name_lower = s.name.to_lowercase();
                        let body_lines = s.end_line.saturating_sub(s.start_line) + 1;
                        let keyword_score =
                            if interest_keywords.iter().any(|k| name_lower.contains(k)) {
                                100
                            } else {
                                0
                            };
                        (keyword_score + body_lines, s)
                    })
                    .collect();
                scored.sort_by(|a, b| b.0.cmp(&a.0));

                // Pick top 2 functions to auto-expand (5-50 lines each)
                let expand_candidates: Vec<&crate::semantic::Symbol> = scored
                    .iter()
                    .filter(|(_, s)| {
                        let body = s.end_line.saturating_sub(s.start_line) + 1;
                        body >= 5 && body <= 50
                    })
                    .take(2)
                    .map(|(_, s)| *s)
                    .collect();

                for s in &symbols {
                    let sig = lines
                        .get(s.start_line.saturating_sub(1))
                        .map(|l| l.trim())
                        .unwrap_or(&s.name);
                    let sig_short = if sig.chars().count() > 70 {
                        format!("{}...", sig.chars().take(67).collect::<String>())
                    } else {
                        sig.to_string()
                    };

                    let body_len = s.end_line.saturating_sub(s.start_line) + 1;
                    if expand_candidates
                        .iter()
                        .any(|c| c.start_line == s.start_line && c.name == s.name)
                    {
                        // Auto-expand: show full body (no read-params needed — already visible)
                        skel.push_str(&format!(
                            "{:>4}| {}  (L{}-{}) [auto-expanded]\n",
                            s.start_line, sig_short, s.start_line, s.end_line
                        ));
                        let start = s.start_line.saturating_sub(1);
                        let end = s.end_line.min(total_lines);
                        for i in (start + 1)..end {
                            if let Some(line) = lines.get(i) {
                                skel.push_str(&format!("{:>4}| {}\n", i + 1, line));
                            }
                        }
                    } else {
                        skel.push_str(&format!(
                            "{:>4}| {}  (L{}-{}, read offset={} limit={})\n",
                            s.start_line,
                            sig_short,
                            s.start_line,
                            s.end_line,
                            s.start_line,
                            body_len
                        ));
                    }
                }
                skel
            } else {
                // Unreachable: list_symbols always returns Some via indent fallback.
                // Kept as safety net — produces minimal skeleton.
                let fname = path
                    .file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default();
                format!("[File skeleton: {} ({} lines) — use grep to find relevant lines, then read with offset/limit.]\n",
                    fname, total_lines)
            };
            if let Some(mtime) = disk_mtime {
                ctx.read_cache
                    .write()
                    .await
                    .insert(cache_key.clone(), (mtime, skeleton.clone(), 1));
            }
            return Ok(ToolResult {
                call_id: String::new(),
                output: skeleton,
                success: true,
            });
        }

        let offset = parsed.offset.unwrap_or(1).max(1) - 1;

        // No hardcoded line limit — Layer A (auto_skeleton) is the only gate.
        // If auto_skeleton didn't fire, the file fits in budget → return all lines.
        // Ignore model-supplied limit when reading from start (offset=0): if the
        // file passed Layer A, the model is just creating fragments by passing
        // limit=100. GLM-5 does this despite "do NOT use offset/limit" instruction.
        let limit = match (parsed.offset, parsed.limit) {
            (None, Some(_)) => total_lines, // offset=0 + limit → ignore limit, give full
            (Some(_), Some(l)) => l,        // explicit range → respect it
            _ => total_lines,               // no limit → full
        };

        // If offset > 0 but auto-expand would give the whole file, reset offset to 0
        let offset = if offset > 0 && limit >= total_lines {
            0
        } else {
            offset
        };
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
                let unseen: Vec<String> = symbols
                    .iter()
                    .filter(|s| s.start_line < offset + 1 || s.start_line > end)
                    .map(|s| {
                        let sig = lines
                            .get(s.start_line.saturating_sub(1))
                            .map(|l| l.trim())
                            .unwrap_or(&s.name);
                        let sig_short: String = sig.chars().take(70).collect();
                        let body_len = s.end_line.saturating_sub(s.start_line) + 1;
                        format!(
                            "{:>4}| {}  (L{}-{}, read offset={} limit={})",
                            s.start_line,
                            sig_short,
                            s.start_line,
                            s.end_line,
                            s.start_line,
                            body_len
                        )
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
                offset + 1,
                end,
                total_lines,
                skeleton
            ));
        }

        // D3 step 3: full reads of LARGE files push their raw content
        // into the shared FileStore and return a pointer + preview.
        // Subsequent regions are fetched via peek_file (zero disk hit,
        // not duplicated in conversation token by token). Partial
        // range reads (model gave offset/limit) skip this branch and
        // return the full inline output as before — that path is
        // already tightly scoped, store would just add overhead.
        const LARGE_FILE_LINE_THRESHOLD: usize = 50;
        const PREVIEW_LINES: usize = 50;
        let final_output = if returned_all && total_lines > LARGE_FILE_LINE_THRESHOLD {
            let store_id = if let Some(mtime) = disk_mtime {
                ctx.file_store
                    .write()
                    .await
                    .insert(path.clone(), content.clone(), mtime)
            } else {
                // No mtime → can't safely cache (stale check needs it).
                // Fall through to inline behaviour for this read.
                String::new()
            };
            if !store_id.is_empty() {
                let preview_end = PREVIEW_LINES.min(total_lines);
                let preview: String = lines[..preview_end]
                    .iter()
                    .enumerate()
                    .map(|(i, line)| format!("{:>4}| {}", i + 1, line))
                    .collect::<Vec<_>>()
                    .join("\n");
                let display_name = std::path::Path::new(&parsed.file_path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| parsed.file_path.clone());
                let size_kb = content.len() as f64 / 1024.0;
                let remaining = total_lines.saturating_sub(preview_end);
                format!(
                    "[Read {} ({} lines, {:.1} KB) → store_id={}]\n\
                     [Preview L1-{}:]\n{}\n\n\
                     [Remaining {} lines cached locally — fetch any region with \
                     peek_file({{\"store_id\":\"{}\",\"lines\":\"X-Y\"}}). \
                     peek_file is free (no disk hit, no model round trip for \
                     duplicate content).]",
                    display_name, total_lines, size_kb, store_id,
                    preview_end, preview,
                    remaining, store_id,
                )
            } else {
                output
            }
        } else {
            output
        };

        if let Some(mtime) = disk_mtime {
            ctx.read_cache
                .write()
                .await
                .insert(cache_key, (mtime, final_output.clone(), 1));
        }
        Ok(ToolResult {
            call_id: String::new(),
            output: final_output,
            success: true,
        })
    }
}

/// Extensions that are plain text in practice but routinely arrive in GBK /
/// GB18030 on Chinese Windows systems. We *only* try GBK for these — for
/// genuine binary formats (.doc/.pdf/etc) the decode would succeed by luck
/// (GBK accepts most byte sequences) and dump random ideographs into the
/// model's context.
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
    let ext = path
        .extension()
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
        assert!(
            r1.output.contains("fn main"),
            "first read should return content"
        );

        let r2 = tool.execute(&args, &ctx).await.unwrap();
        assert!(r2.success);
        assert!(
            r2.output.contains("fn main"),
            "cache hit should return same content"
        );
    }

    /// 2nd+ identical read prepends a count-aware note so the model can see
    /// the file hasn't changed and stop re-issuing the same call. Replaces
    /// the BLOCKED text error from the deleted Pattern 1 guard, which was a
    /// soft signal the model ignored — observed in the field with the cap
    /// counter climbing to "5-call cap" across consecutive turns.
    #[tokio::test]
    async fn read_cache_hits_prepend_repeat_note_after_first_replay() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn main() {}\n").unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = ReadFileTool;
        let args = format!(r#"{{"file_path":"{}"}}"#, path.display());

        // 1st read: cache miss, no note.
        let r1 = tool.execute(&args, &ctx).await.unwrap();
        assert!(!r1.output.starts_with("[NOTE:"), "1st read must not have a note");
        assert!(r1.success);

        // 2nd read: cache hit, note kicks in (count=2).
        let r2 = tool.execute(&args, &ctx).await.unwrap();
        assert!(r2.success, "2nd read must still succeed (not BLOCKED)");
        assert!(
            r2.output.starts_with("[NOTE:"),
            "2nd read must prepend the repeat-read note\nGot: {}",
            &r2.output[..r2.output.len().min(120)]
        );
        assert!(
            r2.output.contains("read 2 times"),
            "note must report the running count\nGot: {}",
            &r2.output[..r2.output.len().min(200)]
        );
        // The actual file content survives below the note.
        assert!(r2.output.contains("fn main"));

        // 3rd read: count keeps escalating.
        let r3 = tool.execute(&args, &ctx).await.unwrap();
        assert!(r3.success);
        assert!(
            r3.output.contains("read 3 times"),
            "3rd read note must report 3, not stay at 2"
        );
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
        assert_ne!(
            r2.output, out1,
            "2nd read must re-read from disk when mtime changed"
        );
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
        assert!(
            r.output.contains("你好世界"),
            "expected decoded text, got: {}",
            r.output
        );
        assert!(!r.output.contains("Binary file"));
    }

    /// Binary formats (Office, PDF) should NOT trigger GBK decode (that would
    /// dump random ideographs into context). Instead the hint path fires.
    #[tokio::test]
    async fn read_docx_returns_recovery_hint_not_garbage() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("spec.docx");
        // Docx is a zip — "PK\x03\x04" + random bytes that aren't valid UTF-8.
        let docx_bytes: Vec<u8> = [b'P', b'K', 0x03, 0x04]
            .iter()
            .copied()
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
        assert!(
            r.output.contains("Recovery"),
            "should give recovery hint: {}",
            r.output
        );
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
        assert!(
            r.output.contains("pdftotext"),
            "should suggest pdftotext: {}",
            r.output
        );
    }

    #[test]
    fn shell_quote_escapes_single_quote() {
        assert_eq!(shell_quote("abc"), "'abc'");
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
        assert_eq!(
            shell_quote("/tmp/file with spaces.doc"),
            "'/tmp/file with spaces.doc'"
        );
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
        assert!(
            r.output.contains("[File skeleton:"),
            "expected skeleton output, got:\n{}",
            r.output
        );
        // A collapsed symbol line must carry the pre-computed read params.
        assert!(
            r.output.contains("read offset=1 limit="),
            "skeleton should expose offset=1 limit=<body_len> for save_session\nGot:\n{}",
            r.output
        );
    }

    /// P0 #4: when a 404 recovery has multiple candidates, the one sharing
    /// the most path prefix with the requested path must come first.
    /// Regression for 426-atom 2026-04-21 session where agent asked for
    /// `/proj/A/index.html` and a wrong-project `index.html` outranked the
    /// correct one.
    #[tokio::test]
    async fn read_404_ranks_by_shared_path_prefix() {
        let dir = TempDir::new().unwrap();
        // Two projects with a same-named file. The one sharing more of the
        // requested path must be listed first.
        std::fs::create_dir_all(dir.path().join("proj-wanted").join("presentation")).unwrap();
        std::fs::create_dir_all(dir.path().join("proj-other")).unwrap();
        std::fs::write(
            dir.path().join("proj-wanted/presentation/index.html"),
            "<html></html>",
        )
        .unwrap();
        std::fs::write(dir.path().join("proj-other/index.html"), "<html></html>").unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = ReadFileTool;
        // Ask for a wrong path in proj-wanted — 404, both candidates found.
        let asked = dir.path().join("proj-wanted/index.html");
        let args = format!(r#"{{"file_path":"{}"}}"#, asked.display());

        let r = tool.execute(&args, &ctx).await.unwrap();
        assert!(!r.success);
        assert!(r.output.contains("Did you mean"));
        // The correct candidate (inside proj-wanted/) must appear before the
        // cross-project noise (inside proj-other/).
        let wanted_pos = r
            .output
            .find("proj-wanted/presentation/index.html")
            .unwrap();
        let other_pos = r.output.find("proj-other/index.html").unwrap();
        assert!(
            wanted_pos < other_pos,
            "proj-wanted match must rank above proj-other. output:\n{}",
            r.output
        );
    }

    /// The key UX case behind option B in the OAuth-fix follow-up:
    /// agent passes a relative basename for a file that doesn't exist
    /// in the working dir AND no fuzzy match turns up. Pre-fix this
    /// fell through to `tokio::fs::read?` and the agent saw a bare
    /// `"No such file or directory (os error 2)"` (or, when a parent
    /// directory's perms tripped the kernel, a misleading
    /// `"Permission denied (os error 13)"`). The fix:
    ///   1. Always early-return a clean `Error: No such file: <input>
    ///      (resolved to <abs path>)` so the agent sees what was tried
    ///   2. Add the absolute-path hint when input was relative —
    ///      pushing the agent to use the path the user actually
    ///      mentioned (e.g. `~/.atomcode/MEMORY.md`) on the next call
    ///      instead of looping.
    #[tokio::test]
    async fn read_404_relative_path_includes_resolved_path_and_absolute_hint() {
        let dir = TempDir::new().unwrap();
        // Working dir has no MEMORY.md and no fuzzy match — so the
        // suggestion list must come back empty and we exercise the
        // "no candidates" branch that previously fell through.
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = ReadFileTool;
        let args = r#"{"file_path":"MEMORY.md"}"#;

        let r = tool.execute(args, &ctx).await.unwrap();
        assert!(!r.success);
        assert!(
            r.output.contains("No such file: MEMORY.md"),
            "must surface the raw input. output:\n{}",
            r.output
        );
        assert!(
            r.output.contains("resolved to"),
            "must surface the resolved absolute path so the agent sees \
             what was actually attempted. output:\n{}",
            r.output
        );
        assert!(
            r.output.contains("absolute path"),
            "relative-input path must include the absolute-path hint. output:\n{}",
            r.output
        );
        // We expect this branch to NEVER leak a bare OS error.
        assert!(
            !r.output.contains("os error"),
            "must not leak the raw OS error string. output:\n{}",
            r.output
        );
    }

    /// Mirror of the relative-path test for absolute input: the
    /// resolved-path line is still useful (shows canonicalisation),
    /// but the absolute-path hint must be suppressed — the agent
    /// already gave us an absolute path.
    #[tokio::test]
    async fn read_404_absolute_path_omits_relative_hint() {
        let dir = TempDir::new().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = ReadFileTool;
        let asked = dir.path().join("MEMORY.md");
        let args = format!(r#"{{"file_path":"{}"}}"#, asked.display());

        let r = tool.execute(&args, &ctx).await.unwrap();
        assert!(!r.success);
        assert!(r.output.contains("No such file"));
        assert!(
            !r.output.contains("absolute path"),
            "absolute-input path must NOT show the relative-path hint. output:\n{}",
            r.output
        );
    }

    // ── D3 FileStore integration ────────────────────────────────────

    /// Helper: write a file with `n_lines` lines (each `line N`) and
    /// return its absolute path. Use file sizes large enough to trip
    /// the FileStore threshold (50 lines).
    fn write_n_line_file(dir: &TempDir, name: &str, n_lines: usize) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let body: String = (1..=n_lines).map(|i| format!("line {}\n", i)).collect();
        std::fs::write(&path, body).unwrap();
        path
    }

    /// Large full reads return a pointer + preview, not the entire
    /// inline content — the body lives in FileStore for cheap follow-up
    /// peek_file calls.
    #[tokio::test]
    async fn d3_full_read_of_large_file_returns_pointer_and_preview() {
        let dir = TempDir::new().unwrap();
        let path = write_n_line_file(&dir, "big.rs", 200);
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let args = format!(r#"{{"file_path":"{}"}}"#, path.display());
        let r = ReadFileTool.execute(&args, &ctx).await.unwrap();
        assert!(r.success);
        assert!(
            r.output.contains("store_id=fs_"),
            "must announce store_id; got:\n{}",
            r.output
        );
        assert!(
            r.output.contains("Preview L1-50:"),
            "must include preview header; got:\n{}",
            r.output
        );
        assert!(
            r.output.contains("peek_file"),
            "must point the model at peek_file; got:\n{}",
            r.output
        );
        // Lines 51-200 must NOT be inlined — that's the whole point of
        // the indirection.
        assert!(
            !r.output.contains("line 100"),
            "preview should stop at line 50; line 100 must not appear:\n{}",
            r.output
        );
        // Store should have one entry now.
        assert_eq!(ctx.file_store.read().await.len(), 1);
    }

    /// Small files (≤ 50 lines) bypass the store entirely — pointer
    /// indirection would just add overhead with no caching upside.
    #[tokio::test]
    async fn d3_small_file_skips_store_and_inlines() {
        let dir = TempDir::new().unwrap();
        let path = write_n_line_file(&dir, "small.rs", 10);
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let args = format!(r#"{{"file_path":"{}"}}"#, path.display());
        let r = ReadFileTool.execute(&args, &ctx).await.unwrap();
        assert!(r.success);
        assert!(
            !r.output.contains("store_id="),
            "small file must inline, not point at store; got:\n{}",
            r.output
        );
        assert_eq!(
            ctx.file_store.read().await.len(),
            0,
            "small file must not push into store"
        );
    }

    /// Range reads (offset/limit) bypass the store — the model is
    /// already asking for a specific slice, no need to cache the whole
    /// file just to serve their narrow request.
    #[tokio::test]
    async fn d3_range_read_skips_store() {
        let dir = TempDir::new().unwrap();
        let path = write_n_line_file(&dir, "big.rs", 200);
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let args = format!(
            r#"{{"file_path":"{}","offset":50,"limit":20}}"#,
            path.display()
        );
        let r = ReadFileTool.execute(&args, &ctx).await.unwrap();
        assert!(r.success);
        assert!(
            !r.output.contains("store_id="),
            "range read must NOT trip the store branch:\n{}",
            r.output
        );
        assert_eq!(ctx.file_store.read().await.len(), 0);
    }

    /// Full lifecycle: read → peek (hit) → edit → peek (stale).
    /// The atomgr-session bug class this prevents: after editing a
    /// file, peek_file would otherwise serve pre-edit bytes — model
    /// reasons against a snapshot that no longer matches disk.
    #[tokio::test]
    async fn d3_edit_invalidates_store_and_peek_reports_stale() {
        let dir = TempDir::new().unwrap();
        let path = write_n_line_file(&dir, "big.rs", 200);
        let ctx = ToolContext::new(dir.path().to_path_buf());

        // Step 1: full read populates the store.
        let read_args = format!(r#"{{"file_path":"{}"}}"#, path.display());
        let r1 = ReadFileTool.execute(&read_args, &ctx).await.unwrap();
        assert!(r1.success);
        // Extract the store_id from the result.
        let store_id = r1
            .output
            .lines()
            .find_map(|l| {
                l.find("store_id=fs_")
                    .map(|i| l[i + "store_id=".len()..].split(']').next().unwrap().to_string())
            })
            .expect("store_id present in read result");
        assert!(store_id.starts_with("fs_"));

        // Step 2: peek before edit succeeds.
        let peek_args = format!(
            r#"{{"store_id":"{}","lines":"100-105"}}"#,
            store_id
        );
        let p1 = crate::tool::peek::PeekFileTool
            .execute(&peek_args, &ctx)
            .await
            .unwrap();
        assert!(p1.success, "peek before edit must succeed; got:\n{}", p1.output);
        assert!(p1.output.contains("line 100"));

        // Step 3: edit invalidates the store.
        let edit_args = format!(
            r#"{{"file_path":"{}","old_string":"line 1\n","new_string":"LINE 1\n"}}"#,
            path.display()
        );
        let e = crate::tool::edit::EditFileTool
            .execute(&edit_args, &ctx)
            .await
            .unwrap();
        assert!(e.success, "edit must succeed; got:\n{}", e.output);
        assert_eq!(
            ctx.file_store.read().await.len(),
            0,
            "edit must invalidate the store entry"
        );

        // Step 4: peek with the now-orphan store_id returns a friendly
        // recovery hint, not pre-edit content.
        let p2 = crate::tool::peek::PeekFileTool
            .execute(&peek_args, &ctx)
            .await
            .unwrap();
        assert!(!p2.success, "peek after edit must fail (stale); got:\n{}", p2.output);
        assert!(
            p2.output.contains("read_file"),
            "stale message must point at re-read; got:\n{}",
            p2.output
        );
    }

    /// Re-reading the same large file replaces (not duplicates) the
    /// store entry. The store_id changes only if content changed.
    #[tokio::test]
    async fn d3_reread_unchanged_file_reuses_store_id() {
        let dir = TempDir::new().unwrap();
        let path = write_n_line_file(&dir, "big.rs", 200);
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let args = format!(r#"{{"file_path":"{}"}}"#, path.display());
        let r1 = ReadFileTool.execute(&args, &ctx).await.unwrap();
        // The READ cache will short-circuit the second read to the
        // exact same output, so we extract id from r1 only — but the
        // FileStore must still have only one entry, not two.
        assert!(r1.success);
        let _ = ReadFileTool.execute(&args, &ctx).await.unwrap();
        assert_eq!(
            ctx.file_store.read().await.len(),
            1,
            "second read of identical file must not duplicate store entry"
        );
    }

    /// peek_file with an unknown id returns a friendly error pointing
    /// at re-read, not a panic or raw error string.
    #[tokio::test]
    async fn d3_peek_unknown_store_id_recovers_gracefully() {
        let dir = TempDir::new().unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let r = crate::tool::peek::PeekFileTool
            .execute(r#"{"store_id":"fs_deadbeef","lines":"1-5"}"#, &ctx)
            .await
            .unwrap();
        assert!(!r.success);
        assert!(
            r.output.contains("Re-issue read_file"),
            "must point the model at recovery; got:\n{}",
            r.output
        );
    }
}
