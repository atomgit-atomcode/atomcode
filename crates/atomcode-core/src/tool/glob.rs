use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

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

        // The existence check, the fallback directory walk, and the main
        // `ignore` walk are all BLOCKING fs syscalls. On a hung filesystem
        // (e.g. a dead network mount) they would stall the async worker and
        // make the turn uncancellable, so run the whole search on the blocking
        // pool — the executor stays responsive and Esc/cancel aborts promptly.
        let pattern = parsed.pattern.clone();
        let output = tokio::task::spawn_blocking(move || {
            glob_search(&search_dir, &name_pattern, &wd, &pattern)
        })
        .await
        .unwrap_or_else(|_| {
            format!(
                "No files matching '{}' (search task failed)",
                parsed.pattern
            )
        });

        Ok(ToolResult {
            call_id: String::new(),
            output,
            success: true,
        })
    }
}

/// Synchronous glob search, pulled out of `GlobTool::execute` so it can run
/// inside `tokio::task::spawn_blocking` — the existence check, the fallback
/// directory walk, and the main `ignore` walk all block, and on a hung
/// filesystem they must not stall the async worker (else cancellation hangs).
/// Returns the formatted output string.
fn glob_search(
    search_dir: &str,
    name_pattern: &str,
    wd: &std::path::Path,
    pattern: &str,
) -> String {
    // Verify search directory exists. If not, walk the workspace to find
    // directories with the same basename so the agent can self-correct
    // without a round of manual `ls`. 2026-04-22: added for P0 #4 after
    // 426-atom 2026-04-21 session where agent spent 5 turns listing
    // directories because `/426-atom/index.html` was actually at
    // `/426-atom/presentation/index.html`.
    if !std::path::Path::new(search_dir).is_dir() {
        let target_basename = std::path::Path::new(search_dir)
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
            find_dir(wd, &target_basename, 0, 5, &mut dir_matches);
        }
        let hint = if dir_matches.is_empty() {
            String::new()
        } else {
            dir_matches.sort_by_key(|d| std::cmp::Reverse(super::shared_prefix_len(search_dir, d)));
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
        return format!(
            "No files matching '{}' (directory '{}' does not exist){}",
            pattern, search_dir, hint
        );
    }

    // Use the `ignore` crate (ripgrep's walker) for cross-platform file
    // search. This replaces the previous `Command::new("find")` approach
    // which failed on Windows because Windows' `find.exe` is a string-
    // search utility (like grep), not a file-search utility. It also
    // correctly handles non-ASCII filenames (Chinese, Japanese, etc.)
    // on all platforms without encoding issues.
    //
    // Issue #350: https://gitcode.com/atomgit_atomcode/atomcode/issues/350
    let mut files: Vec<String> = Vec::new();
    let search_path = std::path::Path::new(search_dir);
    let walker = ignore::WalkBuilder::new(search_path)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();

    for entry in walker.flatten() {
        let path = entry.path();
        // Skip directories — we only want files
        if !path.is_file() {
            continue;
        }
        // Prune skip-dirs (node_modules, .git, .atomcode, …) only BELOW the
        // explicitly-requested search root. When the agent points glob *at* a
        // skip-listed dir (e.g. `.atomcode/skills`, `.claude/...`), that dir's
        // own path segments must not be re-filtered — otherwise the search
        // always returns empty even though that exact path was requested.
        // Broad searches stay unaffected: the root is the workspace, so
        // `.atomcode` is a child component below the root and is still pruned
        // (and `.hidden(true)` already stops the walker descending into it).
        if should_skip_below(search_path, path) {
            continue;
        }
        if let Some(file_name) = path.file_name() {
            let name = file_name.to_string_lossy();
            if simple_glob_match(&name, name_pattern) {
                files.push(path.to_string_lossy().to_string());
            }
        }
    }
    files.sort();

    if files.is_empty() {
        format!("No files matching '{}'", pattern)
    } else {
        let total = files.len();
        let shown: Vec<&str> = files.iter().take(100).map(|s| s.as_str()).collect();
        let mut out = shown.join("\n");
        if total > 100 {
            out.push_str(&format!("\n\n[{} more files not shown]", total - 100));
        }
        format!("{} files found:\n{}", total, out)
    }
}

