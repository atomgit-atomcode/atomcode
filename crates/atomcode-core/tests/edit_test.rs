//! Edit tool tests — surrounding context, file_history backup, line-number mode, text-match mode.

use atomcode_core::tool::{Tool, ToolContext, ToolResult};
use std::path::PathBuf;

fn test_context() -> ToolContext {
    ToolContext::new(PathBuf::from("/tmp"))
}

use std::sync::atomic::{AtomicU64, Ordering};
static COUNTER: AtomicU64 = AtomicU64::new(0);

fn create_test_file(content: &str) -> String {
    let dir = std::env::temp_dir().join("atomcode_edit_test");
    let _ = std::fs::create_dir_all(&dir);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let id = format!(
        "{}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        seq
    );
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
    let content = (1..=50)
        .map(|i| format!("line {}", i))
        .collect::<Vec<_>>()
        .join("\n");
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
    assert!(
        result.output.contains("Edited"),
        "Should confirm edit: {}",
        result.output
    );

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

    assert!(
        result.output.contains("Edited"),
        "Should confirm edit: {}",
        result.output
    );

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

    // Verify backup exists (file_history should have created one). The edit tool
    // keys file_history by the *canonical* path (via inspect_path_access), so query
    // with the canonical form: on macOS $TMPDIR is /var/... which canonicalizes to
    // /private/var/..., and the raw temp path would never match.
    let canonical = std::fs::canonicalize(&path).unwrap();
    let fh = ctx.file_history.lock().await;
    let latest = fh.latest_version(canonical.to_str().unwrap());
    assert!(
        latest.is_some(),
        "file_history should have a backup version"
    );

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
    assert!(
        result.success,
        "Multi-edit should succeed: {}",
        result.output
    );
    assert!(
        result.output.contains("3 edits applied"),
        "Should report 3 edits: {}",
        result.output
    );

    let new_content = std::fs::read_to_string(&path).unwrap();
    assert!(new_content.contains("useStore"), "Should have new import");
    assert!(
        new_content.contains("computed(() =>"),
        "Should have computed"
    );
    assert!(
        new_content.contains("Store loaded"),
        "Should have new template element"
    );

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
    assert!(
        result.success,
        "Multi-edit text match should succeed: {}",
        result.output
    );

    let new_content = std::fs::read_to_string(&path).unwrap();
    assert!(new_content.contains("'hi'"), "Should have replaced hello");
    assert!(
        new_content.contains("'earth'"),
        "Should have replaced world"
    );
    assert!(
        new_content.contains("console.log"),
        "Untouched code should remain"
    );

    cleanup(&path);
}

#[tokio::test]
async fn multi_edit_overlap_detection() {
    let content = (1..=20)
        .map(|i| format!("line {}", i))
        .collect::<Vec<_>>()
        .join("\n");
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
    // Overlapping edits are auto-merged: second edit (8-15) extends beyond
    // first (5-10), so the merged range is 5-15 with second edit's content.
    assert!(
        result.success,
        "Overlapping edits should be auto-merged, got: {}",
        result.output
    );

    // Verify the merge result: lines 5-15 replaced with "b" (second edit wins)
    let after = std::fs::read_to_string(&path).unwrap();
    let after_lines: Vec<&str> = after.lines().collect();
    assert_eq!(after_lines[0], "line 1"); // untouched
    assert_eq!(after_lines[3], "line 4"); // untouched
    assert_eq!(after_lines[4], "b"); // merged edit
    assert_eq!(after_lines[5], "line 16"); // after the merged range

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
    assert!(
        result.success,
        "Mixed-mode multi-edit should succeed: {}",
        result.output
    );

    let new_content = std::fs::read_to_string(&path).unwrap();
    assert!(new_content.contains("useState"), "Import should be updated");
    assert!(
        new_content.contains("useState('new')"),
        "State hook should be added"
    );

    cleanup(&path);
}

