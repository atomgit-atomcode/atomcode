use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

/// Deserialize a number that may arrive as a JSON string (weak models often quote integers).
fn deserialize_lenient_usize<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<usize>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct LenientUsize;

    impl<'de> de::Visitor<'de> for LenientUsize {
        type Value = Option<usize>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a usize or a string containing a usize")
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
                Err(de::Error::custom("negative line number"))
            }
        }
        fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<Self::Value, E> {
            if v < 0.0 || v.is_nan() {
                return Err(de::Error::custom("negative or NaN value not allowed"));
            }
            Ok(Some(v as usize))
        }
        fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
            v.trim()
                .parse::<usize>()
                .map(Some)
                .map_err(de::Error::custom)
        }
    }

    deserializer.deserialize_any(LenientUsize)
}

/// Atomic write: write to temp file then rename. Prevents corruption on crash.
/// Retries rename once after a short delay — dev servers (Vite, webpack) may
/// briefly lock files during hot-reload, causing transient rename failures.
async fn atomic_write(path: &str, content: &str) -> Result<()> {
    let temp = format!("{}.atomcode.tmp", path);
    tokio::fs::write(&temp, content)
        .await
        .with_context(|| format!("Failed to write temp file {}", temp))?;
    match tokio::fs::rename(&temp, path).await {
        Ok(()) => Ok(()),
        Err(_) => {
            // Retry once after 150ms — likely a transient file lock from dev server.
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            match tokio::fs::rename(&temp, path).await {
                Ok(()) => Ok(()),
                Err(_) => {
                    // Final fallback: direct write (not atomic, but better than failing).
                    let _ = tokio::fs::remove_file(&temp).await;
                    tokio::fs::write(path, content)
                        .await
                        .with_context(|| format!("Failed to write {}", path))?;
                    Ok(())
                }
            }
        }
    }
}

use super::auto_fix;
use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

/// Validate content in memory, write to disk, then run syntax check.
/// Only check remaining: duplicate block detection. Structural checks
/// (brace/HTML/Vue SFC) removed — auto-compile handles those better.
async fn validate_write_check(
    content: &str,
    file_path: &str,
    new_string: &str,
    original_content: &str,
    result: ToolResult,
    ctx: &ToolContext,
) -> Result<(ToolResult, String)> {
    if !result.success {
        return Ok((result, content.to_string()));
    }
    let validated =
        auto_fix::validate_and_fix(content, file_path, new_string, original_content).await;

    // Duplicate detection is the only remaining rejection.
    if validated.rejected {
        let errors = validated.warnings.join("\n");
        return Ok((
            ToolResult {
                call_id: result.call_id,
                output: format!(
                    "EDIT REJECTED — duplicate code detected:\n{}\n\
                     Fix your new_string and retry. The file was NOT modified.",
                    errors
                ),
                success: false,
            },
            content.to_string(),
        ));
    }

    atomic_write(file_path, &validated.fixed_content).await?;

    // Canonicalize once for downstream tools that key by absolute path
    // (FileStore, LSP). `file_path` here may be the model's
    // raw-as-supplied string — e.g. on macOS that's `/var/folders/...`
    // while FileStore stored the symlink-resolved `/private/var/...`.
    // Without this normalization, FileStore's `invalidate(raw_path)`
    // is a silent no-op and a stale `store_id` keeps serving pre-edit
    // bytes to the next peek_file call.
    let raw_path = std::path::Path::new(file_path);
    // tokio::fs so a hung filesystem doesn't block the async worker.
    let canon_path = tokio::fs::canonicalize(raw_path)
        .await
        .unwrap_or_else(|_| raw_path.to_path_buf());

    // Notify LSP that file changed (if LSP is enabled).
    ctx.notify_lsp_file_changed(&canon_path, &validated.fixed_content)
        .await;
    // D3: drop any FileStore entry for this path. peek_file against the
    // pre-edit store_id will return a "stale" hint pointing at re-read,
    // ensuring the model never operates on a snapshot that no longer
    // matches disk after its own edit.
    ctx.file_store.write().await.invalidate(&canon_path);
    // Defense-in-depth: also purge read_cache entries for this path. The
    // mtime gate at read.rs catches most cases, but on FS with coarse
    // mtime granularity (ext4 sec, NFS) an edit within the same tick as
    // the prior read leaves mtime unchanged and the gate fails open.
    // Explicit purge closes that corner case.
    ctx.read_cache
        .write()
        .await
        .retain(|(p, _, _), _| p != &canon_path);

    // 4. Post-write syntax check (needs file on disk)
    let syntax_warn = auto_fix::post_edit_syntax_check(file_path).await;

    let mut all_warnings: Vec<String> = validated.warnings;
    if !syntax_warn.is_empty() {
        all_warnings.push(syntax_warn);
    }

    // 5. Append surrounding context after edit — gives model the current file
    // state around the edit point so it can construct accurate old_string
    // for the next edit without needing a separate read_file/grep call.
    let context_snippet = build_edit_context(&validated.fixed_content, new_string);
    let result = ToolResult {
        output: format!("{}{}", result.output, context_snippet),
        ..result
    };

    if all_warnings.is_empty() {
        Ok((result, validated.fixed_content))
    } else {
        let combined = all_warnings.join("");
        Ok((
            ToolResult {
                output: format!("{}{}", result.output, combined),
                ..result
            },
            validated.fixed_content,
        ))
    }
}

pub struct EditFileTool;

#[derive(Deserialize)]
struct EditFileArgs {
    file_path: String,
    /// Text to find and replace. Required unless using line-number mode (start_line/end_line).
    #[serde(default)]
    old_string: Option<String>,
    /// Not required when using `edits` array mode.
    #[serde(default)]
    new_string: Option<String>,
    #[serde(default)]
    replace_all: bool,
    /// Scope edit to a specific function/class by name (tree-sitter).
    #[serde(default)]
    symbol: Option<String>,
    /// Line-number mode: replace lines start_line..end_line with new_string.
    /// Use line numbers from read_file output. No need to copy text precisely.
    #[serde(default, deserialize_with = "deserialize_lenient_usize")]
    start_line: Option<usize>,
    #[serde(default, deserialize_with = "deserialize_lenient_usize")]
    end_line: Option<usize>,
    /// Multi-edit mode: apply multiple edits to different regions in one call.
    /// Mutually exclusive with single-edit fields (old_string/new_string/start_line/end_line).
    #[serde(default)]
    edits: Option<Vec<SingleEdit>>,
}

#[derive(Deserialize)]
struct SingleEdit {
    #[serde(default, deserialize_with = "deserialize_lenient_usize")]
    start_line: Option<usize>,
    #[serde(default, deserialize_with = "deserialize_lenient_usize")]
    end_line: Option<usize>,
    #[serde(default)]
    old_string: Option<String>,
    new_string: String,
}

