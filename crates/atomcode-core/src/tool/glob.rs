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
        let base_dir =
            match super::inspect_path_access(parsed.path.as_deref().unwrap_or("."), &working_dir) {
                Ok(access) => access.path.to_string_lossy().to_string(),
                Err(_) => return self.approval(args),
            };
        let search_dir = derive_search_dir(&base_dir, &parsed.pattern);
        match super::approval_for_path(
            &search_dir,
            &working_dir,
            super::ExternalPathAction::Enumerate,
        ) {
            Ok(approval) => approval,
            Err(_) => self.approval(args),
        }
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: GlobArgs = serde_json::from_str(args)?;
        let wd = ctx.working_dir.read().await.clone();
        let base_dir = match super::inspect_path_access(parsed.path.as_deref().unwrap_or("."), &wd)
        {
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

        // Verify search directory exists. If not, walk the workspace to find
        // directories with the same basename so the agent can self-correct
        // without a round of manual `ls`. 2026-04-22: added for P0 #4 after
        // 426-atom 2026-04-21 session where agent spent 5 turns listing
        // directories because `/426-atom/index.html` was actually at
        // `/426-atom/presentation/index.html`.
        if !std::path::Path::new(&search_dir).is_dir() {
            let target_basename = std::path::Path::new(&search_dir)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let mut dir_matches: Vec<String> = Vec::new();
            if !target_basename.is_empty() {
                fn find_dir(
                    dir: &std::path::Path,
                    target: &str,
                    depth: usize,
                    max_depth: usize,
                    results: &mut Vec<String>,
                ) {
                    if depth > max_depth || results.len() >= 20 {
                        return;
                    }
                    if let Ok(entries) = std::fs::read_dir(dir) {
                        for entry in entries.flatten() {
                            let name = entry.file_name().to_string_lossy().to_string();
                            if name.starts_with('.') || super::should_skip_dir(&name) {
                                continue;
                            }
                            let p = entry.path();
                            if p.is_dir() {
                                if name == target {
                                    results.push(p.to_string_lossy().to_string());
                                }
                                find_dir(&p, target, depth + 1, max_depth, results);
                            }
                        }
                    }
                }
                find_dir(
                    std::path::Path::new(&wd),
                    &target_basename,
                    0,
                    5,
                    &mut dir_matches,
                );
            }
            let hint = if dir_matches.is_empty() {
                String::new()
            } else {
                dir_matches
                    .sort_by_key(|d| std::cmp::Reverse(super::shared_prefix_len(&search_dir, d)));
                let shown: Vec<String> = dir_matches
                    .iter()
                    .take(3)
                    .map(|d| format!("  {}", d))
                    .collect();
                format!(
                    "\n\nSimilar directories found — did you mean one of these?\n{}",
                    shown.join("\n")
                )
            };
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!(
                    "No files matching '{}' (directory '{}' does not exist){}",
                    parsed.pattern, search_dir, hint
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
        let mut cmd = Command::new("find");
        cmd.args(&find_args)
            .current_dir(&wd);
        crate::process_utils::suppress_console_window(&mut cmd);
        let output = cmd.output().await?;

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
            std::path::Path::new(base_dir)
                .join(dir_part)
                .to_string_lossy()
                .to_string()
        }
    } else if let Some(last_slash) = pattern.rfind('/') {
        let dir_part = &pattern[..last_slash];
        if std::path::Path::new(dir_part).is_absolute() {
            dir_part.to_string()
        } else {
            std::path::Path::new(base_dir)
                .join(dir_part)
                .to_string_lossy()
                .to_string()
        }
    } else {
        base_dir.to_string()
    }
}

fn derive_name_pattern(pattern: &str) -> String {
    if let Some(star_pos) = pattern.find("**/") {
        let after_stars = &pattern[star_pos + 3..];
        after_stars
            .rsplit('/')
            .next()
            .unwrap_or(after_stars)
            .to_string()
    } else if let Some(last_slash) = pattern.rfind('/') {
        pattern[last_slash + 1..].to_string()
    } else {
        pattern.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolContext;
    use tempfile::TempDir;

    /// P0 #4: when a glob's search dir doesn't exist, workspace-walk for dirs
    /// with the same basename and surface top-3 by path-prefix similarity.
    /// Regression for 426-atom 2026-04-21 session where agent burned 5
    /// turns of `ls` to locate `/426-atom/presentation/` after asking glob
    /// under `/426-atom/frontend/` (wrong segment).
    #[tokio::test]
    async fn glob_suggests_similar_directory_when_search_dir_missing() {
        let dir = TempDir::new().unwrap();
        // Set up a workspace with a `presentation/` dir that agent will miss.
        std::fs::create_dir_all(dir.path().join("hermes/presentation")).unwrap();
        std::fs::create_dir_all(dir.path().join("other/presentation")).unwrap();
        std::fs::write(
            dir.path().join("hermes/presentation/app.vue"),
            "<template></template>",
        )
        .unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = GlobTool;
        // Agent asks for `.vue` files under the WRONG path — `hermes/frontend/presentation`
        // doesn't exist, but `hermes/presentation` does.
        let wrong = dir.path().join("hermes/frontend/presentation");
        let args = format!(r#"{{"pattern":"{}/**/*.vue"}}"#, wrong.display());

        let r = tool.execute(&args, &ctx).await.unwrap();
        assert!(r.success);
        assert!(
            r.output.contains("does not exist"),
            "missing exists-check msg: {}",
            r.output
        );
        assert!(
            r.output.contains("Similar directories found"),
            "must suggest similar directories: {}",
            r.output
        );
        // Both `presentation/` dirs exist under wd; the hermes one shares
        // more path prefix with what the agent asked for, so it must be
        // listed first.
        let hermes_pos = r.output.find("hermes/presentation").unwrap();
        let other_pos = r.output.find("other/presentation").unwrap();
        assert!(
            hermes_pos < other_pos,
            "hermes/presentation must outrank other/presentation. output:\n{}",
            r.output
        );
    }

    #[tokio::test]
    async fn glob_existing_dir_does_not_trigger_hint() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/a.ts"), "export {};").unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = GlobTool;
        let args = format!(
            r#"{{"pattern":"{}/**/*.ts"}}"#,
            dir.path().join("src").display()
        );

        let r = tool.execute(&args, &ctx).await.unwrap();
        assert!(r.success);
        assert!(
            !r.output.contains("Similar directories found"),
            "no hint should fire when dir exists: {}",
            r.output
        );
    }
}