#[tokio::test]
async fn multi_edit_string_line_numbers() {
    // Test lenient parsing: model sends line numbers as strings
    let content = "line 1\nline 2\nline 3\nline 4\nline 5\n";
    let path = create_test_file(content);
    let ctx = test_context();
    let tool = atomcode_core::tool::edit::EditFileTool;

    let args = serde_json::json!({
        "file_path": path,
        "edits": [{
            "start_line": "2",
            "end_line": "3",
            "new_string": "replaced"
        }]
    }).to_string();

    let result = tool.execute(&args, &ctx).await.unwrap();
    assert!(
        result.success,
        "String line numbers should work via lenient parsing: {}",
        result.output
    );

    let new_content = std::fs::read_to_string(&path).unwrap();
    assert!(
        new_content.contains("replaced"),
        "Edit should have been applied"
    );
    assert!(
        !new_content.contains("line 2"),
        "Old content should be gone"
    );

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
    assert!(
        result.output.contains("old_string is required"),
        "Should tell model old_string is required: {}",
        result.output
    );

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
    let c_count = new_content
        .lines()
        .filter(|l| l.trim() == "const c = ref(0)")
        .count();
    assert_eq!(
        c_count, 1,
        "const c should appear exactly once (boundary auto-corrected), got:\n{}",
        new_content
    );

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
    assert!(
        result.success,
        "Multi-edit should succeed: {}",
        result.output
    );

    let new_content = std::fs::read_to_string(&path).unwrap();
    let liked_count = new_content
        .lines()
        .filter(|l| l.trim() == "const isLiked = ref(false)")
        .count();
    let count_count = new_content
        .lines()
        .filter(|l| l.trim() == "const count = ref(0)")
        .count();
    assert_eq!(
        liked_count, 1,
        "isLiked should appear once (no duplicate), got:\n{}",
        new_content
    );
    assert_eq!(
        count_count, 1,
        "count should appear once (boundary corrected), got:\n{}",
        new_content
    );
    assert!(
        new_content.contains("showBackToTop"),
        "New code should be present"
    );

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
    let tab_count = new_content
        .lines()
        .filter(|l| l.trim() == "const activeTab = ref('profile')")
        .count();
    assert_eq!(
        tab_count, 1,
        "activeTab should appear exactly once (leading boundary corrected), got:\n{}",
        new_content
    );
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
    assert!(
        result.success,
        "Multi-edit should succeed: {}",
        result.output
    );

    let new_content = std::fs::read_to_string(&path).unwrap();
    let import_ref_count = new_content
        .lines()
        .filter(|l| l.trim() == "import { ref } from 'vue'")
        .count();
    let div_count = new_content
        .lines()
        .filter(|l| l.trim() == "<div>{{ name }}</div>")
        .count();
    assert_eq!(
        import_ref_count, 1,
        "import ref should appear once (leading corrected), got:\n{}",
        new_content
    );
    assert_eq!(
        div_count, 1,
        "div should appear once (leading corrected), got:\n{}",
        new_content
    );

    cleanup(&path);
}

// ═══════════════════════════════════════════════════════════════
// 9. Delta validation — edit on broken file should NOT be rejected
//    if the edit didn't make the balance worse
// ═══════════════════════════════════════════════════════════════

#[tokio::test]
async fn delta_correct_edit_on_broken_file_accepted() {
    // File already has an extra </div> (pre-existing bug).
    // Edit changes paragraph text — doesn't touch divs — should be ACCEPTED.
    let content = "<script setup lang=\"ts\">\nconst name = ref('hello')\n</script>\n\n<template>\n  <div class=\"root\">\n    <div class=\"inner\">\n        <p>content</p>\n    </div>\n    </div>\n  </div>\n</template>";

    let path = create_test_file(content);
    let ctx = test_context();
    let tool = atomcode_core::tool::edit::EditFileTool;

    let args = serde_json::json!({
        "file_path": path,
        "old_string": "<p>content</p>",
        "new_string": "<p>updated content</p>"
    });

    let result = tool.execute(&args.to_string(), &ctx).await.unwrap();
    assert!(
        result.success,
        "Edit on broken file should be accepted if it doesn't worsen balance: {}",
        result.output
    );
    cleanup(&path);
}

