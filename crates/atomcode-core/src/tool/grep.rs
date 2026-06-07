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

fn default_context() -> usize {
    3
}
fn default_max_results() -> usize {
    50
}

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

    fn approval_with_context(&self, args: &str, ctx: &ToolContext) -> ApprovalRequirement {
        let parsed = match serde_json::from_str::<GrepArgs>(args) {
            Ok(parsed) => parsed,
            Err(_) => return self.approval(args),
        };
        let working_dir = match ctx.working_dir.try_read() {
            Ok(wd) => wd.clone(),
            Err(_) => return self.approval(args),
        };
        let raw_path = parsed.path.as_deref().unwrap_or(".");
        match super::approval_for_path(raw_path, &working_dir, super::ExternalPathAction::Read)
        {
            Ok(approval) => approval,
            Err(_) => self.approval(args),
        }
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: GrepArgs = serde_json::from_str(args)?;
        let path = parsed.path.as_deref().unwrap_or(".");

        let wd = ctx.working_dir.read().await.clone();

        // Graph enhancement: build a short header when pattern looks like an
        // identifier and no specific path is given (project-wide search).
        // The header is prepended to ripgrep results — it never replaces them.
        let graph_header = if parsed.path.is_none() {
            self.build_graph_header(&parsed.pattern, ctx, &wd).await
        } else {
            None
        };

        let max = parsed.max_results;
        let context_lines = parsed.context.min(10);
        let resolved = match super::inspect_path_access(path, &wd) {
            Ok(access) => access.path,
            Err(err) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: err.to_string(),
                    success: false,
                });
            }
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
            .hidden(true) // skip hidden files
            .git_ignore(true) // respect .gitignore
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
            if path_str.contains("/datalog/")
                || path_str.ends_with(".log")
                || path_str.contains("/target/")
                || path_str.contains("/dist/")
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
            let rel_path = file_path
                .strip_prefix(&wd)
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
                    if shown.contains(&i) {
                        continue;
                    }
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
        let mut sym_cache: std::collections::HashMap<String, Vec<crate::semantic::Symbol>> =
            std::collections::HashMap::new();

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
                    let symbols = sym_cache
                        .entry(file.to_string())
                        .or_insert_with(|| searcher.list_symbols(&abs_file).unwrap_or_default());
                    if let Some(sym) = symbols
                        .iter()
                        .find(|s| line_no >= s.start_line && line_no <= s.end_line)
                    {
                        annotated.push(format!("{}  ← in {}()", line, sym.name));
                        continue;
                    }
                }
            }
            annotated.push(line.clone());
        }
        drop(searcher);

        let mut output = String::new();

        if let Some(header) = graph_header {
            output.push_str(&header);
            output.push('\n');
        }

        if annotated.is_empty() {
            output.push_str(&format!(
                "No matches found for '{}' in {}",
                parsed.pattern, path
            ));
            output.push_str(&format!(" ({} files searched)", files_searched));
        } else {
            let total = annotated.len();
            output.push_str(&annotated.join("\n"));
            if total >= max {
                output.push_str(&format!("\n\n[Results capped at {} matches]", max));
            }
        };

        // success=true even on zero matches: empty result IS the answer
        // (matches glob's "No files matching" semantics — same screenshot
        // showed glob's empty-result line rendering normally while grep's
        // identical-meaning line painted red ✗ as if the search failed).
        // Real failures (bad path / bad regex) take the early-return
        // branches above with their own success=false.
        Ok(ToolResult {
            call_id: String::new(),
            output,
            success: true,
        })
    }
}

impl GrepTool {
    /// Build a short graph-based header for the grep output.
    /// Returns None if graph is not ready, no identifier is extractable,
    /// or no symbols match.
    async fn build_graph_header(
        &self,
        pattern: &str,
        ctx: &ToolContext,
        wd: &std::path::Path,
    ) -> Option<String> {
        let query_word = extract_graph_candidates(pattern)?;
        let graph = ctx.graph.read().await;
        if !graph.is_ready() {
            return None;
        }

        let symbols = graph.find_by_name(&query_word);
        if symbols.is_empty() {
            return None;
        }

        let mut out = format!(
            "[Graph: {} definitions for '{}']\n",
            symbols.len(),
            query_word
        );
        for sym in symbols.iter().take(5) {
            let rel = sym
                .file
                .strip_prefix(wd)
                .unwrap_or(&sym.file)
                .to_string_lossy();
            out.push_str(&format!(
                "  {} {:?} in {}:{}\n",
                sym.name, sym.kind, rel, sym.start_line
            ));
        }

        Some(out)
    }
}