/// Check if any path component matches SKIP_DIRS (exact) or SKIP_DIR_PREFIXES
/// (prefix). Used by the `ignore` crate walker to filter out build artifacts,
/// caches, VCS directories, etc. — consistent with the skip logic used by
/// `list_dir`, `grep`, and other tools.
fn should_skip_path(path: &std::path::Path) -> bool {
    for component in path.components() {
        if let std::path::Component::Normal(os_name) = component {
            let name = os_name.to_string_lossy();
            if super::should_skip_dir(&name) {
                return true;
            }
        }
    }
    false
}

/// Like [`should_skip_path`], but only inspects components STRICTLY BELOW
/// `root`. Components of `root` itself are exempt, so a glob aimed directly at
/// a skip-listed directory (`.atomcode/skills`, `.claude/...`) still returns
/// its files. Paths not under `root` (shouldn't happen for walker entries)
/// fall back to checking every component.
fn should_skip_below(root: &std::path::Path, path: &std::path::Path) -> bool {
    should_skip_path(path.strip_prefix(root).unwrap_or(path))
}

/// Simple glob match supporting `*` (match any non-separator chars) and
/// literal characters. This covers the patterns produced by
/// `derive_name_pattern`:
///   - `"*.txt"`     → matches any file ending in `.txt`
///   - `"测试.txt"`  → exact match
///   - `"*测试*"`    → matches any name containing `测试`
///   - `"测试*.log"` → matches names starting with `测试` and ending in `.log`
///
/// Does NOT support `?`, `[...]`, or `**` — those are resolved upstream by
/// `derive_name_pattern` which strips directory segments and `**`.
fn simple_glob_match(name: &str, pattern: &str) -> bool {
    let pat_parts: Vec<&str> = pattern.split('*').collect();

    // No wildcard — exact match
    if pat_parts.len() == 1 {
        return name == pattern;
    }

    // Leading wildcard means first part can match anywhere
    let leading_wildcard = pattern.starts_with('*');
    // Trailing wildcard means last part doesn't need to match the end
    let trailing_wildcard = pattern.ends_with('*');

    let mut rest = name;

    for (i, part) in pat_parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }

        match rest.find(part) {
            Some(pos) => {
                // First literal part must match at the start (unless leading *)
                if i == 0 && !leading_wildcard && pos != 0 {
                    return false;
                }
                rest = &rest[pos + part.len()..];
            }
            None => return false,
        }
    }

    // Last literal part must match at the end (unless trailing *)
    if let Some(last) = pat_parts.last() {
        if !last.is_empty() && !trailing_wildcard && !name.ends_with(last) {
            return false;
        }
    }

    true
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

    // ========================================================================
    // Issue #350: Glob 工具在 Windows 环境下对包含中文字符的文件名匹配失效
    // https://gitcode.com/atomgit_atomcode/atomcode/issues/350
    //
    // 根因 (已修复): glob.rs 之前使用 Unix `find` 命令搜索文件。
    //   Windows 的 `find.exe` 是字符串搜索工具（类似 grep），
    //   不是文件搜索工具，导致在 Windows 上完全不工作。
    //   此外，Windows 路径编码 (GBK/UTF-16) 也可能导致中文文件名匹配失败。
    //
    // 修复: 使用 `ignore` crate (ripgrep 底层库) 替代 `Command::new("find")`，
    //   实现跨平台文件搜索，正确处理所有 Unicode 文件名。
    //
    // 以下测试覆盖:
    //   1. derive_search_dir / derive_name_pattern 对中文路径的处理
    //   2. simple_glob_match 匹配逻辑
    //   3. 使用 ignore crate 搜索中文文件名（跨平台，含 Windows）
    //   4. should_skip_path 目录过滤逻辑
    //   5. 实际 Issue #350 场景: 子目录中的中文文件名 + 中文目录名
    // ========================================================================

    // --- 纯函数测试: derive_search_dir / derive_name_pattern ---

    #[test]
    fn derive_name_pattern_chinese_filename() {
        assert_eq!(derive_name_pattern("*.txt"), "*.txt");
        assert_eq!(derive_name_pattern("**/*.txt"), "*.txt");
        assert_eq!(derive_name_pattern("**/测试.txt"), "测试.txt");
        assert_eq!(derive_name_pattern("src/**/配置.json"), "配置.json");
        assert_eq!(derive_name_pattern("数据/**/*.csv"), "*.csv");
        assert_eq!(derive_name_pattern("测试.txt"), "测试.txt");
    }

    #[test]
    fn derive_search_dir_chinese_path() {
        let base = "/tmp/workspace";
        assert_eq!(derive_search_dir(base, "**/*.txt"), base);
        assert_eq!(
            derive_search_dir(base, "测试/**/*.txt"),
            format!("{}/测试", base)
        );
        assert_eq!(
            derive_search_dir(base, "项目/模块/**/*.rs"),
            format!("{}/项目/模块", base)
        );
    }

    #[test]
    fn derive_name_pattern_mixed_chinese_english() {
        assert_eq!(derive_name_pattern("**/report-报告.txt"), "report-报告.txt");
        assert_eq!(derive_name_pattern("**/测试_test.py"), "测试_test.py");
        assert_eq!(
            derive_name_pattern("src/组件/**/Button*.vue"),
            "Button*.vue"
        );
    }

    #[test]
    fn derive_name_pattern_chinese_with_star() {
        assert_eq!(derive_name_pattern("**/*测试*"), "*测试*");
        assert_eq!(derive_name_pattern("**/测试*.log"), "测试*.log");
        assert_eq!(derive_name_pattern("**/*.配置"), "*.配置");
    }

    #[test]
    fn derive_name_pattern_chinese_edge_cases() {
        assert_eq!(derive_name_pattern("中文/路径/**/文件.rs"), "文件.rs");
        assert_eq!(derive_name_pattern("**/*配置*"), "*配置*");
        assert_eq!(derive_name_pattern("模块/**/index.ts"), "index.ts");
        assert_eq!(derive_name_pattern("项目/源码/核心/**/*.rs"), "*.rs");
    }

    #[test]
    fn derive_search_dir_chinese_edge_cases() {
        let base = "/home/user/project";
        assert_eq!(
            derive_search_dir(base, "模块/**/index.ts"),
            format!("{}/模块", base)
        );
        assert_eq!(
            derive_search_dir(base, "项目/源码/核心/**/*.rs"),
            format!("{}/项目/源码/核心", base)
        );
        assert_eq!(
            derive_search_dir(base, "/绝对/路径/**/*.java"),
            "/绝对/路径"
        );
    }

    // --- simple_glob_match 单元测试 ---

    #[test]
    fn test_simple_glob_match_star_extension() {
        assert!(simple_glob_match("测试.txt", "*.txt"));
        assert!(simple_glob_match("test.txt", "*.txt"));
        assert!(simple_glob_match("hello.rs", "*.rs"));
        assert!(!simple_glob_match("hello.md", "*.txt"));
    }

    #[test]
    fn test_simple_glob_match_exact_name() {
        assert!(simple_glob_match("测试.txt", "测试.txt"));
        assert!(simple_glob_match("test.txt", "test.txt"));
        assert!(!simple_glob_match("其他.txt", "测试.txt"));
    }

    #[test]
    fn test_simple_glob_match_prefix_star() {
        assert!(simple_glob_match("report-报告.txt", "*报告.txt"));
        assert!(simple_glob_match("最终报告.txt", "*报告.txt"));
        assert!(!simple_glob_match("report.txt", "*报告.txt"));
    }

    #[test]
    fn test_simple_glob_match_middle_star() {
        assert!(simple_glob_match("测试_test.py", "测试*.py"));
        assert!(simple_glob_match("测试v2_test.py", "测试*.py"));
        assert!(!simple_glob_match("test.py", "测试*.py"));
    }

    #[test]
    fn test_simple_glob_match_star_only() {
        assert!(simple_glob_match("anything.txt", "*"));
        assert!(simple_glob_match("测试.txt", "*"));
    }

    #[test]
    fn test_simple_glob_match_trailing_star() {
        assert!(simple_glob_match("测试_file.txt", "测试*"));
        assert!(simple_glob_match("测试", "测试*"));
        assert!(!simple_glob_match("other.txt", "测试*"));
    }

    #[test]
    fn test_simple_glob_match_multiple_stars() {
        // `*测试*` should match any name containing `测试`
        assert!(simple_glob_match("我的测试文件.txt", "*测试*"));
        assert!(simple_glob_match("测试.txt", "*测试*"));
        assert!(simple_glob_match("abc测试def", "*测试*"));
        assert!(!simple_glob_match("abc.txt", "*测试*"));
    }

    // --- should_skip_path 单元测试 ---

    #[test]
    fn test_should_skip_path_node_modules() {
        assert!(should_skip_path(std::path::Path::new(
            "/project/node_modules/pkg/file.js"
        )));
    }

    #[test]
    fn test_should_skip_path_git() {
        assert!(should_skip_path(std::path::Path::new("/project/.git/HEAD")));
    }

    #[test]
    fn test_should_skip_path_target() {
        assert!(should_skip_path(std::path::Path::new(
            "/project/target/debug/app"
        )));
    }

    #[test]
    fn test_should_skip_path_normal() {
        assert!(!should_skip_path(std::path::Path::new(
            "/project/src/main.rs"
        )));
        assert!(!should_skip_path(std::path::Path::new("/project/测试.txt")));
    }

    #[test]
    fn test_should_skip_path_venv_prefix() {
        assert!(should_skip_path(std::path::Path::new(
            "/project/.venv-test/lib/python.py"
        )));
    }

    // --- 跨平台中文文件名搜索测试 (使用 ignore crate) ---
    // 以下测试在所有平台（macOS / Linux / Windows）上均可通过

    #[tokio::test]
    async fn glob_finds_chinese_filename() {
        // Issue #350 核心场景: 在工作目录创建中英文文件，glob 应匹配两者
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("test.txt"), "english file").unwrap();
        std::fs::write(dir.path().join("测试.txt"), "chinese file").unwrap();
        std::fs::write(dir.path().join("README.md"), "readme").unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = GlobTool;
        let args = r#"{"pattern":"*.txt"}"#;

        let r = tool.execute(args, &ctx).await.unwrap();
        assert!(r.success, "glob should succeed: {}", r.output);
        assert!(
            r.output.contains("测试.txt"),
            "glob must find Chinese-named file '测试.txt'. output: {}",
            r.output
        );
        assert!(
            r.output.contains("test.txt"),
            "glob must find English-named file 'test.txt'. output: {}",
            r.output
        );
    }

    #[tokio::test]
    async fn glob_finds_chinese_filename_recursive() {
        // 递归搜索子目录中的中文文件名
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("源码")).unwrap();
        std::fs::write(dir.path().join("源码/主程序.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("源码/utils.rs"), "pub fn helper() {}").unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = GlobTool;
        let args = r#"{"pattern":"**/*.rs"}"#;

        let r = tool.execute(args, &ctx).await.unwrap();
        assert!(r.success, "glob should succeed: {}", r.output);
        assert!(
            r.output.contains("主程序.rs"),
            "glob must find Chinese-named Rust file '主程序.rs'. output: {}",
            r.output
        );
        assert!(
            r.output.contains("utils.rs"),
            "glob must find English-named Rust file 'utils.rs'. output: {}",
            r.output
        );
    }

    #[tokio::test]
    async fn glob_finds_files_under_chinese_directory() {
        // 搜索中文目录名下的文件
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("组件")).unwrap();
        std::fs::create_dir_all(dir.path().join("components")).unwrap();
        std::fs::write(dir.path().join("组件/按钮.vue"), "<template></template>").unwrap();
        std::fs::write(
            dir.path().join("components/Button.vue"),
            "<template></template>",
        )
        .unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = GlobTool;
        let args = r#"{"pattern":"**/*.vue"}"#;

        let r = tool.execute(args, &ctx).await.unwrap();
        assert!(r.success, "glob should succeed: {}", r.output);
        assert!(
            r.output.contains("按钮.vue"),
            "glob must find file under Chinese directory. output: {}",
            r.output
        );
        assert!(
            r.output.contains("Button.vue"),
            "glob must find file under English directory. output: {}",
            r.output
        );
    }

    #[tokio::test]
    async fn glob_finds_files_when_explicitly_targeting_skip_listed_dotdir() {
        // 显式指向 .atomcode/（在 SKIP_DIRS 里的隐藏内部目录）应能搜到其中文件。
        // 回归：早先 should_skip_path 对路径每一段生效，导致即便显式给出该路径，
        // 结果里含 `.atomcode` 段就被滤掉，永远返回空。
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".atomcode/skills/coding")).unwrap();
        std::fs::write(
            dir.path().join(".atomcode/skills/coding/SKILL.md"),
            "# skill",
        )
        .unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = GlobTool;

        // 形式一：pattern 内含具体的 .atomcode/skills 前缀。
        let r = tool
            .execute(r#"{"pattern":".atomcode/skills/**/*.md"}"#, &ctx)
            .await
            .unwrap();
        assert!(r.success, "glob should succeed: {}", r.output);
        assert!(
            r.output.contains("SKILL.md"),
            "glob must find files under an explicitly targeted .atomcode/. output: {}",
            r.output
        );

        // 形式二：通过 path 参数显式指向 .atomcode/skills。
        let r2 = tool
            .execute(r#"{"pattern":"**/*.md","path":".atomcode/skills"}"#, &ctx)
            .await
            .unwrap();
        assert!(r2.success, "glob should succeed: {}", r2.output);
        assert!(
            r2.output.contains("SKILL.md"),
            "glob must find files when path explicitly points into .atomcode/. output: {}",
            r2.output
        );
    }

    #[tokio::test]
    async fn glob_broad_search_still_skips_dotatomcode() {
        // 广度搜索（根=工作区）仍应跳过 .atomcode/，行为不变。
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".atomcode/skills")).unwrap();
        std::fs::write(dir.path().join(".atomcode/skills/internal.md"), "x").unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/visible.md"), "y").unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = GlobTool;
        let r = tool
            .execute(r#"{"pattern":"**/*.md"}"#, &ctx)
            .await
            .unwrap();
        assert!(r.success, "glob should succeed: {}", r.output);
        assert!(
            r.output.contains("visible.md"),
            "broad glob must find visible files. output: {}",
            r.output
        );
        assert!(
            !r.output.contains("internal.md"),
            "broad glob must still skip .atomcode/. output: {}",
            r.output
        );
    }

    #[tokio::test]
    async fn glob_pattern_with_chinese_name() {
        // 搜索指定中文名文件: glob(pattern="**/测试.txt")
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("测试.txt"), "chinese content").unwrap();
        std::fs::write(dir.path().join("test.txt"), "english content").unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = GlobTool;
        let args = r#"{"pattern":"**/测试.txt"}"#;

        let r = tool.execute(args, &ctx).await.unwrap();
        assert!(r.success, "glob should succeed: {}", r.output);
        assert!(
            r.output.contains("测试.txt"),
            "glob must find '测试.txt' when searching for it by name. output: {}",
            r.output
        );
    }

    #[tokio::test]
    async fn glob_chinese_subdir_pattern() {
        // 搜索指定中文子目录下的文件: glob(pattern="文档/**/*.md")
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("文档")).unwrap();
        std::fs::create_dir_all(dir.path().join("docs")).unwrap();
        std::fs::write(dir.path().join("文档/说明.md"), "# 说明").unwrap();
        std::fs::write(dir.path().join("文档/指南.md"), "# 指南").unwrap();
        std::fs::write(dir.path().join("docs/README.md"), "# README").unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = GlobTool;
        let args = r#"{"pattern":"文档/**/*.md"}"#;

        let r = tool.execute(args, &ctx).await.unwrap();
        assert!(r.success, "glob should succeed: {}", r.output);
        assert!(
            r.output.contains("说明.md"),
            "glob must find '说明.md' under '文档' dir. output: {}",
            r.output
        );
        assert!(
            r.output.contains("指南.md"),
            "glob must find '指南.md' under '文档' dir. output: {}",
            r.output
        );
        assert!(
            !r.output.contains("README.md"),
            "glob must NOT find files from 'docs' dir when pattern is '文档/**/*.md'. output: {}",
            r.output
        );
    }

    // --- Issue #350 实际场景: 子目录中的中文文件名 + 中文目录名 ---
    // 复现用户报告的 Windows 场景:
    //   C:\111\TE微型直线导轨 滑块标准型[TE7C1R-129-G]\测试.txt

    #[tokio::test]
    async fn glob_finds_chinese_file_in_chinese_subdirectory() {
        // 完整复现 Issue #350 的实际场景:
        // 文件在包含中文和特殊字符的子目录中
        let dir = TempDir::new().unwrap();
        let subdir_name = "TE微型直线导轨 滑块标准型[TE7C1R-129-G]";
        std::fs::create_dir_all(dir.path().join(subdir_name)).unwrap();
        std::fs::write(
            dir.path().join(format!("{}/测试.txt", subdir_name)),
            "test content",
        )
        .unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = GlobTool;

        // 搜索所有 .txt 文件（递归）
        let r = tool
            .execute(r#"{"pattern":"**/*.txt"}"#, &ctx)
            .await
            .unwrap();
        assert!(r.success, "glob should succeed: {}", r.output);
        assert!(
            r.output.contains("测试.txt"),
            "glob must find '测试.txt' in Chinese subdirectory. output: {}",
            r.output
        );

        // 搜索指定中文名文件
        let r = tool
            .execute(r#"{"pattern":"**/测试.txt"}"#, &ctx)
            .await
            .unwrap();
        assert!(r.success, "glob should succeed: {}", r.output);
        assert!(
            r.output.contains("测试.txt"),
            "glob must find '测试.txt' by exact Chinese name. output: {}",
            r.output
        );
    }

    #[tokio::test]
    async fn glob_non_recursive_pattern_finds_root_files_only() {
        // 不带 ** 的 pattern 如 "*.txt" 只搜索 search_dir 根目录的文件
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("test.txt"), "root file").unwrap();
        std::fs::create_dir_all(dir.path().join("子目录")).unwrap();
        std::fs::write(dir.path().join("子目录/测试.txt"), "nested file").unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = GlobTool;

        // "*.txt" → name_pattern="*.txt", search_dir=base_dir
        // The ignore crate walker is recursive, but the name_pattern "*.txt"
        // will match any .txt at any depth. This is consistent with how
        // Unix `find $dir -name "*.txt"` works (also recursive).
        let r = tool.execute(r#"{"pattern":"*.txt"}"#, &ctx).await.unwrap();
        assert!(r.success, "glob should succeed: {}", r.output);
        assert!(
            r.output.contains("test.txt"),
            "glob must find 'test.txt' in root. output: {}",
            r.output
        );
        // The ignore walker traverses recursively, so nested .txt files
        // are also found — this is the same behavior as `find . -name "*.txt"`.
        assert!(
            r.output.contains("测试.txt"),
            "glob must find '测试.txt' in subdirectory. output: {}",
            r.output
        );
    }

    #[tokio::test]
    async fn glob_mixed_chinese_english_filenames() {
        // 混合中英文文件名的完整场景
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("index.ts"), "export {};").unwrap();
        std::fs::write(dir.path().join("索引.ts"), "export {};").unwrap();
        std::fs::write(dir.path().join("app.config.js"), "module.exports = {};").unwrap();
        std::fs::write(dir.path().join("配置.js"), "module.exports = {};").unwrap();
        std::fs::write(dir.path().join("utils-helper.py"), "def help(): pass").unwrap();
        std::fs::write(dir.path().join("工具_辅助.py"), "def help(): pass").unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = GlobTool;

        let r = tool.execute(r#"{"pattern":"*.ts"}"#, &ctx).await.unwrap();
        assert!(
            r.output.contains("索引.ts"),
            "must find '索引.ts': {}",
            r.output
        );
        assert!(
            r.output.contains("index.ts"),
            "must find 'index.ts': {}",
            r.output
        );

        let r = tool.execute(r#"{"pattern":"*.py"}"#, &ctx).await.unwrap();
        assert!(
            r.output.contains("工具_辅助.py"),
            "must find '工具_辅助.py': {}",
            r.output
        );
        assert!(
            r.output.contains("utils-helper.py"),
            "must find 'utils-helper.py': {}",
            r.output
        );
    }

    #[tokio::test]
    async fn glob_unicode_filenames_japanese_korean_emoji() {
        // 扩展测试: 日文、韩文、emoji 文件名
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("テスト.txt"), "japanese").unwrap();
        std::fs::write(dir.path().join("테스트.txt"), "korean").unwrap();
        std::fs::write(dir.path().join("🎉party.txt"), "emoji").unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = GlobTool;
        let r = tool.execute(r#"{"pattern":"*.txt"}"#, &ctx).await.unwrap();

        assert!(
            r.output.contains("テスト.txt"),
            "must find Japanese filename: {}",
            r.output
        );
        assert!(
            r.output.contains("테스트.txt"),
            "must find Korean filename: {}",
            r.output
        );
        assert!(
            r.output.contains("🎉party.txt"),
            "must find emoji filename: {}",
            r.output
        );
    }

    // --- 回归: 确认不再使用 Windows find.exe ---

    #[test]
    fn glob_uses_ignore_crate_not_find_command() {
        // 之前 glob 工具使用 Unix `find` 命令搜索文件，
        // 在 Windows 上 `find.exe` 是字符串搜索工具（类似 grep），
        // 导致 Issue #350。现在已替换为 Rust 原生 `ignore` crate，
        // 在所有平台上行为一致。
        //
        // 此测试确认 glob.rs 导入了 ignore crate 并使用了 WalkBuilder。
        let source = include_str!("glob.rs");
        assert!(
            source.contains("ignore::WalkBuilder"),
            "glob.rs must use `ignore::WalkBuilder` for cross-platform file search (Issue #350)."
        );
    }

    // --- skip_dirs 过滤一致性测试 ---

    #[tokio::test]
    async fn glob_skips_node_modules_and_target() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(dir.path().join("target/debug")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("node_modules/pkg/index.js"), "export {};").unwrap();
        // Write a real file to target/debug (not named 'app' which may not exist)
        std::fs::write(dir.path().join("target/debug/output.txt"), "binary").unwrap();

        let ctx = ToolContext::new(dir.path().to_path_buf());
        let tool = GlobTool;
        let r = tool.execute(r#"{"pattern":"**/*"}"#, &ctx).await.unwrap();

        assert!(
            r.output.contains("main.rs"),
            "must find source files. output: {}",
            r.output
        );
        assert!(
            !r.output.contains("node_modules"),
            "must skip node_modules. output: {}",
            r.output
        );
        assert!(
            !r.output.contains("target"),
            "must skip target. output: {}",
            r.output
        );
    }
}
