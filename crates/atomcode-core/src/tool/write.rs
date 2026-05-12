use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

/// Skeleton+edits protocol directive returned to the model when a
/// write_file call is rejected for being too large.
///
/// `streamed_bytes` is `None` for the streaming-sentinel path (we don't
/// have a useful number — the args were truncated mid-stream so the
/// length here would mislead). `Some(n)` for the post-stream tier-2
/// path where `n` is the actual full content length the model produced.
///
/// The directive is a copy-paste-ready protocol: marker syntax, exact
/// step ordering, concrete tool call examples. Weak models like glm-5.1
/// respond to imitable templates far better than abstract guidance —
/// "write a skeleton" alone gets ignored; "write a skeleton like THIS"
/// gets followed.
fn build_large_write_directive(file_path: &str, streamed_bytes: Option<usize>) -> String {
    let context_line = match streamed_bytes {
        Some(n) => format!(
            "write_file rejected: content was {} KB. Provider output is hard-capped \
             at ~16K tokens (~38 KB of JSON-encoded content), so anything over ~30 KB \
             deterministically hits `finish_reason=length` mid-stream and wastes 4-5 \
             min per attempt.",
            n / 1024
        ),
        None => "write_file aborted by the framework at the streaming layer — the \
                 args stream had already exceeded 32 KB and would have hit the \
                 provider's output token cap with no way to finish. No retry will \
                 succeed: provider output is hard-capped at ~16K tokens ≈ 38 KB of \
                 JSON-encoded content."
            .to_string(),
    };
    // First line is a UI hook — TUI matches the exact prefix
    // `LARGE_WRITE_AUTOCONVERT_TAG` and replaces the scrollback row with a
    // compact friendly indicator, so the user sees `◇ write_file →
    // skeleton+edits (auto-converted)` instead of a wall of protocol text
    // they don't need to read (the model does — it sees the full body
    // through the conversation). Keep the tag verbatim if you ever
    // reformat this — see `tool/mod.rs::LARGE_WRITE_AUTOCONVERT_TAG`.
    format!(
        "{tag}\n\
         {context_line}\n\
         \n\
         REQUIRED PROTOCOL — switch to this in your VERY NEXT assistant message:\n\
         \n\
         Step 1 — write the SKELETON (one write_file call, ≤5 KB total):\n\
           write_file({{\"file_path\":\"{path}\",\"content\":\"<full structural shell with \
         section placeholders only>\"}})\n\
           The skeleton MUST include every section's container element plus a marker \
         comment for each. Use the marker syntax:\n\
             <!-- SECTION:risks -->\n\
             <!-- SECTION:licenses -->\n\
             <!-- SECTION:vulnerabilities -->\n\
           …one marker per section. Body of each section is JUST the marker — no \
         real content yet.\n\
         \n\
         Step 2 — fill each section with ONE edit_file call (or parallel_edit_files \
         when sections are independent):\n\
           edit_file({{\"file_path\":\"{path}\",\"old_string\":\"<!-- SECTION:risks -->\",\"new_string\":\"<actual risks content>\"}})\n\
           edit_file({{\"file_path\":\"{path}\",\"old_string\":\"<!-- SECTION:licenses -->\",\"new_string\":\"<actual licenses content>\"}})\n\
           …one edit per section.\n\
         \n\
         Each step's tool call stays small (≤30 KB args), so no single call hits the \
         output cap. Do NOT retry write_file with the original large content — it will \
         fail the same way.",
        tag = crate::tool::LARGE_WRITE_AUTOCONVERT_TAG,
        context_line = context_line,
        path = file_path,
    )
}

pub struct WriteFileTool;

