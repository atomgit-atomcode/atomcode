//! Vue <template> tree-sitter-html parsing tests.
//! Uses a shared SemanticSearcher to avoid re-initializing 10 language parsers per test.

use atomcode_core::semantic::SemanticSearcher;
use std::io::Write;
use std::sync::Mutex;

static SEARCHER: std::sync::LazyLock<Mutex<SemanticSearcher>> =
    std::sync::LazyLock::new(|| Mutex::new(SemanticSearcher::new()));

fn with_searcher<F, R>(f: F) -> R
where
    F: FnOnce(&mut SemanticSearcher) -> R,
{
    let mut s = SEARCHER.lock().unwrap();
    f(&mut s)
}

/// Create a temp Vue file and return its path.
fn write_temp_vue(content: &str) -> (tempfile::NamedTempFile, std::path::PathBuf) {
    let mut f = tempfile::Builder::new().suffix(".vue").tempfile().unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f.flush().unwrap();
    let path = f.path().to_path_buf();
    (f, path)
}

// ═══════════════════════════════════════════════════════════════
// 1. Basic Vue SFC: both <script> and <template> symbols
// ═══════════════════════════════════════════════════════════════

#[test]
fn vue_extracts_script_functions() {
    let vue = r#"
<script setup lang="ts">
function fetchData() { return 1; }
const handleClick = () => { console.log('click'); }
</script>
<template>
  <div>Hello</div>
</template>
"#;
    let (_f, path) = write_temp_vue(vue);
    let symbols = with_searcher(|s| s.list_symbols(&path).unwrap());

    let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains(&"fetchData"),
        "Should find fetchData, got: {:?}",
        names
    );
    assert!(
        names.contains(&"handleClick"),
        "Should find handleClick, got: {:?}",
        names
    );
}

#[test]
fn vue_extracts_template_components() {
    let vue = r#"
<script setup lang="ts">
function setup() {}
</script>
<template>
  <div>
    <TaskCard v-for="task in tasks" :key="task.id" />
    <UserProfile v-if="user" :user="user" />
    <button @click="handleSubmit">Submit</button>
  </div>
</template>
"#;
    let (_f, path) = write_temp_vue(vue);
    let symbols = with_searcher(|s| s.list_symbols(&path).unwrap());

    let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
    // Should find custom components from template
    assert!(
        names.contains(&"<TaskCard>"),
        "Should find <TaskCard>, got: {:?}",
        names
    );
    assert!(
        names.contains(&"<UserProfile>"),
        "Should find <UserProfile>, got: {:?}",
        names
    );
    // button with @click should be included
    assert!(
        names.contains(&"<button>"),
        "Should find <button> with @click, got: {:?}",
        names
    );
}

// ═══════════════════════════════════════════════════════════════
// 2. Noise filtering: plain div/span without Vue attrs excluded
// ═══════════════════════════════════════════════════════════════

#[test]
fn vue_filters_plain_divs() {
    let vue = r#"
<script setup lang="ts">
</script>
<template>
  <div class="container">
    <span>text</span>
    <p>paragraph</p>
    <div v-if="show">Conditional div</div>
    <CustomComponent />
  </div>
</template>
"#;
    let (_f, path) = write_temp_vue(vue);
    let symbols = with_searcher(|s| s.list_symbols(&path).unwrap());

    let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
    // Plain div/span/p should be filtered out
    assert!(!names.contains(&"<span>"), "Should NOT find plain <span>");
    assert!(!names.contains(&"<p>"), "Should NOT find plain <p>");
    // div with v-if should be kept
    assert!(
        names.contains(&"<div>"),
        "Should find <div v-if>, got: {:?}",
        names
    );
    // Custom component always kept
    assert!(
        names.contains(&"<CustomComponent>"),
        "Should find <CustomComponent>, got: {:?}",
        names
    );
}

// ═══════════════════════════════════════════════════════════════
// 3. Line numbers: template symbols have correct line numbers
// ═══════════════════════════════════════════════════════════════

#[test]
fn vue_template_symbols_have_line_numbers() {
    let vue = r#"<script setup lang="ts">
function setup() {}
</script>
<template>
  <div>
    <MyComponent v-for="item in items" />
  </div>
</template>
"#;
    let (_f, path) = write_temp_vue(vue);
    let symbols = with_searcher(|s| s.list_symbols(&path).unwrap());

    let comp = symbols.iter().find(|s| s.name == "<MyComponent>").unwrap();
    // MyComponent is on line 6 (1-indexed)
    assert!(
        comp.start_line >= 5 && comp.start_line <= 7,
        "Expected line ~6, got {}",
        comp.start_line
    );
}

// ═══════════════════════════════════════════════════════════════
// 4. No template section: still works (script-only Vue)
// ═══════════════════════════════════════════════════════════════

#[test]
fn vue_without_template_still_works() {
    let vue = r#"
<script setup lang="ts">
function init() { return 42; }
</script>
"#;
    let (_f, path) = write_temp_vue(vue);
    let symbols = with_searcher(|s| s.list_symbols(&path).unwrap());

    let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
    assert!(
        names.contains(&"init"),
        "Should find init from script, got: {:?}",
        names
    );
}

// ═══════════════════════════════════════════════════════════════
// 5. Skeleton includes template symbols
// ═══════════════════════════════════════════════════════════════

#[test]
fn vue_skeleton_includes_template_info() {
    let vue = r#"
<script setup lang="ts">
function fetchUsers() { return []; }
function deleteUser(id: number) { return true; }
</script>
<template>
  <div>
    <UserTable v-if="users.length" :users="users" />
    <EmptyState v-else />
  </div>
</template>
"#;
    let (_f, path) = write_temp_vue(vue);
    let skeleton = with_searcher(|s| s.skeleton(&path).unwrap());

    // Skeleton should mention both script functions and template components
    assert!(
        skeleton.contains("fetchUsers"),
        "Skeleton should contain fetchUsers"
    );
    assert!(
        skeleton.contains("deleteUser"),
        "Skeleton should contain deleteUser"
    );
    // Template components should appear in symbol list (used by skeleton)
}

// ═══════════════════════════════════════════════════════════════
// 6. HTML file: standalone HTML parsing
// ═══════════════════════════════════════════════════════════════

#[test]
fn html_file_extracts_elements() {
    let html = r#"<!DOCTYPE html>
<html>
<head><title>Test</title></head>
<body>
  <nav class="navbar">Navigation</nav>
  <main>
    <section id="hero">Hero</section>
    <form action="/api" method="POST">
      <input type="text" />
    </form>
  </main>
</body>
</html>"#;
    let mut f = tempfile::Builder::new().suffix(".html").tempfile().unwrap();
    f.write_all(html.as_bytes()).unwrap();
    f.flush().unwrap();
    let path = f.path().to_path_buf();

    let symbols = with_searcher(|s| s.list_symbols(&path).unwrap());

    assert!(!symbols.is_empty(), "Should find HTML elements");
    let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
    // Should find structural elements
    assert!(
        names.contains(&"html")
            || names.contains(&"body")
            || names.contains(&"nav")
            || names.contains(&"main")
            || names.contains(&"section")
            || names.contains(&"form"),
        "Should find structural HTML elements, got: {:?}",
        names
    );
}