#[async_trait]
impl Tool for EditFileTool {
    // ── INVARIANT (2026-04-16): edit_file schema MUST expose both modes ──
    // Line mode (start_line/end_line) and text mode (old_string) must BOTH
    // be in the tool schema. Removing start_line/end_line forces the model
    // into old_string-only → match failures → full redo → 14-turn sessions.
    // History: added Phase 3, removed 5e09b86 ("confuses weak models"),
    // restored today. If a model can't handle both, fix the description,
    // don't delete the parameter.
    // ────────────────────────────────────────────────────────────────────
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "edit_file",
            description: "Replace text in a file. ALWAYS prefer this over write_file for existing files.\n\
                Two modes:\n\
                1. Line mode: use start_line + end_line + new_string. Line numbers from read_file or grep output.\n\
                2. Text mode: use old_string + new_string. old_string must match exactly.\n\
                Both modes work. Use whichever is faster — if grep already showed the code, edit directly.\n\
                For multiple changes in one file: make separate edit_file calls, one per region.\n\
                IMPORTANT: If an edit succeeds, do NOT send another edit_file for the same region. \
                Proceed to the next change or call read_file to verify.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Path to the file to edit"
                    },
                    "old_string": {
                        "type": "string",
                        "description": "Text mode: exact text to find and replace. Include enough context to be unique."
                    },
                    "new_string": {
                        "type": "string",
                        "description": "Replacement text. Use empty string to delete."
                    },
                    "start_line": {
                        "type": "integer",
                        "description": "Line mode: first line to replace (1-indexed, from read_file output)"
                    },
                    "end_line": {
                        "type": "integer",
                        "description": "Line mode: last line to replace (inclusive)"
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "Replace ALL occurrences (default: first only). Only for text mode."
                    }
                },
                // Only file_path is universally required. Mode-specific
                // requirements (text/line/edits/symbol) are enforced in
                // validate_args() below — encoding them in `required` is
                // a lie because edits-mode doesn't need top-level
                // new_string, and that lie used to bounce legitimate
                // edits-mode calls.
                "required": ["file_path"]
            }),
        }
    }

    fn validate_args(&self, args: &str) -> std::result::Result<(), String> {
        // Stage 1: structural diagnostic — list provided keys, name what's
        // missing, give a copy-pasteable example. Replaces the raw serde
        // "line 1 column N" error that weak models read as a positional-
        // arg complaint.
        super::diagnose_args(
            "edit_file",
            args,
            &[&["file_path"]],
            "edit_file({\"file_path\": \"<abs>\", \"old_string\": \"<old>\", \"new_string\": \"<new>\"}) \
             — text mode; or use start_line+end_line+new_string for line mode",
        )?;
        let parsed: EditFileArgs = serde_json::from_str(args).map_err(|e| {
            format!(
                "edit_file: {e}. Check that file_path is a string, line numbers are integers, \
                 and old_string/new_string are strings."
            )
        })?;
        // Stage 2: semantic gate — edit_file is a multi-mode tool. Accepts
        // (old_string + new_string) OR (start_line + end_line +
        // new_string) OR (edits array) OR (symbol + new_string). A call
        // with only `file_path` is a truncated/half-formed payload that
        // would deterministically fail in execute(); reject it here so
        // the model gets a structured retry hint without an approval
        // round-trip.
        let has_string_mode = parsed.old_string.is_some() || parsed.new_string.is_some();
        let has_line_mode = parsed.start_line.is_some() || parsed.end_line.is_some();
        let has_edits = parsed.edits.is_some();
        let has_symbol = parsed.symbol.is_some();
        if !has_string_mode && !has_line_mode && !has_edits && !has_symbol {
            return Err(
                "edit_file arguments missing edit content. Provide `old_string`+`new_string`, \
                 `start_line`+`end_line`+`new_string`, an `edits` array, or `symbol`+`new_string`."
                    .to_string(),
            );
        }
        Ok(())
    }

    fn approval(&self, args: &str) -> ApprovalRequirement {
        // The runner's `validate_args` gate already rejects unparseable
        // payloads upstream — when we get here, args are valid JSON
        // matching the schema. Sensitive-path detection still runs as
        // a separate concern: even valid args can target a path that
        // requires explicit user approval.
        let parsed = match serde_json::from_str::<EditFileArgs>(args) {
            Ok(p) => p,
            Err(_) => {
                // Defense in depth: if validate_args was somehow bypassed
                // (e.g. tool invoked from a path that doesn't run the
                // gate), fall back to the original fail-closed behaviour.
                return ApprovalRequirement::RequireApproval(
                    "Could not parse edit_file arguments for safety check.".to_string(),
                );
            }
        };

        if super::is_sensitive_input_path(&parsed.file_path) {
            return ApprovalRequirement::RequireApproval(
                format!("Editing sensitive system path: {}", parsed.file_path),
            );
        }

        ApprovalRequirement::AutoApprove
    }

    fn approval_with_context(&self, args: &str, ctx: &ToolContext) -> ApprovalRequirement {
        // Two checks combine: `approval()` flags sensitive targets (.env,
        // id_rsa, *.pem, /etc/*, ~/.ssh/*) regardless of workspace;
        // `scoped_write_approval` adds the out-of-workspace boundary check and
        // scopes any prompt to the exact file — so pressing [A] remembers THAT
        // file for the session (re-editing it stops re-prompting) while a
        // tool-wide grant can never bypass a sensitive write.
        let base = self.approval(args);
        let parsed = match serde_json::from_str::<EditFileArgs>(args) {
            Ok(parsed) => parsed,
            Err(_) => return base,
        };
        let working_dir = match ctx.working_dir.try_read() {
            Ok(wd) => wd.clone(),
            Err(_) => return base,
        };
        super::scoped_write_approval(&parsed.file_path, &working_dir, base)
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        // Defense-in-depth: validate_args runs at the runner gate, but if
        // it's bypassed we surface the same model-friendly diagnostic
        // instead of bubbling a raw serde error up the call stack.
        if let Err(msg) = super::diagnose_args(
            "edit_file",
            args,
            &[&["file_path"]],
            "edit_file({\"file_path\": \"<abs>\", \"old_string\": \"<old>\", \"new_string\": \"<new>\"})",
        ) {
            return Ok(ToolResult {
                call_id: String::new(),
                output: msg,
                success: false,
            });
        }
        let parsed: EditFileArgs = match serde_json::from_str(args) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: format!(
                        "edit_file: {e}. Check that file_path is a string, line numbers are \
                         integers, and old_string/new_string are strings."
                    ),
                    success: false,
                });
            }
        };
        let working_dir = ctx.working_dir.read().await.clone();
        let file_path = match super::inspect_path_access(&parsed.file_path, &working_dir) {
            Ok(access) => access.path,
            Err(err) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: err.to_string(),
                    success: false,
                });
            }
        };
        let file_path_str = file_path.to_string_lossy().to_string();

        // Backup file before any modification (file-level checkpointing).
        ctx.file_history
            .lock()
            .await
            .backup_before_write(&file_path_str)
            .await;

        let content = tokio::fs::read_to_string(&file_path)
            .await
            .with_context(|| format!("Failed to read {}", file_path.display()))?;

        // ── MULTI-EDIT MODE — disabled ──
        // Multi-edit 25次实测：apply 成功率高但改对率低，N个edit同时出错难回滚。
        // Multi-edit mode: apply multiple edits in one call.
        // Re-enabled with overlapping auto-merge + delta validation.
        if let Some(edits) = parsed.edits {
            if edits.is_empty() {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: "Error: edits array is empty.".to_string(),
                    success: false,
                });
            }
            return self
                .execute_multi_edit(&file_path_str, &content, edits, ctx)
                .await;
        }

        // Single-edit mode: new_string is required
        let new_string = match parsed.new_string {
            Some(s) => s,
            None => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: "Error: missing new_string.\n\
                        To REPLACE: edit_file({file_path, old_string: \"old code\", new_string: \"new code\"})\n\
                        To DELETE:  edit_file({file_path, old_string: \"old code\", new_string: \"\"})\n\
                        You MUST include new_string in every edit_file call.".to_string(),
                    success: false,
                });
            }
        };

        // ── LINE-NUMBER MODE ──
        // Replace lines start_line..=end_line with new_string. No text matching needed.
        if let (Some(mut start), Some(mut end)) = (parsed.start_line, parsed.end_line) {
            let lines: Vec<&str> = content.lines().collect();
            let total = lines.len();

            // Auto-swap if model gave start > end
            if end < start {
                std::mem::swap(&mut start, &mut end);
            }
            if start == 0 || start > total {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: format!(
                        "Invalid line range: {}-{} (file has {} lines)",
                        start, end, total
                    ),
                    success: false,
                });
            }
            let mut end = end.min(total);

            // Boundary overlap auto-correction (trailing): if new_string's trailing lines
            // duplicate lines immediately after end_line, extend end to absorb them.
            let ns_lines: Vec<&str> = new_string.lines().collect();
            if !ns_lines.is_empty() {
                let mut extra = 0usize;
                for i in 0..ns_lines.len() {
                    let ns_idx = ns_lines.len() - 1 - i;
                    let orig_idx = end + extra; // 0-indexed line after current end
                    if orig_idx >= total {
                        break;
                    }
                    if ns_lines[ns_idx].trim() == lines[orig_idx].trim()
                        && !ns_lines[ns_idx].trim().is_empty()
                    {
                        extra += 1;
                    } else {
                        break;
                    }
                }
                if extra > 0 {
                    end = (end + extra).min(total);
                }
            }

            // Boundary overlap auto-correction (leading): if new_string's leading lines
            // duplicate lines immediately before start_line, extend start upward.
            if !ns_lines.is_empty() {
                let mut extra = 0usize;
                for i in 0..ns_lines.len() {
                    if start <= 1 + extra {
                        break;
                    } // can't go above line 1
                    let orig_idx = start - 2 - extra; // 0-indexed line before current start
                    if ns_lines[i].trim() == lines[orig_idx].trim()
                        && !ns_lines[i].trim().is_empty()
                    {
                        extra += 1;
                    } else {
                        break;
                    }
                }
                if extra > 0 {
                    start = start.saturating_sub(extra).max(1);
                }
            }

            // Show what's being replaced
            let old_text: String = lines[start - 1..end].join("\n");
            let removed = end - start + 1;
            let added = new_string.lines().count();

            // Guard: warn (not block) on large single edits on template-heavy files.
            // Previously this was a hard block, but for bug-fix scenarios (corrupted files)
            // the model needs to do large rewrites to restore structure.
            let ext = parsed.file_path.rsplit('.').next().unwrap_or("");
            let _large_edit_warning =
                if removed > 50 && matches!(ext, "vue" | "html" | "svelte" | "tsx" | "jsx") {
                    format!(
                    "\n⚠ Large edit ({} lines replaced). Verify HTML tag balance after this edit.",
                    removed
                )
                } else {
                    String::new()
                };

            // Reconstruct file
            let mut new_lines: Vec<&str> = Vec::with_capacity(total);
            new_lines.extend_from_slice(&lines[..start - 1]);
            // new_string lines go in the middle
            let new_content_lines: Vec<&str> = new_string.lines().collect();
            new_lines.extend_from_slice(&new_content_lines);
            if end < total {
                new_lines.extend_from_slice(&lines[end..]);
            }
            let new_content = if content.ends_with('\n') {
                format!("{}\n", new_lines.join("\n"))
            } else {
                new_lines.join("\n")
            };

            let diff = build_compact_diff(&old_text, &new_string);
            let _new_end = start + added.saturating_sub(1);
            // Concise output: just confirmation + short diff. No outline, no surrounding context.
            let result = ToolResult {
                call_id: String::new(),
                output: format!(
                    "Edited {} lines {}-{} (-{} +{} lines).\n{}",
                    parsed.file_path, start, end, removed, added, diff
                ),
                success: true,
            };
            let (result, _final_content) = validate_write_check(
                &new_content,
                &file_path_str,
                &new_string,
                &content,
                result,
                ctx,
            )
            .await?;
            return Ok(result);
        }

        // ── old_string is required for text-match and symbol modes ──
        let old_string = match parsed.old_string {
            Some(ref s) if !s.is_empty() => s.clone(),
            _ => {
                // old_string is required. Do NOT auto-append — it creates duplicate code
                // when the model intends to replace but forgets old_string.
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: "Error: old_string is required for editing existing files. \
                             Provide the exact text you want to replace, or use start_line/end_line for line-based editing.".to_string(),
                    success: false,
                });
            }
        };

        // If symbol is provided, scope the edit to that symbol's body using tree-sitter.
        // This resolves ambiguity: old_string only needs to be unique within the symbol, not the whole file.
        if let Some(ref symbol_name) = parsed.symbol {
            let path = file_path.as_path();
            let mut searcher = ctx.semantic.lock().await;
            if let Some(slice) = searcher.extract_symbol(path, symbol_name) {
                let sym_text = &content[slice.start_byte..slice.end_byte];
                let sym_count = sym_text.matches(&old_string).count();

                if sym_count == 0 {
                    let (hint, _) = find_closest_match_with_suggestion(sym_text, &old_string);
                    let reread = auto_reread_content(&content, &old_string);
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: format!(
                            "Error: old_string not found in symbol '{}' (lines {}-{}).\n{}\n{}\n\
                             [HINT: Copy the EXACT text from the returned content as your new old_string.]",
                            symbol_name, slice.start_line, slice.end_line, hint, reread
                        ),
                        success: false,
                    });
                }

                if !parsed.replace_all && sym_count > 1 {
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: format!(
                            "Error: old_string found {} times in symbol '{}'. Use replace_all=true or provide more context.",
                            sym_count, symbol_name
                        ),
                        success: false,
                    });
                }

                // Replace within the symbol, reconstruct the full file
                let new_sym_text = if parsed.replace_all {
                    sym_text.replace(&old_string, &new_string)
                } else {
                    sym_text.replacen(&old_string, &new_string, 1)
                };
                let new_content = format!(
                    "{}{}{}",
                    &content[..slice.start_byte],
                    new_sym_text,
                    &content[slice.end_byte..]
                );

                let diff = build_compact_diff(&old_string, &new_string);
                let label = if parsed.replace_all {
                    format!("replaced {} occurrences in {}", sym_count, symbol_name)
                } else {
                    format!(
                        "in {} (lines {}-{})",
                        symbol_name, slice.start_line, slice.end_line
                    )
                };
                let result = ToolResult {
                    call_id: String::new(),
                    output: format!("Edited {} {}.\n{}", parsed.file_path, label, diff),
                    success: true,
                };
                let (result, _final_content) = validate_write_check(
                    &new_content,
                    &file_path_str,
                    &new_string,
                    &content,
                    result,
                    ctx,
                )
                .await?;
                // Invalidate AST cache for this file
                drop(searcher); // release lock before re-acquiring
                let mut searcher = ctx.semantic.lock().await;
                searcher.invalidate(path);
                return Ok(result);
            } else {
                // Symbol not found — list available symbols as hint
                let hint = match searcher.list_symbols(path) {
                    Some(syms) => {
                        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
                        format!(
                            "Symbol '{}' not found. Available: {}",
                            symbol_name,
                            names.join(", ")
                        )
                    }
                    None => format!("Symbol '{}' not found in {}", symbol_name, parsed.file_path),
                };
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: hint,
                    success: false,
                });
            }
        }

        // Standard path: no symbol scoping
        let count = content.matches(&old_string).count();

        if count == 0 {
            // Auto-fuzzy: try whitespace-normalized matching before failing.
            // This handles the common case where the model gets indentation slightly wrong.
            if let Some((fuzzy_result, fuzzy_count)) =
                try_fuzzy_replace(&content, &old_string, &new_string, parsed.replace_all)
            {
                let diff = build_compact_diff(&old_string, &new_string);
                let result = ToolResult {
                    call_id: String::new(),
                    output: format!(
                        "Edited {} (fuzzy match, {} occurrence{}).\n{}",
                        parsed.file_path,
                        fuzzy_count,
                        if fuzzy_count > 1 { "s" } else { "" },
                        diff
                    ),
                    success: true,
                };
                let (result, _final_content) = validate_write_check(
                    &fuzzy_result,
                    &file_path_str,
                    &new_string,
                    &content,
                    result,
                    ctx,
                )
                .await?;
                return Ok(result);
            }

            let (hint, _suggested_old) = find_closest_match_with_suggestion(&content, &old_string);

            // Auto-fallback to line mode: if we found the approximate location,
            // suggest the model use start_line/end_line instead of retrying text match.
            let line_hint = {
                let old_first = old_string
                    .lines()
                    .find(|l| !l.trim().is_empty())
                    .map(|l| l.trim());
                let lines: Vec<&str> = content.lines().collect();
                old_first
                    .and_then(|needle| {
                        lines
                            .iter()
                            .position(|l| l.trim().contains(needle))
                            .map(|center| {
                                let old_line_count = old_string.lines().count();
                                let start = center + 1; // 1-indexed
                                let end = (center + old_line_count).min(lines.len());
                                format!(
                                    "\n[TIP: Use line mode instead — edit_file(file_path=\"{}\", \
                                 start_line={}, end_line={}, new_string=\"...\")]",
                                    parsed.file_path, start, end
                                )
                            })
                    })
                    .unwrap_or_default()
            };

            let reread = auto_reread_content(&content, &old_string);
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!(
                    "Error: old_string not found in {}.\n{}{}\n{}\n\
                     [HINT: old_string did not match. The file content around your target has been \
                     returned above. Copy the EXACT text from the returned content as your new old_string.]\n\
                     [Do not fall back to shell file modification (in-place editors, redirects, \
                     write scripts) — re-issue edit_file with the corrected old_string so the \
                     change is tracked and reversible via /undo.]",
                    parsed.file_path, hint, line_hint, reread
                ),
                success: false,
            });
        }

        // Safety check: warn about large deletions
        let old_lines = old_string.lines().count();
        let new_lines = new_string.lines().count();
        let net_deleted = old_lines.saturating_sub(new_lines);
        let _deletion_warning = if net_deleted > 10 {
            format!(
                "\nWARNING: You removed {} more lines than you added. If you only meant to ADD a skeleton/loading section, \
                 use v-if/v-else to show it ALONGSIDE the existing content, not INSTEAD of it.",
                net_deleted
            )
        } else {
            String::new()
        };

        if parsed.replace_all {
            // Safety check: warn about high replacement count
            let _replace_warning = if count > 10 {
                format!(
                    "\nWARNING: Replaced {} occurrences. This many replacements may have changed structural \
                     elements (tags, brackets) that should not be bulk-replaced. Verify the file structure.",
                    count
                )
            } else {
                String::new()
            };

            let new_content = content.replace(&old_string, &new_string);
            let diff = build_compact_diff(&old_string, &new_string);
            let result = ToolResult {
                call_id: String::new(),
                output: format!(
                    "Edited {} (replaced {} occurrence{}).\n{}",
                    parsed.file_path,
                    count,
                    if count > 1 { "s" } else { "" },
                    diff,
                ),
                success: true,
            };
            let (result, _final_content) = validate_write_check(
                &new_content,
                &file_path_str,
                &new_string,
                &content,
                result,
                ctx,
            )
            .await?;
            Ok(result)
        } else {
            if count > 1 {
                // Auto-disambiguate using tree-sitter: if only ONE symbol contains the match,
                // scope to that symbol automatically. The model doesn't need to pass symbol=.
                let path = file_path.as_path();
                let mut searcher = ctx.semantic.lock().await;
                if let Some(symbols) = searcher.list_symbols(path) {
                    // Find which symbols contain the old_string
                    let matching_syms: Vec<&crate::semantic::Symbol> = symbols
                        .iter()
                        .filter(|sym| {
                            let sym_text =
                                &content[sym.start_byte..sym.end_byte.min(content.len())];
                            sym_text.contains(&*old_string)
                        })
                        .collect();

                    if matching_syms.len() == 1 {
                        // Only one symbol contains it — auto-scope and replace
                        let sym = matching_syms[0];
                        let sym_text = &content[sym.start_byte..sym.end_byte.min(content.len())];
                        let new_sym = sym_text.replacen(&*old_string, &new_string, 1);
                        let new_content = format!(
                            "{}{}{}",
                            &content[..sym.start_byte],
                            new_sym,
                            &content[sym.end_byte.min(content.len())..]
                        );
                        drop(searcher);
                        let diff = build_compact_diff(&old_string, &new_string);
                        let result = ToolResult {
                            call_id: String::new(),
                            output: format!(
                                "Edited {} in {}() (auto-scoped, {} global matches).\n{}",
                                parsed.file_path, sym.name, count, diff
                            ),
                            success: true,
                        };
                        let (result, _final_content) = validate_write_check(
                            &new_content,
                            &file_path_str,
                            &new_string,
                            &content,
                            result,
                            ctx,
                        )
                        .await?;
                        return Ok(result);
                    }
                }
                drop(searcher);

                return Ok(ToolResult {
                    call_id: String::new(),
                    output: format!(
                        "Error: old_string found {} times in {}. Use replace_all=true to replace all, or provide more context to make it unique.",
                        count, parsed.file_path
                    ),
                    success: false,
                });
            }

            let new_content = content.replacen(&old_string, &new_string, 1);

            let removed = old_string.lines().count();
            let added = new_string.lines().count();
            let diff = build_compact_diff(&old_string, &new_string);
            let result = ToolResult {
                call_id: String::new(),
                output: format!(
                    "Edited {} (-{} +{} lines).\n{}",
                    parsed.file_path, removed, added, diff,
                ),
                success: true,
            };
            let (result, _final_content) = validate_write_check(
                &new_content,
                &file_path_str,
                &new_string,
                &content,
                result,
                ctx,
            )
            .await?;
            Ok(result)
        }
    }
}

