//! Phase 2 integration tests — verify task classification + dynamic prompt + planning.
//!
//! These tests use real user messages from actual datalog sessions to ensure
//! the classifier and prompt builder produce correct results in practice.

use atomcode_core::agent::task_classifier::{classify, TaskType};
use atomcode_core::config::prompt_sections::build_rules_for_task;

// ═══════════════════════════════════════════════════════════════
// 1. Task Classifier — real messages from datalogs
// ═══════════════════════════════════════════════════════════════

#[test]
fn classify_real_question_messages() {
    // From datalog 2026-03-31_20-49-47
    assert_eq!(
        classify("现在的逻辑是 一个一个调用还是 批量扔过去一堆", false),
        TaskType::Question
    );
    assert_eq!(classify("为什么用 Redis？", false), TaskType::Question);
    assert_eq!(classify("这个 API 返回什么格式？", false), TaskType::Question);
}

#[test]
fn classify_real_bugfix_messages() {
    // From datalog 2026-03-31_20-51-52
    let t = classify("没有成功，显示没有配置API Key", false);
    assert!(
        matches!(t, TaskType::BugFix),
        "Expected BugFix, got {:?}", t
    );
    // From datalog 2026-03-31_20-59-17
    assert_eq!(
        classify("标签生成了，但是 点击标签又不能 找到文章了", false),
        TaskType::BugFix
    );
    // From datalog 2026-03-31_15-58-27
    assert_eq!(
        classify("这些文章页的标签都是怎么来的？感觉算的不准，点击了任何一个标签获取的都是空的文章列表", false),
        TaskType::BugFix
    );
    // From datalog 2026-03-31_11-49-06
    assert_eq!(
        classify("还是不行，你思考的不对，我点了python 显示58 个文章，前端一个没显示出来", false),
        TaskType::BugFix
    );
}

#[test]
fn classify_real_followup_messages() {
    // From datalog 2026-03-31_21-04-30
    assert_eq!(classify("依然不行", true), TaskType::FollowUp);
    // From datalog 2026-03-31_16-08-58
    assert_eq!(classify("点击标签依然没有 点了python 还是没有", true), TaskType::FollowUp);
    // Short follow-up
    assert_eq!(classify("还是不行", true), TaskType::FollowUp);
    // Without previous turns → NOT follow-up
    assert_ne!(classify("依然不行", false), TaskType::FollowUp);
}

#[test]
fn classify_real_command_messages() {
    // From datalog 2026-03-31_18-57-21
    assert_eq!(classify("重启", false), TaskType::Command);
    assert_eq!(classify("编译一下", false), TaskType::Command);
    assert_eq!(classify("安装依赖", false), TaskType::Command);
}

#[test]
fn classify_real_feature_messages() {
    // From datalog 2026-03-31_18-55-01
    assert_eq!(
        classify("标签太固化，不应该都是开发者标签 应该用AI 自动提取", false),
        TaskType::FeatureDev
    );
    assert_eq!(
        classify("给发现页面添加标签筛选功能", false),
        TaskType::FeatureDev
    );
}

#[test]
fn classify_real_single_file_edit() {
    assert_eq!(
        classify("把 App.vue 的背景改成蓝色", false),
        TaskType::SingleFileEdit
    );
    assert_eq!(
        classify("修改 SecurityConfig.java 加上 /api/tags", false),
        TaskType::SingleFileEdit
    );
}

// ═══════════════════════════════════════════════════════════════
// 2. Dynamic Prompt — content correctness
// ═══════════════════════════════════════════════════════════════

#[test]
fn question_prompt_has_no_planning_no_debug() {
    let prompt = build_rules_for_task(&TaskType::Question);
    assert!(prompt.contains("PRINCIPLES"), "Must have principles");
    assert!(!prompt.contains("PLAN FIRST"), "Question should NOT have planning");
    assert!(!prompt.contains("DEBUGGING"), "Question should NOT have debug guide");
    assert!(!prompt.contains("WORKFLOW"), "Question should NOT have workflow");
    assert!(!prompt.contains("TOOL SELECTION"), "Question should NOT have tool guide");
}

#[test]
fn bugfix_prompt_has_planning_and_debug() {
    let prompt = build_rules_for_task(&TaskType::BugFix);
    assert!(prompt.contains("PLAN FIRST"), "BugFix MUST have planning");
    assert!(prompt.contains("HYPOTHESIS"), "BugFix MUST ask for hypothesis");
    assert!(prompt.contains("DEBUGGING"), "BugFix MUST have debug guide");
    assert!(prompt.contains("Silent failure"), "BugFix MUST cover silent failures");
    assert!(prompt.contains("WORKFLOW"), "BugFix MUST have workflow");
    assert!(prompt.contains("ERROR HANDLING"), "BugFix MUST have error handling");
}

#[test]
fn feature_prompt_has_planning_and_full_rules() {
    let prompt = build_rules_for_task(&TaskType::FeatureDev);
    assert!(prompt.contains("PLAN FIRST"), "Feature MUST have planning");
    assert!(prompt.contains("ADD code alongside"), "Feature MUST warn about not replacing");
    assert!(prompt.contains("NEVER write_file on existing"), "Feature MUST warn about write_file");
    assert!(!prompt.contains("DEBUGGING"), "Feature should NOT have debug guide");
}