#[derive(Deserialize)]
struct WriteFileArgs {
    file_path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteFileTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "write_file",
            description:
                "Writes a file to the local filesystem.\n\
                \n\
                Usage:\n\
                - Prefer the edit_file tool for modifying existing files — it only sends the diff. \
                Only use this tool to create new files or for complete rewrites.\n\
                - ALWAYS prefer editing existing files in the codebase. NEVER write new files unless explicitly required.\n\
                - Parent directories are auto-created if they don't exist.\n\
                - Overwriting an existing file is blocked unless explicitly intended — use edit_file for changes, not write_file.\n\
                \n\
                HARD LIMIT — content body MUST stay under 30 KB / ~800 lines per call:\n\
                The model's output is capped at ~16K tokens per response (~38 KB JSON-encoded). \
                Any single write_file over that cap deterministically gets truncated mid-stream by \
                the provider and wastes 4-5 min before failing. There is NO workaround at this layer — \
                the cap is enforced by the upstream model, not by AtomCode.\n\
                \n\
                For larger files use the SKELETON PROTOCOL (mandatory, not optional):\n\
                  Step 1 — write_file the skeleton (≤5 KB): full structural shell with one marker \
                  per section, body of each section is JUST the marker. Use this exact marker syntax:\n\
                    <!-- SECTION:name -->     (HTML / XML / Markdown)\n\
                    // SECTION:name           (Rust / TS / JS / Go / C-family)\n\
                    # SECTION:name            (Python / shell / YAML)\n\
                  Step 2 — one edit_file (or one parallel_edit_files when sections are independent) \
                  per section, replacing the marker with the real content. Each edit_file's args \
                  stay under 30 KB.\n\
                \n\
                If you submit a write_file with content over the cap, AtomCode aborts at 32 KB of \
                streamed args and returns a tool failure with the protocol directive — you will see \
                this as `success: false` plus the skeleton template in the next assistant message's \
                tool_result. Switch immediately to skeleton+edits when that happens; do not retry \
                with the same large content."
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "Absolute path to the file" },
                    "content": { "type": "string", "description": "The full content to write" }
                },
                "required": ["file_path", "content"]
            }),
        }
    }

    fn validate_args(&self, args: &str) -> std::result::Result<(), String> {
        // Surface a model-friendly diagnostic (provided/missing keys + a
        // one-line example) instead of the raw serde "line 1 column N"
        // error which weak models read as a parser-position complaint and
        // try to "fix" by switching to positional arguments. See
        // `diagnose_args` doc for the failure mode this replaces.
        super::diagnose_args(
            "write_file",
            args,
            &[&["file_path", "content"]],
            "write_file({\"file_path\": \"<absolute path>\", \"content\": \"<file body>\"})",
        )?;
        // Strict struct parse only AFTER the keys are known to be present
        // — catches type mismatches (e.g. content sent as an array).
        serde_json::from_str::<WriteFileArgs>(args)
            .map(|_| ())
            .map_err(|e| {
                format!(
                    "write_file: {e}. Re-issue with file_path as a string and content as a string."
                )
            })
    }

    fn approval(&self, args: &str) -> ApprovalRequirement {
        let parsed = match serde_json::from_str::<WriteFileArgs>(args) {
            Ok(p) => p,
            Err(_) => {
                // Fail-closed: if we can't parse args, require approval rather than auto-approving.
                return ApprovalRequirement::RequireApproval(
                    "Could not parse create_file arguments for safety check.".to_string(),
                );
            }
        };
        if super::is_sensitive_input_path(&parsed.file_path) {
            return ApprovalRequirement::RequireApproval(
                format!("Writing to sensitive system path: {}", parsed.file_path),
            );
        }
        // Overwriting existing files is blocked in execute() — no need to
        // RequireApproval here. Only new file creation is auto-approved.
        ApprovalRequirement::AutoApprove
    }

    fn approval_with_context(&self, args: &str, ctx: &ToolContext) -> ApprovalRequirement {
        let base = self.approval(args);
        let parsed = match serde_json::from_str::<WriteFileArgs>(args) {
            Ok(parsed) => parsed,
            Err(_) => return base,
        };
        let working_dir = match ctx.working_dir.try_read() {
            Ok(wd) => wd.clone(),
            Err(_) => return base,
        };
        match super::approval_for_path(
            &parsed.file_path,
            &working_dir,
            super::ExternalPathAction::Write,
        ) {
            Ok(ApprovalRequirement::RequireApprovalAlways(reason)) => {
                ApprovalRequirement::RequireApprovalAlways(reason)
            }
            Ok(ApprovalRequirement::RequireApproval(reason)) => {
                ApprovalRequirement::RequireApproval(reason)
            }
            Ok(ApprovalRequirement::AutoApprove) => match base {
                ApprovalRequirement::RequireApproval(reason) => {
                    ApprovalRequirement::RequireApprovalAlways(reason)
                }
                other => other,
            },
            Err(_) => base,
        }
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        // Defense-in-depth: validate_args runs at the runner gate, but if
        // it's bypassed (or args mutated between gate and execute), we fall
        // back to the same diagnose_args path so the model sees a uniform
        // recovery hint instead of a raw serde error.
        if let Err(msg) = super::diagnose_args(
            "write_file",
            args,
            &[&["file_path", "content"]],
            "write_file({\"file_path\": \"<absolute path>\", \"content\": \"<file body>\"})",
        ) {
            return Ok(ToolResult {
                call_id: String::new(),
                output: msg,
                success: false,
            });
        }
        let parsed: WriteFileArgs = match serde_json::from_str(args) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: format!(
                        "write_file: {e}. Re-issue with file_path as a string and content as a string."
                    ),
                    success: false,
                });
            }
        };
        // ── Two-tier large-file guard ──
        //
        // PHYSICS: provider output is capped at ~16K tokens (openai.rs:74)
        // ≈ 48KB raw output bytes ≈ ~38KB of JSON-encoded `content` after
        // escape overhead. ANYTHING above that on a single write_file call
        // deterministically hits `finish_reason=length` mid-stream — 4-5 min
        // wasted per attempt on weak models (glm-5.1 / Kimi / DeepSeek).
        //
        // TIER 1 — streaming abort sentinel (cheap path, ~15s)
        //   When `provider/openai.rs` detects accumulated args > 32KB for a
        //   write_file call, it synthesises a clean ToolCallDone whose
        //   `content` field is `LARGE_WRITE_SENTINEL` (below). That signal
        //   reaches us here and we reply with the skeleton-protocol
        //   directive WITHOUT touching disk — the model sees a normal
        //   tool failure and self-heals in the next round (same turn).
        //   Total user-facing latency: ~15s + one short LLM round.
        //
        // TIER 2 — post-stream content cap (defense-in-depth, ~5min)
        //   Even if the streaming sentinel was bypassed (proxy buffered
        //   the entire response before flushing, repair_tool_args
        //   reconstructed a too-large content from a partial JSON stream,
        //   a future provider doesn't run the streaming check, etc.),
        //   any `parsed.content` over LARGE_WRITE_CONTENT_CAP gets
        //   rejected with the same directive. Slower path but identical
        //   semantics — model still self-heals same-turn.
        //
        // The two tiers share the directive message so model behaviour
        // is the same whether the early abort fired or not — no separate
        // mental model needed.
        if parsed.content == crate::tool::LARGE_WRITE_SENTINEL {
            return Ok(ToolResult {
                call_id: String::new(),
                output: build_large_write_directive(&parsed.file_path, None),
                success: false,
            });
        }
        const LARGE_WRITE_CONTENT_CAP: usize = 30 * 1024;
        if parsed.content.len() > LARGE_WRITE_CONTENT_CAP {
            return Ok(ToolResult {
                call_id: String::new(),
                output: build_large_write_directive(
                    &parsed.file_path,
                    Some(parsed.content.len()),
                ),
                success: false,
            });
        }

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

        // Backup before write (git checkpoint + file-level backup)
        ctx.file_history
            .lock()
            .await
            .backup_before_write(&path.to_string_lossy())
            .await;

        // Check if overwriting existing file — build appropriate output message
        let overwrite_info = if path.exists() {
            let old_lines = std::fs::read_to_string(&path)
                .map(|c| c.lines().count())
                .unwrap_or(0);
            Some(old_lines)
        } else {
            None
        };

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let new_lines = parsed.content.lines().count();
        let bytes = parsed.content.len();
        tokio::fs::write(&path, &parsed.content).await?;

        // D3: drop any FileStore entry for this path. The next peek_file
        // against the old store_id will report "stale" and route the
        // model toward a fresh read_file. Without this invalidation a
        // peek_file could hand the model pre-write content that no
        // longer matches what just landed on disk.
        ctx.file_store.write().await.invalidate(&path);
        // Defense-in-depth: read_cache mtime gate is normally sufficient
        // because tokio::fs::write bumps mtime, but on FS with coarse
        // mtime granularity (ext4 1-second precision, NFS) a write within
        // the same tick as the prior read keeps the same mtime and the
        // gate stops protecting us. Explicit purge eliminates that
        // corner case for any path we just wrote.
        ctx.read_cache
            .write()
            .await
            .retain(|(p, _, _), _| p != &path);

        // Notify LSP that file changed (if LSP is enabled).
        ctx.notify_lsp_file_changed(&path, &parsed.content).await;

        let output = if let Some(old_lines) = overwrite_info {
            let diff = new_lines as i64 - old_lines as i64;
            let sign = if diff >= 0 { "+" } else { "" };
            let mut msg = format!(
                "Overwrote {} (was {} lines, now {} lines, {}{})",
                path.display(),
                old_lines,
                new_lines,
                sign,
                diff
            );
            // Warn if significant content reduction (might have lost code)
            if old_lines > 20 && new_lines < old_lines / 2 {
                msg.push_str(&format!(
                    "\n⚠ WARNING: File shrank by {}%. Verify no important code was lost. Use /undo to revert if needed.",
                    100 - (new_lines * 100 / old_lines)
                ));
            }
            msg
        } else {
            format!(
                "Created new file {} ({} bytes, {} lines)",
                path.display(),
                bytes,
                new_lines
            )
        };

        Ok(ToolResult {
            call_id: String::new(),
            output,
            success: true,
        })
    }
}