impl EditFileTool {
    /// Execute multi-edit: apply multiple edits to different regions in one call.
    /// Edits are resolved to line ranges, sorted back-to-front, then applied sequentially.
    async fn execute_multi_edit(
        &self,
        file_path: &str,
        content: &str,
        edits: Vec<SingleEdit>,
        ctx: &ToolContext,
    ) -> Result<ToolResult> {
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();

        // Resolve each edit to a (start, end, new_string) tuple where start/end are 1-indexed line numbers.
        let mut resolved: Vec<(usize, usize, String)> = Vec::with_capacity(edits.len());

        for (i, edit) in edits.iter().enumerate() {
            if let (Some(start), Some(end)) = (edit.start_line, edit.end_line) {
                // Auto-swap if start > end
                let (start, end) = if end < start {
                    (end, start)
                } else {
                    (start, end)
                };
                if start == 0 || start > total {
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: format!(
                            "Error in edit #{}: invalid line range {}-{} (file has {} lines)",
                            i + 1,
                            start,
                            end,
                            total
                        ),
                        success: false,
                    });
                }
                resolved.push((start, end.min(total), edit.new_string.clone()));
            } else if let Some(ref old_str) = edit.old_string {
                if old_str.is_empty() {
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: format!("Error in edit #{}: old_string is empty", i + 1),
                        success: false,
                    });
                }
                // Text-match mode: find the old_string and convert to line range
                match find_text_line_range(content, old_str) {
                    Some((start, end)) => {
                        resolved.push((start, end, edit.new_string.clone()));
                    }
                    None => {
                        return Ok(ToolResult {
                            call_id: String::new(),
                            output: format!(
                                "Error in edit #{}: old_string not found in {}.\nSearched for: {:?}",
                                i + 1, file_path, old_str.lines().next().unwrap_or("")
                            ),
                            success: false,
                        });
                    }
                }
            } else {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: format!(
                        "Error in edit #{}: must specify start_line/end_line or old_string",
                        i + 1
                    ),
                    success: false,
                });
            }
        }

        // Boundary overlap auto-correction: trailing + leading.
        // Trailing: new_string ends duplicate lines after end_line → extend end.
        // Leading: new_string begins duplicate lines before start_line → extend start.
        for (start, end, new_str) in &mut resolved {
            let new_lines: Vec<&str> = new_str.lines().collect();
            if new_lines.is_empty() {
                continue;
            }

            // Trailing overlap
            let mut trail_extra = 0usize;
            for i in 0..new_lines.len() {
                let new_idx = new_lines.len() - 1 - i;
                let orig_idx = *end + trail_extra;
                if orig_idx >= total {
                    break;
                }
                if new_lines[new_idx].trim() == lines[orig_idx].trim()
                    && !new_lines[new_idx].trim().is_empty()
                {
                    trail_extra += 1;
                } else {
                    break;
                }
            }
            if trail_extra > 0 {
                *end = (*end + trail_extra).min(total);
            }

            // Leading overlap
            let mut lead_extra = 0usize;
            for i in 0..new_lines.len() {
                if *start <= 1 + lead_extra {
                    break;
                }
                let orig_idx = *start - 2 - lead_extra;
                if new_lines[i].trim() == lines[orig_idx].trim() && !new_lines[i].trim().is_empty()
                {
                    lead_extra += 1;
                } else {
                    break;
                }
            }
            if lead_extra > 0 {
                *start = start.saturating_sub(lead_extra).max(1);
            }
        }

        // Auto-merge overlapping ranges instead of rejecting.
        // Weak models frequently generate edits like (264-279) + (268-279)
        // where the second edit extends or replaces part of the first.
        // Merge: keep the union range, second edit's new_string wins for
        // the overlapping portion.
        resolved.sort_by_key(|(start, _, _)| *start);
        let mut merged: Vec<(usize, usize, String)> = Vec::new();
        for edit in resolved {
            if let Some(last) = merged.last_mut() {
                if edit.0 <= last.1 {
                    // Overlapping — merge (adjacent edits are NOT merged).
                    // Strategy: the later edit (by position) wins for the
                    // overlapping region.
                    if edit.1 >= last.1 {
                        // Second edit extends beyond first — take second's content
                        // for the entire merged range (matches model intent:
                        // it wanted to replace this whole region).
                        last.1 = edit.1;
                        last.2 = edit.2;
                    }
                    // If second is fully contained in first, first wins (skip second)
                    continue;
                }
            }
            merged.push(edit);
        }
        let mut resolved = merged;

        // Apply edits back-to-front to preserve line numbers
        resolved.sort_by(|a, b| b.0.cmp(&a.0));

        let mut result_lines: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        let mut summary_parts: Vec<String> = Vec::new();

        // Note: large edit guard changed from hard block to warning.
        // Corrupted files need large rewrites to restore structure.
        let _ext = file_path.rsplit('.').next().unwrap_or("");
        if false { // guard disabled — auto_fix handles validation
        }

        for (start, end, new_str) in &resolved {
            let removed = end - start + 1;
            let new_edit_lines: Vec<String> = new_str.lines().map(|l| l.to_string()).collect();
            let added = new_edit_lines.len();
            result_lines.splice((start - 1)..*end, new_edit_lines);
            summary_parts.push(format!("L{}-{} (-{} +{})", start, end, removed, added));
        }
        // Reverse so summary is top-to-bottom
        summary_parts.reverse();

        let new_content = if content.ends_with('\n') {
            format!("{}\n", result_lines.join("\n"))
        } else {
            result_lines.join("\n")
        };

        let edit_count = resolved.len();
        let all_new_strings: String = edits
            .iter()
            .map(|e| e.new_string.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let short_name = std::path::Path::new(file_path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| file_path.to_string());
        let result = ToolResult {
            call_id: String::new(),
            output: format!(
                "Multi-edit: {} edits applied to {} [{}].\n\u{2713} {} updated. Proceed to your next file.",
                edit_count, file_path, summary_parts.join(", "), short_name),
            success: true,
        };
        let (result, _final_content) =
            validate_write_check(&new_content, file_path, &all_new_strings, content, result, ctx)
                .await?;
        Ok(result)
    }
}

