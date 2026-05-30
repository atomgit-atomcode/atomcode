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

fn default_depth() -> usize {
    2
}

#[async_trait]
impl Tool for ListDirTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "list_directory",
            description: "List files and directories as a tree structure. Skips noise directories (node_modules, .git, target, __pycache__, etc.).\n\
                Use this to understand project structure or explore unfamiliar directories.\n\
                For finding specific files by name/extension, prefer glob instead.\n\
                The depth parameter controls how deep to recurse (default 2, max 5).".to_string(),
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

    fn approval_with_context(&self, args: &str, ctx: &ToolContext) -> ApprovalRequirement {
        let parsed = match serde_json::from_str::<ListDirArgs>(args) {
            Ok(parsed) => parsed,
            Err(_) => return self.approval(args),
        };
        let working_dir = match ctx.working_dir.try_read() {
            Ok(wd) => wd.clone(),
            Err(_) => return self.approval(args),
        };
        let raw_path = parsed.path.as_deref().unwrap_or(".");
        match super::approval_for_path(raw_path, &working_dir, super::ExternalPathAction::Enumerate)
        {
            Ok(approval) => approval,
            Err(_) => self.approval(args),
        }
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: ListDirArgs = serde_json::from_str(args)?;
        let working_dir = ctx.working_dir.read().await.clone();
        let path = parsed.path.as_deref().unwrap_or(".");
        let depth = parsed.depth.min(5); // Cap at 5

        let dir = match super::inspect_path_access(path, &working_dir) {
            Ok(access) => access.path,
            Err(err) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: err.to_string(),
                    success: false,
                });
            }
        };
        // The existence/type checks and the recursive `read_dir` traversal are
        // BLOCKING syscalls. On a hung filesystem they would stall the async
        // worker and make the turn uncancellable, so run them on the blocking
        // pool — the executor stays responsive and Esc/cancel aborts promptly.
        let dir_for_scan = dir.clone();
        let scan = tokio::task::spawn_blocking(move || list_dir_scan(&dir_for_scan, depth))
            .await
            .unwrap_or_else(|_| Err("list_directory: scan task failed".to_string()));
        let mut lines = match scan {
            Ok(lines) => lines,
            Err(msg) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: msg,
                    success: false,
                });
            }
        };

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

/// Blocking existence/type checks + recursive directory scan, pulled out of
/// `ListDirTool::execute` so they run inside `tokio::task::spawn_blocking` and
/// don't stall the async worker on a hung filesystem. Returns Ok(lines) or an
/// Err message for the not-found / not-a-directory cases.
fn list_dir_scan(dir: &std::path::Path, depth: usize) -> std::result::Result<Vec<String>, String> {
    if !dir.exists() {
        return Err(format!("Directory not found: {}", dir.display()));
    }
    if !dir.is_dir() {
        return Err(format!("Not a directory: {}", dir.display()));
    }
    let mut lines = Vec::new();
    scan_dir(&mut lines, dir, 0, depth);
    Ok(lines)
}

fn scan_dir(lines: &mut Vec<String>, dir: &std::path::Path, depth: usize, max_depth: usize) {
    if depth > max_depth {
        return;
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut items: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| !super::should_skip_dir(&e.file_name().to_string_lossy()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn rejects_file_path_instead_of_returning_empty_listing() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("README.md");
        std::fs::write(&file, "# notes\n").unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = ListDirTool;
        let args = r#"{"path":"README.md"}"#;

        let result = tool.execute(args, &ctx).await.unwrap();
        assert!(!result.success);
        assert!(
            result.output.contains("Not a directory:"),
            "unexpected output: {}",
            result.output
        );
        assert!(
            result.output.contains("README.md"),
            "output should name the file path: {}",
            result.output
        );
    }
}