#[test]
fn command_prompt_is_minimal() {
    let prompt = build_rules_for_task(&TaskType::Command);
    assert!(prompt.contains("COMMAND DISCIPLINE"), "Command MUST have command rules");
    assert!(!prompt.contains("PLAN FIRST"), "Command should NOT have planning");
    assert!(!prompt.contains("DEBUGGING"), "Command should NOT have debug guide");
    assert!(!prompt.contains("WORKFLOW"), "Command should NOT have workflow");
}

#[test]
fn followup_prompt_has_debug_but_no_planning() {
    let prompt = build_rules_for_task(&TaskType::FollowUp);
    assert!(prompt.contains("DEBUGGING"), "FollowUp MUST have debug guide");
    assert!(prompt.contains("WORKFLOW"), "FollowUp MUST have workflow");
    assert!(!prompt.contains("PLAN FIRST"), "FollowUp should NOT force planning");
}

// ═══════════════════════════════════════════════════════════════
// 3. Token efficiency — dynamic prompt is smaller
// ═══════════════════════════════════════════════════════════════

#[test]
fn dynamic_prompts_are_smaller_than_static() {
    let static_prompt = atomcode_core::config::DEFAULT_SYSTEM_PROMPT;
    let static_tokens = static_prompt.len() / 4; // ~4 chars per token

    for task_type in &[
        TaskType::Question,
        TaskType::Command,
        TaskType::SingleFileEdit,
        TaskType::BugFix,
        TaskType::FeatureDev,
        TaskType::FollowUp,
    ] {
        let dynamic = build_rules_for_task(task_type);
        let dynamic_tokens = dynamic.len() / 4;
        assert!(
            dynamic_tokens <= static_tokens,
            "{:?} dynamic prompt ({} tok) should be <= static ({} tok)",
            task_type, dynamic_tokens, static_tokens
        );
    }
}

#[test]
fn question_prompt_saves_at_least_50_percent() {
    let static_len = atomcode_core::config::DEFAULT_SYSTEM_PROMPT.len();
    let question_len = build_rules_for_task(&TaskType::Question).len();
    let savings = 1.0 - (question_len as f64 / static_len as f64);
    assert!(
        savings >= 0.5,
        "Question prompt should save ≥50% tokens, actual savings: {:.0}%",
        savings * 100.0
    );
}

// ═══════════════════════════════════════════════════════════════
// 4. Planning decision — correct tasks trigger planning
// ═══════════════════════════════════════════════════════════════

#[test]
fn planning_triggered_for_correct_types() {
    assert!(TaskType::BugFix.needs_planning(), "BugFix should trigger planning");
    assert!(TaskType::FeatureDev.needs_planning(), "FeatureDev should trigger planning");
    assert!(!TaskType::Question.needs_planning(), "Question should NOT trigger planning");
    assert!(!TaskType::Command.needs_planning(), "Command should NOT trigger planning");
    assert!(!TaskType::SingleFileEdit.needs_planning(), "SingleFileEdit should NOT trigger planning");
    assert!(!TaskType::FollowUp.needs_planning(), "FollowUp should NOT trigger planning");
}

// ═══════════════════════════════════════════════════════════════
// 5. End-to-end: classify → build prompt → verify content
// ═══════════════════════════════════════════════════════════════

#[test]
fn e2e_bugfix_message_gets_debug_prompt() {
    let msg = "点击标签找不到文章";
    let task_type = classify(msg, false);
    assert_eq!(task_type, TaskType::BugFix);

    let prompt = build_rules_for_task(&task_type);
    assert!(prompt.contains("HYPOTHESIS"), "BugFix prompt must ask for hypothesis");
    assert!(prompt.contains("SecurityConfig"), "BugFix debug should mention auth config");
}

#[test]
fn e2e_question_gets_minimal_prompt() {
    let msg = "这个是批量还是单个调用？";
    let task_type = classify(msg, false);
    assert_eq!(task_type, TaskType::Question);

    let prompt = build_rules_for_task(&task_type);
    // Should be very short
    assert!(prompt.len() < 800, "Question prompt should be <800 chars, got {}", prompt.len());
}

#[test]
fn e2e_followup_with_context_gets_debug() {
    let msg = "还是不行";
    let task_type = classify(msg, true); // has previous turns
    assert_eq!(task_type, TaskType::FollowUp);

    let prompt = build_rules_for_task(&task_type);
    assert!(prompt.contains("DEBUGGING"), "FollowUp must have debug guide");
}

#[test]
fn e2e_command_gets_discipline_only() {
    let msg = "重启";
    let task_type = classify(msg, false);
    assert_eq!(task_type, TaskType::Command);

    let prompt = build_rules_for_task(&task_type);
    assert!(prompt.contains("COMMAND DISCIPLINE"));
    assert!(prompt.len() < 1000, "Command prompt should be <1000 chars, got {}", prompt.len());
}
