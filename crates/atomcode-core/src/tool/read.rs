use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

pub struct ReadFileTool;

#[derive(Deserialize)]
struct ReadFileArgs {
    file_path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReadFileTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "read_file",
            description: "Read the contents of a file. Returns the file text with line numbers. ALWAYS read the entire file (omit offset/limit) unless the file is known to be very large (1000+ lines). Do NOT read files in small chunks.",
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "Absolute path to the file to read" },
                    "offset": { "type": "integer", "description": "Line number to start reading from (1-based). Only use for files over 1000 lines." },
                    "limit": { "type": "integer", "description": "Max lines to read. Defaults to 2000. Only set for files over 1000 lines." }
                },
                "required": ["file_path"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: ReadFileArgs = serde_json::from_str(args)?;
        let path = std::path::Path::new(&parsed.file_path);

        // Auto-recover: if the path is a directory, return a listing instead of an error.
        if path.is_dir() {
            let mut entries: Vec<String> = Vec::new();
            if let Ok(mut rd) = tokio::fs::read_dir(path).await {
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

        let bytes = tokio::fs::read(&parsed.file_path).await?;

        // Check if the file is valid UTF-8; if not, report it as binary.
        let content = match String::from_utf8(bytes.clone()) {
            Ok(s) => s,
            Err(_) => {
                let output = format!(
                    "Binary file ({} bytes), cannot display as text.",
                    bytes.len()
                );
                return Ok(ToolResult { call_id: String::new(), output, success: true });
            }
        };

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();
        let offset = parsed.offset.unwrap_or(1).max(1) - 1;

        // Smart limit selection:
        // - Small files (≤300 lines): auto-expand small requests to return the whole file
        // - Large files (>300 lines) with no offset/limit: return first 200 lines + hint to use grep
        // - Explicit limit: respect it (but auto-expand if file is small)
        let (limit, large_file_truncated) = match (parsed.offset, parsed.limit) {
            (None, None) if total_lines > 300 => {
                // Large file, no targeting — return head + suggest grep
                (200, true)
            }
            (_, Some(l)) if l < 200 && total_lines <= 300 => {
                // Small file + small limit — auto-expand to whole file
                (total_lines, false)
            }
            (_, Some(l)) => (l, false),
            (_, None) => (2000, false),
        };

        // If offset > 0 but auto-expand would give the whole file, reset offset to 0
        let offset = if offset > 0 && limit >= total_lines { 0 } else { offset };

        let end = (offset + limit).min(total_lines);
        let returned_all = offset == 0 && end >= total_lines;

        let mut output: String = lines[offset..end]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{:>4}| {}", offset + i + 1, line))
            .collect::<Vec<_>>()
            .join("\n");

        // Tell the model what it got and what to do next
        if large_file_truncated {
            output.push_str(&format!(
                "\n\n[LARGE FILE: {} lines total. Showing first {} lines. \
                 Use the grep tool to find specific functions/variables, then read_file with offset to see that section. \
                 Do NOT re-read from line 1.]",
                total_lines, end
            ));
        } else if returned_all {
            output.push_str(&format!(
                "\n\n[COMPLETE FILE: {} lines. You have everything. Do NOT re-read sections of this file.]",
                total_lines
            ));
        } else {
            output.push_str(&format!(
                "\n\n[Showing lines {}-{} of {} total.]",
                offset + 1, end, total_lines
            ));
        }

        Ok(ToolResult { call_id: String::new(), output, success: true })
    }
}
