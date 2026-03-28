use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::process::Command;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

pub struct FindReferencesTool;

#[derive(Deserialize)]
struct FindReferencesArgs {
    symbol: String,
    path: Option<String>,
}

#[async_trait]
impl Tool for FindReferencesTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "find_references",
            description: "Find all references to a symbol (function, class, variable) across the project.\n\
                Uses ripgrep for speed, then tree-sitter to classify each match as definition, call, or import.\n\
                Returns the definition location + all call/usage sites with file:line context.\n\
                Examples:\n\
                - {\"symbol\": \"process_data\"} → finds definition + all calls across the project\n\
                - {\"symbol\": \"UserService\", \"path\": \"src/\"} → search only in src/".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "symbol": { "type": "string", "description": "Symbol name to find references for" },
                    "path": { "type": "string", "description": "Directory to search in (default: working directory)" }
                },
                "required": ["symbol"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: FindReferencesArgs = serde_json::from_str(args)?;
        let wd = ctx.working_dir.read().await.clone();
        let search_dir = if let Some(ref p) = parsed.path {
            if std::path::Path::new(p).is_absolute() {
                p.clone()
            } else {
                wd.join(p).to_string_lossy().to_string()
            }
        } else {
            wd.to_string_lossy().to_string()
        };

        // Use ripgrep to find all occurrences (word boundary match)
        let pattern = format!(r"\b{}\b", regex::escape(&parsed.symbol));
        let output = Command::new("rg")
            .args(&[
                "--line-number", "--no-heading", "--color=never",
                "--max-count=30",
                "-w",  // word boundary
                &pattern,
                &search_dir,
            ])
            .output()
            .await;

        let rg_output = match output {
            Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
            Err(_) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: "ripgrep not found. Install it: cargo install ripgrep".to_string(),
                    success: false,
                });
            }
        };

        if rg_output.trim().is_empty() {
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!("No references found for '{}' in {}", parsed.symbol, search_dir),
                success: false,
            });
        }

        // Classify each match using tree-sitter
        let mut searcher = ctx.semantic.lock().await;
        let mut definitions = Vec::new();
        let mut references = Vec::new();

        for line in rg_output.lines().take(30) {
            // Parse rg output: file:line:content
            let parts: Vec<&str> = line.splitn(3, ':').collect();
            if parts.len() < 3 { continue; }
            let file = parts[0];
            let line_no: usize = parts[1].parse().unwrap_or(0);
            let content = parts[2].trim();

            let file_path = std::path::Path::new(file);

            // Try to determine if this is a definition or usage
            let is_def = if let Some(symbols) = searcher.list_symbols(file_path) {
                symbols.iter().any(|s| s.name == parsed.symbol && s.start_line == line_no)
            } else {
                // Heuristic: check if line contains definition keywords
                let trimmed = content.trim();
                trimmed.starts_with("fn ") || trimmed.starts_with("pub fn ")
                    || trimmed.starts_with("def ") || trimmed.starts_with("class ")
                    || trimmed.starts_with("function ") || trimmed.starts_with("func ")
                    || trimmed.starts_with("struct ") || trimmed.starts_with("pub struct ")
                    || trimmed.starts_with("type ") || trimmed.starts_with("interface ")
                    || trimmed.contains("= function") || trimmed.contains("=> {")
            };

            let short_file = file.strip_prefix(&search_dir)
                .unwrap_or(file)
                .trim_start_matches('/');

            let entry = format!("  {}:{}: {}", short_file, line_no, content);
            if is_def {
                definitions.push(entry);
            } else {
                references.push(entry);
            }
        }

        let mut out = format!("References for '{}' in {}:\n\n", parsed.symbol, search_dir);

        if !definitions.is_empty() {
            out.push_str("DEFINITIONS:\n");
            for d in &definitions {
                out.push_str(d);
                out.push('\n');
            }
            out.push('\n');
        }

        if !references.is_empty() {
            out.push_str(&format!("USAGES ({}):\n", references.len()));
            for r in &references {
                out.push_str(r);
                out.push('\n');
            }
        }

        Ok(ToolResult {
            call_id: String::new(),
            output: out,
            success: true,
        })
    }
}
