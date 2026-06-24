use anyhow::Result;
use async_trait::async_trait;
use globset::{Glob, GlobMatcher};
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::json;

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

    fn validate_args(&self, args: &str) -> std::result::Result<(), String> {
        serde_json::from_str::<SearchReplaceArgs>(args)
            .map(|_| ())
            .map_err(|e| format!(
                "{} (could not parse search_replace arguments; check `search` and `replace` are present)",
                e
            ))
    }

    fn approval(&self, args: &str) -> ApprovalRequirement {
        // search_replace is generally cheap to AutoApprove — the model
        // benchmarks confirmed that requiring approval pushed it to
        // emit 30+ edit_file calls instead of one bulk replace. But the
        // "auto-approve everywhere" carve-out is exactly the gap that
        // session-grant bypass exploited on edit_file: a sensitive
        // target (.env / id_rsa / *.pem / /etc/*) should ALWAYS prompt,
        // never silently inherit a prior [A] press on a safe scope.
        let parsed = match serde_json::from_str::<SearchReplaceArgs>(args) {
            Ok(p) => p,
            Err(_) => return ApprovalRequirement::RequireApproval(
                "Cannot parse search_replace args — requiring approval for safety".to_string()
            ),
        };
        let scope = parsed.path.as_deref().unwrap_or(".");
        if super::is_sensitive_input_path(scope) {
            return ApprovalRequirement::RequireApproval(format!(
                "Bulk replace targeting sensitive path: {}",
                scope
            ));
        }
        ApprovalRequirement::AutoApprove
    }

    fn approval_with_context(&self, args: &str, ctx: &ToolContext) -> ApprovalRequirement {
        // Scope any prompt to the exact target file (see
        // `scoped_write_approval`): the sensitivity check from `approval()`
        // and the out-of-workspace boundary check both route through a
        // per-file `RequireApprovalScoped`, so a session [A] remembers THAT
        // file (re-running on it stops re-prompting) while a tool-wide grant
        // can't bypass a sensitive bulk replace.
        let base = self.approval(args);
        let parsed = match serde_json::from_str::<SearchReplaceArgs>(args) {
            Ok(parsed) => parsed,
            Err(_) => return base,
        };
        let working_dir = match ctx.working_dir.try_read() {
            Ok(wd) => wd.clone(),
            Err(_) => return base,
        };
        let raw_path = parsed.path.as_deref().unwrap_or(".");
        super::scoped_write_approval(raw_path, &working_dir, base)
    }

    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: SearchReplaceArgs = serde_json::from_str(args)?;
        let wd = ctx.working_dir.read().await.clone();
        let search_dir =
            match super::inspect_path_access(parsed.path.as_deref().unwrap_or("."), &wd) {
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

        let glob_filter = match parsed.glob.as_deref() {
            Some(pattern) => match FileGlob::new(pattern) {
                Ok(filter) => Some(filter),
                Err(e) => {
                    return Ok(ToolResult {
                        call_id: String::new(),
                        output: format!("Invalid glob '{}': {}", pattern, e),
                        success: false,
                    });
                }
            },
            None => None,
        };

        // Phase 1: walk + read + compute replacements on the BLOCKING pool.
        // The `ignore` walk and per-file `std::fs::read_to_string` would
        // otherwise stall the async worker on a hung filesystem and make the
        // turn uncancellable. Writes + history/LSP bookkeeping need `ctx`'s
        // async locks, so they run in phase 2 below — only for the few matched
        // files; the heavy full-tree traversal is what must stay off-thread.
        let scan_dir = search_dir.clone();
        let replace = parsed.replace.clone();
        let (modified, files_scanned) = tokio::task::spawn_blocking(move || {
            sr_scan(&scan_dir, &re, glob_filter.as_ref(), &replace)
        })
        .await
        .unwrap_or_else(|_| (Vec::new(), 0usize));

        // Phase 2: backup → write → LSP/store bookkeeping per modified file.
        // backup_before_write must precede the write (it snapshots the old
        // on-disk content), so this stays sequential and async.
        let mut total_replacements = 0usize;
        let mut files_modified = Vec::new();
        for (file_path, new_content, count) in modified {
            ctx.file_history
                .lock()
                .await
                .backup_before_write(&file_path.to_string_lossy())
                .await;
            if let Err(e) = tokio::fs::write(&file_path, &new_content).await {
                return Ok(ToolResult {
                    call_id: String::new(),
                    output: format!("Failed to write {}: {}", file_path.display(), e),
                    success: false,
                });
            }
            // Canonicalize so downstream by-path lookups (FileStore, LSP)
            // match what read.rs stored — the walk can yield the un-resolved
            // symlink form (macOS `/var/...` vs `/private/var/...`).
            let canon = tokio::fs::canonicalize(&file_path)
                .await
                .unwrap_or_else(|_| file_path.clone());
            ctx.notify_lsp_file_changed(&canon, &new_content).await;
            ctx.file_store.write().await.invalidate(&canon);
            total_replacements += count;
            files_modified.push(format!("  {} ({} replacements)", file_path.display(), count));
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

/// Synchronous walk + read + replace computation, pulled out of
/// `SearchReplaceTool::execute` so the (blocking) `ignore` walk and per-file
/// reads run inside `tokio::task::spawn_blocking` and don't stall the async
/// worker on a hung filesystem. Does NOT write — returns the list of
/// (path, new_content, replacement_count) for changed files plus the count of
/// files scanned. Writes + history/LSP bookkeeping happen in async phase 2
/// because they need `ctx`'s async locks.
fn sr_scan(
    search_dir: &std::path::Path,
    re: &regex::Regex,
    glob_filter: Option<&FileGlob>,
    replace: &str,
) -> (Vec<(std::path::PathBuf, String, usize)>, usize) {
    let mut walker = WalkBuilder::new(search_dir);
    walker.hidden(true).git_ignore(true);
    let walk = walker.build();

    let mut modified: Vec<(std::path::PathBuf, String, usize)> = Vec::new();
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
        if let Some(filter) = glob_filter {
            if !filter.is_match(file_path, search_dir) {
                continue;
            }
        }
        let content = match std::fs::read_to_string(file_path) {
            Ok(c) => c,
            Err(_) => continue, // skip binary/unreadable files
        };
        files_scanned += 1;
        if !re.is_match(&content) {
            continue;
        }
        let count = re.find_iter(&content).count();
        let new_content = re.replace_all(&content, replace).to_string();
        if new_content != content {
            modified.push((file_path.to_path_buf(), new_content, count));
        }
    }
    (modified, files_scanned)
}

struct FileGlob {
    pattern_has_path: bool,
    matcher: GlobMatcher,
}

impl FileGlob {
    fn new(pattern: &str) -> std::result::Result<Self, globset::Error> {
        let normalized = pattern.replace('\\', "/");
        Ok(Self {
            pattern_has_path: normalized.contains('/'),
            matcher: Glob::new(&normalized)?.compile_matcher(),
        })
    }

    fn is_match(&self, file_path: &std::path::Path, search_dir: &std::path::Path) -> bool {
        let candidate = if self.pattern_has_path {
            file_path.strip_prefix(search_dir).unwrap_or(file_path)
        } else {
            match file_path.file_name().and_then(|name| name.to_str()) {
                Some(name) => return self.matcher.is_match(name),
                None => return false,
            }
        };

        let normalized = candidate.to_string_lossy().replace('\\', "/");
        self.matcher.is_match(normalized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{Tool, ToolContext};
    use tempfile::TempDir;

    #[tokio::test]
    async fn search_replace_path_glob_matches_relative_paths() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        std::fs::write(dir.path().join("src/app.ts"), "const v = 'needle';\n").unwrap();
        std::fs::write(dir.path().join("tests/app.ts"), "const v = 'needle';\n").unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let args = r#"{"search":"needle","replace":"replaced","glob":"src/**/*.ts"}"#;
        let result = SearchReplaceTool.execute(args, &ctx).await.unwrap();

        assert!(result.success, "{}", result.output);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/app.ts")).unwrap(),
            "const v = 'replaced';\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("tests/app.ts")).unwrap(),
            "const v = 'needle';\n"
        );
    }

    /// Regression: sensitive in-workspace path must return
    /// RequireApprovalScoped so a tool-wide session [A] on search_replace
    /// cannot disarm the guard for `.env` / `id_rsa` / `*.pem` etc., while a
    /// per-file [A] is remembered for that exact file. Mirrors the edit.rs fix.
    #[test]
    fn search_replace_sensitive_in_workspace_path_returns_scoped() {
        let workspace = tempfile::TempDir::new().unwrap();
        // is_sensitive_input_path matches SECRET_FILE_NAMES by file_name
        // anywhere on disk. `.env` is the canonical example.
        let secret = workspace.path().join(".env");
        let args = serde_json::json!({
            "search": "foo",
            "replace": "bar",
            "path": secret.to_string_lossy(),
        })
        .to_string();
        let ctx = ToolContext::new(workspace.path().to_path_buf());
        let approval = SearchReplaceTool.approval_with_context(&args, &ctx);
        assert!(
            matches!(approval, ApprovalRequirement::RequireApprovalScoped { .. }),
            "sensitive in-workspace path (.env) must require a per-file scoped prompt",
        );
    }

    /// Cross-layer: session grant on search_replace must NOT bypass the
    /// sensitive-path Always. Pins the end-to-end contract.
    #[test]
    fn search_replace_sensitive_path_through_store_with_session_grant_asks() {
        use crate::tool::{PermissionDecision, PermissionStore};
        let workspace = tempfile::TempDir::new().unwrap();
        let secret = workspace.path().join(".env");
        let args = serde_json::json!({
            "search": "foo",
            "replace": "bar",
            "path": secret.to_string_lossy(),
        })
        .to_string();
        let ctx = ToolContext::new(workspace.path().to_path_buf());
        let mut store = PermissionStore::new();
        store.grant_session("search_replace");
        let approval = SearchReplaceTool.approval_with_context(&args, &ctx);
        let decision = store.check("search_replace", &approval);
        assert!(
            matches!(decision, PermissionDecision::Ask(_)),
            "session grant must NOT bypass sensitive-path guard, got {decision:?}",
        );
    }

    /// Negative control: ordinary in-workspace path remains AutoApprove
    /// so the "model uses 30+ edit_file instead of one search_replace"
    /// regression the original AutoApprove was guarding against stays
    /// fixed.
    #[test]
    fn search_replace_ordinary_in_workspace_path_is_auto_approve() {
        let workspace = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(workspace.path().join("src")).unwrap();
        let args = serde_json::json!({
            "search": "foo",
            "replace": "bar",
            "path": workspace.path().join("src").to_string_lossy(),
        })
        .to_string();
        let ctx = ToolContext::new(workspace.path().to_path_buf());
        let approval = SearchReplaceTool.approval_with_context(&args, &ctx);
        assert!(
            matches!(approval, ApprovalRequirement::AutoApprove),
            "non-sensitive in-workspace path must stay AutoApprove",
        );
    }

    #[tokio::test]
    async fn search_replace_filename_glob_still_matches_nested_files() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/app.ts"), "const v = 'needle';\n").unwrap();
        std::fs::write(dir.path().join("src/app.md"), "needle\n").unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let args = r#"{"search":"needle","replace":"replaced","glob":"*.ts"}"#;
        let result = SearchReplaceTool.execute(args, &ctx).await.unwrap();

        assert!(result.success, "{}", result.output);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/app.ts")).unwrap(),
            "const v = 'replaced';\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/app.md")).unwrap(),
            "needle\n"
        );
    }
}
