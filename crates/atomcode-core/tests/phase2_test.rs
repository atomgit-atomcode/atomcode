//! Phase 2 tests — unified prompt + subtask driver.

use atomcode_core::config::prompt_sections::build_rules;
use atomcode_core::agent::subtask_driver::SubtaskDriver;

// ═══════════════════════════════════════════════════════════════
// 1. Unified prompt — always complete, never too short
// ═══════════════════════════════════════════════════════════════

#[test]
fn unified_prompt_has_all_sections() {
    let prompt = build_rules();
    assert!(prompt.contains("PRINCIPLES"), "Must have principles");
    assert!(prompt.contains("PLAN FIRST"), "Must have planning");
    assert!(prompt.contains("WORKFLOW"), "Must have workflow");
    assert!(prompt.contains("TOOL SELECTION"), "Must have tool guide");
    assert!(prompt.contains("DEBUGGING"), "Must have debug guide");
    assert!(prompt.contains("COMMAND DISCIPLINE"), "Must have command rules");
    assert!(prompt.contains("ERROR HANDLING"), "Must have error handling");
    assert!(prompt.contains("RULES"), "Must have rules");
}

#[test]
fn unified_prompt_has_key_guidance() {
    let prompt = build_rules();
    assert!(prompt.contains("HYPOTHESIS"), "Must ask for hypothesis");
    assert!(prompt.contains("Silent failure"), "Must cover silent failures");
    // SecurityConfig removed — hardcoding Java class names violates tech-stack neutrality.
    // Auth debugging is covered by generic "DEBUGGING AUTH ERRORS" section in system prompt.
    assert!(prompt.contains("ONE file at a time"), "Must guide per-file editing");
}

#[test]
fn unified_prompt_size_reasonable() {
    let prompt = build_rules();
    let tokens = prompt.len() / 4;
    assert!(tokens > 500, "Too short: {} tokens", tokens);
    assert!(tokens < 1500, "Too long: {} tokens", tokens);
}

// ═══════════════════════════════════════════════════════════════
// 2. Subtask driver
// ═══════════════════════════════════════════════════════════════

#[test]
fn subtask_extracts_files_from_plan() {
    let mut driver = SubtaskDriver::new();
    driver.extract_from_plan("修改 TagRebuildTaskService.java, AITagExtractionService.java 和 SettingsView.vue");
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
    driver.extract_from_plan("修改 TagService.java 和 SettingsView.vue");
    let instr = driver.current_instruction().unwrap();
    assert!(instr.contains("Subtask 1/2"));
    assert!(instr.contains("ONE edit"));
}

#[test]
fn subtask_advance_and_complete() {
    let mut driver = SubtaskDriver::new();
    driver.extract_from_plan("修改 A.java, B.java, C.vue");
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
    driver.extract_from_plan("我觉得需要修改一些代码");
    assert!(!driver.active);
}