#[cfg(test)]
mod large_write_directive_tests {
    use super::*;

    /// The autoconvert tag must be the FIRST line of the directive so the
    /// TUI's `output.starts_with(LARGE_WRITE_AUTOCONVERT_TAG)` check fires
    /// cleanly. If the tag ever moves off line 1 (or a leading newline
    /// sneaks in via a format-string change), the user will see the full
    /// protocol wall-of-text in scrollback instead of the compact `↪
    /// auto-converting` row.
    #[test]
    fn directive_begins_with_autoconvert_tag() {
        let none_path = build_large_write_directive("/abs/report.html", None);
        assert!(
            none_path.starts_with(crate::tool::LARGE_WRITE_AUTOCONVERT_TAG),
            "stream-cap directive must lead with the TUI marker tag"
        );
        let some_path = build_large_write_directive("/abs/report.html", Some(40_000));
        assert!(
            some_path.starts_with(crate::tool::LARGE_WRITE_AUTOCONVERT_TAG),
            "execute-cap directive must lead with the TUI marker tag"
        );
    }

    /// Path interpolation must reach the example tool calls so the model
    /// can copy-paste them with no edits. Without this the directive
    /// reads "edit_file(\"<path>\", ...)" with a literal `<path>` and
    /// weak models will paste it verbatim — wasting another turn on
    /// `file_path: "<path>"` which then fails as a non-existent path.
    #[test]
    fn directive_embeds_file_path_in_step_examples() {
        let body = build_large_write_directive("/abs/audit.html", Some(40_000));
        // Skeleton step
        assert!(
            body.contains("\"file_path\":\"/abs/audit.html\""),
            "skeleton write_file example must carry the actual path"
        );
        // Both edit_file examples
        assert!(
            body.matches("edit_file({\"file_path\":\"/abs/audit.html\"")
                .count()
                >= 2,
            "edit_file examples must carry the actual path for copy-paste"
        );
    }

    /// Two-tier guard: the streaming-sentinel path (`None`) and the
    /// post-stream content-cap path (`Some(n)`) MUST produce different
    /// context lines so post-hoc grep can tell which gate fired. The
    /// streaming path doesn't know the model's intended full length
    /// (it was cut at 32 KB) — claiming a byte count there would lie.
    #[test]
    fn directive_distinguishes_stream_vs_execute_paths() {
        let stream = build_large_write_directive("/x.html", None);
        let execute = build_large_write_directive("/x.html", Some(50_000));
        assert!(stream.contains("aborted by the framework at the streaming layer"));
        assert!(!stream.contains(" KB."));
        assert!(execute.contains("content was 48 KB"));
    }
}
