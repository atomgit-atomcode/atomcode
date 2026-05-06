//! `peek_file` — D3 companion tool to `read_file`.
//!
//! Why this exists: `read_file` of a large file pushes raw content
//! into the shared `FileStore` and returns a `store_id` pointer plus
//! a small preview. Subsequent regions of that same file are fetched
//! via `peek_file({store_id, lines})` — zero disk hit, no duplicated
//! content carried turn-by-turn. See `crate::ctx::file_store` for the
//! store, and `tool/read.rs` for the upstream half.
//!
//! Stale handling: if the file's on-disk mtime has moved since the
//! store entry was created, `peek_file` refuses with a recovery hint
//! pointing at re-read. `edit_file` / `write_file` proactively
//! invalidate the store entry on success so a stale `store_id` never
//! lingers for a path the model just modified.

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

pub struct PeekFileTool;

#[derive(Deserialize)]
struct PeekArgs {
    store_id: String,
    /// `"100-150"` (inclusive range), or `"100"` (single line). Defaults
    /// to the entire stored content if omitted — useful when the model
    /// peek'd a small region first and now wants the whole file.
    #[serde(default)]
    lines: Option<String>,
}

#[async_trait]
impl Tool for PeekFileTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "peek_file",
            description:
                "Fetch a region of a previously-read file from the local cache. \
                 Use this AFTER read_file when you only need a specific portion. \
                 Free: no disk hit, no extra round trip for duplicate content. \
                 The store_id comes from a prior read_file result.\n\
                 Args:\n\
                 - store_id (required): the id from `read_file` (looks like `fs_abc12345`)\n\
                 - lines (optional): `\"100-150\"` for an inclusive range, or `\"100\"` \
                 for a single line. Omit to get the whole stored content.\n\
                 Errors with a re-read hint if the file was modified since read_file."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "store_id": {
                        "type": "string",
                        "description": "Store id from a prior read_file result (e.g. fs_abc12345)."
                    },
                    "lines": {
                        "type": "string",
                        "description": "Inclusive line range \"X-Y\" or single line \"X\". Optional — omit for full content."
                    }
                },
                "required": ["store_id"]
            }),
        }
    }

    fn validate_args(&self, args: &str) -> std::result::Result<(), String> {
        super::diagnose_args(
            "peek_file",
            args,
            &[&["store_id"]],
            "peek_file({\"store_id\": \"fs_...\", \"lines\": \"100-150\"})",
        )?;
        serde_json::from_str::<PeekArgs>(args).map(|_| ()).map_err(|e| {
            format!(
                "peek_file: {e}. store_id must be a string, lines must be \"X-Y\" or \"X\"."
            )
        })
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        // Read-only, in-memory only — no path/sensitivity surface to gate.
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        if let Err(msg) = super::diagnose_args(
            "peek_file",
            args,
            &[&["store_id"]],
            "peek_file({\"store_id\": \"fs_...\", \"lines\": \"100-150\"})",
        ) {
            return Ok(ToolResult {
                call_id: String::new(),
                output: msg,
                success: false,
            });
        }
        let parsed: PeekArgs = match serde_json::from_str(args) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: format!(
                        "peek_file: {e}. store_id must be a string, lines must be \"X-Y\" or \"X\"."
                    ),
                    success: false,
                });
            }
        };

        // Snapshot the entry while holding the read lock briefly, then
        // release before doing any disk I/O. The mtime check happens
        // outside the lock so we don't block other tools' writes.
        let (path, recorded_mtime, line_count) = {
            let store = ctx.file_store.read().await;
            match store.get(&parsed.store_id) {
                Some(e) => (e.path.clone(), e.mtime, e.line_count),
                None => {
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: format!(
                            "peek_file: unknown store_id `{}`. The id may have been \
                             invalidated by a write/edit, evicted, or never existed. \
                             Re-issue read_file to obtain a fresh store_id.",
                            parsed.store_id
                        ),
                        success: false,
                    });
                }
            }
        };

        // Stale check: file modified since we cached it (other tool wrote
        // outside our edit/write tracking, or the user changed it on
        // disk). Refuse rather than serving outdated bytes — the model
        // would otherwise edit against a snapshot that no longer matches
        // the file's current content.
        let current_mtime = tokio::fs::metadata(&path).await.ok().and_then(|m| m.modified().ok());
        if let Some(cur) = current_mtime {
            if cur != recorded_mtime {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: format!(
                        "peek_file: `{}` has been modified since it was read \
                         (mtime moved). The cached snapshot is stale. Re-issue \
                         read_file to obtain a fresh store_id and updated content.",
                        path.display()
                    ),
                    success: false,
                });
            }
        }

        let (start, end) = match parsed.lines.as_deref() {
            None => (1usize, line_count),
            Some(spec) => match parse_line_spec(spec, line_count) {
                Ok(range) => range,
                Err(msg) => {
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: format!("peek_file: {}", msg),
                        success: false,
                    });
                }
            },
        };

        let region = {
            let store = ctx.file_store.read().await;
            match store.peek_lines(&parsed.store_id, start, end) {
                Some(r) => r,
                None => {
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: format!(
                            "peek_file: store_id `{}` was invalidated between \
                             validation and fetch — re-issue read_file.",
                            parsed.store_id
                        ),
                        success: false,
                    });
                }
            }
        };

        // Render with line numbers so the model can edit_file by line
        // range without doing arithmetic. Same `{:>4}| ` format as
        // read_file uses, so peek output is drop-in.
        let numbered: String = region
            .lines()
            .enumerate()
            .map(|(i, line)| format!("{:>4}| {}", start + i, line))
            .collect::<Vec<_>>()
            .join("\n");
        let display = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| path.display().to_string());

        Ok(ToolResult {
            call_id: String::new(),
            output: format!(
                "[Peek {} L{}-{} of {} (from store_id={}):]\n{}",
                display, start, end, line_count, parsed.store_id, numbered
            ),
            success: true,
        })
    }
}

