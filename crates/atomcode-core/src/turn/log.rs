use crate::conversation::message::Message;
use crate::tool::{ToolCall, ToolDef};
use std::path::Path;

/// Log the complete LLM request (messages + tools + metadata) under
/// `<working_dir>/datalog/llm/YYYY-MM-DD_HH-MM-SS_NNN.json`.
///
/// Moved from `~/.atomcode/logs/` (2026-04-14): per-project logs colocate with
/// the turn-level datalog `.md` files so a project's entire session history is
/// in one place and can be zipped/deleted/shared atomically.
/// This is fire-and-forget — logging failures are silently ignored.
pub fn log_llm_request(
    working_dir: &Path,
    messages: &[Message],
    tool_defs: &[ToolDef],
    model: &str,
    context_window: usize,
    step: usize,
) {
    use std::io::Write;

    let log_dir = working_dir.join("datalog").join("llm");
    let _ = std::fs::create_dir_all(&log_dir);

    // Build timestamp filename.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let millis = now.subsec_millis();
    // Format as readable timestamp (UTC).
    let ts = {
        // Simple manual formatting to avoid adding chrono dependency.
        let s = secs % 60;
        let m = (secs / 60) % 60;
        let h = (secs / 3600) % 24;
        let days = secs / 86400;
        // Days since epoch → approximate date (good enough for filenames).
        let (y, mo, d) = epoch_days_to_ymd(days);
        format!("{:04}-{:02}-{:02}_{:02}-{:02}-{:02}_{:03}", y, mo, d, h, m, s, millis)
    };
    let path = log_dir.join(format!("{}.json", ts));

    // Serialize messages.
    let msgs_json = serde_json::to_value(messages).unwrap_or(serde_json::json!([]));

    // Serialize tool defs (not Serialize-derived, so do it manually).
    let tools_json: Vec<serde_json::Value> = tool_defs.iter().map(|td| {
        serde_json::json!({
            "name": td.name,
            "description": td.description,
            "parameters": td.parameters,
        })
    }).collect();

    // Token estimate.
    let total_tokens: usize = messages.iter().map(|m| m.estimate_tokens()).sum();

    let log = serde_json::json!({
        "timestamp": ts,
        "model": model,
        "context_window": context_window,
        "step": step,
        "message_count": messages.len(),
        "estimated_tokens": total_tokens,
        "tool_count": tool_defs.len(),
        "messages": msgs_json,
        "tools": tools_json,
    });

    // Write atomically via temp file.
    let tmp = path.with_extension("json.tmp");
    if let Ok(mut f) = std::fs::File::create(&tmp) {
        let _ = f.write_all(serde_json::to_string_pretty(&log).unwrap_or_default().as_bytes());
        let _ = std::fs::rename(&tmp, &path);
    }

    // Append one-line call record to calls.log (CSV: timestamp, model, step, msgs, tokens, ctx_window)
    let calls_path = log_dir.join("calls.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&calls_path) {
        let _ = f.write_all(format!(
            "{},{},{},{},{},{}\n",
            ts, model, step, messages.len(), total_tokens, context_window
        ).as_bytes());
    }
}

/// Log the LLM response (text + tool calls) under
/// `<working_dir>/datalog/llm/YYYY-MM-DD_HH-MM-SS_NNN_response.json`.
pub fn log_llm_response(
    working_dir: &Path,
    text: &str,
    tool_calls: &[ToolCall],
    model: &str,
    step: usize,
    duration_ms: u64,
) {
    use std::io::Write;

    let log_dir = working_dir.join("datalog").join("llm");
    let _ = std::fs::create_dir_all(&log_dir);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let millis = now.subsec_millis();
    let ts = {
        let s = secs % 60;
        let m = (secs / 60) % 60;
        let h = (secs / 3600) % 24;
        let days = secs / 86400;
        let (y, mo, d) = epoch_days_to_ymd(days);
        format!("{:04}-{:02}-{:02}_{:02}-{:02}-{:02}_{:03}", y, mo, d, h, m, s, millis)
    };
    let path = log_dir.join(format!("{}_response.json", ts));

    let tools_json: Vec<serde_json::Value> = tool_calls.iter().map(|tc| {
        serde_json::json!({
            "id": tc.id,
            "name": tc.name,
            "arguments": tc.arguments,
        })
    }).collect();

    let log = serde_json::json!({
        "timestamp": ts,
        "model": model,
        "step": step,
        "duration_ms": duration_ms,
        "text": text,
        "tool_calls": tools_json,
    });

    let tmp = path.with_extension("json.tmp");
    if let Ok(mut f) = std::fs::File::create(&tmp) {
        let _ = f.write_all(serde_json::to_string_pretty(&log).unwrap_or_default().as_bytes());
        let _ = std::fs::rename(&tmp, &path);
    }

    // Append to calls.log: response line with duration and tool count
    let calls_path = log_dir.join("calls.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&calls_path) {
        let tool_names: Vec<&str> = tool_calls.iter().map(|tc| tc.name.as_str()).collect();
        let _ = f.write_all(format!(
            "  → {}ms, {} tool(s): {}\n",
            duration_ms,
            tool_calls.len(),
            if tool_names.is_empty() { "text_only".to_string() } else { tool_names.join(", ") }
        ).as_bytes());
    }
}

/// Convert days since Unix epoch to (year, month, day). Simple civil calendar math.
/// Algorithm from http://howardhinnant.github.io/date_algorithms.html
fn epoch_days_to_ymd(days: u64) -> (u64, u64, u64) {
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::message::{Message, Role};
    use crate::tool::ToolDef;

    #[test]
    fn test_log_llm_request_creates_json() {
        let tmp = tempfile::TempDir::new().unwrap();
        let messages = vec![
            Message::new(Role::System, "You are helpful."),
            Message::new(Role::User, "Hello"),
        ];
        let tool_defs = vec![ToolDef {
            name: "bash",
            description: "Run a command".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }];

        log_llm_request(tmp.path(), &messages, &tool_defs, "test-model", 16000, 3);

        // Should have created <tmp>/datalog/llm/*.json — exactly one file.
        let log_dir = tmp.path().join("datalog").join("llm");
        let files: Vec<_> = std::fs::read_dir(&log_dir).unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().map_or(false, |ext| ext == "json"))
            .collect();
        assert_eq!(files.len(), 1, "Expected exactly 1 JSON log under {:?}, got {}", log_dir, files.len());

        // Verify JSON content
        let content = std::fs::read_to_string(&files[0]).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();

        assert_eq!(json["model"], "test-model");
        assert_eq!(json["context_window"], 16000);
        assert_eq!(json["step"], 3);
        assert_eq!(json["message_count"], 2);
        assert_eq!(json["tool_count"], 1);
        assert!(json["messages"].is_array());
        assert!(json["tools"].is_array());
        assert_eq!(json["tools"][0]["name"], "bash");
        // TempDir auto-cleans when dropped.
    }

    #[test]
    fn test_epoch_days_to_ymd() {
        // Unix epoch day 0 = 1970-01-01
        assert_eq!(epoch_days_to_ymd(0), (1970, 1, 1));
        // 1970-01-02
        assert_eq!(epoch_days_to_ymd(1), (1970, 1, 2));
        // 2000-01-01 = day 10957
        assert_eq!(epoch_days_to_ymd(10957), (2000, 1, 1));
        // 2024-03-01: days since epoch = 19783
        // Verify it's in the right year/month
        let (y, m, _d) = epoch_days_to_ymd(19783);
        assert_eq!(y, 2024);
        assert_eq!(m, 3);
    }
}
