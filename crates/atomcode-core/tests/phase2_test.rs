//! Unified prompt tests.

use atomcode_config::config::prompt_sections::build_rules;

// ═══════════════════════════════════════════════════════════════
// 1. Unified prompt — minimal but complete
// ═══════════════════════════════════════════════════════════════

#[test]
fn unified_prompt_has_all_sections() {
    let prompt = build_rules();
    assert!(prompt.contains("WORKFLOW"), "Must have workflow section");
    assert!(prompt.contains("TOOLS"), "Must have tool guide");
    assert!(prompt.contains("WHEN STUCK"), "Must have stuck section");
    assert!(prompt.contains("OUTPUT"), "Must have output guidelines");
    assert!(prompt.contains("edit_file"), "Must mention edit_file");
    assert!(prompt.contains("write_file"), "Must mention write_file");
}

#[test]
fn unified_prompt_has_key_guidance() {
    let prompt = build_rules();
    assert!(prompt.contains("edit_file"), "Must guide edit mode format");
    assert!(
        prompt.contains("search_replace"),
        "Must guide search-replace editing"
    );
    assert!(
        prompt.contains("never with `bash`"),
        "Must ban bash file mutation"
    );
}

#[test]
fn unified_prompt_size_reasonable() {
    let prompt = build_rules();
    let tokens = prompt.len() / 4;
    assert!(
        tokens > 50,
        "Too short: {} tokens — rules may be missing",
        tokens
    );
    assert!(
        tokens < 2500,
        "Too long: {} tokens — violates Less is More principle",
        tokens
    );
}
