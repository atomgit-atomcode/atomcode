//! Cross-session project knowledge persistence.
//!
//! Records what was done in each session (files edited, test commands used)
//! to `.atomcode/knowledge.md`. Injected into system prompt so future sessions
//! don't waste turns rediscovering the same information.
//!
//! Zero hardcoded patterns — all knowledge is extracted from runtime data.

use std::path::Path;

const MAX_ENTRIES: usize = 10;

/// Record a completed session's activity to knowledge.md.
/// Called from finish_turn when files were edited.
pub fn record_session(
    working_dir: &Path,
    user_task: &str,
    files_edited: &[String],
    last_curl_cmd: Option<&str>,
    build_ok: bool,
) {
    if files_edited.is_empty() {
        return;
    }

    let dir = working_dir.join(".atomcode");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("knowledge.md");

    // Build entry
    let task_short: String = if user_task.chars().count() > 50 {
        format!("{}...", user_task.chars().take(47).collect::<String>())
    } else {
        user_task.to_string()
    };
    // Clean control chars from user task
    let task_clean: String = task_short.chars()
        .filter(|c| !c.is_control() || *c == ' ')
        .collect();

    let files_short: Vec<String> = files_edited.iter()
        .map(|f| std::path::Path::new(f)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| f.clone()))
        .collect();

    let now = {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let h = (secs / 3600) % 24;
        let m = (secs / 60) % 60;
        format!("{:02}:{:02}", h, m)
    };

    let status = if build_ok { "OK" } else { "FAILED" };
    let entry = format!(
        "- [{}] \"{}\" → edited: {} | {}",
        now, task_clean, files_short.join(", "), status
    );

    // Read existing entries
    let mut entries: Vec<String> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("- ["))
        .map(|l| l.to_string())
        .collect();

    // Append new entry
    entries.push(entry);

    // Also record/update test command if found
    if let Some(curl) = last_curl_cmd {
        // Remove old test entry
        entries.retain(|l| !l.starts_with("- [test]"));
        let curl_short: String = curl.chars().take(120).collect();
        entries.push(format!("- [test] {}", curl_short));
    }

    // FIFO: keep last MAX_ENTRIES
    while entries.len() > MAX_ENTRIES {
        entries.remove(0);
    }

    let content = format!(
        "# Project Knowledge (auto-generated)\n\n{}\n",
        entries.join("\n")
    );
    let _ = std::fs::write(&path, content);
}

/// Save a user-triggered knowledge entry ("记住这个" / "remember this").
pub fn save_user_knowledge(working_dir: &Path, _category: &str, content: &str) {
    let dir = working_dir.join(".atomcode");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("knowledge.md");

    let mut entries: Vec<String> = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("- "))
        .map(|l| l.to_string())
        .collect();

    let short: String = content.chars().take(100).collect();
    entries.push(format!("- [note] {}", short));

    while entries.len() > MAX_ENTRIES {
        entries.remove(0);
    }

    let out = format!(
        "# Project Knowledge (auto-generated)\n\n{}\n",
        entries.join("\n")
    );
    let _ = std::fs::write(&path, out);
}

/// Load knowledge for injection into system prompt.
pub fn load_knowledge(working_dir: &Path) -> String {
    let path = working_dir.join(".atomcode/knowledge.md");
    match std::fs::read_to_string(&path) {
        Ok(content) if !content.trim().is_empty() => {
            let lines: Vec<&str> = content.lines()
                .filter(|l| l.starts_with("- "))
                .collect();
            if lines.is_empty() {
                return String::new();
            }
            format!(
                "=== PROJECT KNOWLEDGE (from previous sessions) ===\n{}\n",
                lines.join("\n")
            )
        }
        _ => String::new(),
    }
}

/// Find the last successful curl command in conversation messages.
pub fn find_last_curl(messages: &[crate::conversation::message::Message]) -> Option<String> {
    for msg in messages.iter().rev() {
        if let crate::conversation::message::MessageContent::AssistantWithToolCalls { tool_calls, .. } = &msg.content {
            for tc in tool_calls.iter().rev() {
                if tc.name == "bash" {
                    if let Ok(args) = serde_json::from_str::<serde_json::Value>(&tc.arguments) {
                        if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
                            if cmd.contains("curl ") {
                                return Some(cmd.to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    None
}
