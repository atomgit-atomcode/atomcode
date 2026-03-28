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

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: GlobArgs = serde_json::from_str(args)?;
        let wd = ctx.working_dir.read().await.clone();
        let base_dir = match &parsed.path {
            Some(p) if std::path::Path::new(p).is_absolute() => p.clone(),
            Some(p) => wd.join(p).to_string_lossy().to_string(),
            None => wd.to_string_lossy().to_string(),
        };

        // Parse pattern: split into (search_dir, name_pattern).
        // Handles all forms:
        //   "**/*.java"                          → (base_dir, "*.java")
        //   "src/views/**/*.vue"                 → (base_dir/src/views, "*.vue")
        //   "/absolute/path/**/*.java"           → (/absolute/path, "*.java")
        //   "/absolute/path/**/*Auth*.java"      → (/absolute/path, "*Auth*.java")
        //   "*.vue"                              → (base_dir, "*.vue")
        //   "**/config.ts"                       → (base_dir, "config.ts")
        let (search_dir, name_pattern) = {
            let p = &parsed.pattern;

            // Split at the FIRST occurrence of "**/" — everything before is directory,
            // everything after (minus further path segments) is the name pattern.
            if let Some(star_pos) = p.find("**/") {
                let dir_part = &p[..star_pos];
                let after_stars = &p[star_pos + 3..]; // skip "**/"

                // after_stars might be "*.java" or "sub/*.java" — take the last segment as name
                let name_part = after_stars.rsplit('/').next().unwrap_or(after_stars);

                // Build search directory: dir_part might be absolute, relative, or empty
                let dir = if dir_part.is_empty() {
                    base_dir.clone()
                } else {
                    let trimmed = dir_part.trim_end_matches('/');
                    if std::path::Path::new(trimmed).is_absolute() {
                        trimmed.to_string()
                    } else {
                        std::path::Path::new(&base_dir)
                            .join(trimmed)
                            .to_string_lossy()
                            .to_string()
                    }
                };
                (dir, name_part.to_string())
            } else if let Some(last_slash) = p.rfind('/') {
                // No "**/" but has directory: "src/components/*.vue"
                let dir_part = &p[..last_slash];
                let name_part = &p[last_slash + 1..];
                let dir = if std::path::Path::new(dir_part).is_absolute() {
                    dir_part.to_string()
                } else {
                    std::path::Path::new(&base_dir)
                        .join(dir_part)
                        .to_string_lossy()
                        .to_string()
                };
                (dir, name_part.to_string())
            } else {
                // Bare pattern: "*.vue" or "config.ts"
                (base_dir.clone(), p.clone())
            }
        };

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
