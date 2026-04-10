use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

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
            description: "Read a file. Returns text with line numbers.\n\
                Reads the FULL file by default — do NOT use offset/limit unless instructed.\n\
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

        // Dynamic skeleton threshold based on remaining context budget.
        // Full content when ctx has room, skeleton when filling up.
        // Single file capped at 20% of remaining budget (÷5).
        let file_tokens = total_lines * 12;
        let budget = ctx.ctx_budget_hint.load(std::sync::atomic::Ordering::Relaxed);
        let single_file_limit = budget / 5;
        let auto_skeleton = file_tokens > single_file_limit
            && parsed.offset.is_none()
            && parsed.limit.is_none();

        if auto_skeleton {
            let mut searcher = ctx.semantic.lock().await;
            let skeleton = if let Some(symbols) = searcher.list_symbols(path) {
                let fname = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
                let mut skel = format!("[File skeleton: {} ({} lines) — to see full content, call read_file again without offset/limit:]\n\n",
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

                    if expand_candidates.iter().any(|c| c.start_line == s.start_line && c.name == s.name) {
                        // Auto-expand: show full body
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
                        skel.push_str(&format!("{:>4}| {}  (L{}-{})\n",
                            s.start_line, sig_short, s.start_line, s.end_line));
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
            return Ok(ToolResult { call_id: String::new(), output: skeleton, success: true });
        }

        let offset = parsed.offset.unwrap_or(1).max(1) - 1;

        let limit = match parsed.limit {
            Some(l) => l,
            None => {
                if total_lines > 1500 && parsed.offset.is_none() {
                    1500
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
                "\n\n[Showing lines {}-{} of {} total. \
                 To read the full file: call read_file without offset/limit.]{}",
                offset + 1, end, total_lines, remaining_skeleton
            ));
        }

        Ok(ToolResult { call_id: String::new(), output, success: true })
    }
}