/// Extract a likely code identifier from a grep pattern for graph lookup.
///
/// Only returns Some when the word strongly resembles a code symbol:
/// - Contains underscore (snake_case): "fetch_weather" → Some("fetch_weather")
/// - Contains mixed case (CamelCase): "SearchFilter" → Some("SearchFilter")
/// - Pure lowercase words like "error", "table", "search" are rejected to
///   avoid false positives with common English words.
///
/// Examples:
/// - "fetch_weather" → Some("fetch_weather")
/// - "SearchFilter"  → Some("SearchFilter")
/// - "NdarrayMixin"  → Some("NdarrayMixin")
/// - "weather|天气"  → None  (no identifier-like word)
/// - "Structured ndarray gets viewed" → None  (plain English)
/// - "error.*line" → None  (too generic)
/// - "console\\.log" → None  (too generic)
fn extract_graph_candidates(pattern: &str) -> Option<String> {
    let skip_keywords = [
        "pub", "fn", "struct", "enum", "impl", "use", "let", "const", "async", "trait", "type",
        "mod", "crate", "self", "super", "for", "def", "class", "function", "var", "import",
        "from", "export", "return", "match", "where", "static", "mut", "ref", "true", "false",
        "none", "some", "null", "this", "not", "and", "the", "with",
    ];

    let mut best: Option<String> = None;
    let mut best_len = 0;

    for word in pattern.split(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
        let w = word.trim();
        if w.len() < 4 {
            continue;
        }
        if !w
            .chars()
            .next()
            .map(|c| c.is_ascii_alphabetic() || c == '_')
            .unwrap_or(false)
        {
            continue;
        }
        if !w.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        if skip_keywords.contains(&w.to_lowercase().as_str()) {
            continue;
        }

        let has_underscore = w.contains('_');
        // Require real CamelCase (lower→upper transition like "SearchFilter")
        // not just a capitalized word ("Structured").
        let has_camel_transition = w
            .as_bytes()
            .windows(2)
            .any(|pair| pair[0].is_ascii_lowercase() && pair[1].is_ascii_uppercase());

        if !has_underscore && !has_camel_transition {
            continue;
        }

        if w.len() > best_len {
            best = Some(w.to_string());
            best_len = w.len();
        }
    }

    best
}

#[cfg(test)]
mod tests {
    use super::extract_graph_candidates;
    use super::GrepTool;
    use crate::tool::{ApprovalRequirement, Tool, ToolContext};
    use tempfile::TempDir;

    #[test]
    fn grep_outside_workspace_non_sensitive_requires_approval() {
        let workspace = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let ctx = ToolContext::new(workspace.path().to_path_buf());
        let args = format!(
            r#"{{"pattern":"foo","path":"{}"}}"#,
            outside.path().display()
        );
        assert!(matches!(
            GrepTool.approval_with_context(&args, &ctx),
            ApprovalRequirement::RequireApproval(_)
        ));
    }

    #[test]
    fn grep_sensitive_path_still_requires_always() {
        let workspace = TempDir::new().unwrap();
        let ctx = ToolContext::new(workspace.path().to_path_buf());
        // /etc is in the system-protected prefixes list.
        let args = r#"{"pattern":"PermitRoot","path":"/etc"}"#;
        assert!(matches!(
            GrepTool.approval_with_context(args, &ctx),
            ApprovalRequirement::RequireApprovalAlways(_)
        ));
    }