#[tokio::test]
async fn delta_bad_edit_on_good_file_accepted() {
    // Structural delta validation removed — auto-compile handles structural errors.
    // Edits that break balance are now accepted (written to disk) and caught by compile.
    let content = "<script setup lang=\"ts\">\nconst x = 1\n</script>\n\n<template>\n  <div class=\"root\">\n    <div class=\"inner\">\n      <p>hello</p>\n    </div>\n  </div>\n</template>";

    let path = create_test_file(content);
    let ctx = test_context();
    let tool = atomcode_core::tool::edit::EditFileTool;

    let args = serde_json::json!({
        "file_path": path,
        "old_string": "    </div>\n  </div>",
        "new_string": "  </div>"
    });

    let result = tool.execute(&args.to_string(), &ctx).await.unwrap();
    assert!(
        result.success,
        "Edit should be accepted (auto-compile catches structural issues): {}",
        result.output
    );
    cleanup(&path);
}

#[tokio::test]
async fn delta_fix_edit_on_broken_file_accepted() {
    // File has missing </div>. Edit adds it back — should be ACCEPTED (improves balance).
    let content = "<script setup lang=\"ts\">\nconst x = 1\n</script>\n\n<template>\n  <div class=\"root\">\n    <div class=\"inner\">\n      <p>hello</p>\n  </div>\n</template>";

    let path = create_test_file(content);
    let ctx = test_context();
    let tool = atomcode_core::tool::edit::EditFileTool;

    let args = serde_json::json!({
        "file_path": path,
        "old_string": "      <p>hello</p>",
        "new_string": "      <p>hello</p>\n    </div>"
    });

    let result = tool.execute(&args.to_string(), &ctx).await.unwrap();
    assert!(
        result.success,
        "Fix edit should be accepted: {}",
        result.output
    );
    cleanup(&path);
}

#[tokio::test]
async fn delta_multi_edit_on_broken_file_accepted() {
    // File has pre-existing balance issue. Multi-edit changes script + template
    // without changing div balance — should be ACCEPTED.
    let content = "<script setup lang=\"ts\">\nimport { ref } from 'vue'\nconst count = ref(0)\n</script>\n\n<template>\n  <div class=\"app\">\n    <div class=\"header\">\n      <h1>Title</h1>\n    </div>\n    <div class=\"body\">\n      <p>{{ count }}</p>\n  </div>\n</template>";

    let path = create_test_file(content);
    let ctx = test_context();
    let tool = atomcode_core::tool::edit::EditFileTool;

    let args = serde_json::json!({
        "file_path": path,
        "edits": [
            { "old_string": "const count = ref(0)", "new_string": "const count = ref(42)" },
            { "old_string": "<h1>Title</h1>", "new_string": "<h1>New Title</h1>" }
        ]
    });

    let result = tool.execute(&args.to_string(), &ctx).await.unwrap();
    assert!(
        result.success,
        "Multi-edit that doesn't worsen balance should be accepted: {}",
        result.output
    );

    let after = std::fs::read_to_string(&path).unwrap();
    assert!(after.contains("ref(42)"));
    assert!(after.contains("New Title"));
    cleanup(&path);
}

