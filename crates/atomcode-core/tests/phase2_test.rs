//! Unified prompt tests.

use atomcode_core::config::prompt_sections::build_rules;

// ═══════════════════════════════════════════════════════════════
// 1. Unified prompt — minimal but complete
// ═══════════════════════════════════════════════════════════════

#[test]
fn unified_prompt_has_all_sections() {
    let prompt = build_rules();
    assert!(prompt.contains("WORKFLOW"), "Must have workflow section");
    assert!(prompt.contains("RULES"), "Must have rules section");
    assert!(prompt.contains("TOOLS"), "Must have tool guide");
    assert!(prompt.contains("SCOPE"), "Must have scope discipline");
    assert!(prompt.contains("VERIFY"), "Must have verify step");
    assert!(prompt.contains("edit_file"), "Must mention edit_file");
    assert!(prompt.contains("create_file"), "Must mention write_file");
}

#[test]
fn unified_prompt_has_key_guidance() {
    let prompt = build_rules();
    assert!(
        prompt.contains("### File:"),
        "Must guide EXECUTE mode format"
    );
    assert!(
        prompt.contains("old_string/new_string"),
        "Must guide text-match editing"
    );
    assert!(
        prompt.contains("NEVER write_file on existing"),
        "Must ban write_file on existing files"
    );
}

#[test]
fn unified_prompt_size_reasonable() {
    let prompt = build_rules();
    let tokens = prompt.len() / 4;
    // After "Less is More" refactor: ~80-200 tokens. Keep it minimal.
    assert!(
        tokens > 50,
        "Too short: {} tokens — rules may be missing",
        tokens
    );
    assert!(
        tokens < 500,
        "Too long: {} tokens — violates Less is More principle",
        tokens
    );
}
