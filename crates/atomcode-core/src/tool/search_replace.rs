use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use ignore::WalkBuilder;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

pub struct SearchReplaceTool;

#[derive(Deserialize)]
struct SearchReplaceArgs {
    /// Search pattern (literal string by default, regex if `regex` is true)
    search: String,
    /// Replacement string (supports regex capture groups $1, $2 if regex mode)
    replace: String,
    /// File glob pattern to limit scope, e.g. "*.vue", "*.css", "*.ts" (default: all files)
    #[serde(default)]
    glob: Option<String>,
    /// Directory to search in (default: working directory)
    #[serde(default)]
    path: Option<String>,
    /// Use regex mode (default: false = literal string matching)
    #[serde(default)]
    regex: bool,
}

#[async_trait]
impl Tool for SearchReplaceTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "search_replace",
            description: "Search and replace text across multiple files. Replaces ALL occurrences in ALL matching files.\n\
                When to use:\n\
                - Rename a CSS class, variable, or import across the entire project\n\
                - Change colors, sizes, or other repeated values in bulk\n\
                - Migrate API endpoints, config keys, or string literals\n\
                - Any change that affects many files with the same pattern\n\
                When NOT to use:\n\
                - Editing a single file: use edit_file instead\n\
                - Complex structural refactoring: use edit_file per file\n\
                Examples:\n\
                - Change color: {\"search\": \"bg-blue-600\", \"replace\": \"bg-violet-600\", \"glob\": \"*.vue\"}\n\
                - Rename class: {\"search\": \"rounded-2xl\", \"replace\": \"rounded-lg\", \"glob\": \"*.vue\"}\n\
                - Regex rename: {\"search\": \"bg-blue-(\\\\d+)\", \"replace\": \"bg-violet-$1\", \"glob\": \"*.vue\", \"regex\": true}".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "search": { "type": "string", "description": "Text or regex pattern to find" },
                    "replace": { "type": "string", "description": "Replacement text (use $1, $2 for regex captures)" },
                    "glob": { "type": "string", "description": "File pattern to limit scope, e.g. \"*.vue\", \"*.css\" (default: all files)" },
                    "path": { "type": "string", "description": "Directory to search in (default: working directory)" },
                    "regex": { "type": "boolean", "description": "Use regex matching (default: false = literal)" }
                },
                "required": ["search", "replace"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        // Bulk replacement across files is potentially destructive
        // Auto-approve: search_replace is safe (literal/regex matching, respects .gitignore).
        // Requiring approval caused the model to avoid it and use 30+ edit_file calls instead.
        ApprovalRequirement::AutoApprove
    }

    fn approval_with_context(&self, args: &str, ctx: &ToolContext) -> ApprovalRequirement {
        let parsed = match serde_json::from_str::<SearchReplaceArgs>(args) {
            Ok(parsed) => parsed,
            Err(_) => return self.approval(args),
        };
        let working_dir = match ctx.working_dir.try_read() {
            Ok(wd) => wd.clone(),
            Err(_) => return self.approval(args),
        };
        let raw_path = parsed.path.as_deref().unwrap_or(".");
        match super::approval_for_path(raw_path, &working_dir, super::ExternalPathAction::Write) {
            Ok(approval) => approval,
            Err(_) => self.approval(args),
        }
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: SearchReplaceArgs = serde_json::from_str(args)?;
        let wd = ctx.working_dir.read().await.clone();
        let search_dir = match super::inspect_path_access(parsed.path.as_deref().unwrap_or("."), &wd) {
            Ok(access) => access.path,
            Err(err) => {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: err.to_string(),
                    success: false,
                });
            }
        };

        if !search_dir.exists() {
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!("Directory not found: {}", search_dir.display()),
                success: false,
            });
        }

        // Build regex or literal matcher
        let re = if parsed.regex {
            match regex::Regex::new(&parsed.search) {
                Ok(r) => r,
                Err(e) => {
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: format!("Invalid regex '{}': {}", parsed.search, e),
                        success: false,
                    });
                }
            }
        } else {
            // Escape the literal string to use as regex
            regex::Regex::new(&regex::escape(&parsed.search)).unwrap()
        };

        // Walk files
        let mut walker = WalkBuilder::new(&search_dir);
        walker.hidden(true).git_ignore(true);
        let walk = walker.build();

        let mut total_replacements = 0usize;
        let mut files_modified = Vec::new();
        let mut files_scanned = 0usize;

        for entry in walk {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };

            if !entry.file_type().map_or(false, |ft| ft.is_file()) {
                continue;
            }

            let file_path = entry.path();

            // Apply glob filter
            if let Some(ref glob_pat) = parsed.glob {
                let name = file_path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                if !glob_match(glob_pat, name) {
                    continue;
                }
            }

            // Read file
            let content = match std::fs::read_to_string(file_path) {
                Ok(c) => c,
                Err(_) => continue, // skip binary/unreadable files
            };

            files_scanned += 1;

            // Quick check: does the file contain the search pattern?
            if !re.is_match(&content) {
                continue;
            }

            // Count and replace
            let count = re.find_iter(&content).count();
            let new_content = re.replace_all(&content, parsed.replace.as_str()).to_string();

            if new_content != content {
                // Backup before write
                ctx.file_history.lock().await
                    .backup_before_write(&file_path.to_string_lossy()).await;
                // Write back
                if let Err(e) = std::fs::write(file_path, &new_content) {
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: format!("Failed to write {}: {}", file_path.display(), e),
                        success: false,
                    });
                }
                total_replacements += count;
                files_modified.push(format!("  {} ({} replacements)", file_path.display(), count));
            }
        }

        if files_modified.is_empty() {
            return Ok(ToolResult {
                call_id: String::new(),
                output: format!(
                    "No matches found for '{}' in {} ({} files scanned)",
                    parsed.search,
                    search_dir.display(),
                    files_scanned,
                ),
                success: false,
            });
        }

        let output = format!(
            "Replaced '{}' → '{}': {} replacements across {} files.\n{}",
            parsed.search,
            parsed.replace,
            total_replacements,
            files_modified.len(),
            files_modified.join("\n"),
        );

        Ok(ToolResult {
            call_id: String::new(),
            output,
            success: true,
        })
    }
}

/// Simple glob matching: supports *.ext and exact match.
fn glob_match(pattern: &str, filename: &str) -> bool {
    if pattern.starts_with("*.") {
        let ext = &pattern[1..]; // ".vue", ".css"
        filename.ends_with(ext)
    } else if pattern.contains('*') {
        // Basic wildcard: convert to simple check
        let parts: Vec<&str> = pattern.split('*').collect();
        if parts.len() == 2 {
            filename.starts_with(parts[0]) && filename.ends_with(parts[1])
        } else {
            filename == pattern
        }
    } else {
        filename == pattern
    }
}
