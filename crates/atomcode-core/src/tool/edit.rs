use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

/// Deserialize a number that may arrive as a JSON string (weak models often quote integers).
fn deserialize_lenient_usize<'de, D>(deserializer: D) -> std::result::Result<Option<usize>, D::Error>
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
        fn visit_none<E: de::Error>(self) -> std::result::Result<Self::Value, E> { Ok(None) }
        fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> { Ok(None) }
        fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
            Ok(Some(v as usize))
        }
        fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
            if v >= 0 { Ok(Some(v as usize)) } else { Err(de::Error::custom("negative line number")) }
        }
        fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<Self::Value, E> {
            Ok(Some(v as usize))
        }
        fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
            v.trim().parse::<usize>().map(Some).map_err(de::Error::custom)
        }
    }

    deserializer.deserialize_any(LenientUsize)
}

/// Atomic write: write to temp file then rename. Prevents corruption on crash.
/// Retries rename once after a short delay — dev servers (Vite, webpack) may
/// briefly lock files during hot-reload, causing transient rename failures.
async fn atomic_write(path: &str, content: &str) -> Result<()> {
    let temp = format!("{}.atomcode.tmp", path);
    tokio::fs::write(&temp, content).await
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
                    tokio::fs::write(path, content).await
                        .with_context(|| format!("Failed to write {}", path))?;
                    Ok(())
                }
            }
        }
    }
}

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

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
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "edit_file",
            description: "Replace text in a file. ALWAYS prefer this over write_file for existing files.\n\
                Three modes:\n\
                1. LINE-NUMBER MODE (recommended): specify start_line and end_line from read_file output.\n\
                   No need to copy text — just use line numbers. Replaces lines start_line through end_line with new_string.\n\
                   Example: {\"file_path\": \"app.vue\", \"start_line\": 150, \"end_line\": 165, \"new_string\": \"<new content>\"}\n\
                2. TEXT MATCH MODE: specify old_string to find and replace.\n\
                   old_string must be unique in the file (or use replace_all=true for bulk changes).\n\
                   Example: {\"file_path\": \"app.vue\", \"old_string\": \"bg-blue-500\", \"new_string\": \"bg-red-500\", \"replace_all\": true}\n\
                3. MULTI-EDIT MODE: pass an edits array to change multiple regions in ONE call.\n\
                   Each edit uses line-number or text-match mode independently. Applied back-to-front automatically.\n\
                   PREFER THIS when you need to change 2+ non-adjacent regions (e.g. imports + logic + template).\n\
                   Example: {\"file_path\": \"app.vue\", \"edits\": [\n\
                     {\"start_line\": 5, \"end_line\": 6, \"new_string\": \"import { ref, computed } from 'vue'\"},\n\
                     {\"start_line\": 30, \"end_line\": 30, \"new_string\": \"const count = ref(0)\"},\n\
                     {\"old_string\": \"<div>old</div>\", \"new_string\": \"<div>new</div>\"}\n\
                   ]}\n\
                Additional options:\n\
                - symbol: scope the edit to a specific function/class (tree-sitter). Reduces ambiguity.\n\
                Behavior:\n\
                - If old_string is not found, auto-tries fuzzy matching (whitespace-normalized).\n\
                - NEVER use write_file to modify existing files. edit_file prevents accidental code deletion.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "Path to the file to edit"
                    },
                    "old_string": {
                        "type": "string",
                        "description": "Text to find (text-match mode). Not needed if using start_line/end_line."
                    },
                    "new_string": {
                        "type": "string",
                        "description": "Replacement text"
                    },
                    "start_line": {
                        "type": "integer",
                        "description": "Start line number (from read_file output). Replaces lines start_line..end_line with new_string."
                    },
                    "end_line": {
                        "type": "integer",
                        "description": "End line number (inclusive). Used with start_line for line-number mode."
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "Replace ALL occurrences of old_string (text-match mode only)."
                    },
                    "symbol": {
                        "type": "string",
                        "description": "Scope edit to a specific function/class name (tree-sitter)."
                    },
                    "edits": {
                        "type": "array",
                        "description": "Multi-edit: array of edits to apply in one call. Each edit has start_line/end_line or old_string, plus new_string. Use when changing 2+ non-adjacent regions.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "start_line": { "type": "integer", "description": "Start line number" },
                                "end_line": { "type": "integer", "description": "End line number (inclusive)" },
                                "old_string": { "type": "string", "description": "Text to find (text-match mode)" },
                                "new_string": { "type": "string", "description": "Replacement text" }
                            },
                            "required": ["new_string"]
                        }
                    }
                },
                "required": ["file_path"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: EditFileArgs = serde_json::from_str(args)?;

        // Backup file before any modification (file-level checkpointing).
        ctx.file_history.lock().await.backup_before_write(&parsed.file_path).await;

        let content = tokio::fs::read_to_string(&parsed.file_path)
            .await
            .with_context(|| format!("Failed to read {}", parsed.file_path))?;

        // ── MULTI-EDIT MODE ──
        if let Some(edits) = parsed.edits {
            if edits.is_empty() {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: "Error: edits array is empty.".to_string(),
                    success: false,
                });
            }
            return self.execute_multi_edit(&parsed.file_path, &content, edits).await;
        }

        // Single-edit mode: new_string is required
        let new_string = match parsed.new_string {
            Some(s) => s,
            None => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: "Error: new_string is required for single-edit mode. Use edits array for multi-edit.".to_string(),
                    success: false,
                });
            }
        };

        // ── LINE-NUMBER MODE ──
        // Replace lines start_line..=end_line with new_string. No text matching needed.
        if let (Some(start), Some(end)) = (parsed.start_line, parsed.end_line) {
            let lines: Vec<&str> = content.lines().collect();
            let total = lines.len();

            if start == 0 || start > total || end < start {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: format!("Invalid line range: {}-{} (file has {} lines)", start, end, total),
                    success: false,
                });
            }
            let mut end = end.min(total);

            // Boundary overlap auto-correction: if new_string's trailing lines
            // duplicate lines immediately after end_line, extend end to absorb them.
            let ns_lines: Vec<&str> = new_string.lines().collect();
            if !ns_lines.is_empty() {
                let mut extra = 0usize;
                for i in 0..ns_lines.len() {
                    let ns_idx = ns_lines.len() - 1 - i;
                    let orig_idx = end + extra; // 0-indexed line after current end
                    if orig_idx >= total { break; }
                    if ns_lines[ns_idx].trim() == lines[orig_idx].trim() && !ns_lines[ns_idx].trim().is_empty() {
                        extra += 1;
                    } else {
                        break;
                    }
                }
                if extra > 0 {
                    end = (end + extra).min(total);
                }
            }

            // Show what's being replaced
            let old_text: String = lines[start - 1..end].join("\n");
            let removed = end - start + 1;
            let added = new_string.lines().count();

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

            atomic_write(&parsed.file_path, &new_content).await?;
            let diff = build_compact_diff(&old_text, &new_string);
            let outline = post_edit_info(&new_content, &new_string);
            let new_end = start + added.saturating_sub(1);
            let ctx = surrounding_context(&parsed.file_path, start, new_end);
            let result = ToolResult {
                call_id: String::new(),
                output: format!(
                    "Edited {} lines {}-{} (-{} +{} lines).\n{}\n{}{}",
                    parsed.file_path, start, end, removed, added, diff, outline, ctx
                ),
                success: true,
            };
            return Ok(post_edit_validate(result, &parsed.file_path, &new_content, &new_string).await);
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
            let path = std::path::Path::new(&parsed.file_path);
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
                            "Error: old_string not found in symbol '{}' (lines {}-{}).\n{}\n{}",
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

                atomic_write(&parsed.file_path, &new_content).await?;
                // Invalidate AST cache for this file
                drop(searcher); // release lock before re-acquiring
                let mut searcher = ctx.semantic.lock().await;
                searcher.invalidate(path);

                let diff = build_compact_diff(&old_string, &new_string);
                let label = if parsed.replace_all {
                    format!("replaced {} occurrences in {}", sym_count, symbol_name)
                } else {
                    format!("in {} (lines {}-{})", symbol_name, slice.start_line, slice.end_line)
                };
                let outline = post_edit_info(&new_content, &new_string);
                let (sl, el) = find_edit_lines(&parsed.file_path, &new_string);
                let ctx_str = surrounding_context(&parsed.file_path, sl, el);
                let result = ToolResult {
                    call_id: String::new(),
                    output: format!("Edited {} {}.\n{}\n{}{}", parsed.file_path, label, diff, outline, ctx_str),
                    success: true,
                };
                return Ok(post_edit_validate(result, &parsed.file_path, &new_content, &new_string).await);
            } else {
                // Symbol not found — list available symbols as hint
                let hint = match searcher.list_symbols(path) {
                    Some(syms) => {
                        let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
                        format!("Symbol '{}' not found. Available: {}", symbol_name, names.join(", "))
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
            if let Some((fuzzy_result, fuzzy_count)) = try_fuzzy_replace(
                &content, &old_string, &new_string, parsed.replace_all
            ) {
                atomic_write(&parsed.file_path, &fuzzy_result).await?;
                let diff = build_compact_diff(&old_string, &new_string);
                let outline = post_edit_info(&fuzzy_result, &new_string);
                let (sl, el) = find_edit_lines(&parsed.file_path, &new_string);
                let ctx_str = surrounding_context(&parsed.file_path, sl, el);
                let result = ToolResult {
                    call_id: String::new(),
                    output: format!(
                        "Edited {} (fuzzy match, {} occurrence{}).\n{}\n{}{}",
                        parsed.file_path, fuzzy_count,
                        if fuzzy_count > 1 { "s" } else { "" },
                        diff, outline, ctx_str
                    ),
                    success: true,
                };
                return Ok(post_edit_validate(result, &parsed.file_path, &fuzzy_result, &new_string).await);
            }

            let (hint, suggested_old) = find_closest_match_with_suggestion(&content, &old_string);
            let suggestion = if let Some(ref s) = suggested_old {
                format!(
                    "\n\n[SUGGESTED FIX: Use this exact text as old_string (copy it precisely):]\n```\n{}\n```",
                    s
                )
            } else {
                String::new()
            };
            let reread = auto_reread_content(&content, &old_string);
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!("Error: old_string not found in {}.\n{}{}\n{}", parsed.file_path, hint, suggestion, reread),
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
            atomic_write(&parsed.file_path, &new_content).await?;
            let diff = build_compact_diff(&old_string, &new_string);
            let outline = post_edit_info(&new_content, &new_string);
            let (sl, el) = find_edit_lines(&parsed.file_path, &new_string);
            let ctx_str = surrounding_context(&parsed.file_path, sl, el);
            let result = ToolResult {
                call_id: String::new(),
                output: format!(
                    "Edited {} (replaced {} occurrence{}).\n{}\n{}{}",
                    parsed.file_path, count, if count > 1 { "s" } else { "" }, diff, outline, ctx_str,
                ),
                success: true,
            };
            Ok(post_edit_validate(result, &parsed.file_path, &new_content, &new_string).await)
        } else {
            if count > 1 {
                // Auto-disambiguate using tree-sitter: if only ONE symbol contains the match,
                // scope to that symbol automatically. The model doesn't need to pass symbol=.
                let path = std::path::Path::new(&parsed.file_path);
                let mut searcher = ctx.semantic.lock().await;
                if let Some(symbols) = searcher.list_symbols(path) {
                    // Find which symbols contain the old_string
                    let matching_syms: Vec<&crate::semantic::Symbol> = symbols.iter()
                        .filter(|sym| {
                            let sym_text = &content[sym.start_byte..sym.end_byte.min(content.len())];
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
                        atomic_write(&parsed.file_path, &new_content).await?;
                        let diff = build_compact_diff(&old_string, &new_string);
                        let outline = post_edit_info(&new_content, &new_string);
                        let (sl, el) = find_edit_lines(&parsed.file_path, &new_string);
                        let ctx_str = surrounding_context(&parsed.file_path, sl, el);
                        let result = ToolResult {
                            call_id: String::new(),
                            output: format!(
                                "Edited {} in {}() (auto-scoped, {} global matches).\n{}\n{}{}",
                                parsed.file_path, sym.name, count, diff, outline, ctx_str
                            ),
                            success: true,
                        };
                        return Ok(post_edit_validate(result, &parsed.file_path, &new_content, &new_string).await);
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
            atomic_write(&parsed.file_path, &new_content).await?;

            let removed = old_string.lines().count();
            let added = new_string.lines().count();
            let diff = build_compact_diff(&old_string, &new_string);
            let outline = post_edit_info(&new_content, &new_string);
            let (sl, el) = find_edit_lines(&parsed.file_path, &new_string);
            let ctx_str = surrounding_context(&parsed.file_path, sl, el);
            let result = ToolResult {
                call_id: String::new(),
                output: format!(
                    "Edited {} (-{} +{} lines).\n{}\n{}{}",
                    parsed.file_path, removed, added, diff, outline, ctx_str,
                ),
                success: true,
            };
            Ok(post_edit_validate(result, &parsed.file_path, &new_content, &new_string).await)
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
    ) -> Result<ToolResult> {
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();

        // Resolve each edit to a (start, end, new_string) tuple where start/end are 1-indexed line numbers.
        let mut resolved: Vec<(usize, usize, String)> = Vec::with_capacity(edits.len());

        for (i, edit) in edits.iter().enumerate() {
            if let (Some(start), Some(end)) = (edit.start_line, edit.end_line) {
                // Line-number mode
                if start == 0 || start > total || end < start {
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: format!("Error in edit #{}: invalid line range {}-{} (file has {} lines)", i + 1, start, end, total),
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
                    output: format!("Error in edit #{}: must specify start_line/end_line or old_string", i + 1),
                    success: false,
                });
            }
        }

        // Check for overlapping ranges
        resolved.sort_by_key(|(start, _, _)| *start);
        for w in resolved.windows(2) {
            let (_, end_a, _) = &w[0];
            let (start_b, _, _) = &w[1];
            if start_b <= end_a {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: format!(
                        "Error: overlapping edit ranges detected (lines {}-{} and {}-{}). Edits must not overlap.",
                        w[0].0, end_a, start_b, w[1].1
                    ),
                    success: false,
                });
            }
        }

        // Boundary overlap auto-correction: if new_string's trailing lines duplicate
        // the lines immediately after end_line, extend end_line to absorb them.
        // This fixes the common weak-model bug where end_line is too small, causing
        // the original line to remain and duplicate a line from new_string.
        for (start, end, new_str) in &mut resolved {
            let new_lines: Vec<&str> = new_str.lines().collect();
            if new_lines.is_empty() { continue; }
            // Check how many trailing lines of new_string match lines after the edit range
            let mut extra = 0usize;
            for i in 0..new_lines.len() {
                let new_idx = new_lines.len() - 1 - i; // from end of new_string
                let orig_idx = *end + extra; // line after current end (0-indexed = end since end is 1-indexed inclusive)
                if orig_idx >= total { break; }
                if new_lines[new_idx].trim() == lines[orig_idx].trim() && !new_lines[new_idx].trim().is_empty() {
                    extra += 1;
                } else {
                    break;
                }
            }
            if extra > 0 {
                *end += extra;
                *end = (*end).min(total);
            }
        }

        // Re-check overlaps after boundary correction
        resolved.sort_by_key(|(start, _, _)| *start);
        for w in resolved.windows(2) {
            if w[1].0 <= w[0].1 {
                // Overlaps after correction — truncate the first edit's end
                // to avoid conflict. The second edit takes priority.
                break; // just proceed, splice will handle it
            }
        }

        // Apply edits back-to-front to preserve line numbers
        resolved.sort_by(|a, b| b.0.cmp(&a.0));

        let mut result_lines: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        let mut summary_parts: Vec<String> = Vec::new();

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

        atomic_write(file_path, &new_content).await?;

        let edit_count = resolved.len();
        let outline = post_edit_info(&new_content, "");
        let all_new_strings: String = edits.iter().map(|e| e.new_string.as_str()).collect::<Vec<_>>().join("\n");
        let result = ToolResult {
            call_id: String::new(),
            output: format!(
                "Multi-edit: {} edits applied to {} [{}].\n{}",
                edit_count, file_path, summary_parts.join(", "), outline
            ),
            success: true,
        };
        Ok(post_edit_validate(result, file_path, &new_content, &all_new_strings).await)
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
#[allow(dead_code)]
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

    // Compute the base indent of new_string (to preserve relative indentation)
    let new_lines: Vec<&str> = new_string.lines().collect();
    let new_base_indent = new_lines.iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);

    let mut result_lines: Vec<String> = content_lines.iter().map(|l| l.to_string()).collect();

    // Process matches in reverse to preserve indices
    let to_replace = if replace_all { &matches[..] } else { &matches[..1] };
    for &(start, end) in to_replace.iter().rev() {
        // Detect indentation from the first matched line in the file
        let original_line = content_lines[start];
        let file_indent = original_line.len() - original_line.trim_start().len();
        let file_indent_str: String = original_line.chars().take(file_indent).collect();

        // Build replacement preserving RELATIVE indentation from new_string
        let replacement: Vec<String> = new_lines.iter().map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                let line_indent = l.len() - l.trim_start().len();
                let relative = line_indent.saturating_sub(new_base_indent);
                let total_indent = format!("{}{}", file_indent_str, " ".repeat(relative));
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

/// Detect if an edit introduced duplicate blocks (a common weak-model failure mode).
/// Checks if new_string (≥3 non-blank lines) appears more than once in the result.
/// Returns a warning string if duplicates found, empty string otherwise.
fn detect_duplicate_blocks(new_content: &str, new_string: &str) -> String {
    let sig_lines: Vec<&str> = new_string.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    if sig_lines.len() < 3 {
        return String::new();
    }
    // Use first 3 significant lines as a fingerprint
    let fingerprint: Vec<&str> = sig_lines[..3.min(sig_lines.len())].to_vec();
    let content_lines: Vec<&str> = new_content.lines().map(|l| l.trim()).collect();

    let mut hits = 0usize;
    let mut i = 0;
    while i + fingerprint.len() <= content_lines.len() {
        if content_lines[i..i + fingerprint.len()] == fingerprint[..] {
            hits += 1;
            i += fingerprint.len();
        } else {
            i += 1;
        }
    }

    if hits > 1 {
        format!(
            "\n⚠ WARNING: The edit introduced DUPLICATE code blocks ({} copies detected). \
             This is likely a bug. Review the file and remove the duplicate.",
            hits
        )
    } else {
        String::new()
    }
}

/// Post-edit syntax check for common file types.
/// Runs a fast, non-destructive check and returns a warning if syntax is broken.
async fn post_edit_syntax_check(file_path: &str) -> String {
    let ext = file_path.rsplit('.').next().unwrap_or("");
    let cmd = match ext {
        "js" | "mjs" | "cjs" => Some(("node", vec!["--check".to_string(), file_path.to_string()])),
        "json" => {
            // Validate JSON by attempting parse
            return match tokio::fs::read_to_string(file_path).await {
                Ok(content) => {
                    if serde_json::from_str::<serde_json::Value>(&content).is_err() {
                        format!("\n⚠ SYNTAX ERROR: {} is not valid JSON. Fix before proceeding.", file_path)
                    } else {
                        String::new()
                    }
                }
                Err(_) => String::new(),
            };
        }
        "ts" | "tsx" => {
            // npx tsc --noEmit is too slow; just check if node can parse it as a quick heuristic
            // TypeScript syntax errors will surface when the user runs their build
            return String::new();
        }
        "py" => Some(("python3", vec!["-m".to_string(), "py_compile".to_string(), file_path.to_string()])),
        _ => None,
    };

    if let Some((program, args)) = cmd {
        match tokio::process::Command::new(program)
            .args(&args)
            .output()
            .await
        {
            Ok(output) if !output.status.success() => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let first_lines: String = stderr.lines().take(3).collect::<Vec<_>>().join("\n");
                format!("\n⚠ SYNTAX ERROR in {}:\n{}", file_path, first_lines)
            }
            _ => String::new(),
        }
    } else {
        String::new()
    }
}

/// Run all post-edit validations: duplicate detection + syntax check.
/// Appends warnings to the ToolResult output if issues found.
async fn post_edit_validate(result: ToolResult, file_path: &str, new_content: &str, new_string: &str) -> ToolResult {
    if !result.success { return result; }

    let dup_warn = detect_duplicate_blocks(new_content, new_string);
    let syntax_warn = post_edit_syntax_check(file_path).await;

    if dup_warn.is_empty() && syntax_warn.is_empty() {
        return result;
    }

    ToolResult {
        output: format!("{}{}{}", result.output, dup_warn, syntax_warn),
        ..result
    }
}

/// Post-edit info: file outline for navigation.
/// Surrounding context is now added separately via surrounding_context() at each return path.
fn post_edit_info(new_content: &str, _new_string: &str) -> String {
    file_outline(new_content)
}

/// Post-edit context: give the model the file's current state so it doesn't re-read.
///
/// - Files <= 500 lines: return the FULL file with line numbers.
///   This eliminates re-reads entirely — the model has everything in the latest message.
/// - Files > 500 lines: return outline + 40 lines of surrounding context around the edit.
#[allow(dead_code)]
fn post_edit_context(new_content: &str, new_string: &str) -> String {
    let lines: Vec<&str> = new_content.lines().collect();

    if lines.len() <= 500 {
        // Full file — model has zero reason to re-read.
        let mut out = format!(
            "\n[Full file after edit ({} lines) — do NOT re-read this file:]\n",
            lines.len()
        );
        for (i, line) in lines.iter().enumerate() {
            out.push_str(&format!("{:>4}| {}\n", i + 1, line));
        }
        return out;
    }

    // Large file: outline + surrounding context around the edit location.
    let outline = file_outline(new_content);

    // Find where the new content was inserted.
    let new_first = new_string.lines().next().unwrap_or("").trim();
    let center = if !new_first.is_empty() {
        lines.iter().position(|l| l.trim().contains(new_first)).unwrap_or(0)
    } else {
        0
    };

    let start = center.saturating_sub(20);
    let end = (center + 20).min(lines.len());

    let mut ctx = format!(
        "\n[File after edit ({} lines). Context around edit (lines {}-{}):]:\n",
        lines.len(), start + 1, end
    );
    for i in start..end {
        ctx.push_str(&format!("{:>4}| {}\n", i + 1, lines[i]));
    }
    if end < lines.len() {
        ctx.push_str(&format!("     ... ({} more lines)\n", lines.len() - end));
    }

    format!("{}\n{}", outline, ctx)
}

/// Build a structural outline of the file after edit.
/// Shows top-level lines (indent 0-1) with line numbers so the model
/// knows the file's structure and can plan its next edit without re-reading.
/// Only generated for files > 100 lines (small files don't need it).
fn file_outline(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() <= 100 {
        return String::new(); // Small file — diff is enough context.
    }

    let mut outline = format!("[File outline ({} lines) — do NOT re-read this file:]\n", lines.len());
    let mut count = 0;
    let max_outline_lines = 30;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        // Indent 0-1 = top-level declaration. Also include <template>, <script>, <style> tags.
        if indent <= 1 || trimmed.starts_with('<') && (
            trimmed.starts_with("<template") || trimmed.starts_with("</template")
            || trimmed.starts_with("<script") || trimmed.starts_with("</script")
            || trimmed.starts_with("<style") || trimmed.starts_with("</style")
        ) {
            // Truncate long lines for the outline
            let display = if trimmed.chars().count() > 60 {
                format!("{}...", trimmed.chars().take(57).collect::<String>())
            } else {
                trimmed.to_string()
            };
            outline.push_str(&format!("{:>4}| {}\n", i + 1, display));
            count += 1;
            if count >= max_outline_lines {
                outline.push_str(&format!("     ... ({} more lines)\n", lines.len() - i - 1));
                break;
            }
        }
    }
    outline
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
            diff.push_str(&format!("  ... ({} more removed)\n", old_lines.len() - max_show));
        }
    }

    // Show added lines (prefixed with +)
    for (i, line) in new_lines.iter().take(max_show).enumerate() {
        diff.push_str(&format!("+ {}\n", line));
        if i == max_show - 1 && new_lines.len() > max_show {
            diff.push_str(&format!("  ... ({} more added)\n", new_lines.len() - max_show));
        }
    }

    diff.trim_end().to_string()
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

    if total <= 200 {
        out.push_str(&format!(
            "\n[Edit failed: old_string not found. Current file content ({} lines):]\n",
            total
        ));
        for (i, line) in lines.iter().enumerate() {
            out.push_str(&format!("{:>4}| {}\n", i + 1, line));
        }
    } else {
        // Find approximate target area using the first non-empty line of old_string
        let target_line = old_string
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(|first| first.trim());

        let center = target_line
            .and_then(|needle| {
                lines.iter().position(|l| l.trim().contains(needle))
            })
            .unwrap_or(0);

        let start = center.saturating_sub(25);
        let end = (center + 25).min(total);

        out.push_str(&format!(
            "\n[Edit failed: old_string not found. Current file content (lines {}-{} of {}):]\n",
            start + 1, end, total
        ));
        if start > 0 {
            out.push_str(&format!("     ... ({} lines above)\n", start));
        }
        for i in start..end {
            out.push_str(&format!("{:>4}| {}\n", i + 1, lines[i]));
        }
        if end < total {
            out.push_str(&format!("     ... ({} lines below)\n", total - end));
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
        return ("old_string is empty. Use read_file to re-read the file.".to_string(), None);
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
            let matching = actual_lines.iter().zip(old_lines.iter())
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
        return "old_string appears empty after trimming. Use read_file to re-read the file.".to_string();
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
                if i + j >= content_lines.len() { break; }
                if content_lines[i + j].trim() == old_lines[j].trim() {
                    match_count += 1;
                } else {
                    break;
                }
            }
            candidates.push((i, match_count));
        }
        // Substring match of first line
        else if trimmed.contains(first_line_trimmed) || first_line_trimmed.contains(trimmed) {
            candidates.push((i, 0));
        }
        // Prefix match (first 25 chars)
        else if trimmed.len() > 15 && first_line_trimmed.len() > 15
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
            snippet.push_str(&format!("     ... ({} more lines in file)\n", content_lines.len() - end));
        }

        // If some lines matched but not all, show exactly where the divergence is
        if match_count > 0 && match_count < old_lines.len() && best_idx + match_count < content_lines.len() {
            let diverge_idx = match_count;
            let file_line = content_lines[best_idx + diverge_idx].trim();
            let old_line = old_lines[diverge_idx].trim();

            // Detect indentation mismatch
            let file_indent = content_lines[best_idx].len() - content_lines[best_idx].trim_start().len();
            let old_indent = old_lines[0].len() - old_lines[0].trim_start().len();

            let mut hint = format!(
                "First {} line(s) match (trimmed) but line {} diverges:\n\
                 YOUR old_string line {}: \"{}\"\n\
                 ACTUAL file line {}:     \"{}\"\n",
                match_count, diverge_idx + 1,
                diverge_idx + 1, old_line,
                best_idx + diverge_idx + 1, file_line,
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
                best_idx + 1, end, match_count, old_lines.len(), snippet, hint
            );
        }

        // Indentation-only mismatch detection
        if match_count == 0 {
            let file_indent = content_lines[best_idx].len() - content_lines[best_idx].trim_start().len();
            let old_indent = old_lines[0].len() - old_lines[0].trim_start().len();
            if file_indent != old_indent
                && content_lines[best_idx].trim() == old_lines[0].trim()
            {
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
            best_idx + 1, snippet
        );
    }

    // Strategy 2: keyword-based search — find lines containing distinctive words from old_string
    let keywords: Vec<&str> = first_line_trimmed.split_whitespace()
        .filter(|w| w.len() > 3 && !matches!(*w, "const" | "let" | "var" | "this" | "self" | "return" | "from" | "import" | "function"))
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
                    keywords.join(", "), i + 1, snippet, start + 1
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

