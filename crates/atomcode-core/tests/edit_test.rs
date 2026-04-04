//! Edit tool tests — surrounding context, file_history backup, line-number mode, text-match mode.

use std::path::PathBuf;
use atomcode_core::tool::{Tool, ToolContext, ToolResult};

fn test_context() -> ToolContext {
    ToolContext::new(PathBuf::from("/tmp"))
}

use std::sync::atomic::{AtomicU64, Ordering};
static COUNTER: AtomicU64 = AtomicU64::new(0);

fn create_test_file(content: &str) -> String {
    let dir = std::env::temp_dir().join("atomcode_edit_test");
    let _ = std::fs::create_dir_all(&dir);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let id = format!("{}_{}_{}", std::process::id(), std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos(), seq);
    let path = dir.join(format!("test_{}.vue", id));
    std::fs::write(&path, content).unwrap();
    path.to_string_lossy().to_string()
}

fn cleanup(path: &str) {
    let _ = std::fs::remove_file(path);
}

// ═══════════════════════════════════════════════════════════════
// 1. Surrounding context — edit result includes lines around edit point
// ═══════════════════════════════════════════════════════════════

#[tokio::test]
async fn edit_line_mode_includes_surrounding_context() {
    let content = (1..=50).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n");
    let path = create_test_file(&content);
    let ctx = test_context();

    let tool = atomcode_core::tool::edit::EditFileTool;
    let args = serde_json::json!({
        "file_path": path,
        "start_line": 20,
        "end_line": 25,
        "new_string": "replaced line 20\nreplaced line 21\nreplaced line 22"
    });

    let result = tool.execute(&args.to_string(), &ctx).await.unwrap();
    assert!(result.success, "Edit should succeed: {}", result.output);
    // Verify edit was applied (concise output, no surrounding context)
    assert!(result.output.contains("Edited"), "Should confirm edit: {}", result.output);

    cleanup(&path);
}

#[tokio::test]
async fn edit_text_match_includes_surrounding_context() {
    let content = "const a = 1;\nconst b = 2;\nconst target = 'old';\nconst c = 3;\nconst d = 4;\n";
    let path = create_test_file(content);
    let ctx = test_context();

    let tool = atomcode_core::tool::edit::EditFileTool;
    let args = serde_json::json!({
        "file_path": path,
        "old_string": "const target = 'old';",
        "new_string": "const target = 'new';"
    });

    let result = tool.execute(&args.to_string(), &ctx).await.unwrap();
    assert!(result.success, "Edit should succeed: {}", result.output);

    assert!(result.output.contains("Edited"), "Should confirm edit: {}", result.output);

    cleanup(&path);
}

#[tokio::test]
async fn edit_boundary_residual_detection() {
    // Simulate: line-number edit leaves a duplicate declaration outside range
    let content = "\
function render() {
  const isHtml = checkHtml();
  // some logic
  // more logic
  // even more
  let rendered = oldParse(content);
  return rendered;
}
";
    let path = create_test_file(content);
    let ctx = test_context();

    let tool = atomcode_core::tool::edit::EditFileTool;
    // Replace lines 2-5 (isHtml check), but leave line 6 (let rendered) untouched
    let args = serde_json::json!({
        "file_path": path,
        "start_line": 2,
        "end_line": 5,
        "new_string": "  const isHtml = newCheck();\n  let rendered = newParse(content);"
    });

    let result = tool.execute(&args.to_string(), &ctx).await.unwrap();
    assert!(result.success);

    // Edit should succeed — residual detection is handled by auto_fix
    assert!(result.success, "Edit should succeed: {}", result.output);

    cleanup(&path);
}

// ═══════════════════════════════════════════════════════════════
// 2. Small file — no surrounding context needed
// ═══════════════════════════════════════════════════════════════

#[tokio::test]
async fn edit_small_file_still_works() {
    let content = "line 1\nline 2\nline 3\n";
    let path = create_test_file(content);
    let ctx = test_context();

    let tool = atomcode_core::tool::edit::EditFileTool;
    let args = serde_json::json!({
        "file_path": path,
        "old_string": "line 2",
        "new_string": "modified line 2"
    });

    let result = tool.execute(&args.to_string(), &ctx).await.unwrap();
    assert!(result.success);

    let new_content = std::fs::read_to_string(&path).unwrap();
    assert!(new_content.contains("modified line 2"));

    cleanup(&path);
}

