use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

pub struct ListSymbolsTool;

#[derive(Deserialize)]
struct ListSymbolsArgs {
    file_path: String,
}

#[async_trait]
impl Tool for ListSymbolsTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "list_symbols",
            description: "List all functions, classes, structs, and other top-level symbols in a file.\n\
                Returns symbol names with line ranges. Use this to understand a file's structure before editing.\n\
                This is faster and more precise than read_file for understanding file structure.\n\
                Examples:\n\
                - {\"file_path\": \"/path/to/main.rs\"} → lists all functions, structs, impls with line numbers".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "Absolute path to the source file" }
                },
                "required": ["file_path"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    fn approval_with_context(&self, args: &str, ctx: &ToolContext) -> ApprovalRequirement {
        let parsed = match serde_json::from_str::<ListSymbolsArgs>(args) {
            Ok(parsed) => parsed,
            Err(_) => return self.approval(args),
        };
        let working_dir = match ctx.working_dir.try_read() {
            Ok(wd) => wd.clone(),
            Err(_) => return self.approval(args),
        };
        match super::approval_for_path(
            &parsed.file_path,
            &working_dir,
            super::ExternalPathAction::Read,
        ) {
            Ok(approval) => approval,
            Err(_) => self.approval(args),
        }
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: ListSymbolsArgs = serde_json::from_str(args)?;
        let working_dir = ctx.working_dir.read().await.clone();
        let path = match super::inspect_path_access(&parsed.file_path, &working_dir) {
            Ok(access) => access.path,
            Err(err) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: err.to_string(),
                    success: false,
                });
            }
        };

        if !path.exists() {
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!("File not found: {}", parsed.file_path),
                success: false,
            });
        }

        let mut searcher = ctx.semantic.lock().await;
        match searcher.list_symbols(&path) {
            Some(symbols) if symbols.is_empty() => Ok(ToolResult {
                call_id: String::new(),
                output: format!("No symbols found in {}", parsed.file_path),
                success: true,
            }),
            Some(symbols) => {
                let mut out = format!(
                    "Symbols in {} ({} total):\n\n",
                    parsed.file_path,
                    symbols.len()
                );
                for sym in &symbols {
                    out.push_str(&format!(
                        "  {:4}-{:4}  {}  ({})\n",
                        sym.start_line, sym.end_line, sym.name, sym.kind
                    ));
                }
                out.push_str("\n[Use read_symbol to read any symbol's full source code]");
                Ok(ToolResult {
                    call_id: String::new(),
                    output: out,
                    success: true,
                })
            }
            None => Ok(ToolResult {
                call_id: String::new(),
                output: format!("Failed to parse {}", parsed.file_path),
                success: false,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn approval_auto_for_workspace_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let args = serde_json::json!({ "file_path": "main.rs" }).to_string();

        assert!(matches!(
            ListSymbolsTool.approval_with_context(&args, &ctx),
            ApprovalRequirement::AutoApprove
        ));
    }

    #[tokio::test]
    async fn approval_requires_read_confirmation_for_external_file() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let file = outside.path().join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        let ctx = ToolContext::new(workspace.path().to_path_buf());
        let args = serde_json::json!({ "file_path": file }).to_string();

        assert!(matches!(
            ListSymbolsTool.approval_with_context(&args, &ctx),
            ApprovalRequirement::RequireApproval(_)
        ));
    }
}
