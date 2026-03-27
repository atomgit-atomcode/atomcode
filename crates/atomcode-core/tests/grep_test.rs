use std::path::PathBuf;
use atomcode_core::tool::{Tool, ToolContext};
use atomcode_core::tool::grep::GrepTool;

async fn run_grep(pattern: &str, path: &str, wd: &str) -> atomcode_core::tool::ToolResult {
    let tool = GrepTool;
    let ctx = ToolContext::new(PathBuf::from(wd));
    let args = serde_json::json!({
        "pattern": pattern,
        "path": path,
    }).to_string();
    tool.execute(&args, &ctx).await.unwrap()
}

async fn run_grep_no_path(pattern: &str, wd: &str) -> atomcode_core::tool::ToolResult {
    let tool = GrepTool;
    let ctx = ToolContext::new(PathBuf::from(wd));
    let args = serde_json::json!({
        "pattern": pattern,
    }).to_string();
    tool.execute(&args, &ctx).await.unwrap()
}

fn wd() -> String {
    env!("CARGO_MANIFEST_DIR").to_string()
}

#[tokio::test]
async fn test_simple_pattern() {
    let result = run_grep("fn ", "src/tool", &wd()).await;
    assert!(result.success, "should find 'fn ' in src/tool. Output: {}", result.output);
}

#[tokio::test]
async fn test_regex_or() {
    // First: verify rg works directly via Command
    let direct = tokio::process::Command::new("rg")
        .args(&["--line-number", "--no-heading", "--color=never",
                "GrepTool|BashTool", "src/tool"])
        .current_dir(&wd())
        .output().await.unwrap();
    let direct_out = String::from_utf8_lossy(&direct.stdout);
    eprintln!("DIRECT rg: {} lines, exit={:?}", direct_out.lines().count(), direct.status.code());
    assert!(!direct_out.is_empty(), "direct rg should find results");

    // Then: verify via grep tool
    let result = run_grep("GrepTool|BashTool", "src/tool", &wd()).await;
    assert!(result.success, "grep tool should also find results. Output: {}", result.output);
}

#[tokio::test]
async fn test_relative_path() {
    let result = run_grep("fn ", "src", &wd()).await;
    assert!(result.success, "relative path should work. Output: {}", result.output);
}

#[tokio::test]
async fn test_absolute_path() {
    let abs = format!("{}/src/tool", env!("CARGO_MANIFEST_DIR"));
    let result = run_grep("fn ", &abs, &wd()).await;
    assert!(result.success, "absolute path should work. Output: {}", result.output);
}

#[tokio::test]
async fn test_default_path() {
    let result = run_grep_no_path("GrepTool", &wd()).await;
    assert!(result.success, "default path (.) should work. Output: {}", result.output);
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
    let has_target = result.output.lines().any(|l| l.contains("/target/"));
    assert!(!has_target, "target/ should be excluded. Output: {}", result.output);
}

#[tokio::test]
async fn test_nonexistent_path() {
    let result = run_grep("fn ", "nonexistent/path", &wd()).await;
    assert!(!result.success);
}

#[tokio::test]
async fn test_mixed_cjk_ascii_or() {
    let result = run_grep("搜索|search|keyword", "src", &wd()).await;
    assert!(result.success, "CJK|ASCII OR should find 'search'. Output: {}", result.output);
}
