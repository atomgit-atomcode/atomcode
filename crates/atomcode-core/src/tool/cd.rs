use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

pub struct CdTool;

#[derive(Deserialize)]
struct CdArgs {
    path: String,
}

#[async_trait]
impl Tool for CdTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "change_dir",
            description: "Change the working directory. All subsequent file operations and bash commands will execute in the new directory.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "The directory path to change to. Can be absolute or relative to current working directory."
                    }
                },
                "required": ["path"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    fn approval_with_context(&self, args: &str, ctx: &ToolContext) -> ApprovalRequirement {
        let parsed = match serde_json::from_str::<CdArgs>(args) {
            Ok(parsed) => parsed,
            Err(_) => return self.approval(args),
        };
        let working_dir = match ctx.working_dir.try_read() {
            Ok(wd) => wd.clone(),
            Err(_) => return self.approval(args),
        };
        match super::approval_for_path(
            &parsed.path,
            &working_dir,
            super::ExternalPathAction::Enumerate,
        ) {
            Ok(approval) => approval,
            Err(_) => self.approval(args),
        }
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: CdArgs = serde_json::from_str(args)?;
        let path = parsed.path.as_str();

        // Resolve the path (expand ~ if needed, resolve relative to current working_dir)
        let current_wd = ctx.working_dir.read().await.clone();
        let target = if path == "~" {
            super::real_home_dir().unwrap_or_else(|| PathBuf::from(path))
        } else if let Some(rest) = path.strip_prefix("~/") {
            super::real_home_dir()
                .map(|h| h.join(rest))
                .unwrap_or_else(|| PathBuf::from(path))
        } else if path.starts_with('/') {
            PathBuf::from(path)
        } else {
            current_wd.join(path)
        };

        // Validate the target is a directory
        if target.is_dir() {
            let resolved = std::fs::canonicalize(&target).unwrap_or(target);
            // Update shared working directory
            let mut wd = ctx.working_dir.write().await;
            *wd = resolved.clone();
            Ok(ToolResult {
                call_id: String::new(),
                output: format!("Changed working directory to {}", resolved.display()),
                success: true,
            })
        } else if target.exists() {
            Ok(ToolResult {
                call_id: String::new(),
                output: format!("Not a directory: {}", target.display()),
                success: false,
            })
        } else {
            Ok(ToolResult {
                call_id: String::new(),
                output: format!("Path does not exist: {}", target.display()),
                success: false,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn tilde_prefixed_relative_dir_is_not_expanded_to_home() {
        let workspace = TempDir::new().unwrap();
        let target = workspace.path().join("~cache");
        std::fs::create_dir(&target).unwrap();

        let ctx = ToolContext::new(workspace.path().to_path_buf());
        let tool = CdTool;

        let result = tool.execute(r#"{"path":"~cache"}"#, &ctx).await.unwrap();
        assert!(result.success, "unexpected output: {}", result.output);
        assert_eq!(
            *ctx.working_dir.read().await,
            std::fs::canonicalize(target).unwrap()
        );
    }

    #[tokio::test]
    async fn slash_after_tilde_still_expands_to_home() {
        let Some(home) = super::super::real_home_dir() else {
            return;
        };

        let ctx = ToolContext::new(PathBuf::from("/tmp"));
        let tool = CdTool;

        let result = tool.execute(r#"{"path":"~/"}"#, &ctx).await.unwrap();
        assert!(result.success, "unexpected output: {}", result.output);
        assert_eq!(
            *ctx.working_dir.read().await,
            std::fs::canonicalize(home).unwrap()
        );
    }
}
