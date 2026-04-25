//! Phase 2 tests — unified prompt + subtask driver.

use atomcode_core::agent::subtask_driver::SubtaskDriver;
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

// ═══════════════════════════════════════════════════════════════
// 2. Subtask driver
// ═══════════════════════════════════════════════════════════════

#[test]
fn subtask_extracts_files_from_plan() {
    let mut driver = SubtaskDriver::new();
    driver.extract_from_plan("\u{4FEE}\u{6539} TagRebuildTaskService.java, AITagExtractionService.java \u{548C} SettingsView.vue");
    assert!(driver.active);
    assert_eq!(driver.subtasks.len(), 3);
    // Backend first
    assert!(driver.subtasks[0].file.ends_with(".java"));
    // Frontend last
    assert!(driver.subtasks[2].file.ends_with(".vue"));
}

#[test]
fn subtask_instruction_format() {
    let mut driver = SubtaskDriver::new();
    driver.extract_from_plan("\u{4FEE}\u{6539} TagService.java \u{548C} SettingsView.vue");
    let instr = driver.current_instruction().unwrap();
    assert!(instr.contains("Subtask 1/2"));
    assert!(instr.contains("ONE edit"));
}

#[test]
fn subtask_advance_and_complete() {
    let mut driver = SubtaskDriver::new();
    driver.extract_from_plan("\u{4FEE}\u{6539} A.java, B.java, C.vue");
    assert_eq!(driver.subtasks.len(), 3);
    driver.advance();
    driver.advance();
    driver.advance();
    assert!(driver.all_done());
    assert!(!driver.active);
}

#[test]
fn subtask_empty_plan() {
    let mut driver = SubtaskDriver::new();
    driver.extract_from_plan(
        "\u{6211}\u{89C9}\u{5F97}\u{9700}\u{8981}\u{4FEE}\u{6539}\u{4E00}\u{4E9B}\u{4EE3}\u{7801}",
    );
    assert!(!driver.active);
}
