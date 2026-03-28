use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

pub struct ReadSymbolTool;

#[derive(Deserialize)]
struct ReadSymbolArgs {
    file_path: String,
    symbol: String,
}

#[async_trait]
impl Tool for ReadSymbolTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "read_symbol",
            description: "Read the complete source code of a specific function, class, or struct by name.\n\
                More precise than read_file — returns exactly the symbol you need with line numbers.\n\
                Use list_symbols first to discover available symbols, then read_symbol to get the code.\n\
                Examples:\n\
                - {\"file_path\": \"/path/to/main.rs\", \"symbol\": \"process_data\"}\n\
                - {\"file_path\": \"/path/to/app.py\", \"symbol\": \"UserService\"}".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "Absolute path to the source file" },
                    "symbol": { "type": "string", "description": "Name of the function, class, or struct to read" }
                },
                "required": ["file_path", "symbol"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: ReadSymbolArgs = serde_json::from_str(args)?;
        let path = std::path::Path::new(&parsed.file_path);

        if !path.exists() {
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!("File not found: {}", parsed.file_path),
                success: false,
            });
        }

        let mut searcher = ctx.semantic.lock().await;
        match searcher.extract_symbol(path, &parsed.symbol) {
            Some(slice) => {
                let mut out = format!(
                    "{}  ({}, lines {}-{})\n\n",
                    slice.name, slice.kind, slice.start_line, slice.end_line
                );
                // Add line numbers to the source
                for (i, line) in slice.text.lines().enumerate() {
                    out.push_str(&format!("{:4}| {}\n", slice.start_line + i, line));
                }
                Ok(ToolResult {
                    call_id: String::new(),
                    output: out,
                    success: true,
                })
            }
            None => {
                // Try to list available symbols as a helpful hint
                let hint = match searcher.list_symbols(path) {
                    Some(symbols) => {
                        let names: Vec<String> = symbols.iter().map(|s| s.name.clone()).collect();
                        format!(
                            "Symbol '{}' not found in {}.\nAvailable symbols: {}",
                            parsed.symbol, parsed.file_path, names.join(", ")
                        )
                    }
                    None => format!("Symbol '{}' not found in {}", parsed.symbol, parsed.file_path),
                };
                Ok(ToolResult {
                    call_id: String::new(),
                    output: hint,
                    success: false,
                })
            }
        }
    }
}