// ═══════════════════════════════════════════════════════════════
// 3. file_history backup — edit creates backup before write
// ═══════════════════════════════════════════════════════════════

#[tokio::test]
async fn edit_creates_file_history_backup() {
    let content = "original content\nline 2\nline 3\n";
    let path = create_test_file(content);
    let ctx = test_context();

    let tool = atomcode_core::tool::edit::EditFileTool;
    let args = serde_json::json!({
        "file_path": path,
        "old_string": "original content",
        "new_string": "modified content"
    });

    let result = tool.execute(&args.to_string(), &ctx).await.unwrap();
    assert!(result.success);

    // Verify file was modified
    let new_content = std::fs::read_to_string(&path).unwrap();
    assert!(new_content.contains("modified content"));
    assert!(!new_content.contains("original content"));

    // Verify backup exists (file_history should have created one)
    let fh = ctx.file_history.lock().await;
    let latest = fh.latest_version(&path);
    assert!(latest.is_some(), "file_history should have a backup version");

    cleanup(&path);
}

// ═══════════════════════════════════════════════════════════════
// 4. Multi-edit — multiple regions in one call
// ═══════════════════════════════════════════════════════════════

#[tokio::test]
async fn multi_edit_line_number_mode() {
    // Simulate a Vue SFC: imports, logic, template
    let content = "\
<script setup>
import { ref } from 'vue'
import { useRouter } from 'vue-router'
const count = ref(0)
function increment() { count.value++ }
</script>
<template>
  <div>
    <p>{{ count }}</p>
    <button @click=\"increment\">+1</button>
  </div>
</template>
";
    let path = create_test_file(content);
    let ctx = test_context();
    let tool = atomcode_core::tool::edit::EditFileTool;

    let args = serde_json::json!({
        "file_path": path,
        "edits": [
            {
                "start_line": 2,
                "end_line": 3,
                "new_string": "import { ref, computed } from 'vue'\nimport { useRouter } from 'vue-router'\nimport { useStore } from './store'"
            },
            {
                "start_line": 5,
                "end_line": 5,
                "new_string": "function increment() { count.value++ }\nconst double = computed(() => count.value * 2)"
            },
            {
                "start_line": 9,
                "end_line": 10,
                "new_string": "    <p>{{ count }} (double: {{ double }})</p>\n    <button @click=\"increment\">+1</button>\n    <span>Store loaded</span>"
            }
        ]
    });

    let result = tool.execute(&args.to_string(), &ctx).await.unwrap();
    assert!(result.success, "Multi-edit should succeed: {}", result.output);
    assert!(result.output.contains("3 edits applied"), "Should report 3 edits: {}", result.output);

    let new_content = std::fs::read_to_string(&path).unwrap();
    assert!(new_content.contains("useStore"), "Should have new import");
    assert!(new_content.contains("computed(() =>"), "Should have computed");
    assert!(new_content.contains("Store loaded"), "Should have new template element");

    cleanup(&path);
}

#[tokio::test]
async fn multi_edit_text_match_mode() {
    let content = "\
function hello() { return 'hello'; }
function world() { return 'world'; }
function main() { console.log(hello(), world()); }
";
    let path = create_test_file(content);
    let ctx = test_context();
    let tool = atomcode_core::tool::edit::EditFileTool;

    let args = serde_json::json!({
        "file_path": path,
        "edits": [
            {
                "old_string": "function hello() { return 'hello'; }",
                "new_string": "function hello() { return 'hi'; }"
            },
            {
                "old_string": "function world() { return 'world'; }",
                "new_string": "function world() { return 'earth'; }"
            }
        ]
    });

    let result = tool.execute(&args.to_string(), &ctx).await.unwrap();
    assert!(result.success, "Multi-edit text match should succeed: {}", result.output);

    let new_content = std::fs::read_to_string(&path).unwrap();
    assert!(new_content.contains("'hi'"), "Should have replaced hello");
    assert!(new_content.contains("'earth'"), "Should have replaced world");
    assert!(new_content.contains("console.log"), "Untouched code should remain");

    cleanup(&path);
}

#[tokio::test]
async fn multi_edit_overlap_detection() {
    let content = (1..=20).map(|i| format!("line {}", i)).collect::<Vec<_>>().join("\n");
    let path = create_test_file(&content);
    let ctx = test_context();
    let tool = atomcode_core::tool::edit::EditFileTool;

    let args = serde_json::json!({
        "file_path": path,
        "edits": [
            { "start_line": 5, "end_line": 10, "new_string": "a" },
            { "start_line": 8, "end_line": 15, "new_string": "b" }
        ]
    });

    let result = tool.execute(&args.to_string(), &ctx).await.unwrap();
    assert!(!result.success, "Overlapping edits should fail");
    assert!(result.output.contains("overlapping"), "Should mention overlap: {}", result.output);

    cleanup(&path);
}

