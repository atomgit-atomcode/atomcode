use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
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

    fn approval_with_context(&self, args: &str, ctx: &ToolContext) -> ApprovalRequirement {
        let parsed = match serde_json::from_str::<FindReferencesArgs>(args) {
            Ok(parsed) => parsed,
            Err(_) => return self.approval(args),
        };
        let working_dir = match ctx.working_dir.try_read() {
            Ok(wd) => wd.clone(),
            Err(_) => return self.approval(args),
        };
        let raw_path = parsed.path.as_deref().unwrap_or(".");
        match super::approval_for_path(raw_path, &working_dir, super::ExternalPathAction::Read) {
            Ok(approval) => approval,
            Err(_) => self.approval(args),
        }
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: FindReferencesArgs = serde_json::from_str(args)?;
        let wd = ctx.working_dir.read().await.clone();
        let search_dir =
            match super::inspect_path_access(parsed.path.as_deref().unwrap_or("."), &wd) {
                Ok(access) => access.path.to_string_lossy().to_string(),
                Err(err) => {
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: err.to_string(),
                        success: false,
                    });
                }
            };

        // Use ripgrep to find all occurrences (word boundary match)
        let pattern = format!(r"\b{}\b", regex::escape(&parsed.symbol));
        let mut cmd = Command::new("rg");
        cmd.args(&[
            "--json",
            "--line-number",
            "--color=never",
            "--max-count=30",
            "-w", // word boundary
            &pattern,
            &search_dir,
        ]);
        crate::process_utils::suppress_console_window(&mut cmd);
        let output = cmd.output().await;

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
                output: format!(
                    "No references found for '{}' in {}",
                    parsed.symbol, search_dir
                ),
                success: false,
            });
        }

        // Classify each match using tree-sitter
        let mut searcher = ctx.semantic.lock().await;
        let mut definitions = Vec::new();
        let mut references = Vec::new();

        for matched in parse_rg_json_matches(&rg_output).into_iter().take(30) {
            let file_path = std::path::Path::new(&matched.file);

            // Try to determine if this is a definition or usage
            let is_def = if let Some(symbols) = searcher.list_symbols(file_path) {
                symbols
                    .iter()
                    .any(|s| s.name == parsed.symbol && s.start_line == matched.line_no)
            } else {
                // Heuristic: check if line contains definition keywords
                let trimmed = matched.content.trim();
                trimmed.starts_with("fn ")
                    || trimmed.starts_with("pub fn ")
                    || trimmed.starts_with("def ")
                    || trimmed.starts_with("class ")
                    || trimmed.starts_with("function ")
                    || trimmed.starts_with("func ")
                    || trimmed.starts_with("struct ")
                    || trimmed.starts_with("pub struct ")
                    || trimmed.starts_with("type ")
                    || trimmed.starts_with("interface ")
                    || trimmed.contains("= function")
                    || trimmed.contains("=> {")
            };

            let short_file = matched
                .file
                .strip_prefix(&search_dir)
                .unwrap_or(&matched.file)
                .trim_start_matches('/');

            let entry = format!(
                "  {}:{}: {}",
                short_file,
                matched.line_no,
                matched.content.trim()
            );
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

struct RgMatch {
    file: String,
    line_no: usize,
    content: String,
}

fn parse_rg_json_matches(output: &str) -> Vec<RgMatch> {
    output
        .lines()
        .filter_map(|line| {
            let value: Value = serde_json::from_str(line).ok()?;
            if value.get("type")?.as_str()? != "match" {
                return None;
            }
            let data = value.get("data")?;
            let file = data.get("path")?.get("text")?.as_str()?.to_string();
            let line_no = data.get("line_number")?.as_u64()? as usize;
            let content = data.get("lines")?.get("text")?.as_str()?.to_string();
            Some(RgMatch {
                file,
                line_no,
                content,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rg_json_with_colons_in_matched_file_paths() {
        let output = r#"{"type":"begin","data":{"path":{"text":"src:main.rs"}}}
{"type":"match","data":{"path":{"text":"src:main.rs"},"lines":{"text":"fn target_symbol() {}\n"},"line_number":1,"absolute_offset":0,"submatches":[]}}
{"type":"match","data":{"path":{"text":"src:main.rs"},"lines":{"text":"fn caller() { target_symbol(); }\n"},"line_number":2,"absolute_offset":22,"submatches":[]}}
{"type":"end","data":{"path":{"text":"src:main.rs"},"binary_offset":null,"stats":{}}}
"#;

        let matches = parse_rg_json_matches(output);
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].file, "src:main.rs");
        assert_eq!(matches[0].line_no, 1);
        assert_eq!(matches[0].content, "fn target_symbol() {}\n");
        assert_eq!(matches[1].file, "src:main.rs");
        assert_eq!(matches[1].line_no, 2);
        assert_eq!(matches[1].content, "fn caller() { target_symbol(); }\n");
    }
}
