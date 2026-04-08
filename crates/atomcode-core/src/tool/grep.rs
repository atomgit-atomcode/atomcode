use anyhow::Result;
use async_trait::async_trait;
use ignore::WalkBuilder;
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::json;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

pub struct GrepTool;

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    path: Option<String>,
    #[serde(default = "default_max_results")]
    max_results: usize,
    #[serde(default = "default_context")]
    context: usize,
}

fn default_context() -> usize { 3 }
fn default_max_results() -> usize { 50 }

#[async_trait]
impl Tool for GrepTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "grep",
            description: "Search file contents for a pattern. Returns matching lines with surrounding context.\n\
                Usage:\n\
                - Use this to find where a function, variable, string, or UI element is defined or used.\n\
                - Use this BEFORE editing when the user's request is ambiguous — find ALL candidates first.\n\
                - Pattern is regex by default (case-insensitive unless uppercase is used).\n\
                - Escape special regex chars: . → \\\\. , ( → \\\\( , [ → \\\\[\n\
                - If regex fails, the tool automatically retries with literal string matching.\n\
                - NEVER use bash grep/rg — always use this tool.\n\
                Examples:\n\
                - Find a function: {\"pattern\": \"def process_data\"}\n\
                - Find a string with dots: {\"pattern\": \"console\\\\.log\"}\n\
                - Find across alternatives: {\"pattern\": \"upload|上传\"}\n\
                - Search specific directory: {\"pattern\": \"import\", \"path\": \"src/views\"}".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Search pattern (regex by default). Escape dots/parens: console\\.log\\(" },
                    "path": { "type": "string", "description": "Directory or file to search (default: working directory)" },
                    "max_results": { "type": "integer", "description": "Max results to return (default 50)" },
                    "context": { "type": "integer", "description": "Lines of context around each match (default 3)" }
                },
                "required": ["pattern"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: GrepArgs = serde_json::from_str(args)?;
        let path = parsed.path.as_deref().unwrap_or(".");

        // Graph-first: if pattern is a simple identifier, check graph before ripgrep.
        // Graph lookup is instant and returns structured results (definition + references + call chain).
        if is_simple_identifier(&parsed.pattern) {
            let graph = ctx.graph.read().await;
            if graph.is_ready() {
                let symbols = graph.find_by_name(&parsed.pattern);
                if !symbols.is_empty() {
                    let mut out = String::new();
                    for sym in symbols.iter().take(5) {
                        let short_file = sym.file.file_name()
                            .map(|f| f.to_string_lossy().to_string())
                            .unwrap_or_default();
                        out.push_str(&format!(
                            "{} {:?} in {}:{}\n",
                            sym.name, sym.kind, short_file, sym.start_line
                        ));
                    }

                    // Add call chain for functions
                    let funcs: Vec<_> = symbols.iter()
                        .filter(|s| matches!(s.kind,
                            crate::graph::SymbolKind::Function | crate::graph::SymbolKind::Method))
                        .collect();
                    if let Some(func) = funcs.first() {
                        // Callers
                        let callers = graph.trace_callers(func.id, 2);
                        if !callers.is_empty() {
                            out.push_str("\nCalled by:\n");
                            for (cid, depth) in &callers {
                                if let Some(n) = graph.node(*cid) {
                                    let f = n.file.file_name()
                                        .map(|f| f.to_string_lossy().to_string())
                                        .unwrap_or_default();
                                    out.push_str(&format!("  {}{}() ({}:{})\n",
                                        "  ".repeat(*depth), n.name, f, n.start_line));
                                }
                            }
                        }
                        // Callees
                        let callees = graph.trace_callees(func.id, 2);
                        if !callees.is_empty() {
                            out.push_str("\nCalls:\n");
                            for (cid, depth) in &callees {
                                if let Some(n) = graph.node(*cid) {
                                    let f = n.file.file_name()
                                        .map(|f| f.to_string_lossy().to_string())
                                        .unwrap_or_default();
                                    out.push_str(&format!("  {}{}() ({}:{})\n",
                                        "  ".repeat(*depth), n.name, f, n.start_line));
                                }
                            }
                        }
                    }

                    // Also include file dependency info
                    if let Some(func) = funcs.first() {
                        let fname = func.file.file_name()
                            .map(|f| f.to_string_lossy().to_string())
                            .unwrap_or_default();
                        if let Some(dep) = graph.file_dependency_summary(&fname) {
                            out.push_str(&format!("\n{}\n", dep));
                        }
                    }

                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: format!("[Graph: {} results for '{}']\n{}", symbols.len(), parsed.pattern, out),
                        success: true,
                    });
                }
            }
        }

        // Fallback: normal ripgrep
        let max = parsed.max_results;
        let context_lines = parsed.context.min(10);

        // Resolve path against working directory
        let wd = ctx.working_dir.read().await.clone();
        let resolved = if std::path::Path::new(path).is_absolute() {
            std::path::PathBuf::from(path)
        } else {
            wd.join(path)
        };

        if !resolved.exists() {
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!("Path not found: {}", resolved.display()),
                success: false,
            });
        }

        // Build regex (smart-case: case-insensitive if pattern has no uppercase)
        let has_uppercase = parsed.pattern.chars().any(|c| c.is_uppercase());
        let re = match RegexBuilder::new(&parsed.pattern)
            .case_insensitive(!has_uppercase)
            .build()
        {
            Ok(r) => r,
            Err(_) => {
                // Regex failed — try as literal
                match RegexBuilder::new(&regex::escape(&parsed.pattern))
                    .case_insensitive(!has_uppercase)
                    .build()
                {
                    Ok(r) => r,
                    Err(e) => {
                        return Ok(ToolResult {
                            call_id: String::new(),
                            output: format!("Invalid pattern '{}': {}", parsed.pattern, e),
                            success: false,
                        });
                    }
                }
            }
        };

        // Walk files using ignore crate (respects .gitignore, skips binary, multi-threaded)
        let walker = WalkBuilder::new(&resolved)
            .hidden(true)        // skip hidden files
            .git_ignore(true)    // respect .gitignore
            .git_global(true)
            .git_exclude(true)
            .build();

        let mut matches: Vec<String> = Vec::new();
        let mut files_searched = 0usize;
        let mut match_count = 0usize;

        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            if !entry.file_type().map_or(false, |ft| ft.is_file()) {
                continue;
            }

            let file_path = entry.path();

            // Skip known noise directories/files not covered by .gitignore
            let path_str = file_path.to_string_lossy();
            if path_str.contains("/datalog/") || path_str.ends_with(".log")
                || path_str.contains("/target/") || path_str.contains("/dist/")
                || path_str.contains("/node_modules/")
            {
                continue;
            }

            // Read file (skip binary)
            let content = match std::fs::read_to_string(file_path) {
                Ok(c) => c,
                Err(_) => continue, // binary or unreadable
            };

            files_searched += 1;
            let lines: Vec<&str> = content.lines().collect();

            // Find matching lines
            let mut file_matches: Vec<usize> = Vec::new();
            for (i, line) in lines.iter().enumerate() {
                if re.is_match(line) {
                    file_matches.push(i);
                    if match_count + file_matches.len() >= max {
                        break;
                    }
                }
            }

            if file_matches.is_empty() {
                continue;
            }

            // Format matches with context
            let rel_path = file_path.strip_prefix(&wd)
                .unwrap_or(file_path)
                .to_string_lossy();

            let mut shown: std::collections::HashSet<usize> = std::collections::HashSet::new();
            for &match_line in &file_matches {
                let start = match_line.saturating_sub(context_lines);
                let end = (match_line + context_lines + 1).min(lines.len());

                // Separator between non-contiguous chunks
                if !shown.is_empty() && start > 0 && !shown.contains(&(start - 1)) {
                    matches.push("--".to_string());
                }

                for i in start..end {
                    if shown.contains(&i) { continue; }
                    shown.insert(i);

                    let prefix = if i == match_line {
                        format!("{}:{}:", rel_path, i + 1)
                    } else {
                        format!("{}-{}-", rel_path, i + 1)
                    };
                    matches.push(format!("{}{}", prefix, lines[i]));
                }
            }

            match_count += file_matches.len();
            if match_count >= max {
                break;
            }
        }

        // Annotate matching lines with enclosing function name (tree-sitter)
        let mut searcher = ctx.semantic.lock().await;
        let mut annotated: Vec<String> = Vec::new();
        let mut sym_cache: std::collections::HashMap<String, Vec<crate::semantic::Symbol>> = std::collections::HashMap::new();

        for line in &matches {
            let parts: Vec<&str> = line.splitn(3, ':').collect();
            if parts.len() >= 3 {
                if let Ok(line_no) = parts[1].parse::<usize>() {
                    let file = parts[0];
                    let abs_file = if std::path::Path::new(file).is_absolute() {
                        std::path::PathBuf::from(file)
                    } else {
                        wd.join(file)
                    };
                    let symbols = sym_cache.entry(file.to_string()).or_insert_with(|| {
                        searcher.list_symbols(&abs_file).unwrap_or_default()
                    });
                    if let Some(sym) = symbols.iter().find(|s| line_no >= s.start_line && line_no <= s.end_line) {
                        annotated.push(format!("{}  ← in {}()", line, sym.name));
                        continue;
                    }
                }
            }
            annotated.push(line.clone());
        }
        drop(searcher);

        let output = if annotated.is_empty() {
            let mut msg = format!("No matches found for '{}' in {}", parsed.pattern, path);
            msg.push_str(&format!(" ({} files searched)", files_searched));
            msg
        } else {
            let total = annotated.len();
            let mut out = annotated.join("\n");
            if total >= max {
                out.push_str(&format!("\n\n[Results capped at {} matches]", max));
            }
            out
        };

        Ok(ToolResult {
            call_id: String::new(),
            output,
            success: !matches.is_empty(),
        })
    }
}

/// Check if a pattern is a simple identifier (no regex special chars).
/// e.g., "fetch_weather", "SearchFilter", "QueryRouter" → true
/// e.g., "fetch|query", "error.*line", "def\s+" → false
fn is_simple_identifier(pattern: &str) -> bool {
    !pattern.is_empty()
        && pattern.len() >= 3
        && pattern.chars().all(|c| c.is_alphanumeric() || c == '_')
}