/// Find the line range (1-indexed, inclusive) where `needle` appears in `content`.
/// Returns None if not found or if found multiple times.
fn find_text_line_range(content: &str, needle: &str) -> Option<(usize, usize)> {
    let needle_lines: Vec<&str> = needle.lines().collect();
    if needle_lines.is_empty() {
        return None;
    }

    let content_lines: Vec<&str> = content.lines().collect();
    let mut matches: Vec<usize> = Vec::new();

    // Try exact match first
    for i in 0..content_lines.len().saturating_sub(needle_lines.len() - 1) {
        if content_lines[i..i + needle_lines.len()] == needle_lines[..] {
            matches.push(i);
        }
    }

    // If no exact match, try trimmed (fuzzy) match
    if matches.is_empty() {
        let needle_trimmed: Vec<&str> = needle_lines.iter().map(|l| l.trim()).collect();
        for i in 0..content_lines.len().saturating_sub(needle_trimmed.len() - 1) {
            let window: Vec<&str> = content_lines[i..i + needle_trimmed.len()]
                .iter()
                .map(|l| l.trim())
                .collect();
            if window == needle_trimmed {
                matches.push(i);
            }
        }
    }

    if matches.len() == 1 {
        let start = matches[0] + 1; // 1-indexed
        let end = start + needle_lines.len() - 1;
        Some((start, end))
    } else {
        None // not found or ambiguous
    }
}