/// Regression for hermes 2026-04-22_20-41-00 session: `find_closest_match`
/// used to return `Closest match near line 3` when line 3 was a blank
/// line, because `first_line_trimmed.contains("")` is always true so every
/// blank line in the file became a substring "candidate". The bogus
/// closest-match pointed the agent's attention at the wrong section of
/// the file (lines 1-16 dumped as recovery hint) and prevented any real
/// recovery. Fixed by requiring both sides of the substring match to
/// carry ≥ 4 chars of signal.
#[tokio::test]
async fn edit_404_does_not_point_at_blank_line_as_closest_match() {
    // File has blank lines scattered throughout (typical Rust/Python/JS)
    // plus a real target at the tail that the agent's (wrongly-cased)
    // old_string won't match.
    let content = "\
use std::path::PathBuf;

struct Foo {
    bar: i32,
}

impl Foo {
    fn new() -> Self {
        Self { bar: 0 }
    }
}

fn main() {
    let app = builder()
        .run(context!());
}
";
    let path = create_test_file(content);
    let ctx = test_context();
    let tool = atomcode_core::tool::edit::EditFileTool;

    // Miscased old_string — model "remembered" `.Run` but file has `.run`.
    let args = serde_json::json!({
        "file_path": path,
        "old_string": "        .Run(context!());\n}",
        "new_string": "        .run(context!());\n    extra();\n}"
    });
    let r: ToolResult = tool.execute(&args.to_string(), &ctx).await.unwrap();

    assert!(!r.success);
    // The bug: output contained "Closest match found near line 3" (a
    // blank line). After fix, either no closest-match is reported, or if
    // one is reported, it must NOT point at a blank line.
    if let Some(idx) = r.output.find("Closest match found near line ") {
        let tail = &r.output[idx + "Closest match found near line ".len()..];
        let line_num_str: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
        let line_num: usize = line_num_str.parse().expect("line number in closest-match");
        let file_lines: Vec<&str> = content.lines().collect();
        let target = file_lines.get(line_num - 1).copied().unwrap_or("");
        assert!(
            !target.trim().is_empty(),
            "closest-match must not point at a blank line (line {}). output:\n{}",
            line_num,
            r.output
        );
    }
    cleanup(&path);
}

// ═══════════════════════════════════════════════════════════════
// P1 #14c+11 — fuzzy match indent stability (2026-04-23)
// ═══════════════════════════════════════════════════════════════
//
// Regression: hermes session 2026-04-22_21-06 showed fuzzy match
// destroying indentation across consecutive edits — line 276 drifted
// 8 → 17 → 26 spaces over two edits. Root cause: `try_fuzzy_replace`
// computed `new_base_indent` via `.min()` across all non-empty lines,
// including outdented lines like `}` (at column 0). A block of indented
// code followed by a closing brace made `min = 0`, inflating the
// relative indent of the nested lines.
//
// Fix: anchor on the FIRST non-empty line's indent; support outdented
// lines via signed relative diff with saturation at 0.

#[tokio::test]
async fn fuzzy_match_preserves_indent_on_miscased_old_string() {
    // Exact hermes 21-06 shape: model writes old_string at 9 spaces
    // (one extra), file has 8 spaces, fuzzy-match should succeed and
    // OUTPUT at 8 spaces (file's true indent).
    let content = "\
fn main() {
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect(\"error while running tauri application\");
}
";
    let path = create_test_file(content);
    let ctx = test_context();
    let tool = atomcode_core::tool::edit::EditFileTool;

    // Model's args — 9-space indent instead of 8, plus adding a marker line
    let args = serde_json::json!({
        "file_path": path,
        "old_string": "         .run(tauri::generate_context!())\n         .expect(\"error while running tauri application\");\n}",
        "new_string": "         .run(tauri::generate_context!())\n         .expect(\"error while running tauri application\");\n}\nundefined_marker();"
    });
    let r: ToolResult = tool.execute(&args.to_string(), &ctx).await.unwrap();
    assert!(r.success, "fuzzy match should succeed; got: {}", r.output);

    let after = std::fs::read_to_string(&path).unwrap();
    // The critical assertion: `.run(...)` must still be at 8 spaces,
    // not 17 (8 + 9 drift) nor any other amount.
    let run_line = after.lines().find(|l| l.contains(".run(tauri::"))
        .expect("`.run(` line must still exist after edit");
    let run_indent = run_line.len() - run_line.trim_start().len();
    assert_eq!(
        run_indent, 8,
        "indent drift detected: `.run(...)` should stay at 8 spaces, got {} — content:\n{}",
        run_indent, after
    );
    // Expect `.expect(...)` on its own 8-space-indented line too.
    assert!(after.contains("        .expect(\"error while running"),
        "`.expect(...)` should keep 8-space indent; got:\n{}", after);
    // Marker was added (new_string included it).
    assert!(after.contains("undefined_marker"), "marker should be present");
    cleanup(&path);
}

