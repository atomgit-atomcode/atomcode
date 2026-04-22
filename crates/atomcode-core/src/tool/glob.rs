use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::process::Command;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

pub struct GlobTool;

#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
    path: Option<String>,
}

#[async_trait]
impl Tool for GlobTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "glob",
            description: "Find files by name pattern. Returns matching file paths.\n\
                Use this when you need to find files by name or extension, NOT by content (use grep for content search).\n\
                Pattern examples:\n\
                - All Rust files: \"**/*.rs\"\n\
                - Vue files in views: \"src/views/**/*.vue\"\n\
                - Specific filename anywhere: \"**/config.ts\"\n\
                - All files in a folder: \"src/components/*\"\n\
                Common use cases:\n\
                - Find all view/page files before deciding which to edit.\n\
                - Find config or entry files in an unfamiliar project.\n\
                - Check what files exist in a directory.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Glob pattern (e.g. **/*.rs, src/**/*.ts)" },
                    "path": { "type": "string", "description": "Base directory (default: working directory)" }
                },
                "required": ["pattern"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    fn approval_with_context(&self, args: &str, ctx: &ToolContext) -> ApprovalRequirement {
        let parsed = match serde_json::from_str::<GlobArgs>(args) {
            Ok(parsed) => parsed,
            Err(_) => return self.approval(args),
        };
        let working_dir = match ctx.working_dir.try_read() {
            Ok(wd) => wd.clone(),
            Err(_) => return self.approval(args),
        };
        let base_dir = match super::inspect_path_access(parsed.path.as_deref().unwrap_or("."), &working_dir) {
            Ok(access) => access.path.to_string_lossy().to_string(),
            Err(_) => return self.approval(args),
        };
        let search_dir = derive_search_dir(&base_dir, &parsed.pattern);
        match super::approval_for_path(&search_dir, &working_dir, super::ExternalPathAction::Enumerate) {
            Ok(approval) => approval,
            Err(_) => self.approval(args),
        }
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: GlobArgs = serde_json::from_str(args)?;
        let wd = ctx.working_dir.read().await.clone();
        let base_dir = match super::inspect_path_access(parsed.path.as_deref().unwrap_or("."), &wd) {
            Ok(access) => access.path.to_string_lossy().to_string(),
            Err(err) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: err.to_string(),
                    success: false,
                });
            }
        };

        // Parse pattern: split into (search_dir, name_pattern).
        // Handles all forms:
        //   "**/*.java"                          → (base_dir, "*.java")
        //   "src/views/**/*.vue"                 → (base_dir/src/views, "*.vue")
        //   "/absolute/path/**/*.java"           → (/absolute/path, "*.java")
        //   "/absolute/path/**/*Auth*.java"      → (/absolute/path, "*Auth*.java")
        //   "*.vue"                              → (base_dir, "*.vue")
        //   "**/config.ts"                       → (base_dir, "config.ts")
        let search_dir = derive_search_dir(&base_dir, &parsed.pattern);
        let name_pattern = derive_name_pattern(&parsed.pattern);

        // Verify search directory exists.
        if !std::path::Path::new(&search_dir).is_dir() {
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!(
                    "No files matching '{}' (directory '{}' does not exist)",
                    parsed.pattern, search_dir
                ),
                success: true,
            });
        }

        let mut find_args = vec![search_dir.clone(), "-name".to_string(), name_pattern];
        for skip in super::SKIP_DIRS {
            find_args.push("-not".to_string());
            find_args.push("-path".to_string());
            find_args.push(format!("*/{skip}/*"));
        }
        // Also skip prefix-matched directories (e.g. .venv-*)
        for prefix in super::SKIP_DIR_PREFIXES {
            find_args.push("-not".to_string());
            find_args.push("-path".to_string());
            find_args.push(format!("*/{prefix}*/*"));
        }
        let output = Command::new("find")
            .args(&find_args)
            .current_dir(&wd)
            .output()
            .await?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut files: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
        files.sort();

        let result = if files.is_empty() {
            format!("No files matching '{}'", parsed.pattern)
        } else {
            let total = files.len();
            let shown: Vec<&str> = files.into_iter().take(100).collect();

            let mut out = shown.join("\n");
            if total > 100 {
                out.push_str(&format!("\n\n[{} more files not shown]", total - 100));
            }
            format!("{} files found:\n{}", total, out)
        };

        Ok(ToolResult {
            call_id: String::new(),
            output: result,
            success: true,
        })
    }
}

fn derive_search_dir(base_dir: &str, pattern: &str) -> String {
    if let Some(star_pos) = pattern.find("**/") {
        let dir_part = pattern[..star_pos].trim_end_matches('/');
        if dir_part.is_empty() {
            base_dir.to_string()
        } else if std::path::Path::new(dir_part).is_absolute() {
            dir_part.to_string()
        } else {
            std::path::Path::new(base_dir).join(dir_part).to_string_lossy().to_string()
        }
    } else if let Some(last_slash) = pattern.rfind('/') {
        let dir_part = &pattern[..last_slash];
        if std::path::Path::new(dir_part).is_absolute() {
            dir_part.to_string()
        } else {
            std::path::Path::new(base_dir).join(dir_part).to_string_lossy().to_string()
        }
    } else {
        base_dir.to_string()
    }
}

fn derive_name_pattern(pattern: &str) -> String {
    if let Some(star_pos) = pattern.find("**/") {
        let after_stars = &pattern[star_pos + 3..];
        after_stars.rsplit('/').next().unwrap_or(after_stars).to_string()
    } else if let Some(last_slash) = pattern.rfind('/') {
        pattern[last_slash + 1..].to_string()
    } else {
        pattern.to_string()
    }
}