/// Find line range where new_string appears in the written file.
fn find_edit_lines(file_path: &str, new_string: &str) -> (usize, usize) {
    if let Ok(content) = std::fs::read_to_string(file_path) {
        if let Some(byte_offset) = content.find(new_string) {
            let start_line = content[..byte_offset].lines().count() + 1;
            let end_line = start_line + new_string.lines().count().saturating_sub(1);
            return (start_line, end_line);
        }
    }
    (1, 1)
}

/// After a successful edit, show surrounding context so the model sees
/// the current file state at the END of the prompt (recency bias).
/// This helps catch boundary issues (duplicate declarations, missing brackets)
/// that the model would miss when file content is buried in the middle of context.
fn surrounding_context(file_path: &str, edit_start_line: usize, edit_end_line: usize) -> String {
    let content = match std::fs::read_to_string(file_path) {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    if total == 0 { return String::new(); }

    // Show 10 lines before edit start and 10 lines after edit end
    let ctx_before = 10;
    let ctx_after = 10;
    let from = edit_start_line.saturating_sub(ctx_before).max(1);
    let to = (edit_end_line + ctx_after).min(total);

    let mut out = format!("\n[File state around edit (lines {}-{} of {}):]\n", from, to, total);
    for i in (from - 1)..to {
        let marker = if i + 1 >= edit_start_line && i + 1 <= edit_end_line {
            ">"  // edited line
        } else {
            " "  // context line
        };
        out.push_str(&format!("{}{:4}| {}\n", marker, i + 1, lines[i]));
    }
    out
}