/// Try fuzzy matching: normalize whitespace (trim each line) and try to match.
/// `replace_all` controls whether all matches or just a unique one should be replaced.
fn try_fuzzy_replace(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Option<(String, usize)> {
    let old_normalized: Vec<&str> = old_string.lines().map(|l| l.trim()).collect();
    if old_normalized.is_empty() || old_normalized.iter().all(|l| l.is_empty()) {
        return None;
    }

    let content_lines: Vec<&str> = content.lines().collect();
    let has_trailing_newline = content.ends_with('\n');
    let mut matches: Vec<(usize, usize)> = Vec::new();

    // Only attempt fuzzy match if old_string has substantial content (not just short fragments)
    let total_non_ws: usize = old_normalized.iter().map(|l| l.len()).sum();
    if total_non_ws < 10 {
        return None; // Too short for reliable fuzzy matching
    }

    // Slide window — skip overlapping matches
    let mut i = 0;
    while i + old_normalized.len() <= content_lines.len() {
        let window: Vec<&str> = content_lines[i..i + old_normalized.len()]
            .iter()
            .map(|l| l.trim())
            .collect();
        if window == old_normalized {
            matches.push((i, i + old_normalized.len()));
            i += old_normalized.len(); // skip past this match
        } else {
            i += 1;
        }
    }

    if matches.is_empty() {
        return None;
    }

    // If replace_all=false, require exactly one match
    if !replace_all && matches.len() > 1 {
        return None; // caller will handle the "multiple matches" error
    }

    // Compute the base indent of new_string used as the re-anchor point.
    //
    // 2026-04-23 (P1 #14c+11): changed from `.min()` to "first non-empty
    // line's indent".
    //
    // BUG the old `.min()` version caused (hermes session 2026-04-22_21-06):
    //   Model's new_string had 4 lines — `.run(...)` at 9 spaces,
    //   `.expect(...)` at 9 spaces, `}` at 0 spaces, `marker()` at 0 spaces.
    //   `.min()` picked 0 (from the outdented `}`). Then `.run(...)` got
    //   `relative = 9 - 0 = 9`, added to the file's 8-space prefix → output
    //   at 17 spaces. Next fuzzy edit on that corrupted file repeated the
    //   shift → 17 → 26 → accumulating indent drift.
    //
    // NEW ANCHOR semantics: the model's first non-empty line in new_string
    // represents the OUTER indent the model intended to match in old_string.
    // Relative differences (lines indented more OR less than the anchor)
    // are preserved as signed offsets from the file's actual indent.
    let new_lines: Vec<&str> = new_string.lines().collect();
    let new_base_indent = new_lines.iter()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .unwrap_or(0);

    let mut result_lines: Vec<String> = content_lines.iter().map(|l| l.to_string()).collect();

    // Process matches in reverse to preserve indices
    let to_replace = if replace_all {
        &matches[..]
    } else {
        &matches[..1]
    };
    for &(start, end) in to_replace.iter().rev() {
        // Detect indentation from the first matched line in the file
        let original_line = content_lines[start];
        let file_indent = original_line.len() - original_line.trim_start().len();
        let file_indent_str: String = original_line.chars().take(file_indent).collect();

        // Build replacement preserving RELATIVE indentation from new_string,
        // supporting both deeper-than-anchor (add spaces) and outdented-from-
        // anchor (trim the file's indent prefix) cases.
        let replacement: Vec<String> = new_lines.iter().map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                let line_indent = l.len() - l.trim_start().len();
                let signed_relative = line_indent as isize - new_base_indent as isize;
                let total_indent = if signed_relative >= 0 {
                    // Same or deeper than anchor — keep file's indent prefix
                    // (preserves tabs/spaces mix) and extend with plain spaces.
                    format!("{}{}", file_indent_str, " ".repeat(signed_relative as usize))
                } else {
                    // Outdented from anchor — drop `abs(signed_relative)`
                    // chars from the tail of file_indent_str. Preserves the
                    // leading whitespace semantics up to the needed depth.
                    let drop = (-signed_relative) as usize;
                    let keep = file_indent.saturating_sub(drop);
                    file_indent_str.chars().take(keep).collect()
                };
                format!("{}{}", total_indent, l.trim())
            }
        }).collect();

        result_lines.splice(start..end, replacement);
    }

    let mut result = result_lines.join("\n");
    // Preserve trailing newline
    if has_trailing_newline && !result.ends_with('\n') {
        result.push('\n');
    }

    let count = if replace_all { matches.len() } else { 1 };
    Some((result, count))
}

