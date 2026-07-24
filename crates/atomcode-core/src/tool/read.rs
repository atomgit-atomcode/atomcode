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

/// Upper bound on bytes returned in a single `read_file` response when
/// streaming the slice path (offset/limit). A 100 k-line file with very
/// long lines otherwise pushes a 10 MB payload through a 1 MB-context
/// model. opencode uses 50 KB; we go larger since AtomCode targets
/// bigger-context models, but still well below MAX_FULL_BYTES so the
/// "small file: just slurp" path isn't pre-empted.
pub(crate) const MAX_BYTES_PER_RESPONSE: usize = 256 * 1024;

/// Hard cap on bytes the full-content (no offset/limit) path will slurp.
/// Files above this go through the streaming slice reader when sliced,
/// or are refused with a "use offset/limit or bash head/grep" hint when
/// fully read (the refusal is read.rs #3). Reads with offset/limit on
/// files BELOW this stay on the slurp+cache path — cheap, and lets
/// FileStore amortize repeat slices.
pub(crate) const MAX_FULL_BYTES: u64 = 5 * 1024 * 1024;

/// Max chars per line in the streaming slice path. A single 100 MB
/// minified-JS line otherwise blows the response budget on one entry.
/// Truncated lines get a "... (line truncated to N chars)" suffix so
/// the model knows the line is incomplete.
pub(crate) const MAX_LINE_LENGTH: usize = 2000;

/// Default `limit` for the streaming slice path when the model passes
/// only `offset` (or wants a sensible page size). Mirrors opencode's
/// `DEFAULT_READ_LIMIT`.
pub(crate) const DEFAULT_READ_LIMIT: usize = 2000;

/// Sample window for binary detection. Read this many leading bytes,
/// sniff for NUL / control-char ratio, and refuse the full slurp if
/// the file looks binary — saves loading a 10 GB `.tar.gz` into RAM
/// just to discover it's not text. opencode uses 4 KB; we go slightly
/// higher to catch text files with a small binary preamble (BOMs,
/// minified-JS source maps embedded as data: URIs, etc.) cleanly.
pub(crate) const SAMPLE_BYTES: usize = 8192;

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
            if v < 0.0 || v.is_nan() {
                return Err(de::Error::custom("negative or NaN value not allowed"));
            }
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

/// Strip a single trailing `{…}` group from a filename if it's a *clean*
/// trailing placeholder. Returns `Some(rest)` for `foo.md{}` /
/// `foo.md{path}` / `foo.md{0}`, `None` for filenames without trailing
/// braces, for placement other than the end (`{}foo.md`, `foo{}bar.md`),
/// and for nested / interleaved braces (`foo{a}b}`) which we can't safely
/// classify as a single template token.
///
/// Motivates the `{…}` strip-retry in `read_file`: the model occasionally
/// copies SLF4J / log4j format-string fragments (`log.info("Read {}",
/// path)` style) from the project's Java source into its `file_path`
/// argument verbatim, so `ISSUES-INDEX.md{}` arrives as the requested
/// path and every read errors NotFound. Detect that pattern, try the
/// stripped path, and prepend an inline note so the model corrects
/// future calls.
/// Recursive workspace search for a file by basename — the "did you mean"
/// fallback when a read targets a non-existent path. Uses blocking `read_dir`,
/// so callers run it inside `tokio::task::spawn_blocking` to keep cancellation
/// responsive on a hung filesystem. Was a nested fn inside `ReadTool::execute`;
/// hoisted to module scope so the blocking closure can reach it.
fn find_file_recursive(
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
            if name.starts_with('.') || name == "node_modules" || name == "target" || name == ".git"
            {
                continue;
            }
            let p = entry.path();
            if p.is_dir() {
                find_file_recursive(&p, target, depth + 1, max_depth, results);
            } else if name == target {
                results.push(p.to_string_lossy().to_string());
            }
        }
    }
}

fn strip_trailing_braces(s: &str) -> Option<&str> {
    let without_close = s.strip_suffix('}')?;
    let open = without_close.rfind('{')?;
    // Body between `{` and the suffix-`}` must not contain another brace:
    // an interleaved `{` or `}` makes this not a single clean trailing
    // group (e.g. `foo{a}b}` — trailing `}` exists but the prefix doesn't
    // end at a placeholder boundary).
    if without_close[open + 1..].contains(['{', '}']) {
        return None;
    }
    let stripped = &s[..open];
    // Refuse stripping to an empty filename — a raw `{}` filename is
    // pathological enough that we don't want to silently re-target the
    // operation to the parent directory.
    if stripped.is_empty() {
        None
    } else {
        Some(stripped)
    }
}

/// Path-aware wrapper around `strip_trailing_braces`: rebuilds the parent
/// + stripped basename. Returns `None` if there's no trailing placeholder,
/// no parent, or the basename isn't UTF-8 (rare on Windows but
/// non-stripping is the safe behaviour).
fn strip_trailing_placeholder(path: &std::path::Path) -> Option<std::path::PathBuf> {
    let parent = path.parent()?;
    let name = path.file_name()?.to_str()?;
    let stripped = strip_trailing_braces(name)?;
    Some(parent.join(stripped))
}

