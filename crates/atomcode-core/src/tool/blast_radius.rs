use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

pub struct BlastRadiusTool;

#[derive(Deserialize)]
struct BlastRadiusArgs {
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
impl Tool for BlastRadiusTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "blast_radius",
            description: "Estimate the blast radius of changing a file. Shows direct dependents \
                (depth 1), indirect dependents (depth 2-3), and total impacted file count.\n\
                Use before refactoring to understand the scope of changes.\n\
                Example: {\"file\": \"src/tool/mod.rs\"}"
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
        let parsed = match serde_json::from_str::<BlastRadiusArgs>(args) {
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
        let parsed: BlastRadiusArgs = serde_json::from_str(args)?;
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

        // Direct dependents (depth 1): files whose symbols directly call this file's symbols
        let mut direct = HashSet::new();
        for &sym_id in &symbols {
            if let Some(edges) = graph.callers(sym_id) {
                for edge in edges {
                    if let Some(node) = graph.node(edge.to) {
                        if node.file != file_path {
                            direct.insert(node.file.clone());
                        }
                    }
                }
            }
        }

        // Indirect dependents (depth 2-3): use file_dependents with depth 3
        let all_dependents = graph.file_dependents(&file_path, 3);
        let mut indirect = HashSet::new();
        for dep in &all_dependents {
            if !direct.contains(dep) {
                indirect.insert(dep.clone());
            }
        }

        let total = direct.len() + indirect.len();

        let mut out = format!("Blast radius for {}:\n\n", shorten_path(&file_path));

        out.push_str(&format!("DIRECT DEPENDENTS ({} files):\n", direct.len()));
        if direct.is_empty() {
            out.push_str("  (none)\n");
        } else {
            let mut sorted: Vec<_> = direct.iter().collect();
            sorted.sort();
            for f in sorted {
                out.push_str(&format!("  {}\n", shorten_path(f)));
            }
        }

        out.push_str(&format!(
            "\nINDIRECT DEPENDENTS ({} files):\n",
            indirect.len()
        ));
        if indirect.is_empty() {
            out.push_str("  (none)\n");
        } else {
            let mut sorted: Vec<_> = indirect.iter().collect();
            sorted.sort();
            for f in sorted {
                out.push_str(&format!("  {}\n", shorten_path(f)));
            }
        }

        out.push_str(&format!("\nTOTAL IMPACT: {} files\n", total));

        Ok(ToolResult {
            call_id: String::new(),
            output: out,
            success: true,
        })
    }
}
