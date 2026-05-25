use atomcode_core::tool::write::WriteFileTool;
use atomcode_core::tool::{Tool, ToolContext};
use serde_json::json;

fn make_ctx(dir: &std::path::Path) -> ToolContext {
    ToolContext::with_session(dir.to_path_buf(), "test")
}

#[tokio::test]
async fn test_write_new_file() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(dir.path());
    let tool = WriteFileTool;

    let file_path = dir.path().join("new.rs").to_string_lossy().to_string();
    let args = json!({
        "file_path": file_path,
        "content": "fn main() {\n    println!(\"hello\");\n}\n"
    })
    .to_string();

    let result = tool.execute(&args, &ctx).await.unwrap();
    assert!(result.success, "should succeed: {}", result.output);
    assert!(
        result.output.contains("Created new file"),
        "output: {}",
        result.output
    );
    assert!(std::path::Path::new(&file_path).exists());
}

#[tokio::test]
async fn test_write_overwrite_existing() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(dir.path());
    let tool = WriteFileTool;

    let file_path = dir.path().join("app.rs").to_string_lossy().to_string();

    // Create original file
    std::fs::write(&file_path, "fn old() {}\nfn old2() {}\nfn old3() {}\n").unwrap();

    // Overwrite with new content
    let args = json!({
        "file_path": file_path,
        "content": "fn new_main() {\n    println!(\"rewritten\");\n}\n"
    })
    .to_string();

    let result = tool.execute(&args, &ctx).await.unwrap();
    assert!(result.success, "should succeed: {}", result.output);
    assert!(
        result.output.contains("Overwrote"),
        "should say Overwrote: {}",
        result.output
    );
    assert!(
        result.output.contains("was 3 lines"),
        "should mention old line count: {}",
        result.output
    );

    // Verify content is new
    let content = std::fs::read_to_string(&file_path).unwrap();
    assert!(
        content.contains("new_main"),
        "file should contain new content"
    );
    assert!(
        !content.contains("old"),
        "file should not contain old content"
    );
}

#[tokio::test]
async fn test_write_overwrite_warns_on_shrink() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(dir.path());
    let tool = WriteFileTool;

    let file_path = dir.path().join("big.rs").to_string_lossy().to_string();

    // Create a 30-line file
    let old_content = (0..30)
        .map(|i| format!("fn func_{}() {{}}", i))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(&file_path, &old_content).unwrap();

    // Overwrite with 5-line file (significant shrink)
    let args = json!({
        "file_path": file_path,
        "content": "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\nfn e() {}\n"
    })
    .to_string();

    let result = tool.execute(&args, &ctx).await.unwrap();
    assert!(result.success);
    assert!(
        result.output.contains("WARNING"),
        "should warn on significant shrink: {}",
        result.output
    );
    assert!(
        result.output.contains("shrank"),
        "should mention shrank: {}",
        result.output
    );
}

#[tokio::test]
async fn test_write_overwrite_no_warn_on_growth() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(dir.path());
    let tool = WriteFileTool;

    let file_path = dir.path().join("small.rs").to_string_lossy().to_string();

    // Create a 5-line file
    std::fs::write(
        &file_path,
        "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\nfn e() {}\n",
    )
    .unwrap();

    // Overwrite with 30-line file (growth, not shrink)
    let new_content = (0..30)
        .map(|i| format!("fn func_{}() {{}}", i))
        .collect::<Vec<_>>()
        .join("\n");
    let args = json!({
        "file_path": file_path,
        "content": new_content
    })
    .to_string();

    let result = tool.execute(&args, &ctx).await.unwrap();
    assert!(result.success);
    assert!(
        !result.output.contains("WARNING"),
        "should NOT warn on growth: {}",
        result.output
    );
}

#[tokio::test]
async fn test_write_empty_args_returns_friendly_error() {
    // Reproduces the user-reported bug: provider emits `{}` on max_tokens cutoff,
    // and WriteFileTool used to propagate the raw serde error
    // ("missing field `file_path` at line 1 column 2") which told the model
    // nothing. Expected: success=false with actionable recovery hint.
    let dir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(dir.path());
    let tool = WriteFileTool;

    let result = tool.execute("{}", &ctx).await.unwrap();
    assert!(!result.success, "empty args should fail gracefully");
    assert!(
        result.output.contains("missing field"),
        "keep serde detail: {}",
        result.output
    );
    assert!(
        result.output.contains("truncated") || result.output.contains("max_tokens"),
        "should hint at root cause: {}",
        result.output,
    );
    assert!(
        result.output.contains("edit_file"),
        "should suggest edit_file: {}",
        result.output
    );
}

#[tokio::test]
async fn test_write_creates_parent_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = make_ctx(dir.path());
    let tool = WriteFileTool;

    let file_path = dir
        .path()
        .join("deep/nested/dir/file.rs")
        .to_string_lossy()
        .to_string();
    let args = json!({
        "file_path": file_path,
        "content": "fn nested() {}\n"
    })
    .to_string();

    let result = tool.execute(&args, &ctx).await.unwrap();
    assert!(
        result.success,
        "should create parent dirs: {}",
        result.output
    );
    assert!(std::path::Path::new(&file_path).exists());
}