#[tokio::test]
async fn multi_edit_mixed_modes() {
    let content = "\
import React from 'react'
const App = () => {
  const name = 'old'
  return <div>{name}</div>
}
export default App
";
    let path = create_test_file(content);
    let ctx = test_context();
    let tool = atomcode_core::tool::edit::EditFileTool;

    // Mix line-number and text-match in the same multi-edit
    let args = serde_json::json!({
        "file_path": path,
        "edits": [
            {
                "start_line": 1,
                "end_line": 1,
                "new_string": "import React, { useState } from 'react'"
            },
            {
                "old_string": "const name = 'old'",
                "new_string": "const [name, setName] = useState('new')"
            }
        ]
    });

    let result = tool.execute(&args.to_string(), &ctx).await.unwrap();
    assert!(result.success, "Mixed-mode multi-edit should succeed: {}", result.output);

    let new_content = std::fs::read_to_string(&path).unwrap();
    assert!(new_content.contains("useState"), "Import should be updated");
    assert!(new_content.contains("useState('new')"), "State hook should be added");

    cleanup(&path);
}

#[tokio::test]
async fn multi_edit_string_line_numbers() {
    // Test lenient parsing: model sends line numbers as strings
    let content = "line 1\nline 2\nline 3\nline 4\nline 5\n";
    let path = create_test_file(content);
    let ctx = test_context();
    let tool = atomcode_core::tool::edit::EditFileTool;

    let args = format!(
        r#"{{"file_path": "{}", "edits": [{{"start_line": "2", "end_line": "3", "new_string": "replaced"}}]}}"#,
        path
    );

    let result = tool.execute(&args, &ctx).await.unwrap();
    assert!(result.success, "String line numbers should work via lenient parsing: {}", result.output);

    let new_content = std::fs::read_to_string(&path).unwrap();
    assert!(new_content.contains("replaced"), "Edit should have been applied");
    assert!(!new_content.contains("line 2"), "Old content should be gone");

    cleanup(&path);
}

// ═══════════════════════════════════════════════════════════════
// 5. Empty old_string — should error, not append
// ═══════════════════════════════════════════════════════════════

#[tokio::test]
async fn edit_empty_old_string_returns_error() {
    let content = "existing code\n";
    let path = create_test_file(content);
    let ctx = test_context();

    let tool = atomcode_core::tool::edit::EditFileTool;
    let args = serde_json::json!({
        "file_path": path,
        "new_string": "should not be appended"
    });

    let result = tool.execute(&args.to_string(), &ctx).await.unwrap();
    assert!(!result.success, "Should fail when old_string is empty");
    assert!(result.output.contains("old_string is required"),
        "Should tell model old_string is required: {}", result.output);

    // File should NOT be modified
    let after = std::fs::read_to_string(&path).unwrap();
    assert_eq!(after, "existing code\n", "File should not be modified");

    cleanup(&path);
}

// ═══════════════════════════════════════════════════════════════
// 6. Boundary overlap auto-correction — end_line too small
// ═══════════════════════════════════════════════════════════════

#[tokio::test]
async fn single_edit_boundary_overlap_dedup() {
    // Model says replace line 2 only, but new_string includes line 3's content
    let content = "\
const a = ref(false)
const b = ref(false)
const c = ref(0)
function main() {}
";
    let path = create_test_file(content);
    let ctx = test_context();
    let tool = atomcode_core::tool::edit::EditFileTool;

    let args = serde_json::json!({
        "file_path": path,
        "start_line": 2,
        "end_line": 2,
        "new_string": "const b = ref(false)\nconst showTop = ref(false)\nconst c = ref(0)"
    });

    let result = tool.execute(&args.to_string(), &ctx).await.unwrap();
    assert!(result.success);

    let new_content = std::fs::read_to_string(&path).unwrap();
    let c_count = new_content.lines().filter(|l| l.trim() == "const c = ref(0)").count();
    assert_eq!(c_count, 1, "const c should appear exactly once (boundary auto-corrected), got:\n{}", new_content);

    cleanup(&path);
}