/// Build a compact diff showing removed/added lines (max 8 lines total).
fn build_compact_diff(old: &str, new: &str) -> String {
    let mut diff = String::new();
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    let max_show = 4; // max lines per side

    // Show removed lines (prefixed with -)
    for (i, line) in old_lines.iter().take(max_show).enumerate() {
        diff.push_str(&format!("- {}\n", line));
        if i == max_show - 1 && old_lines.len() > max_show {
            diff.push_str(&format!(
                "  ... ({} more removed)\n",
                old_lines.len() - max_show
            ));
        }
    }

    // Show added lines (prefixed with +)
    for (i, line) in new_lines.iter().take(max_show).enumerate() {
        diff.push_str(&format!("+ {}\n", line));
        if i == max_show - 1 && new_lines.len() > max_show {
            diff.push_str(&format!(
                "  ... ({} more added)\n",
                new_lines.len() - max_show
            ));
        }
    }

    diff.trim_end().to_string()
}

/// Build a context snippet showing ±4 lines around the edited region.
/// This lets the model see the current file state after the edit,
/// so it can construct accurate old_string for the next edit without re-reading.
///
/// Threshold raised 20 → 100: for small/medium files the diff is already enough
/// context, and the snippet just duplicates content that recency-reinjection or
/// the next read_file will surface. Large files (> 100 lines) still get the
/// snippet because they're the only case where re-reading is expensive.
fn build_edit_context(content: &str, new_string: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= 20 {
        return String::new();
    }

    // Find where new_string appears in the final content using substring search.
    // More reliable than line-by-line matching (handles indentation changes).
    let new_trimmed = new_string.trim();
    if new_trimmed.is_empty() {
        return String::new();
    }

    // Find the first non-empty line of new_string in the file
    let search_line = new_trimmed
        .lines()
        .find(|l| l.trim().len() >= 5)
        .unwrap_or("");
    if search_line.is_empty() {
        return String::new();
    }

    let center = match lines.iter().position(|l| l.contains(search_line.trim())) {
        Some(idx) => idx,
        None => return String::new(),
    };

    let ctx = 4;
    let new_lines_count = new_string.lines().count();
    let start = center.saturating_sub(ctx);
    let end = (center + new_lines_count + ctx).min(lines.len());

    let mut snippet = format!("\n[File after edit, lines {}-{}:]\n", start + 1, end);
    for (i, line) in lines[start..end].iter().enumerate() {
        snippet.push_str(&format!("{:>4}| {}\n", start + i + 1, line));
    }
    snippet
}

/// Auto re-read: when old_string match fails, include current file content
/// so the model can retry immediately without a separate read_file call.
///
/// - Files <= 200 lines: full content with line numbers.
/// - Files > 200 lines: 50 lines around the approximate target area.
fn auto_reread_content(content: &str, old_string: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();

    if total == 0 {
        return String::new();
    }

    let mut out = String::new();

    // Find approximate target area
    let target_line = old_string
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|first| first.trim());

    let center = target_line
        .and_then(|needle| lines.iter().position(|l| l.trim().contains(needle)))
        .unwrap_or(0);

    // Cap output to prevent context explosion when multiple edits fail in one turn.
    // ≤100 lines: return full file (~1.2K tok, safe even with 5 failures = 6K tok)
    // 101-300 lines: return ±15 lines around target (~30 lines = 400 tok)
    // >300 lines: return ±7 lines around target (~15 lines = 200 tok)
    if total <= 100 {
        out.push_str(&format!(
            "\n[Edit failed. Full file ({} lines) — copy EXACT text for old_string:]\n",
            total
        ));
        for (i, line) in lines.iter().enumerate() {
            out.push_str(&format!("{:>4}| {}\n", i + 1, line));
        }
    } else {
        let half = if total <= 300 { 15 } else { 7 };
        let start = center.saturating_sub(half);
        let end = (center + half + 1).min(total);

        out.push_str(&format!(
            "\n[Edit failed. Lines {}-{} of {} (use EXACT text from below as old_string):]\n",
            start + 1,
            end,
            total
        ));
        for i in start..end {
            out.push_str(&format!("{:>4}| {}\n", i + 1, lines[i]));
        }
    }

    out
}

