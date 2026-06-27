use atomcode_core::tool::grep::GrepTool;
use atomcode_core::tool::{Tool, ToolContext};
use std::path::PathBuf;

async fn run_grep(pattern: &str, path: &str, wd: &str) -> atomcode_core::tool::ToolResult {
    let tool = GrepTool;
    let ctx = ToolContext::new(PathBuf::from(wd));
    let args = serde_json::json!({
        "pattern": pattern,
        "path": path,
    })
    .to_string();
    tool.execute(&args, &ctx).await.unwrap()
}

async fn run_grep_no_path(pattern: &str, wd: &str) -> atomcode_core::tool::ToolResult {
    let tool = GrepTool;
    let ctx = ToolContext::new(PathBuf::from(wd));
    let args = serde_json::json!({
        "pattern": pattern,
    })
    .to_string();
    tool.execute(&args, &ctx).await.unwrap()
}

fn wd() -> String {
    env!("CARGO_MANIFEST_DIR").to_string()
}

#[tokio::test]
async fn test_simple_pattern() {
    let result = run_grep("fn ", "src/tool", &wd()).await;
    assert!(
        result.success,
        "should find 'fn ' in src/tool. Output: {}",
        result.output
    );
}

#[tokio::test]
async fn test_regex_or() {
    // The grep tool uses a pure-Rust walker (`grep_walk`), not ripgrep, so we test
    // the tool directly. (An earlier `Command::new("rg")` sanity-check preamble was
    // removed: it depended on a real `rg` binary on PATH, which fails where `rg` is a
    // shell function / absent, even though the tool itself never shells out to it.)
    let result = run_grep("GrepTool|BashTool", "src/tool", &wd()).await;
    assert!(
        result.success,
        "grep tool should also find results. Output: {}",
        result.output
    );
}

#[tokio::test]
async fn test_relative_path() {
    let result = run_grep("fn ", "src", &wd()).await;
    assert!(
        result.success,
        "relative path should work. Output: {}",
        result.output
    );
}

#[tokio::test]
async fn test_absolute_path() {
    let abs = format!("{}/src/tool", env!("CARGO_MANIFEST_DIR"));
    let result = run_grep("fn ", &abs, &wd()).await;
    assert!(
        result.success,
        "absolute path should work. Output: {}",
        result.output
    );
}

#[tokio::test]
async fn test_default_path() {
    let result = run_grep_no_path("GrepTool", &wd()).await;
    assert!(
        result.success,
        "default path (.) should work. Output: {}",
        result.output
    );
}

#[tokio::test]
async fn test_no_crash_on_chinese() {
    let result = run_grep("不存在的中文内容xyz", "src", &wd()).await;
    // No results expected, should not panic
    let _ = result;
}

#[tokio::test]
async fn test_excludes_target() {
    let result = run_grep("GrepTool", ".", &wd()).await;
    assert!(result.success);
    // Check that no result *file path* comes from target/ — only inspect the
    // path prefix before the first `:`, not the matched source text which may
    // contain the literal string "target/" (e.g. this very test).
    let has_target = result.output.lines().any(|l| {
        let path = l.split(':').next().unwrap_or("");
        path.contains("/target/") || path.starts_with("target/")
    });
    assert!(
        !has_target,
        "target/ should be excluded. Output: {}",
        result.output
    );
}

#[tokio::test]
async fn test_nonexistent_path() {
    let result = run_grep("fn ", "nonexistent/path", &wd()).await;
    assert!(!result.success);
}

#[tokio::test]
async fn test_mixed_cjk_ascii_or() {
    let result = run_grep("搜索|search|keyword", "src", &wd()).await;
    assert!(
        result.success,
        "CJK|ASCII OR should find 'search'. Output: {}",
        result.output
    );
}

#[tokio::test]
async fn test_with_path_always_returns_code_lines() {
    // When `path` is specified, grep must return actual matching code lines,
    // never a Graph-only summary. Verify output contains `:` line-number format.
    let result = run_grep("GrepTool", "src/tool/grep.rs", &wd()).await;
    assert!(
        result.success,
        "should find GrepTool in grep.rs. Output: {}",
        result.output
    );
    assert!(
        !result.output.starts_with("[Graph:"),
        "with path specified, output must not be Graph-only. Output: {}",
        result.output
    );
    let has_line_ref = result.output.lines().any(|l| {
        // ripgrep output format: "file:line:content"
        let parts: Vec<&str> = l.splitn(3, ':').collect();
        parts.len() >= 3 && parts[1].parse::<usize>().is_ok()
    });
    assert!(
        has_line_ref,
        "output must contain file:line:content format. Output: {}",
        result.output
    );
}

#[tokio::test]
async fn test_no_path_still_returns_code_lines() {
    // Even without path, ripgrep results should always be present.
    let result = run_grep_no_path("GrepTool", &wd()).await;
    assert!(result.success);
    let has_line_ref = result.output.lines().any(|l| {
        let parts: Vec<&str> = l.splitn(3, ':').collect();
        parts.len() >= 3 && parts[1].parse::<usize>().is_ok()
    });
    assert!(
        has_line_ref,
        "output must contain ripgrep line results even when graph is active. Output: {}",
        result.output
    );
}
