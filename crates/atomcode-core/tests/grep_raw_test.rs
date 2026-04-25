/// Raw rg command test — bypasses grep tool, directly calls rg
/// to isolate whether the issue is in rg invocation or grep tool logic.
///
/// These tests depend on an external repo (devpress2.0) and are slow.
/// Run explicitly with: cargo test -p atomcode-core --test grep_raw_test -- --ignored

#[tokio::test]
#[ignore = "depends on external repo /Users/yubangxu/project/devpress2.0"]
async fn test_raw_rg_from_rust() {
    let wd = "/Users/yubangxu/project/devpress2.0";
    let search_path = format!("{}/backend/src", wd);

    let output = tokio::process::Command::new("rg")
        .args(&[
            "--line-number",
            "--no-heading",
            "--color=never",
            "--smart-case",
            "--glob",
            "!**/datalog/**",
            "--glob",
            "!**/*.log",
            "--glob",
            "!**/target/**",
            "--glob",
            "!**/dist/**",
            "--glob",
            "!**/node_modules/**",
            "--glob",
            "!**/.git/**",
            "--max-count=50",
            "--context=3",
            "jwt",
            &search_path,
        ])
        .current_dir(wd)
        .output()
        .await
        .expect("rg should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("STDOUT lines: {}", stdout.lines().count());
    eprintln!("STDERR: {}", stderr);
    eprintln!("EXIT: {:?}", output.status.code());
    eprintln!(
        "FIRST 3 LINES: {:?}",
        stdout.lines().take(3).collect::<Vec<_>>()
    );

    assert!(
        !stdout.is_empty(),
        "rg should find jwt in backend/src. stderr={}",
        stderr
    );
    assert!(
        stdout.contains("JwtService") || stdout.contains("jwt"),
        "should contain jwt matches"
    );
}

#[tokio::test]
#[ignore = "depends on external repo /Users/yubangxu/project/devpress2.0"]
async fn test_raw_rg_regex_or() {
    let wd = "/Users/yubangxu/project/devpress2.0";
    let search_path = format!("{}/backend/src", wd);

    let output = tokio::process::Command::new("rg")
        .args(&[
            "--line-number",
            "--no-heading",
            "--color=never",
            "--smart-case",
            "--max-count=5",
            "--context=0",
            "jwt|token",
            &search_path,
        ])
        .current_dir(wd)
        .output()
        .await
        .expect("rg should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    eprintln!("OR PATTERN stdout lines: {}", stdout.lines().count());
    eprintln!("OR PATTERN stderr: {}", stderr);

    assert!(
        !stdout.is_empty(),
        "rg with | OR should find results. stderr={}",
        stderr
    );
}

#[tokio::test]
#[ignore = "depends on external repo /Users/yubangxu/project/devpress2.0"]
async fn test_raw_rg_relative_path() {
    let wd = "/Users/yubangxu/project/devpress2.0";

    let output = tokio::process::Command::new("rg")
        .args(&[
            "--line-number",
            "--no-heading",
            "--color=never",
            "--smart-case",
            "--max-count=5",
            "jwt",
            "backend/src",
        ])
        .current_dir(wd)
        .output()
        .await
        .expect("rg should execute");

    let stdout = String::from_utf8_lossy(&output.stdout);
    eprintln!("RELATIVE PATH stdout lines: {}", stdout.lines().count());

    assert!(
        !stdout.is_empty(),
        "rg with relative path + current_dir should work"
    );
}

#[tokio::test]
#[ignore = "depends on external repo /Users/yubangxu/project/devpress2.0"]
async fn test_grep_tool_on_devpress() {
    use atomcode_core::tool::grep::GrepTool;
    use atomcode_core::tool::{Tool, ToolContext};
    use std::path::PathBuf;

    let wd = "/Users/yubangxu/project/devpress2.0";
    if !std::path::Path::new(wd).exists() {
        eprintln!("SKIP: devpress2.0 not found");
        return;
    }

    let tool = GrepTool;
    let ctx = ToolContext::new(PathBuf::from(wd));

    // Test 1: simple pattern, absolute path
    let args = serde_json::json!({
        "pattern": "jwt",
        "path": format!("{}/backend/src", wd),
    })
    .to_string();
    let result = tool.execute(&args, &ctx).await.unwrap();
    eprintln!(
        "TEST1 (abs path): success={} output_lines={}",
        result.success,
        result.output.lines().count()
    );
    assert!(
        result.success,
        "grep jwt with absolute path failed. Output: {}",
        result.output
    );

    // Test 2: simple pattern, relative path
    let args = serde_json::json!({
        "pattern": "jwt",
        "path": "backend/src",
    })
    .to_string();
    let result = tool.execute(&args, &ctx).await.unwrap();
    eprintln!(
        "TEST2 (rel path): success={} output_lines={}",
        result.success,
        result.output.lines().count()
    );
    assert!(
        result.success,
        "grep jwt with relative path failed. Output: {}",
        result.output
    );

    // Test 3: regex OR pattern
    let args = serde_json::json!({
        "pattern": "jwt|token",
        "path": "backend/src",
    })
    .to_string();
    let result = tool.execute(&args, &ctx).await.unwrap();
    eprintln!(
        "TEST3 (regex OR): success={} output_lines={}",
        result.success,
        result.output.lines().count()
    );
    assert!(
        result.success,
        "grep jwt|token failed. Output: {}",
        result.output
    );

    // Test 4: search in frontend (the pattern that failed in datalogs)
    let args = serde_json::json!({
        "pattern": "search",
        "path": "frontend/src",
    })
    .to_string();
    let result = tool.execute(&args, &ctx).await.unwrap();
    eprintln!(
        "TEST4 (search in frontend): success={} output_lines={}",
        result.success,
        result.output.lines().count()
    );
    assert!(
        result.success,
        "grep 'search' in frontend/src failed. Output: {}",
        result.output
    );

    // Test 5: CJK + ASCII OR (the exact pattern from failing datalogs)
    let args = serde_json::json!({
        "pattern": "搜索|search|keyword",
        "path": "frontend/src",
    })
    .to_string();
    let result = tool.execute(&args, &ctx).await.unwrap();
    eprintln!(
        "TEST5 (CJK|ASCII): success={} output_lines={}",
        result.success,
        result.output.lines().count()
    );
    assert!(
        result.success,
        "grep '搜索|search|keyword' failed. Output: {}",
        result.output
    );
}