/// Find the closest match and return (hint_message, suggested_old_string).
/// The suggested_old_string is the exact text from the file that the model
/// should use — it can copy-paste this into old_string to retry immediately
/// without re-reading the file.
fn find_closest_match_with_suggestion(content: &str, old_string: &str) -> (String, Option<String>) {
    let old_lines: Vec<&str> = old_string.lines().collect();
    let content_lines: Vec<&str> = content.lines().collect();

    if old_lines.is_empty() {
        return (
            "old_string is empty. Use read_file to re-read the file.".to_string(),
            None,
        );
    }

    let old_first_trimmed = old_lines[0].trim();
    if old_first_trimmed.is_empty() && old_lines.len() > 1 {
        let hint = find_closest_match(content, old_string);
        return (hint, None);
    }

    // Try to find where the first line matches (trimmed) in the file
    for (i, line) in content_lines.iter().enumerate() {
        if line.trim() == old_first_trimmed {
            // Found potential match start. Extract the same number of lines from file.
            let end = (i + old_lines.len()).min(content_lines.len());
            let actual_lines = &content_lines[i..end];

            // Check if it's a plausible match (at least 30% of lines match trimmed)
            let matching = actual_lines
                .iter()
                .zip(old_lines.iter())
                .filter(|(a, b)| a.trim() == b.trim())
                .count();

            if matching >= old_lines.len() / 3 || matching >= 2 {
                let suggested = actual_lines.join("\n");
                let hint = find_closest_match(content, old_string);
                return (hint, Some(suggested));
            }
        }
    }

    let hint = find_closest_match(content, old_string);
    (hint, None)
}

/// Find the closest matching region in the file to help the model fix old_string.
/// Three strategies: (1) whitespace-normalized multi-line match, (2) first-line match, (3) keyword search.
fn find_closest_match(content: &str, old_string: &str) -> String {
    let old_lines: Vec<&str> = old_string.lines().collect();
    let content_lines: Vec<&str> = content.lines().collect();

    if old_lines.is_empty() {
        return "old_string is empty. Use read_file to re-read the file.".to_string();
    }

    let old_first_trimmed = old_lines[0].trim();
    if old_first_trimmed.is_empty() && old_lines.len() > 1 {
        // First line is empty — try second line
        return find_closest_match_inner(content, &content_lines, old_lines[1].trim(), &old_lines);
    }

    find_closest_match_inner(content, &content_lines, old_first_trimmed, &old_lines)
}

fn find_closest_match_inner(
    _content: &str,
    content_lines: &[&str],
    first_line_trimmed: &str,
    old_lines: &[&str],
) -> String {
    if first_line_trimmed.is_empty() {
        return "old_string appears empty after trimming. Use read_file to re-read the file."
            .to_string();
    }

    // Strategy 1: Find where the first line matches (trimmed) and show divergence point
    let mut candidates: Vec<(usize, usize)> = Vec::new(); // (line_idx, match_score)

    for (i, line) in content_lines.iter().enumerate() {
        let trimmed = line.trim();
        // Exact trimmed match of first line
        if trimmed == first_line_trimmed {
            // Check how many subsequent lines also match (trimmed)
            let mut match_count = 1;
            for j in 1..old_lines.len() {
                if i + j >= content_lines.len() {
                    break;
                }
                if content_lines[i + j].trim() == old_lines[j].trim() {
                    match_count += 1;
                } else {
                    break;
                }
            }
            candidates.push((i, match_count));
        }
        // Substring match of first line — require both sides to carry real
        // signal. Without a length floor, `first_line_trimmed.contains("")`
        // is TRUE for every blank line in the file (trim() → "") and
        // `contains("}")` / `contains(")")` fire on every closing bracket,
        // so the "closest match" result regularly pointed at the first
        // blank line (session 2026-04-22 20-41: `.Run(...)` old_string →
        // "Closest match found near line 3" which was an empty line, with
        // a lines-1-16 snippet unrelated to the model's real target near
        // line 270). Bumping to 4 chars filters blanks and single-token
        // syntactic noise while still catching short identifiers like
        // `main`, `impl`, `pub` that the length-15 prefix check would miss.
        else if trimmed.len() >= 4
            && first_line_trimmed.len() >= 4
            && (trimmed.contains(first_line_trimmed) || first_line_trimmed.contains(trimmed))
        {
            candidates.push((i, 0));
        }
        // Prefix match (first 25 chars)
        else if trimmed.len() > 15
            && first_line_trimmed.len() > 15
            && trimmed.chars().take(25).collect::<String>()
                == first_line_trimmed.chars().take(25).collect::<String>()
        {
            candidates.push((i, 0));
        }
    }

    // Sort by match_count (highest first)
    candidates.sort_by(|a, b| b.1.cmp(&a.1));

    if let Some(&(best_idx, match_count)) = candidates.first() {
        let start = best_idx.saturating_sub(1);
        // Cap snippet to 20 lines max — large snippets waste context without helping
        let end = (best_idx + old_lines.len().min(18) + 2).min(content_lines.len());

        let mut snippet = String::new();
        for i in start..end {
            snippet.push_str(&format!("{:>4}| {}\n", i + 1, content_lines[i]));
        }
        if best_idx + old_lines.len() + 2 > end {
            snippet.push_str(&format!(
                "     ... ({} more lines in file)\n",
                content_lines.len() - end
            ));
        }

        // If some lines matched but not all, show exactly where the divergence is
        if match_count > 0
            && match_count < old_lines.len()
            && best_idx + match_count < content_lines.len()
        {
            let diverge_idx = match_count;
            let file_line = content_lines[best_idx + diverge_idx].trim();
            let old_line = old_lines[diverge_idx].trim();

            // Detect indentation mismatch
            let file_indent =
                content_lines[best_idx].len() - content_lines[best_idx].trim_start().len();
            let old_indent = old_lines[0].len() - old_lines[0].trim_start().len();

            let mut hint = format!(
                "First {} line(s) match (trimmed) but line {} diverges:\n\
                 YOUR old_string line {}: \"{}\"\n\
                 ACTUAL file line {}:     \"{}\"\n",
                match_count,
                diverge_idx + 1,
                diverge_idx + 1,
                old_line,
                best_idx + diverge_idx + 1,
                file_line,
            );

            if file_indent != old_indent {
                hint.push_str(&format!(
                    "INDENTATION MISMATCH: file uses {} spaces, your old_string uses {} spaces.\n",
                    file_indent, old_indent,
                ));
            }

            return format!(
                "Partial match at lines {}-{} ({}/{} lines match).\n{}\n{}\n\
                 Copy the EXACT text from above (including indentation) for old_string.",
                best_idx + 1,
                end,
                match_count,
                old_lines.len(),
                snippet,
                hint
            );
        }

        // Indentation-only mismatch detection
        if match_count == 0 {
            let file_indent =
                content_lines[best_idx].len() - content_lines[best_idx].trim_start().len();
            let old_indent = old_lines[0].len() - old_lines[0].trim_start().len();
            if file_indent != old_indent && content_lines[best_idx].trim() == old_lines[0].trim() {
                return format!(
                    "INDENTATION MISMATCH at line {}. File uses {} spaces, your old_string uses {} spaces.\n\
                     Actual file content:\n{}\n\
                     Copy the EXACT text (with correct indentation) for old_string.",
                    best_idx + 1, file_indent, old_indent, snippet
                );
            }
        }

        return format!(
            "Closest match found near line {}:\n{}\n\
             Copy the EXACT text from above for old_string (preserve indentation).",
            best_idx + 1,
            snippet
        );
    }

    // Strategy 2: keyword-based search — find lines containing distinctive words from old_string
    let keywords: Vec<&str> = first_line_trimmed
        .split_whitespace()
        .filter(|w| {
            w.len() > 3
                && !matches!(
                    *w,
                    "const"
                        | "let"
                        | "var"
                        | "this"
                        | "self"
                        | "return"
                        | "from"
                        | "import"
                        | "function"
                )
        })
        .take(3)
        .collect();

    if !keywords.is_empty() {
        for (i, line) in content_lines.iter().enumerate() {
            let lower = line.to_lowercase();
            if keywords.iter().all(|kw| lower.contains(&kw.to_lowercase())) {
                let start = i.saturating_sub(2);
                let end = (i + 5).min(content_lines.len());
                let mut snippet = String::new();
                for j in start..end {
                    snippet.push_str(&format!("{:>4}| {}\n", j + 1, content_lines[j]));
                }
                return format!(
                    "No exact match, but keywords [{}] found near line {}:\n{}\n\
                     Use read_file with offset={} limit=20 to see the exact content.",
                    keywords.join(", "),
                    i + 1,
                    snippet,
                    start + 1
                );
            }
        }
    }

    format!(
        "No similar text found in the file ({} lines total). \
         The content may have changed. Use read_file to re-read the file.",
        content_lines.len()
    )
}
#[cfg(test)]
mod security_tests {
    use super::*;
    use crate::tool::{ApprovalRequirement, Tool, ToolContext};
    use serial_test::serial;
    use tempfile::TempDir;