/// Parse `"100-150"` or `"100"` into an inclusive `(start, end)` range,
/// clamped to `line_count`. Returns Err with a friendly message on
/// malformed input.
fn parse_line_spec(spec: &str, line_count: usize) -> Result<(usize, usize), String> {
    let trimmed = spec.trim();
    if let Some((a, b)) = trimmed.split_once('-') {
        let s: usize = a.trim().parse().map_err(|_| {
            format!(
                "lines spec `{}`: start `{}` is not a number (use \"X-Y\" or \"X\")",
                spec,
                a.trim()
            )
        })?;
        let e: usize = b.trim().parse().map_err(|_| {
            format!(
                "lines spec `{}`: end `{}` is not a number (use \"X-Y\" or \"X\")",
                spec,
                b.trim()
            )
        })?;
        if s == 0 {
            return Err(format!("lines spec `{}`: start must be ≥ 1", spec));
        }
        if e < s {
            return Err(format!(
                "lines spec `{}`: end {} < start {} (give the range in increasing order)",
                spec, e, s
            ));
        }
        Ok((s, e.min(line_count)))
    } else {
        let n: usize = trimmed.parse().map_err(|_| {
            format!(
                "lines spec `{}` is not a number or range (use \"X-Y\" or \"X\")",
                spec
            )
        })?;
        if n == 0 {
            return Err(format!("lines spec `{}`: line numbers are 1-indexed", spec));
        }
        Ok((n, n.min(line_count)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_line_spec_range() {
        assert_eq!(parse_line_spec("10-20", 100).unwrap(), (10, 20));
        assert_eq!(parse_line_spec(" 10 - 20 ", 100).unwrap(), (10, 20));
    }

    #[test]
    fn parse_line_spec_single() {
        assert_eq!(parse_line_spec("42", 100).unwrap(), (42, 42));
    }

    #[test]
    fn parse_line_spec_clamps_end_to_line_count() {
        assert_eq!(parse_line_spec("90-200", 100).unwrap(), (90, 100));
    }

    #[test]
    fn parse_line_spec_rejects_zero_start() {
        assert!(parse_line_spec("0-5", 100).is_err());
        assert!(parse_line_spec("0", 100).is_err());
    }

    #[test]
    fn parse_line_spec_rejects_inverted_range() {
        let err = parse_line_spec("50-10", 100).unwrap_err();
        assert!(err.contains("end"), "msg should mention end < start: {}", err);
    }

    #[test]
    fn parse_line_spec_rejects_garbage() {
        assert!(parse_line_spec("abc", 100).is_err());
        assert!(parse_line_spec("10-x", 100).is_err());
        assert!(parse_line_spec("x-20", 100).is_err());
    }
}
