use super::*;
use atomcode_kernel::tool::ToolCall;

fn receipt(id: &str, command: &str, output: &str, failed: bool) -> Vec<Message> {
    vec![
        Message::assistant(
            "",
            vec![ToolCall {
                id: id.into(),
                name: "bash".into(),
                arguments: json!({"command": command}).to_string(),
            }],
        ),
        Message::tool_result(id, output, failed),
    ]
}

#[test]
fn old_failures_cannot_hide_current_validation_receipts() {
    let mut messages = vec![Message::user("earlier task")];
    for n in 0..6 {
        messages.extend(receipt(
            &format!("old-{n}"),
            "old command",
            "OLD_MCP_FAILURE",
            true,
        ));
    }
    messages.push(Message::user(
        "continue and validate the existing artifacts",
    ));
    messages.extend(receipt(
        "tests",
        "npm test",
        &format!(
            "{}\n19 tests passed; zero failed",
            "ordinary output\n".repeat(300)
        ),
        false,
    ));
    messages.extend(receipt(
        "build",
        "npm run build:h5 && npm run build:mp-weixin",
        "H5 DONE; mp-weixin DONE",
        false,
    ));
    messages.extend(receipt(
        "files",
        "ls docs/IMPLEMENTATION.md docs/TEST-REPORT.md",
        "docs/IMPLEMENTATION.md\ndocs/TEST-REPORT.md",
        false,
    ));
    messages.extend(receipt("todos", "todo status", "(no tasks)", false));
    messages.push(Message::assistant(
        format!(
            "{}\nVerified artifacts: 19 tests passed; docs/TEST-REPORT.md exists.",
            "Implementation notes. ".repeat(30)
        ),
        vec![],
    ));
    let summary = summarize_for_goal(&messages, None);
    assert!(!summary.contains("OLD_MCP_FAILURE"), "{summary}");
    for evidence in [
        "19 tests passed",
        "H5 DONE",
        "mp-weixin DONE",
        "docs/TEST-REPORT.md",
        "npm test",
    ] {
        assert!(summary.contains(evidence), "missing {evidence}: {summary}");
    }
}

#[test]
fn current_failures_and_successes_have_independent_space() {
    let mut messages = vec![Message::synthetic_user(
        "Goal not yet met: verify the build",
    )];
    for n in 0..8 {
        messages.extend(receipt(
            &format!("bad-{n}"),
            "build",
            &format!("CURRENT_FAILURE_{n}"),
            true,
        ));
    }
    for n in 0..8 {
        messages.extend(receipt(
            &format!("good-{n}"),
            "test",
            &format!("CURRENT_SUCCESS_{n}"),
            false,
        ));
    }
    let summary = summarize_for_goal(&messages, None);
    assert!(summary.contains("CURRENT_FAILURE_7"), "{summary}");
    assert!(summary.contains("CURRENT_SUCCESS_7"), "{summary}");
    assert!(!summary.contains("CURRENT_FAILURE_0"), "{summary}");
}

#[test]
fn long_unicode_reports_and_outputs_keep_the_end_without_exceeding_the_budget() {
    let mut messages = vec![Message::user("verify")];
    for n in 0..10 {
        messages.extend(receipt(
            &format!("r-{n}"),
            &"验证".repeat(2000),
            &format!("{}\nTAIL_RECEIPT_{n}", "输出".repeat(4000)),
            n % 2 == 0,
        ));
    }
    messages.push(Message::assistant(
        format!("{}\nFINAL_REPORT_END", "过程".repeat(8000)),
        vec![],
    ));
    let summary = summarize_for_goal(&messages, Some(&"旧结论".repeat(2000)));
    assert!(summary.contains("TAIL_RECEIPT_9"), "{summary}");
    assert!(summary.contains("FINAL_REPORT_END"), "{summary}");
    assert!(
        summary.chars().count() <= 6012,
        "{}",
        summary.chars().count()
    );
}
