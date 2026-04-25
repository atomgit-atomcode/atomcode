use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

pub struct FileDependenciesTool;

#[derive(Deserialize)]
struct FileDepsArgs {
    file: String,
}

fn shorten_path(path: &Path) -> String {
    let components: Vec<_> = path.components().collect();
    if components.len() <= 3 {
        return path.display().to_string();
    }
    let last3: Vec<_> = components[components.len() - 3..]
        .iter()
        .map(|c| c.as_os_str())
        .collect();
    format!(
        ".../{}",
        last3
            .iter()
            .map(|s| s.to_string_lossy())
            .collect::<Vec<_>>()
            .join("/")
    )
}

#[async_trait]
impl Tool for FileDependenciesTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "file_dependencies",
            description:
                "Show file-level dependencies: which files this file USES (imports/calls into) \
                and which files USE this file (depend on it).\n\
                Accepts relative or absolute file paths.\n\
                Example: {\"file\": \"src/agent/mod.rs\"}"
                    .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "file": { "type": "string", "description": "File path (relative to working dir or absolute)" }
                },
                "required": ["file"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    fn approval_with_context(&self, args: &str, ctx: &ToolContext) -> ApprovalRequirement {
        let parsed = match serde_json::from_str::<FileDepsArgs>(args) {
            Ok(parsed) => parsed,
            Err(_) => return self.approval(args),
        };
        let working_dir = match ctx.working_dir.try_read() {
            Ok(wd) => wd.clone(),
            Err(_) => return self.approval(args),
        };
        match super::approval_for_path(
            &parsed.file,
            &working_dir,
            super::ExternalPathAction::Enumerate,
        ) {
            Ok(approval) => approval,
            Err(_) => self.approval(args),
        }
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: FileDepsArgs = serde_json::from_str(args)?;
        let wd = ctx.working_dir.read().await.clone();
        let file_path = match super::inspect_path_access(&parsed.file, &wd) {
            Ok(access) => access.path,
            Err(err) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: err.to_string(),
                    success: false,
                });
            }
        };

        let graph = ctx.graph.read().await;

        if !graph.is_ready() {
            return Ok(ToolResult {
                call_id: String::new(),
                output: "Code graph is not yet indexed. The graph will be available after the \
                    background indexer completes. Try again shortly."
                    .to_string(),
                success: false,
            });
        }

        let symbols = match graph.symbols_in_file(&file_path) {
            Some(ids) => ids.clone(),
            None => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: format!(
                        "File '{}' not found in code graph. Check the path or wait for indexing.",
                        parsed.file
                    ),
                    success: false,
                });
            }
        };

        // USES: files that this file's symbols call into
        let mut uses_files = HashSet::new();
        for &sym_id in &symbols {
            if let Some(edges) = graph.callees(sym_id) {
                for edge in edges {
                    if let Some(node) = graph.node(edge.to) {
                        if node.file != file_path {
                            uses_files.insert(node.file.clone());
                        }
                    }
                }
            }
        }

        // USED BY: files that call into this file's symbols
        let mut used_by_files = HashSet::new();
        for &sym_id in &symbols {
            if let Some(edges) = graph.callers(sym_id) {
                for edge in edges {
                    if let Some(node) = graph.node(edge.to) {
                        if node.file != file_path {
                            used_by_files.insert(node.file.clone());
                        }
                    }
                }
            }
        }

        let mut out = format!("File dependencies for {}:\n\n", shorten_path(&file_path));

        out.push_str(&format!("USES ({} files):\n", uses_files.len()));
        if uses_files.is_empty() {
            out.push_str("  (none)\n");
        } else {
            let mut sorted: Vec<_> = uses_files.iter().collect();
            sorted.sort();
            for f in sorted {
                out.push_str(&format!("  {}\n", shorten_path(f)));
            }
        }

        out.push_str(&format!("\nUSED BY ({} files):\n", used_by_files.len()));
        if used_by_files.is_empty() {
            out.push_str("  (none)\n");
        } else {
            let mut sorted: Vec<_> = used_by_files.iter().collect();
            sorted.sort();
            for f in sorted {
                out.push_str(&format!("  {}\n", shorten_path(f)));
            }
        }

        Ok(ToolResult {
            call_id: String::new(),
            output: out,
            success: true,
        })
    }
}