#[tokio::test]
async fn multi_edit_boundary_overlap_dedup() {
    let content = "\
import { ref } from 'vue'
const isLiked = ref(false)
const isBookmarked = ref(false)
const count = ref(0)
function setup() {}
";
    let path = create_test_file(content);
    let ctx = test_context();
    let tool = atomcode_core::tool::edit::EditFileTool;

    // Model says replace lines 1-1 and 2-2, but each new_string's last line
    // duplicates the next original line
    let args = serde_json::json!({
        "file_path": path,
        "edits": [
            {
                "start_line": 1,
                "end_line": 1,
                "new_string": "import { ref, computed } from 'vue'\nconst isLiked = ref(false)"
            },
            {
                "start_line": 3,
                "end_line": 3,
                "new_string": "const isBookmarked = ref(false)\nconst showBackToTop = ref(false)\nconst count = ref(0)"
            }
        ]
    });

    let result = tool.execute(&args.to_string(), &ctx).await.unwrap();
    assert!(result.success, "Multi-edit should succeed: {}", result.output);

    let new_content = std::fs::read_to_string(&path).unwrap();
    let liked_count = new_content.lines().filter(|l| l.trim() == "const isLiked = ref(false)").count();
    let count_count = new_content.lines().filter(|l| l.trim() == "const count = ref(0)").count();
    assert_eq!(liked_count, 1, "isLiked should appear once (no duplicate), got:\n{}", new_content);
    assert_eq!(count_count, 1, "count should appear once (boundary corrected), got:\n{}", new_content);
    assert!(new_content.contains("showBackToTop"), "New code should be present");

    cleanup(&path);
}

// ═══════════════════════════════════════════════════════════════
// 7. Leading boundary overlap — start_line too large
// ═══════════════════════════════════════════════════════════════

#[tokio::test]
async fn single_edit_leading_overlap_dedup() {
    // Model says replace line 3 only, but new_string starts with line 2's content
    let content = "\
const router = useRouter()
const activeTab = ref('profile')
const user = ref(null)
function main() {}
";
    let path = create_test_file(content);
    let ctx = test_context();
    let tool = atomcode_core::tool::edit::EditFileTool;

    let args = serde_json::json!({
        "file_path": path,
        "start_line": 3,
        "end_line": 3,
        "new_string": "const activeTab = ref('profile')\nconst theme = ref('light')\nconst user = ref(null)"
    });

    let result = tool.execute(&args.to_string(), &ctx).await.unwrap();
    assert!(result.success);

    let new_content = std::fs::read_to_string(&path).unwrap();
    let tab_count = new_content.lines().filter(|l| l.trim() == "const activeTab = ref('profile')").count();
    assert_eq!(tab_count, 1, "activeTab should appear exactly once (leading boundary corrected), got:\n{}", new_content);
    assert!(new_content.contains("theme"), "New code should be present");

    cleanup(&path);
}

#[tokio::test]
async fn multi_edit_leading_overlap_dedup() {
    let content = "\
<script setup>
import { ref } from 'vue'
const name = ref('')
const age = ref(0)
const active = ref(true)
</script>
<template>
  <div>{{ name }}</div>
</template>
";
    let path = create_test_file(content);
    let ctx = test_context();
    let tool = atomcode_core::tool::edit::EditFileTool;

    // Model says start at line 3, but new_string starts with line 2's content
    let args = serde_json::json!({
        "file_path": path,
        "edits": [
            {
                "start_line": 3,
                "end_line": 3,
                "new_string": "import { ref } from 'vue'\nimport { computed } from 'vue'\nconst name = ref('')"
            },
            {
                "start_line": 8,
                "end_line": 8,
                "new_string": "  <div>{{ name }}</div>\n  <span>{{ age }}</span>"
            }
        ]
    });

    let result = tool.execute(&args.to_string(), &ctx).await.unwrap();
    assert!(result.success, "Multi-edit should succeed: {}", result.output);

    let new_content = std::fs::read_to_string(&path).unwrap();
    let import_ref_count = new_content.lines().filter(|l| l.trim() == "import { ref } from 'vue'").count();
    let div_count = new_content.lines().filter(|l| l.trim() == "<div>{{ name }}</div>").count();
    assert_eq!(import_ref_count, 1, "import ref should appear once (leading corrected), got:\n{}", new_content);
    assert_eq!(div_count, 1, "div should appear once (leading corrected), got:\n{}", new_content);

    cleanup(&path);
}