#[tokio::test]
async fn fuzzy_match_stable_across_two_consecutive_edits() {
    // Simulates the exact session: add marker (edit 1), delete marker
    // (edit 2). Without the fix, indent drifts 8 → 17 → 26. With the
    // fix, indent stays at 8 after both edits.
    let content = "\
fn main() {
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect(\"error while running tauri application\");
}
";
    let path = create_test_file(content);
    let ctx = test_context();
    let tool = atomcode_core::tool::edit::EditFileTool;

    // Edit 1: add marker (model sends 9-space old/new_string)
    let args1 = serde_json::json!({
        "file_path": path,
        "old_string": "         .run(tauri::generate_context!())\n         .expect(\"error while running tauri application\");\n}",
        "new_string": "         .run(tauri::generate_context!())\n         .expect(\"error while running tauri application\");\n}\nmarker();"
    });
    let r1 = tool.execute(&args1.to_string(), &ctx).await.unwrap();
    assert!(r1.success);

    // Edit 2: delete marker (same miscased old_string shape)
    let args2 = serde_json::json!({
        "file_path": path,
        "old_string": "         .run(tauri::generate_context!())\n         .expect(\"error while running tauri application\");\n}\nmarker();",
        "new_string": "         .run(tauri::generate_context!())\n         .expect(\"error while running tauri application\");\n}"
    });
    let r2 = tool.execute(&args2.to_string(), &ctx).await.unwrap();
    assert!(r2.success);

    let after = std::fs::read_to_string(&path).unwrap();
    let run_line = after.lines().find(|l| l.contains(".run(tauri::"))
        .expect("`.run(` line");
    let run_indent = run_line.len() - run_line.trim_start().len();
    // Without the fix this would be 17 or 26. With the fix it's 8.
    assert_eq!(
        run_indent, 8,
        "two-edit indent stability failed: `.run(...)` at {} spaces. content:\n{}",
        run_indent, after
    );
    // Marker should be gone.
    assert!(!after.contains("marker"), "marker should be deleted, got:\n{}", after);
    cleanup(&path);
}

#[tokio::test]
async fn fuzzy_match_handles_outdented_lines_correctly() {
    // Verify the signed-arithmetic outdent path works: new_string has
    // a line that's outdented RELATIVE to the anchor — it should land
    // at the corresponding outdented position in the output file, NOT
    // get inflated to the anchor's indent.
    let content = "\
    if cond {
        foo();
        bar();
    }
";
    let path = create_test_file(content);
    let ctx = test_context();
    let tool = atomcode_core::tool::edit::EditFileTool;

    // Replace the block. new_string has `.foo()` at 2 spaces (outdented
    // by 6 relative to the 8-space `foo();`) and `.bar()` at 0.
    let args = serde_json::json!({
        "file_path": path,
        "old_string": "        foo();\n        bar();",
        "new_string": "        updated_foo();\n  outdented();\nbareline();"
    });
    let r = tool.execute(&args.to_string(), &ctx).await.unwrap();
    assert!(r.success, "got: {}", r.output);

    let after = std::fs::read_to_string(&path).unwrap();
    // `updated_foo` should keep its 8-space indent (anchor match).
    let foo_line = after.lines().find(|l| l.contains("updated_foo"))
        .expect("updated_foo line");
    assert_eq!(foo_line.len() - foo_line.trim_start().len(), 8);
    // `outdented()` was 6 less than anchor → 8 - 6 = 2 spaces.
    let out_line = after.lines().find(|l| l.contains("outdented"))
        .expect("outdented line");
    assert_eq!(out_line.len() - out_line.trim_start().len(), 2);
    // `bareline()` was 8 less than anchor → clamp to 0.
    let bare_line = after.lines().find(|l| l.contains("bareline"))
        .expect("bareline");
    assert_eq!(bare_line.len() - bare_line.trim_start().len(), 0);
    cleanup(&path);
}