    #[test]
    fn edit_file_requires_approval_for_sensitive_paths() {
        let tool = EditFileTool;
        let args = serde_json::json!({
            "file_path": "/etc/hosts",
            "old_string": "old",
            "new_string": "new"
        })
        .to_string();

        assert!(matches!(
            tool.approval(&args),
            ApprovalRequirement::RequireApproval(_)
        ));
    }

    #[test]
    fn edit_file_auto_approves_regular_paths() {
        let tool = EditFileTool;
        let args = serde_json::json!({
            "file_path": "src/main.rs",
            "old_string": "old",
            "new_string": "new"
        })
        .to_string();

        assert!(matches!(tool.approval(&args), ApprovalRequirement::AutoApprove));
    }

    #[test]
    fn edit_file_requires_approval_when_args_do_not_parse() {
        let tool = EditFileTool;
        assert!(matches!(
            tool.approval("{not valid json"),
            ApprovalRequirement::RequireApproval(_)
        ));
    }

    // Regression: a single `[A]` on edit_file for a safe edit must NOT
    // silently disarm the sensitive-path guard for OTHER files. Pre-fix
    // `approval_with_context` only ran the workspace-boundary check and
    // dropped the `is_sensitive_input_path` result from `approval()`, so
    // editing a workspace-local `.env` came back as AutoApprove — worse
    // than the bash session-grant bypass (no prompt at all). Fix contract:
    // sensitive in-workspace paths return RequireApprovalScoped so a
    // tool-wide session grant on edit_file cannot bypass approval, while a
    // per-file [A] is remembered for that exact file (see
    // `edit_file_always_grant_persists_for_same_file_only`).
    #[test]
    fn edit_file_sensitive_in_workspace_path_returns_scoped_approval() {
        let workspace = TempDir::new().unwrap();
        let dotenv = workspace.path().join(".env");
        let args = serde_json::json!({
            "file_path": dotenv.to_string_lossy(),
            "old_string": "old",
            "new_string": "new"
        })
        .to_string();
        let ctx = ToolContext::new(workspace.path().to_path_buf());
        assert!(
            matches!(
                EditFileTool.approval_with_context(&args, &ctx),
                ApprovalRequirement::RequireApprovalScoped { .. }
            ),
            "sensitive in-workspace path (.env) must return RequireApprovalScoped so a \
             tool-wide session grant on edit_file cannot bypass approval",
        );
    }

    // Regression for the reported bug: editing the SAME file repeatedly,
    // pressing [A] (Always) must stop re-prompting for THAT file — while a
    // different sensitive/external file still prompts. Pre-fix the approval
    // was RequireApprovalAlways, which `PermissionStore::check` routes to Ask
    // unconditionally, so the recorded grant was dead and every edit
    // re-prompted.
    #[test]
    fn edit_file_always_grant_persists_for_same_file_only() {
        use crate::tool::{ApprovalRequirement, PermissionDecision, PermissionStore};
        let workspace = TempDir::new().unwrap();
        let dotenv = workspace.path().join(".env"); // sensitive → prompts
        let other = workspace.path().join("secret.pem"); // different sensitive file
        let mk = |p: &std::path::Path| {
            serde_json::json!({
                "file_path": p.to_string_lossy(),
                "old_string": "old",
                "new_string": "new"
            })
            .to_string()
        };
        let ctx = ToolContext::new(workspace.path().to_path_buf());
        let mut store = PermissionStore::new();

        // First edit of .env prompts; simulate the user pressing [A] by
        // recording the scoped grant the decider would store.
        let approval = EditFileTool.approval_with_context(&mk(&dotenv), &ctx);
        assert!(matches!(store.check("edit_file", &approval), PermissionDecision::Ask(_)));
        match &approval {
            ApprovalRequirement::RequireApprovalScoped { scope, .. } => {
                store.grant_session_scope(scope)
            }
            other => panic!("expected scoped approval, got {other:?}"),
        }

        // Re-editing the SAME file no longer prompts.
        let again = EditFileTool.approval_with_context(&mk(&dotenv), &ctx);
        assert!(
            matches!(store.check("edit_file", &again), PermissionDecision::Allow),
            "[A] on a file must stop re-prompting for the SAME file",
        );
        // A DIFFERENT sensitive file still prompts (scope is per-file).
        let different = EditFileTool.approval_with_context(&mk(&other), &ctx);
        assert!(
            matches!(store.check("edit_file", &different), PermissionDecision::Ask(_)),
            "a [A] on one file must NOT unlock edits to a different sensitive file",
        );
    }

    // Cross-layer integration: edit_file on a sensitive in-workspace path
    // with an existing `grant_session("edit_file")` must still Ask. Pins
    // the contract end-to-end against a future refactor of either layer.
    #[test]
    fn edit_file_sensitive_path_through_store_with_session_grant_asks() {
        use crate::tool::{PermissionDecision, PermissionStore};
        let workspace = TempDir::new().unwrap();
        let dotenv = workspace.path().join(".env");
        let args = serde_json::json!({
            "file_path": dotenv.to_string_lossy(),
            "old_string": "old",
            "new_string": "new"
        })
        .to_string();
        let ctx = ToolContext::new(workspace.path().to_path_buf());
        let mut store = PermissionStore::new();
        store.grant_session("edit_file"); // simulate prior [A] on a safe edit
        let approval = EditFileTool.approval_with_context(&args, &ctx);
        let decision = store.check("edit_file", &approval);
        assert!(
            matches!(decision, PermissionDecision::Ask(_)),
            "edit on sensitive in-workspace path (.env) must prompt the user even with a \
             session grant, got {decision:?}",
        );
    }

    #[tokio::test]
    #[serial]
    async fn edit_file_writes_relative_path_against_tool_working_dir() {
        let workspace = TempDir::new().unwrap();
        let process_cwd = TempDir::new().unwrap();
        std::fs::create_dir_all(workspace.path().join("src")).unwrap();
        std::fs::create_dir_all(process_cwd.path().join("src")).unwrap();
        std::fs::write(workspace.path().join("src/app.rs"), "fn main() {\n    old();\n}\n")
            .unwrap();

        let original_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(process_cwd.path()).unwrap();
        let result = EditFileTool
            .execute(
                r#"{"file_path":"src/app.rs","old_string":"old();","new_string":"new();"}"#,
                &ToolContext::new(workspace.path().to_path_buf()),
            )
            .await;
        std::env::set_current_dir(original_cwd).unwrap();

        let result = result.unwrap();
        assert!(result.success, "{}", result.output);
        assert_eq!(
            std::fs::read_to_string(workspace.path().join("src/app.rs")).unwrap(),
            "fn main() {\n    new();\n}\n"
        );
        assert!(
            !process_cwd.path().join("src/app.rs").exists(),
            "edit_file must not write relative paths against the process cwd"
        );
    }
}
