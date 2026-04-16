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

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: ReadFileArgs = serde_json::from_str(args)?;
        let path = std::path::Path::new(&parsed.file_path);

        // ── Read cache: performance optimization only, NOT a STUB gate ──
        // Cache stores (mtime, rendered_output). If mtime matches, skip disk read +
        // UTF-8 parse + tree-sitter. Returns full content so the model always gets
        // what it asked for. STUB behavior was tried and reverted — it blocks
        // legitimate re-reads and doesn't prevent short-distance duplicates
        // (model ignores STUB text due to lost-at-the-end attention).
        let cache_key: crate::tool::ReadCacheKey = (
            path.to_path_buf(),
            parsed.offset,
            parsed.limit,
        );
        let disk_mtime = tokio::fs::metadata(&parsed.file_path).await.ok()
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

        // If file doesn't exist, auto-find similar filenames and suggest.
        // Saves 2-3 turns of path guessing (7% of sessions hit this).
        if !path.exists() {
            let filename = path.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if !filename.is_empty() {
                let wd = ctx.working_dir.read().await;
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
                find_file(&wd, &filename, 0, 7, &mut matches);
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

        let bytes = tokio::fs::read(&parsed.file_path).await?;

        // Check if the file is valid UTF-8; if not, report it as binary.
        let content = match String::from_utf8(bytes.clone()) {
            Ok(s) => s,
            Err(_) => {
                let output = format!(
                    "Binary file ({} bytes), cannot display as text.",
                    bytes.len()
                );
                if let Some(mtime) = disk_mtime {
                    ctx.read_cache.write().await.insert(cache_key.clone(), (mtime, output.clone()));
                }
                return Ok(ToolResult { call_id: String::new(), output, success: true });
            }
        };

        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        // ── Layer A: full content default, skeleton for large files (>300 lines) ──
        // Skeleton is the FALLBACK, not the default. ≤300 lines return full content
        // so the model can grep→old_string→edit in 2 steps. >300 lines return
        // skeleton because GLM-5 gets lost in the middle at ~685 lines.
        // With offset/limit: always return exact content (model chose a range).
        let auto_skeleton = total_lines > 300
            && parsed.offset.is_none()
            && parsed.limit.is_none();

        if auto_skeleton {
            let mut searcher = ctx.semantic.lock().await;
            let skeleton = if let Some(symbols) = searcher.list_symbols(path) {
                let fname = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
                let mut skel = format!("[File skeleton: {} ({} lines). Use read_file with offset and limit to read specific sections.]\n\n",
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
            let skeleton = if let Some(symbols) = searcher.list_symbols(path) {
                let unseen: Vec<String> = symbols.iter()
                    .filter(|s| s.start_line < offset + 1 || s.start_line > end)
                    .map(|s| {
                        let sig = lines.get(s.start_line.saturating_sub(1))
                            .map(|l| l.trim())
                            .unwrap_or(&s.name);
                        let sig_short: String = sig.chars().take(70).collect();
                        format!("{:>4}| {}  (L{}-{})", s.start_line, sig_short, s.start_line, s.end_line)
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
}