    // Regression: zero-match grep used to return success=false, which
    // made the TUI render the result row red ✗ as if the search itself
    // failed (screenshot 43.png had two such red rows next to genuine
    // path-not-found errors, making the agent's normal exploration look
    // like a wall of failures). "No matches" is a valid answer — it's
    // the *path* / *regex* errors that are real failures (those still
    // return success=false via the early-return branches). Pin the
    // semantics so future churn doesn't regress.
    #[tokio::test]
    async fn grep_zero_matches_reports_success_true() {
        let workspace = TempDir::new().unwrap();
        // Seed a file so the search has something to walk; the pattern
        // intentionally won't match.
        std::fs::write(
            workspace.path().join("a.rs"),
            "fn alpha() {}\nfn beta() {}\n",
        )
        .unwrap();
        let ctx = ToolContext::new(workspace.path().to_path_buf());
        let args = r#"{"pattern":"definitely_not_in_file_xyz"}"#;
        let result = GrepTool.execute(args, &ctx).await.unwrap();
        assert!(
            result.success,
            "zero-match grep must return success=true so TUI doesn't \
             paint it red. Output was: {}",
            result.output
        );
        assert!(
            result.output.contains("No matches found"),
            "zero-match output should explain what happened, got: {}",
            result.output
        );
    }

    // Real failure modes (bad regex / missing path) MUST still be
    // success=false — those genuinely indicate the model needs to
    // change inputs, not just learn that "no rows match".
    #[tokio::test]
    async fn grep_path_not_found_reports_success_false() {
        let workspace = TempDir::new().unwrap();
        let ctx = ToolContext::new(workspace.path().to_path_buf());
        let args = r#"{"pattern":"foo","path":"/nonexistent/path/xyz123"}"#;
        let result = GrepTool.execute(args, &ctx).await.unwrap();
        assert!(
            !result.success,
            "path-not-found must remain success=false — output: {}",
            result.output
        );
    }

    // Default path "." resolves to the workspace itself — must auto-approve.
    #[test]
    fn grep_default_path_auto_approves() {
        let workspace = TempDir::new().unwrap();
        let ctx = ToolContext::new(workspace.path().to_path_buf());
        let args = r#"{"pattern":"foo"}"#;
        assert!(matches!(
            GrepTool.approval_with_context(args, &ctx),
            ApprovalRequirement::AutoApprove
        ));
    }

    #[test]
    fn snake_case_identifier() {
        assert_eq!(
            extract_graph_candidates("fetch_weather"),
            Some("fetch_weather".into())
        );
    }

    #[test]
    fn camel_case_identifier() {
        assert_eq!(
            extract_graph_candidates("SearchFilter"),
            Some("SearchFilter".into())
        );
        assert_eq!(
            extract_graph_candidates("NdarrayMixin"),
            Some("NdarrayMixin".into())
        );
    }

    #[test]
    fn mixed_cjk_ascii_no_identifier() {
        assert_eq!(extract_graph_candidates("weather|天气"), None);
    }

    #[test]
    fn plain_english_rejected() {
        assert_eq!(
            extract_graph_candidates("Structured ndarray gets viewed"),
            None
        );
        assert_eq!(extract_graph_candidates("error"), None);
        assert_eq!(extract_graph_candidates("table"), None);
        assert_eq!(extract_graph_candidates("search"), None);
        assert_eq!(extract_graph_candidates("console"), None);
    }

    #[test]
    fn regex_pattern_rejected() {
        assert_eq!(extract_graph_candidates("error.*line"), None);
        assert_eq!(extract_graph_candidates("console\\.log"), None);
    }

    #[test]
    fn keywords_rejected() {
        assert_eq!(extract_graph_candidates("pub struct"), None);
        assert_eq!(extract_graph_candidates("import from"), None);
    }

    #[test]
    fn keyword_then_identifier() {
        assert_eq!(
            extract_graph_candidates("pub struct QueryIntent"),
            Some("QueryIntent".into())
        );
        assert_eq!(
            extract_graph_candidates("def process_data"),
            Some("process_data".into())
        );
    }

    #[test]
    fn pure_chinese_rejected() {
        assert_eq!(extract_graph_candidates("(科技|财经|体育)"), None);
    }

    #[test]
    fn short_words_rejected() {
        assert_eq!(extract_graph_candidates("fn"), None);
        assert_eq!(extract_graph_candidates("FnX"), None); // 3 chars, below threshold of 4
    }

    #[test]
    fn or_pattern_picks_best_identifier() {
        assert_eq!(
            extract_graph_candidates("SearchFilter|from_intent"),
            Some("SearchFilter".into()),
        );
    }

    #[test]
    fn not_prefix_rejected() {
        // "not data_is_mixin" — "not" is a keyword, "data_is_mixin" has underscore → match
        assert_eq!(
            extract_graph_candidates("not data_is_mixin"),
            Some("data_is_mixin".into())
        );
    }
}