/// Prepend the auto-strip note (if present) to a body string. Kept as a
/// free function so each ToolResult return site is one line: every Ok
/// path in `execute` calls this before constructing the result so the
/// model sees the correction notice on success and on failure paths
/// (too-large / binary / invalid-offset) alike.
fn prepend_note(note: &Option<String>, body: String) -> String {
    match note {
        Some(n) => format!("{}\n{}", n, body),
        None => body,
    }
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
        let path0 = match super::inspect_path_access(&parsed.file_path, &working_dir) {
            Ok(access) => access.path,
            Err(err) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: err.to_string(),
                    success: false,
                });
            }
        };

        // A: `{…}` placeholder strip-retry. If the resolved path doesn't
        // exist but its filename ends in a `{…}` template fragment, try
        // the path with that fragment stripped. The model occasionally
        // copies SLF4J / log4j placeholder syntax (`log.info("Read {}",
        // p)`) from project source into its `file_path` arg verbatim,
        // sending paths like `…\\ISSUES-INDEX.md{}`. If the stripped
        // path exists, swap to it and record an inline note that gets
        // prepended to whatever result we eventually return — that way
        // the model both gets the file it actually wanted AND a
        // self-correcting hint for its next call.
        //
        // Guarded by the original path not existing, so files that
        // genuinely have a `{…}` suffix in their basename (vanishingly
        // rare on Windows; possible on POSIX with editor temp files)
        // continue to take the happy path unchanged.
        let (path, auto_strip_note): (std::path::PathBuf, Option<String>) = if path0.exists() {
            (path0, None)
        } else {
            match strip_trailing_placeholder(&path0) {
                Some(stripped) if stripped.exists() => {
                    let note = format!(
                        "[note: requested file_path ended with a `{{…}}` template fragment \
                         (likely an SLF4J / log4j placeholder copied from project source) — \
                         atomcode stripped it and read {} instead. Future read_file calls on \
                         this file should omit the `{{…}}` suffix.]",
                        stripped.display()
                    );
                    (stripped, Some(note))
                }
                _ => (path0, None),
            }
        };
        let path_ref = path.as_path();

        // ── Read cache: pure performance optimization ──
        // Cache stores (mtime, rendered_output). If mtime matches the
        // current disk state, return the cached output directly —
        // saves UTF-8 decode + tree-sitter cost on identical re-reads.
        // No model-visible meta-commentary on cache hits: the cached
        // bytes are returned silently, same way Claude Code's Read
        // tool replays content. Aligns with the "framework doesn't
        // educate the model about its own behaviour" principle.
        let cache_key: crate::tool::ReadCacheKey = (path.clone(), parsed.offset, parsed.limit);
        let disk_meta = tokio::fs::metadata(&path).await.ok();
        let disk_mtime = disk_meta.as_ref().and_then(|m| m.modified().ok());
        let disk_len = disk_meta.as_ref().map(|m| m.len());
        if let Some(mtime) = disk_mtime {
            let cached = ctx.read_cache.read().await.get(&cache_key).cloned();
            if let Some((cached_mtime, cached_output, _)) = cached {
                if cached_mtime == mtime {
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: prepend_note(&auto_strip_note, cached_output),
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
                output: prepend_note(
                    &auto_strip_note,
                    format!(
                        "[NOTE: {} is a directory, not a file. Here are its contents:]\n{}",
                        parsed.file_path,
                        entries.join("\n")
                    ),
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
                // The recursive `read_dir` walk blocks; run it on the blocking
                // pool so a hung filesystem can't stall cancellation. (Helper
                // `find_file_recursive` lives at module scope.) The trailing
                // `{…}`-placeholder variant is searched too, so a model that
                // copied an SLF4J-style `{}` suffix still gets the real file
                // surfaced in "Did you mean".
                let scan_wd = working_dir.clone();
                let scan_name = filename.clone();
                matches = tokio::task::spawn_blocking(move || {
                    let mut found: Vec<String> = Vec::new();
                    find_file_recursive(&scan_wd, &scan_name, 0, 7, &mut found);
                    if let Some(stripped) = strip_trailing_braces(&scan_name) {
                        if !stripped.is_empty() && stripped != scan_name {
                            find_file_recursive(&scan_wd, stripped, 0, 7, &mut found);
                        }
                    }
                    found
                })
                .await
                .unwrap_or_default();
                // Dedup (the same path could surface from both filename
                // variants when the workspace contains both naming
                // conventions, or when A's retry already pre-checked
                // the exact-parent stripped path).
                matches.sort();
                matches.dedup();
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

        // D3 (merged): consult FileStore before reading disk. If we've
        // read this path before AND mtime hasn't moved, every range
        // read of any subsequent offset/limit can be served from
        // memory. The previous design exposed this as a separate
        // `peek_file` tool, but the model often defaulted to
        // read_file anyway (datalog 2026-05-06_15-33-23: 13 peeks vs
        // 59 reads — 18% adoption). Routing the cache hit through
        // read_file's own path makes the optimisation transparent and
        // tool-surface-neutral: the model has one tool, the framework
        // decides disk vs cache.
        let store_hit: Option<String> = if let Some(mtime) = disk_mtime {
            let store = ctx.file_store.read().await;
            store
                .store_id_for_path(&path)
                .map(|s| s.to_string())
                .and_then(|id| store.get(&id).cloned())
                .filter(|entry| entry.mtime == mtime)
                .map(|entry| entry.content)
        } else {
            None
        };
        let served_from_store = store_hit.is_some();

        let content = if let Some(c) = store_hit {
            // Store entries only ever hold text (we never push binary
            // bytes), so we can short-circuit the UTF-8 / GBK decode
            // dance. mtime check above guarantees the content matches
            // what's currently on disk.
            c
        } else {
            // Sniff first SAMPLE_BYTES for a binary verdict BEFORE slurping
            // the whole file. Catches the "model reads a 10 GB .tar.gz / .exe
            // / .mp4 and OOMs" case at near-zero cost.
            let sample = read_file_head(&path, SAMPLE_BYTES)
                .await
                .with_context(|| format!("Failed to read {}", path.display()))?;
            if is_binary_sample(path_ref, &sample) {
                let size_for_msg = disk_len.unwrap_or(sample.len() as u64);
                let output = format!(
                    "Binary file ({} bytes), cannot display as text.{}",
                    size_for_msg,
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
                    output: prepend_note(&auto_strip_note, output),
                    success: true,
                });
            }

            // Size-aware dispatch. With sniff done, we know it's text;
            // metadata gives the size. Three large-file outcomes:
            //   - slice  + large   → stream (read.rs #2)
            //   - full   + large   → refuse with offset/limit & bash hints
            //                        (this block; read.rs #3)
            //   - either + small   → fall through to slurp + cache + skeleton
            //                        path below (unchanged)
            // Refusing early protects RAM: the model can't usefully consume
            // a 50 MB file (≈ context budget × 50) anyway, and `tokio::fs::read`
            // would lock that much RAM until the response is rendered AND the
            // FileStore caches another copy indefinitely.
            let is_slice = parsed.offset.is_some() || parsed.limit.is_some();
            let is_large = disk_len.map_or(false, |l| l > MAX_FULL_BYTES);
            if is_large && !is_slice {
                let n = disk_len.unwrap_or(0);
                let q = shell_quote(&parsed.file_path);
                // Rough offset estimate for "tail-ish" suggestion: assume
                // ~100 bytes/line average. This is a hint, not a contract —
                // model is expected to use `wc -l` if it needs exact.
                let tail_offset = (n / 100).saturating_sub(200).max(1);
                let output = format!(
                    "File too large to read in full: {n} bytes ({mb:.1} MB). \
                     read_file caps full reads at {cap_mb} MB so the response \
                     stays inside the model's context budget — anything larger \
                     wouldn't fit anyway, and slurping it would pin RAM in the \
                     FileStore cache long after the call returned.\n\n\
                     Read a slice instead (streamed, O(limit) memory):\n  \
                     read_file({{\"file_path\":\"{path}\",\"offset\":1,\"limit\":200}})       # head\n  \
                     read_file({{\"file_path\":\"{path}\",\"offset\":{tail_offset},\"limit\":200}})  # rough tail\n\n\
                     Or use bash for log / grep workflows:\n  \
                     wc -l {q}                      # exact line count first\n  \
                     head -n 200 {q}\n  \
                     tail -n 200 {q}\n  \
                     sed -n '1000,1500p' {q}        # specific line range\n  \
                     grep -n 'pattern' {q} | head -50",
                    n = n,
                    mb = n as f64 / 1_048_576.0,
                    cap_mb = MAX_FULL_BYTES / 1_048_576,
                    path = parsed.file_path,
                    tail_offset = tail_offset,
                    q = q,
                );
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: prepend_note(&auto_strip_note, output),
                    success: false,
                });
            }
            if is_slice && is_large {
                if !looks_utf8(&sample) {
                    // GB18030 / Latin-1 stream isn't supported by line-by-line
                    // reader. Slurp+decode would defeat the OOM protection on
                    // a 10 GB file. Hand the model the bash escape hatch.
                    let start = parsed.offset.unwrap_or(1).max(1);
                    let take = parsed.limit.unwrap_or(DEFAULT_READ_LIMIT);
                    let end = start.saturating_add(take).saturating_sub(1);
                    let output = format!(
                        "File too large to slurp ({} bytes) and not UTF-8 — \
                         streaming slice reader only supports UTF-8 input.\n\n\
                         Decode + slice via bash:\n  \
                         iconv -f GBK -t UTF-8 {q} | sed -n '{start},{end}p'\n  \
                         # detect encoding first if unsure: `chardetect {q}` or `file -i {q}`",
                        disk_len.unwrap_or(0),
                        q = shell_quote(&parsed.file_path),
                        start = start,
                        end = end,
                    );
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: prepend_note(&auto_strip_note, output),
                        success: false,
                    });
                }
                let slice = stream_slice_lines(&path, parsed.offset, parsed.limit)
                    .await
                    .with_context(|| format!("Failed to stream-read {}", path.display()))?;
                let output = format_streamed_slice(&parsed.file_path, &slice);
                if let Some(mtime) = disk_mtime {
                    ctx.read_cache
                        .write()
                        .await
                        .insert(cache_key.clone(), (mtime, output.clone(), 1));
                }
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: prepend_note(&auto_strip_note, output),
                    success: true,
                });
            }

            let bytes = tokio::fs::read(&path)
                .await
                .with_context(|| format!("Failed to read {}", path.display()))?;

            // Decode: UTF-8 first (vast majority of text), then GBK fallback
            // for plain-text extensions (Chinese Windows legacy `.txt`),
            // then declare binary. Consume `bytes` via `String::from_utf8`
            // to avoid the prior `.clone()` (which doubled peak memory for
            // large files); on Err recover the original `Vec` via
            // `e.into_bytes()` so the GBK and binary paths still see the
            // raw content without ever holding two copies.
            match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(e) => {
                    let bytes = e.into_bytes();
                    match decode_non_utf8_text(path_ref, &bytes) {
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
                                output: prepend_note(&auto_strip_note, output),
                                success: true,
                            });
                        }
                    }
                }
            }
        };

        // Push fresh disk content into the FileStore exactly once,
        // upstream of every output-shaping branch (skeleton / D3a /
        // range-slice). Subsequent reads of the same path at any range
        // hit the store path above (`store_hit`) and skip disk
        // entirely. Idempotent: re-reading after an edit pushes the
        // new content under the same path key, replacing the prior
        // entry. Skipped when we just served from store — content is
        // already there.
        if !served_from_store {
            if let Some(mtime) = disk_mtime {
                ctx.file_store
                    .write()
                    .await
                    .insert(path.clone(), content.clone(), mtime);
            }
        }

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
            // The upstream `served_from_store ? skip : push` block
            // already populated FileStore with the raw content; this
            // skeleton path does NOT need its own push. Subsequent
            // range reads of this file hit FileStore transparently
            // via the upstream `store_hit` branch — no model-visible
            // metadata in the result.
            if let Some(mtime) = disk_mtime {
                ctx.read_cache
                    .write()
                    .await
                    .insert(cache_key.clone(), (mtime, skeleton.clone(), 1));
            }
            return Ok(ToolResult {
                call_id: String::new(),
                output: prepend_note(&auto_strip_note, skeleton),
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

        // After the merge of peek_file → read_file, the previous
        // "pointer + preview" branch (LARGE_FILE_LINE_THRESHOLD) is
        // gone. Range reads are served from FileStore transparently
        // via the upstream `store_hit` check, so there's no separate
        // store-id pointer the model needs to track. The renderer
        // just emits full inline content (skeleton already handled
        // very-large files above).
        if let Some(mtime) = disk_mtime {
            ctx.read_cache
                .write()
                .await
                .insert(cache_key, (mtime, output.clone(), 1));
        }
        Ok(ToolResult {
            call_id: String::new(),
            output: prepend_note(&auto_strip_note, output),
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

/// Read up to `max` bytes from the start of `path`. Returns whatever
/// the first `read()` produced — used purely as a binary-sniff probe,
/// so partial reads (large pipe-buffer reads only refilled on next
/// poll) are fine.
async fn read_file_head(path: &std::path::Path, max: usize) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut file = tokio::fs::File::open(path).await?;
    let mut buf = vec![0u8; max];
    let n = file.read(&mut buf).await?;
    buf.truncate(n);
    Ok(buf)
}

/// Result of `stream_slice_lines`: the captured lines plus enough
/// metadata to render the truncation hints that tell the model how
/// to continue reading.
struct SliceResult {
    /// 1-indexed file line of `lines[0]` (= the `offset` we honored).
    start_line: usize,
    /// The captured lines, post per-line truncation.
    lines: Vec<String>,
    /// True if we stopped before EOF (limit hit or byte budget hit).
    more: bool,
    /// True specifically if the byte budget (`MAX_BYTES_PER_RESPONSE`)
    /// hit — separate from `more` so the message says "Output capped"
    /// vs "Showing lines …" so the model can distinguish "ask for the
    /// next page" from "this single page is huge, narrow your query".
    cut: bool,
    /// Total line count, available only if we read to EOF. None when
    /// we stopped early — counting to EOF would defeat the point of
    /// streaming a 10 GB log.
    total_lines: Option<usize>,
}

/// True iff a byte sample is consistent with UTF-8: either fully valid,
/// or invalid only because the trailing codepoint was cut mid-bytes by
/// the sample boundary. Used to gate the streaming slice path so we
/// don't UTF-8-decode a GB18030 / Latin-1 file line-by-line and produce
/// mojibake — those files go through the slurp+decode_non_utf8_text path.
fn looks_utf8(sample: &[u8]) -> bool {
    match std::str::from_utf8(sample) {
        Ok(_) => true,
        Err(e) => e.error_len().is_none(),
    }
}

/// Read a byte-budgeted slice of `path` line by line, skipping the
/// first `offset-1` lines and taking up to `limit` lines (defaulting
/// to `DEFAULT_READ_LIMIT`). Stops early when the accumulated UTF-8
/// byte size exceeds `MAX_BYTES_PER_RESPONSE`. Memory is bounded by
/// `limit * MAX_LINE_LENGTH`, independent of file size — that's the
/// point of this path vs the slurp path.
///
/// Per-line truncation at `MAX_LINE_LENGTH` chars protects against
/// minified-source-on-one-line pathology. The truncation suffix
/// `... (line truncated to N chars)` mirrors opencode so the model
/// can see what happened without inspecting metadata.
async fn stream_slice_lines(
    path: &std::path::Path,
    offset: Option<usize>,
    limit: Option<usize>,
) -> std::io::Result<SliceResult> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let file = tokio::fs::File::open(path).await?;
    let mut lines_iter = BufReader::new(file).lines();

    let start = offset.unwrap_or(1).max(1);
    let take = limit.unwrap_or(DEFAULT_READ_LIMIT);

    let mut lines: Vec<String> = Vec::with_capacity(take.min(2048));
    let mut bytes_acc: usize = 0;
    let mut current: usize = 1;
    let mut more = false;
    let mut cut = false;
    let mut total_lines: Option<usize> = None;

    loop {
        match lines_iter.next_line().await? {
            Some(text) => {
                if current < start {
                    current += 1;
                    continue;
                }
                if lines.len() >= take {
                    more = true;
                    break;
                }
                // Per-line truncation. char-counted (UTF-8 aware) so
                // CJK content isn't cut at half a codepoint.
                let line = if text.chars().count() > MAX_LINE_LENGTH {
                    let truncated: String = text.chars().take(MAX_LINE_LENGTH).collect();
                    format!(
                        "{}... (line truncated to {} chars)",
                        truncated, MAX_LINE_LENGTH
                    )
                } else {
                    text
                };
                // +1 for the joining `\n` we'd insert when rendering;
                // matches opencode's accounting.
                let line_bytes = line.len().saturating_add(1);
                if !lines.is_empty() && bytes_acc + line_bytes > MAX_BYTES_PER_RESPONSE {
                    cut = true;
                    more = true;
                    break;
                }
                bytes_acc += line_bytes;
                lines.push(line);
                current += 1;
            }
            None => {
                // Reached EOF cleanly — record total line count.
                // `current` points to the next line index after the
                // last consumed one, so total = current - 1.
                total_lines = Some(current.saturating_sub(1));
                break;
            }
        }
    }

    Ok(SliceResult {
        start_line: start,
        lines,
        more,
        cut,
        total_lines,
    })
}

/// Render a `SliceResult` into the same `   N| <content>` body the
/// slurp path emits, plus a trailing footer that tells the model what
/// to do next (continue with offset=N+1, narrow the query if cut, or
/// stop because we hit EOF). Mirrors the slurp path's "Showing lines
/// X-Y of total" footer where possible so the model sees consistent
/// truncation language across slurp and stream paths.
fn format_streamed_slice(file_path_for_msg: &str, slice: &SliceResult) -> String {
    let mut output = String::new();
    for (i, line) in slice.lines.iter().enumerate() {
        if i > 0 {
            output.push('\n');
        }
        let line_num = slice.start_line + i;
        output.push_str(&format!("{:>4}| {}", line_num, line));
    }
    if slice.lines.is_empty() {
        // Caller asked for an offset past EOF, or limit=0. Be explicit
        // — silent empty body would otherwise look like a tool failure.
        output.push_str(&format!(
            "[no lines in requested range (start={}, count=0)]",
            slice.start_line
        ));
        return output;
    }
    let last = slice.start_line + slice.lines.len() - 1;
    let next = last + 1;
    if slice.cut {
        output.push_str(&format!(
            "\n\n[Output capped at {} KB. Streamed lines {}-{}. Use offset={} to continue.]",
            MAX_BYTES_PER_RESPONSE / 1024,
            slice.start_line,
            last,
            next,
        ));
    } else if slice.more {
        output.push_str(&format!(
            "\n\n[Streamed lines {}-{}. Use offset={} to continue. \
             Total line count unknown — file is large; run `wc -l {q}` to count if needed.]",
            slice.start_line,
            last,
            next,
            q = shell_quote(file_path_for_msg),
        ));
    } else if let Some(total) = slice.total_lines {
        output.push_str(&format!(
            "\n\n[Streamed lines {}-{} of {} total (EOF).]",
            slice.start_line, last, total,
        ));
    }
    output
}

/// Decide whether the file is binary by looking at extension + a small
/// leading sample. Two-pronged because each prong has blind spots:
///   - extension alone misses files saved without one (raw `.bin`)
///   - content alone misses zip-based formats (.docx / .xlsx) whose
///     first KB looks like plain ASCII (PK signature + filenames)
///
/// Sample heuristic: any NUL byte is an instant binary verdict
/// (UTF-8 text never contains `\0`); otherwise count non-printable
/// control chars (anything below 32 except TAB/LF/FF/CR) and call it
/// binary if their share exceeds 30%. Mirrors opencode's check, plus
/// the NUL-shortcut from `file(1)`'s heuristic.
fn is_binary_sample(path: &std::path::Path, sample: &[u8]) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    const BINARY_EXTENSIONS: &[&str] = &[
        // archives & compressed
        "zip", "tar", "gz", "tgz", "bz2", "tbz2", "xz", "txz", "7z", "rar", "zst",
        // executables & objects
        "exe", "dll", "so", "dylib", "a", "lib", "o", "obj", "wasm",
        // jvm / .net / python compiled
        "class", "jar", "war", "pyc", "pyo",
        // office (binary or zip-based — handled via recovery_hint downstream)
        "doc", "docx", "xls", "xlsx", "ppt", "pptx", "odt", "ods", "odp", // images
        "png", "jpg", "jpeg", "gif", "bmp", "webp", "tiff", "tif", "ico", "heic", "heif", "avif",
        // audio / video
        "mp3", "mp4", "wav", "flac", "ogg", "opus", "m4a", "aac", "avi", "mov", "mkv", "webm",
        "wmv", "flv", "mpg", "mpeg", // raw binary blobs
        "bin", "dat", "iso", "img", "dmg", // databases
        "db", "sqlite", "sqlite3", // PDF (handled via recovery_hint)
        "pdf",
    ];
    if BINARY_EXTENSIONS.contains(&ext.as_str()) {
        return true;
    }

    if sample.is_empty() {
        return false;
    }

    let mut non_printable: usize = 0;
    for &b in sample {
        if b == 0 {
            // NUL byte: text files never contain this. file(1) uses
            // the same shortcut. One sighting is enough.
            return true;
        }
        // Allow TAB (9), LF (10), FF (12), CR (13). Everything below
        // 32 outside that set is non-printable.
        if b < 9 || b == 11 || (b > 13 && b < 32) {
            non_printable += 1;
        }
    }
    non_printable * 10 > sample.len() * 3 // >30% non-printable
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
        let args = serde_json::json!({
            "file_path": path.to_string_lossy().to_string()
        })
        .to_string();

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

    /// 2nd+ identical read returns the cached output silently — no
    /// model-visible meta-commentary. Aligns with Claude Code's Read
    /// tool behaviour: cache is a performance optimisation, not a
    /// teaching tool. The "you've read this N times" preamble that
    /// the previous version prepended has been removed.
    #[tokio::test]
    async fn read_cache_hits_replay_silently() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn main() {}\n").unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = ReadFileTool;
        let args = format!(r#"{{"file_path":"{}"}}"#, path.display());

        let r1 = tool.execute(&args, &ctx).await.unwrap();
        let r2 = tool.execute(&args, &ctx).await.unwrap();
        let r3 = tool.execute(&args, &ctx).await.unwrap();
        assert!(r1.success && r2.success && r3.success);
        // No "you've read N times" preamble on any replay.
        for r in [&r2, &r3] {
            assert!(
                !r.output.contains("times this session"),
                "no meta-commentary on cache hits; got:\n{}",
                r.output
            );
        }
    }

    /// Cache miss after file content changes — mtime shifts, cached entry is ignored.
    #[tokio::test]
    async fn read_cache_misses_when_mtime_changes() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("b.rs");
        std::fs::write(&path, "fn main() {}\n").unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = ReadFileTool;
        let args = serde_json::json!({
            "file_path": path.to_string_lossy().to_string()
        })
        .to_string();

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

    /// D3 SMOKE TEST: edit_file invalidates both read_cache (via mtime) and
    /// FileStore (via explicit invalidate). This is the load-bearing assumption
    /// for Task 1 of plans/2026-05-07-readfile-skip-and-edit-verify.md — if
    /// this test fails, weak models will read stale post-edit content and
    /// the read_file-exempt-from-collapse_committed strategy collapses.
    ///
    /// Sequence: write A → read (populates caches) → edit A→B → read again →
    /// must observe B, not cached A.
    #[tokio::test]
    async fn d3_edit_invalidates_caches_for_subsequent_read() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("payload.rs");
        std::fs::write(&path, "fn before() {}\n").unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let read_tool = ReadFileTool;
        let edit_tool = crate::tool::edit::EditFileTool;
        let read_args = format!(r#"{{"file_path":"{}"}}"#, path.display());

        // Step 1: initial read populates read_cache and FileStore.
        let r1 = read_tool.execute(&read_args, &ctx).await.unwrap();
        assert!(r1.output.contains("fn before"));
        assert_eq!(
            ctx.file_store.read().await.len(),
            1,
            "FileStore should have 1 entry after read"
        );
        assert_eq!(
            ctx.read_cache.read().await.len(),
            1,
            "read_cache should have 1 entry after read"
        );

        // NO SLEEP: deliberately worst-case. On filesystems with coarse mtime
        // granularity (ext4 sec-precision), the post-edit mtime may equal the
        // pre-edit mtime, defeating the read_cache mtime gate. Then the only
        // line of defense is the explicit `invalidate(canon_path)` in edit.rs.
        // If this test passes without sleeping, both layers are working.

        // Step 2: edit_file replaces "before" with "after".
        let edit_args = format!(
            r#"{{"file_path":"{}","old_string":"fn before() {{}}","new_string":"fn after() {{ /* edited */ }}"}}"#,
            path.display()
        );
        let e = edit_tool.execute(&edit_args, &ctx).await.unwrap();
        assert!(e.success, "edit should succeed; got: {}", e.output);

        // Sanity: disk now holds B.
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(
            on_disk.contains("fn after"),
            "disk content not updated: {}",
            on_disk
        );

        // FileStore should be invalidated for this path. (Either entry gone,
        // or replaced with new content. Both are correct outcomes.)
        let fs_state_after_edit = {
            let store = ctx.file_store.read().await;
            store
                .store_id_for_path(&path)
                .and_then(|id| store.get(id).cloned())
                .map(|e| e.content)
        };
        if let Some(content) = &fs_state_after_edit {
            assert!(
                content.contains("fn after"),
                "FileStore retained pre-edit content: {}",
                content
            );
        }
        // (If None, that's even better — fully invalidated.)

        // Defense-layer probe (BEFORE the second read): both caches are
        // now explicitly purged by edit.rs.
        //
        // FileStore: explicitly invalidated by edit.rs — entry gone OR
        //   overwritten with new content (already asserted above).
        // read_cache: explicitly purged by edit.rs (defense-in-depth for
        //   FS with coarse mtime granularity where the mtime gate alone
        //   could fail). Map should hold no entries for this path.
        let read_cache_post_edit = ctx.read_cache.read().await.clone();
        let stale_cache_for_path = read_cache_post_edit
            .keys()
            .filter(|(p, _, _)| p == &path)
            .count();
        assert_eq!(
            stale_cache_for_path, 0,
            "read_cache must be purged for edited path; lingering entries \
             would let coarse-mtime FS serve stale content"
        );

        // Step 3: re-read must surface B, NOT cached A.
        let r2 = read_tool.execute(&read_args, &ctx).await.unwrap();
        assert!(
            r2.output.contains("fn after"),
            "POST-EDIT READ SERVED STALE CONTENT: {}",
            r2.output
        );
        assert!(
            !r2.output.contains("fn before"),
            "post-edit read still mentions pre-edit symbol: {}",
            r2.output
        );
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
        let args = serde_json::json!({
            "file_path": path.to_string_lossy().to_string()
        })
        .to_string();

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
        let args = serde_json::json!({
            "file_path": path.to_string_lossy().to_string()
        })
        .to_string();

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
        let args = serde_json::json!({
            "file_path": path.to_string_lossy().to_string()
        })
        .to_string();

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

    /// Every fresh disk read pushes its content into FileStore so
    /// subsequent reads of any range hit the in-memory snapshot
    /// instead of touching disk again. After the peek_file → read_file
    /// merge, the model never sees a store_id — the cache is purely
    /// internal.
    #[tokio::test]
    async fn d3_full_read_pushes_to_store_returns_inline_content() {
        let dir = TempDir::new().unwrap();
        let path = write_n_line_file(&dir, "big.rs", 200);
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let args = format!(r#"{{"file_path":"{}"}}"#, path.display());
        let r = ReadFileTool.execute(&args, &ctx).await.unwrap();
        assert!(r.success);
        // No more pointer/preview formatting — model gets the content
        // directly. store_id is internal-only after the merge.
        assert!(
            !r.output.contains("store_id="),
            "store_id must NOT leak into model output:\n{}",
            r.output
        );
        assert!(
            !r.output.contains("peek_file"),
            "peek_file no longer exists, must not be referenced:\n{}",
            r.output
        );
        // Full content is inline.
        assert!(r.output.contains("line 1"));
        assert!(r.output.contains("line 100"));
        assert!(r.output.contains("line 200"));
        // Store populated for future range reads.
        assert_eq!(ctx.file_store.read().await.len(), 1);
    }

    /// Small files also populate the store — uniform behaviour means
    /// the "Nth read" hint can fire on any file, and a future range
    /// read of a small file (rare but possible) hits cache too.
    #[tokio::test]
    async fn d3_small_file_pushes_to_store_after_merge() {
        let dir = TempDir::new().unwrap();
        let path = write_n_line_file(&dir, "small.rs", 10);
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let args = format!(r#"{{"file_path":"{}"}}"#, path.display());
        let r = ReadFileTool.execute(&args, &ctx).await.unwrap();
        assert!(r.success);
        assert_eq!(
            ctx.file_store.read().await.len(),
            1,
            "fresh disk read must populate store regardless of file size"
        );
    }

    /// THE merge's core promise: a range read after a full read of
    /// the same path is served from FileStore (no disk hit). After
    /// the CC-alignment cleanup the store-served path is silent —
    /// the model gets the requested range with no model-visible
    /// metadata about cache origin. Test pins behaviour by checking
    /// (a) the requested lines are returned, (b) no leaked
    /// "FileStore" / "cache" preamble appears, and (c) the store
    /// still has the entry (so we know the cache was actually used).
    #[tokio::test]
    async fn d3_range_read_after_full_read_silently_serves_from_store() {
        let dir = TempDir::new().unwrap();
        let path = write_n_line_file(&dir, "big.rs", 200);
        let ctx = ToolContext::new(dir.path().to_path_buf());

        let full_args = format!(r#"{{"file_path":"{}"}}"#, path.display());
        let _ = ReadFileTool.execute(&full_args, &ctx).await.unwrap();

        let range_args = format!(
            r#"{{"file_path":"{}","offset":100,"limit":5}}"#,
            path.display()
        );
        let r = ReadFileTool.execute(&range_args, &ctx).await.unwrap();
        assert!(r.success);
        assert!(r.output.contains("line 100"));
        assert!(
            !r.output.contains("FileStore"),
            "store-served read must NOT leak any FileStore preamble:\n{}",
            r.output
        );
        assert_eq!(
            ctx.file_store.read().await.len(),
            1,
            "FileStore must retain the entry across both reads"
        );
    }

    /// Edit invalidates the cache so the next read sees fresh disk
    /// content, not a stale snapshot. Without this, the model would
    /// reason against bytes that no longer match what's on disk after
    /// its own edit.
    #[tokio::test]
    async fn d3_edit_invalidates_cache_next_read_hits_disk() {
        let dir = TempDir::new().unwrap();
        let path = write_n_line_file(&dir, "big.rs", 200);
        let ctx = ToolContext::new(dir.path().to_path_buf());

        let read_args = format!(r#"{{"file_path":"{}"}}"#, path.display());
        let _ = ReadFileTool.execute(&read_args, &ctx).await.unwrap();
        assert_eq!(ctx.file_store.read().await.len(), 1);

        let edit_args = format!(
            r#"{{"file_path":"{}","old_string":"line 1\n","new_string":"LINE 1\n"}}"#,
            path.display()
        );
        let e = crate::tool::edit::EditFileTool
            .execute(&edit_args, &ctx)
            .await
            .unwrap();
        assert!(e.success, "edit must succeed:\n{}", e.output);
        assert_eq!(
            ctx.file_store.read().await.len(),
            0,
            "edit must invalidate the store entry"
        );

        // Range read after edit: store was invalidated, so this is a
        // fresh disk read. Output must NOT carry the cache notice and
        // store gets repopulated.
        let range_args = format!(
            r#"{{"file_path":"{}","offset":1,"limit":3}}"#,
            path.display()
        );
        let r = ReadFileTool.execute(&range_args, &ctx).await.unwrap();
        assert!(r.success);
        assert!(
            !r.output.contains("FileStore cache"),
            "post-edit read must come from disk, not stale cache:\n{}",
            r.output
        );
        assert_eq!(ctx.file_store.read().await.len(), 1);
    }

    /// Re-reading the same path with the same args keeps store size
    /// at 1 — the entry is replaced, not duplicated. Guards against
    /// a regression where every call grew the store unboundedly.
    #[tokio::test]
    async fn d3_reread_unchanged_file_keeps_one_entry() {
        let dir = TempDir::new().unwrap();
        let path = write_n_line_file(&dir, "big.rs", 200);
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let args = format!(r#"{{"file_path":"{}"}}"#, path.display());
        let _ = ReadFileTool.execute(&args, &ctx).await.unwrap();
        let _ = ReadFileTool.execute(&args, &ctx).await.unwrap();
        assert_eq!(ctx.file_store.read().await.len(), 1);
    }

    /// Auto-skeleton path (>300 lines) populates the store too — fix
    /// for the early-return bug that left huge files completely
    /// outside the cache (datalog 2026-05-06_14-29-08: 19 reads of a
    /// single 753-line file, zero cache hits, before this guard).
    #[tokio::test]
    async fn d3_skeleton_path_pushes_to_store() {
        let dir = TempDir::new().unwrap();
        let path = write_n_line_file(&dir, "huge.rs", 350);
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let args = format!(r#"{{"file_path":"{}"}}"#, path.display());
        let r = ReadFileTool.execute(&args, &ctx).await.unwrap();
        assert!(r.success);
        assert!(
            r.output.contains("File skeleton:"),
            "huge file should still get skeleton:\n{}",
            r.output
        );
        // Skeleton path used to expose store_id; merge removed that.
        // The store is populated invisibly so future range reads can
        // hit cache.
        assert!(
            !r.output.contains("store_id="),
            "merged design hides store_id from model:\n{}",
            r.output
        );
        assert_eq!(
            ctx.file_store.read().await.len(),
            1,
            "auto_skeleton path must populate FileStore"
        );
    }

    /// After CC alignment: subsequent reads of the same path do NOT
    /// surface any framework-side "Nth read of X" preamble. The
    /// model gets the same shape of output every time. Pins the
    /// removal of the R2 hint that earlier datalogs (2026-05-06)
    /// showed glm-5.1 ignoring anyway — keeping it was both
    /// hardcoded metadata-injection and ineffective.
    #[tokio::test]
    async fn d3_subsequent_reads_have_no_framework_preamble() {
        let dir = TempDir::new().unwrap();
        let path = write_n_line_file(&dir, "big.rs", 200);
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let args1 = format!(r#"{{"file_path":"{}"}}"#, path.display());
        let args2 = format!(
            r#"{{"file_path":"{}","offset":50,"limit":10}}"#,
            path.display()
        );
        let args3 = format!(
            r#"{{"file_path":"{}","offset":100,"limit":10}}"#,
            path.display()
        );
        let r1 = ReadFileTool.execute(&args1, &ctx).await.unwrap();
        let r2 = ReadFileTool.execute(&args2, &ctx).await.unwrap();
        let r3 = ReadFileTool.execute(&args3, &ctx).await.unwrap();
        assert!(r1.success && r2.success && r3.success);
        for (i, r) in [&r1, &r2, &r3].iter().enumerate() {
            assert!(
                !r.output.contains("read of `") && !r.output.contains("FileStore cache"),
                "read #{} must not carry framework metadata; got:\n{}",
                i + 1,
                r.output
            );
        }
    }

    // --- Pre-slurp binary detection regression tests ---
    //
    // Before this layer, `read_file` would happily `tokio::fs::read()` a
    // 10 GB tarball into RAM, then `bytes.clone()` for UTF-8 validation,
    // peak ~20 GB and OOM. Now `is_binary_sample` rejects the file from
    // the first 8 KB and the full slurp never happens.

    #[test]
    fn binary_sample_nul_byte_short_circuits() {
        let p = std::path::Path::new("/tmp/no-such.txt");
        assert!(is_binary_sample(p, b"hello\0world"), "single NUL = binary");
    }

    #[test]
    fn binary_sample_extension_blacklist_triggers_on_empty() {
        // .tar.gz / .exe etc: extension is enough — sample can be empty.
        // Critical for the OOM case: we never even read the 10 GB tarball.
        let p = std::path::Path::new("foo.tar.gz");
        assert!(is_binary_sample(p, &[]), "extension alone should be enough");
        assert!(is_binary_sample(std::path::Path::new("foo.exe"), &[]));
        assert!(is_binary_sample(std::path::Path::new("foo.png"), &[]));
        assert!(is_binary_sample(std::path::Path::new("foo.mp4"), &[]));
    }

    #[test]
    fn binary_sample_plain_text_passes() {
        let p = std::path::Path::new("a.rs");
        assert!(!is_binary_sample(p, b"fn main() {}\n"));
        // Chinese UTF-8 is multi-byte but all bytes are >= 0x80 (continuation
        // / start), none below 32 — must NOT be flagged as binary.
        assert!(!is_binary_sample(p, "你好世界\n".as_bytes()));
        // Source code with tabs/newlines/CRLF: ASCII printable + whitelisted controls.
        let mixed = b"line1\n\tindented\r\nline3\n";
        assert!(!is_binary_sample(p, mixed));
    }

    #[test]
    fn binary_sample_high_nonprintable_ratio_flags() {
        let p = std::path::Path::new("unknown");
        // >30% bytes in (1..9) ∪ {11} ∪ (14..32) range.
        let mut bytes = vec![b'A'; 10];
        bytes.extend(std::iter::repeat(0x01u8).take(5)); // 5/15 = 33% non-printable
        assert!(is_binary_sample(p, &bytes));
    }

    /// End-to-end: a file with a binary extension + binary contents
    /// rejects via `read_file` without slurping. We don't write a
    /// huge file (slow on CI) — just assert the binary-error branch
    /// fires and includes byte count from metadata, not from a slurp.
    #[tokio::test]
    async fn read_file_rejects_binary_via_sample() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("blob.tar.gz");
        // Real-ish gzip magic: 1f 8b ... + some bytes
        let payload: Vec<u8> = b"\x1f\x8b\x08\x00\x00\x00\x00\x00\x00\x03"
            .iter()
            .chain(std::iter::repeat(&0u8).take(1024))
            .copied()
            .collect();
        std::fs::write(&path, &payload).unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = ReadFileTool;
        let args = format!(r#"{{"file_path":"{}"}}"#, path.display());
        let r = tool.execute(&args, &ctx).await.unwrap();
        assert!(r.success); // intentional: "successfully detected as binary"
        assert!(
            r.output.contains("Binary file"),
            "should be binary-rejected, got: {}",
            r.output
        );
        // Recovery hint matches the .tar.gz family (no specific handler →
        // generic). Critical assertion: the byte count came from metadata,
        // so it should equal the file size, not the 8 KB sample.
        let expected_len = std::fs::metadata(&path).unwrap().len();
        assert!(
            r.output.contains(&expected_len.to_string()),
            "binary error must report full file size from metadata ({}), got: {}",
            expected_len,
            r.output
        );
    }

    // --- Streaming slice path regression tests ---
    //
    // For large files with offset/limit, read_file now reads line-by-line
    // with a byte budget instead of slurping. These tests pin the unit-level
    // contract (stream_slice_lines) and the end-to-end integration (big
    // file + slice → streaming path triggered).

    #[tokio::test]
    async fn stream_slice_lines_skips_offset_then_takes_limit() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.txt");
        let body = (1..=20)
            .map(|n| format!("line{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, body).unwrap();

        let r = stream_slice_lines(&path, Some(5), Some(3)).await.unwrap();
        assert_eq!(r.start_line, 5);
        assert_eq!(r.lines, vec!["line5", "line6", "line7"]);
        assert!(r.more, "limit hit before EOF");
        assert!(!r.cut);
        assert!(r.total_lines.is_none(), "early stop must not claim a total");
    }

    #[tokio::test]
    async fn stream_slice_lines_reads_to_eof_reports_total() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "a\nb\nc\n").unwrap();

        let r = stream_slice_lines(&path, Some(1), Some(100)).await.unwrap();
        assert_eq!(r.lines, vec!["a", "b", "c"]);
        assert!(!r.more);
        assert_eq!(r.total_lines, Some(3));
    }

    #[tokio::test]
    async fn stream_slice_lines_truncates_overlong_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.txt");
        let long_line: String = "x".repeat(MAX_LINE_LENGTH + 500);
        std::fs::write(&path, &long_line).unwrap();

        let r = stream_slice_lines(&path, None, Some(10)).await.unwrap();
        assert_eq!(r.lines.len(), 1);
        assert!(
            r.lines[0].contains("line truncated to"),
            "expected truncation suffix: {}",
            &r.lines[0][..100.min(r.lines[0].len())]
        );
        // Should be MAX_LINE_LENGTH chars + the suffix, not the full 2500.
        assert!(r.lines[0].chars().count() < MAX_LINE_LENGTH + 100);
    }

    #[tokio::test]
    async fn stream_slice_lines_byte_budget_cuts_early() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.txt");
        // 1 KB lines × 1000 = 1 MB, way over MAX_BYTES_PER_RESPONSE (256 KB).
        // Streaming must stop short, marking cut=true.
        let line: String = "x".repeat(1024);
        let body = std::iter::repeat(line)
            .take(1000)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, body).unwrap();

        let r = stream_slice_lines(&path, Some(1), Some(1000))
            .await
            .unwrap();
        assert!(r.cut, "byte budget should fire before limit on 1 KB × 1000");
        assert!(r.more);
        assert!(
            r.lines.len() < 1000,
            "expected fewer than 1000 lines, got {}",
            r.lines.len()
        );
    }

    #[tokio::test]
    async fn stream_slice_lines_offset_past_eof_is_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "a\nb\nc\n").unwrap();

        let r = stream_slice_lines(&path, Some(100), Some(10))
            .await
            .unwrap();
        assert!(r.lines.is_empty());
        assert_eq!(r.total_lines, Some(3));
    }

    /// End-to-end: a 6 MB file with offset/limit must go through the
    /// streaming path (not the slurp). Verified by (a) the "Streamed
    /// lines" marker in the footer that only the stream path emits and
    /// (b) correct line content at the requested offset.
    #[tokio::test]
    async fn read_file_large_file_slice_streams_not_slurps() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("big.log");
        // 6 MB target — comfortably > MAX_FULL_BYTES (5 MB).
        // ~100-byte lines × 65 k lines ≈ 6.5 MB.
        let mut body = String::with_capacity(7 * 1024 * 1024);
        for i in 1..=65_000usize {
            body.push_str(&format!(
                "line {:0>6} padded with x's to about 100 chars: {}\n",
                i,
                "x".repeat(60)
            ));
        }
        std::fs::write(&path, &body).unwrap();
        assert!(
            std::fs::metadata(&path).unwrap().len() > MAX_FULL_BYTES,
            "test fixture must exceed MAX_FULL_BYTES",
        );

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = ReadFileTool;
        let args = format!(
            r#"{{"file_path":"{}","offset":100,"limit":5}}"#,
            path.display()
        );
        let r = tool.execute(&args, &ctx).await.unwrap();
        assert!(r.success, "{}", r.output);
        assert!(
            r.output.contains("Streamed lines"),
            "large slice must take the streaming path (footer marker absent); got:\n{}",
            &r.output[..r.output.len().min(500)],
        );
        // Sanity: requested offset visible, beyond-limit line not.
        assert!(
            r.output.contains(" 100| line 000100"),
            "offset line missing"
        );
        assert!(
            r.output.contains(" 104| line 000104"),
            "limit-1 line missing"
        );
        assert!(
            !r.output.contains(" 105| line 000105"),
            "limit must stop at 104, but 105 appeared",
        );
    }

    // --- Large-file full-read refusal regression tests (read.rs #3) ---
    //
    // For files > MAX_FULL_BYTES read WITHOUT offset/limit, slurping is
    // both wasteful (>200× the model's effective context budget) and
    // dangerous (FileStore would pin the same memory indefinitely).
    // We refuse early with a redirect message.

    /// 6 MB file + full read (no offset/limit) → refused with message
    /// pointing the model at slicing or bash. The slurp must never run:
    /// asserted by the size annotation in the refusal text matching the
    /// metadata len exactly (a slurp-then-decide path would carry the
    /// real bytes through).
    #[tokio::test]
    async fn read_file_full_read_refuses_when_above_max_full_bytes() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("big.log");
        let mut body = String::with_capacity(6 * 1024 * 1024);
        for i in 1..=60_000usize {
            body.push_str(&format!("line {:0>6}: {}\n", i, "x".repeat(80)));
        }
        std::fs::write(&path, &body).unwrap();
        let len = std::fs::metadata(&path).unwrap().len();
        assert!(len > MAX_FULL_BYTES, "fixture must exceed cap");

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = ReadFileTool;
        let args = format!(r#"{{"file_path":"{}"}}"#, path.display());
        let r = tool.execute(&args, &ctx).await.unwrap();
        assert!(!r.success, "full read of large file must be refused");
        assert!(
            r.output.contains("File too large to read in full"),
            "expected size refusal, got:\n{}",
            r.output
        );
        // Size header from metadata, not from a (non-existent) slurp.
        assert!(
            r.output.contains(&len.to_string()),
            "refusal must report metadata byte count {}, got:\n{}",
            len,
            r.output
        );
        // Concrete next-step suggestions to keep the model unstuck.
        assert!(r.output.contains("offset"), "missing offset suggestion");
        assert!(r.output.contains("head -n"), "missing bash head hint");
        assert!(r.output.contains("grep -n"), "missing grep hint");
    }

    /// Slice of the same large file is NOT refused — it goes through
    /// the streaming path from read.rs #2. Negative-control for the
    /// refusal above.
    #[tokio::test]
    async fn read_file_slice_of_large_file_is_not_refused() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("big.log");
        let mut body = String::with_capacity(6 * 1024 * 1024);
        for i in 1..=60_000usize {
            body.push_str(&format!("line {:0>6}: {}\n", i, "x".repeat(80)));
        }
        std::fs::write(&path, &body).unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = ReadFileTool;
        let args = format!(
            r#"{{"file_path":"{}","offset":50,"limit":3}}"#,
            path.display()
        );
        let r = tool.execute(&args, &ctx).await.unwrap();
        assert!(r.success, "{}", r.output);
        assert!(
            !r.output.contains("File too large to read in full"),
            "slice path must NOT trip the full-read refusal; got:\n{}",
            r.output
        );
        assert!(r.output.contains("  50| line 000050"));
    }

    /// Files at or below MAX_FULL_BYTES continue to work as full reads.
    /// Belt-and-suspenders: a too-tight threshold would silently break
    /// the dominant "read a normal source file" case.
    #[tokio::test]
    async fn read_file_full_read_under_cap_still_works() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("normal.rs");
        // 100 KB — well under 5 MB cap and well under skeleton threshold.
        let body: String = (1..=400)
            .map(|n| format!("fn f{n}() {{ println!(\"{n}\"); }}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, &body).unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = ReadFileTool;
        let args = format!(r#"{{"file_path":"{}"}}"#, path.display());
        let r = tool.execute(&args, &ctx).await.unwrap();
        assert!(r.success);
        assert!(!r.output.contains("File too large"));
        assert!(r.output.contains("fn f1()"));
    }

    /// Small files keep the slurp path so FileStore can amortize repeat
    /// slices — guard against an over-eager `is_slice && is_large` that
    /// would force small files through the (slightly slower per-call)
    /// streaming reader and lose the cache benefit.
    #[tokio::test]
    async fn read_file_small_file_slice_uses_slurp_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("small.txt");
        let body = (1..=50)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, body).unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = ReadFileTool;
        let args = format!(
            r#"{{"file_path":"{}","offset":10,"limit":3}}"#,
            path.display()
        );
        let r = tool.execute(&args, &ctx).await.unwrap();
        assert!(r.success);
        // Slurp path emits "Showing lines X-Y of Z total" — streaming
        // emits "Streamed lines …". Small file must stay on slurp.
        assert!(
            !r.output.contains("Streamed lines"),
            "small file slice must not be routed through streaming; got:\n{}",
            r.output
        );
        assert!(r.output.contains("  10| line 10"));
        assert!(r.output.contains("  12| line 12"));
    }

    /// Symmetric positive case: a real text file with the same byte
    /// count must NOT be falsely rejected. Guards against the binary
    /// detection getting tightened to the point that legitimate code
    /// files (e.g. Rust with `#![…]` byte-order-marky preamble) trip it.
    #[tokio::test]
    async fn read_file_accepts_normal_text_after_sniff() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("normal.rs");
        let mut content = String::from("// header\nfn main() {\n");
        for _ in 0..200 {
            content.push_str("    println!(\"x\");\n");
        }
        content.push_str("}\n");
        std::fs::write(&path, &content).unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = ReadFileTool;
        let args = format!(r#"{{"file_path":"{}"}}"#, path.display());
        let r = tool.execute(&args, &ctx).await.unwrap();
        assert!(r.success);
        assert!(
            !r.output.contains("Binary file"),
            "normal Rust file must not be flagged binary, got: {}",
            r.output
        );
    }

    // ─── A: `{…}` placeholder strip-retry ───
    //
    // The model sometimes copies SLF4J / log4j format-string placeholders
    // (`log.info("Read {}", path)`) from project source into its
    // `file_path` argument verbatim. Verify the read_file tool detects
    // the trailing `{…}` group, swaps to the stripped path when that
    // file actually exists, and prepends an inline note so the model
    // self-corrects on the next call.

    #[test]
    fn strip_trailing_braces_empty_braces() {
        assert_eq!(
            strip_trailing_braces("ISSUES-INDEX.md{}"),
            Some("ISSUES-INDEX.md")
        );
    }

    #[test]
    fn strip_trailing_braces_filled_braces() {
        assert_eq!(strip_trailing_braces("foo.md{path}"), Some("foo.md"));
        assert_eq!(strip_trailing_braces("foo.md{0}"), Some("foo.md"));
    }

    #[test]
    fn strip_trailing_braces_returns_none_without_trailing_close() {
        assert_eq!(strip_trailing_braces("foo.md"), None);
        assert_eq!(strip_trailing_braces("{}foo.md"), None);
        assert_eq!(strip_trailing_braces("foo{}bar.md"), None);
    }

    #[test]
    fn strip_trailing_braces_rejects_interleaved_braces() {
        // `foo{a}b}` ends with `}` but the body has another `}` — not a
        // single clean trailing placeholder, refuse to mangle.
        assert_eq!(strip_trailing_braces("foo{a}b}"), None);
    }

    #[test]
    fn strip_trailing_braces_refuses_empty_stripped_result() {
        // Bare `{}` filename: stripping yields "" which would re-target
        // the read to the parent directory. Conservative refuse.
        assert_eq!(strip_trailing_braces("{}"), None);
    }

    #[test]
    fn strip_trailing_placeholder_path_aware() {
        use std::path::{Path, PathBuf};
        assert_eq!(
            strip_trailing_placeholder(Path::new("/tmp/foo.md{}")),
            Some(PathBuf::from("/tmp/foo.md"))
        );
        assert!(strip_trailing_placeholder(Path::new("/tmp/foo.md")).is_none());
    }

    /// End-to-end A: model sends `file.md{}`, real `file.md` exists →
    /// read_file silently corrects, returns the real content AND a note
    /// the model can use to fix its next call.
    #[tokio::test]
    async fn read_file_strips_trailing_placeholder_and_serves_real_content() {
        let dir = TempDir::new().unwrap();
        let real_path = dir.path().join("ISSUES-INDEX.md");
        std::fs::write(&real_path, "# Index\n\n- entry one\n- entry two\n").unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = ReadFileTool;
        // Mimic the model's bug: append `{}` to the real path.
        let args = format!(
            r#"{{"file_path":"{}{{}}"}}"#,
            real_path.display().to_string().replace('\\', "\\\\")
        );
        let r = tool.execute(&args, &ctx).await.unwrap();
        assert!(
            r.success,
            "should serve the stripped real file, got: {}",
            r.output
        );
        assert!(
            r.output.contains("entry one"),
            "stripped read should return real content, got: {}",
            r.output
        );
        assert!(
            r.output.contains("template fragment"),
            "model-facing correction note must mention the strip, got: {}",
            r.output
        );
    }

    /// Strip-retry must NOT fire when the original path exists — files
    /// that legitimately have a `{…}` suffix in their basename (rare on
    /// Windows, possible on POSIX) read normally with no note.
    #[tokio::test]
    async fn read_file_preserves_legitimate_brace_suffix_when_path_exists() {
        let dir = TempDir::new().unwrap();
        let real_path = dir.path().join("template.md{}");
        std::fs::write(&real_path, "real content\n").unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = ReadFileTool;
        let args = format!(
            r#"{{"file_path":"{}"}}"#,
            real_path.display().to_string().replace('\\', "\\\\")
        );
        let r = tool.execute(&args, &ctx).await.unwrap();
        assert!(r.success);
        assert!(r.output.contains("real content"));
        assert!(
            !r.output.contains("template fragment"),
            "no auto-strip note should fire when the brace-suffixed file exists",
        );
    }

    /// C: when the stripped path doesn't help (wrong base directory),
    /// the workspace-wide "Did you mean" search uses the stripped
    /// filename so the real file gets surfaced as a candidate even when
    /// it lives in a different directory.
    #[tokio::test]
    async fn read_file_did_you_mean_search_uses_stripped_basename() {
        let dir = TempDir::new().unwrap();
        // Real file lives in a subdirectory the model didn't guess.
        let subdir = dir.path().join("docs").join("ISSUES");
        std::fs::create_dir_all(&subdir).unwrap();
        let real_path = subdir.join("ISSUES-INDEX.md");
        std::fs::write(&real_path, "indexed\n").unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = ReadFileTool;
        // Model guessed the wrong parent dir AND appended `{}`.
        let bad_path = dir.path().join("ISSUES-INDEX.md{}");
        let args = format!(
            r#"{{"file_path":"{}"}}"#,
            bad_path.display().to_string().replace('\\', "\\\\")
        );
        let r = tool.execute(&args, &ctx).await.unwrap();
        assert!(!r.success, "should fail — neither path exists");
        assert!(
            r.output.contains("Did you mean"),
            "should surface 'Did you mean' candidates, got: {}",
            r.output
        );
        assert!(
            r.output.contains("ISSUES-INDEX.md"),
            "candidate list should include the real file (found via stripped basename), got: {}",
            r.output
        );
    }
}
