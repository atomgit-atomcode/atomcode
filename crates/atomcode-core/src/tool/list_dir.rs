use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

pub struct ListDirTool;

#[derive(Deserialize)]
struct ListDirArgs {
    path: Option<String>,
    #[serde(default = "default_depth")]
    depth: usize,
}

fn default_depth() -> usize { 2 }

use super::SKIP_DIRS;

#[async_trait]
impl Tool for ListDirTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "list_directory",
            description: "List files and directories in a path. Returns a tree structure. Skips common noise directories (node_modules, .git, target, etc.).",
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Directory to list (default: working directory)" },
                    "depth": { "type": "integer", "description": "Max depth to recurse (default 2)" }
                },
                "required": []
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: ListDirArgs = serde_json::from_str(args)?;
        let path = parsed.path.as_deref().unwrap_or(".");
        let depth = parsed.depth.min(5); // Cap at 5

        let dir = std::path::Path::new(path);
        if !dir.exists() {
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!("Directory not found: {}", path),
                success: false,
            });
        }

        let mut lines = Vec::new();
        scan_dir(&mut lines, dir, 0, depth);

        if lines.len() > 200 {
            lines.truncate(200);
            lines.push("  ... (truncated at 200 entries)".to_string());
        }

        Ok(ToolResult {
            call_id: String::new(),
            output: lines.join("\n"),
            success: true,
        })
    }
}

fn scan_dir(lines: &mut Vec<String>, dir: &std::path::Path, depth: usize, max_depth: usize) {
    if depth > max_depth { return; }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut items: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| !SKIP_DIRS.contains(&e.file_name().to_string_lossy().as_ref()))
        .collect();
    items.sort_by_key(|e| e.file_name());

    for entry in &items {
        let name = entry.file_name().to_string_lossy().to_string();
        let indent = "  ".repeat(depth);
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            lines.push(format!("{}{}/", indent, name));
            scan_dir(lines, &entry.path(), depth + 1, max_depth);
        } else {
            lines.push(format!("{}{}", indent, name));
        }
    }
}
