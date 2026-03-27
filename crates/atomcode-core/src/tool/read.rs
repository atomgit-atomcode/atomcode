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
            description: "Read the contents of a file. Returns file text with line numbers (cat -n format).\n\
                Usage:\n\
                - file_path must be an absolute path, not a relative path.\n\
                - By default reads the full file. For large files (500+ lines), use offset and limit to read specific sections.\n\
                - When you already know which part of the file you need (e.g. a specific function), only read that part.\n\
                - If the path is a directory, returns a listing of its contents instead of an error.\n\
                - Binary files are detected and reported (not dumped as garbage text).\n\
                - You can call read_file multiple times in parallel to read several files at once.\n\
                - NEVER use bash (cat/head/tail/sed) to read files — always use this tool.",
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

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
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

        // Large files (>500 lines) without explicit offset/limit: cap at 300 lines.
        // Prevents context overflow when the model reads multiple large files in one turn.
        // Model can use offset/limit to read specific sections.
        let limit = match parsed.limit {
            Some(l) => l,
            None => {
                if total_lines > 500 && parsed.offset.is_none() {
                    300
                } else {
                    2000
                }
            }
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

        if !returned_all {
            // Generate skeleton of the unseen portion using tree-sitter.
            // This tells the model what functions/sections exist beyond the truncation point,
            // so it can target-read with offset instead of re-reading the full file.
            let remaining_skeleton = if end < total_lines && parsed.offset.is_none() {
                let mut searcher = ctx.semantic.lock().await;
                if let Some(symbols) = searcher.list_symbols(path) {
                    let beyond: Vec<String> = symbols.iter()
                        .filter(|s| s.start_line > end)
                        .map(|s| {
                            let sig = lines.get(s.start_line - 1)
                                .map(|l| l.trim())
                                .unwrap_or(&s.name);
                            let sig_short = if sig.chars().count() > 60 {
                                format!("{}...", sig.chars().take(57).collect::<String>())
                            } else {
                                sig.to_string()
                            };
                            format!("{:>4}| {}  (L{}-{})", s.start_line, sig_short, s.start_line, s.end_line)
                        })
                        .collect();
                    if !beyond.is_empty() {
                        format!("\n\n[Remaining structure (lines {}-{}):\n{}]",
                            end + 1, total_lines, beyond.join("\n"))
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                }
            } else {
                String::new()
            };

            output.push_str(&format!(
                "\n\n[Showing lines {}-{} of {} total. To read more: offset={} limit=200. \
                 To edit: use start_line/end_line.]{}",
                offset + 1, end, total_lines, end + 1, remaining_skeleton
            ));
        }

        Ok(ToolResult { call_id: String::new(), output, success: true })
    }
}
