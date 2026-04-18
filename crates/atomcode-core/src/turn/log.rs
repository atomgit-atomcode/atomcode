use crate::conversation::message::Message;
use crate::tool::{ToolCall, ToolDef};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Per-round LLM log files live under `<working_dir>/datalog/llm/`.
/// One file per LLM round-trip, containing both `request` and `response`
/// sections. Filename = timestamp. calls.log is a one-line-per-round index.
///
/// Split-file layout (prior design) produced two JSONs per round plus a CSV
/// entry per half — hard to read and review. One-file-per-round is both
/// AI-friendly (single JSON to grep/diff/feed back) and human-friendly.

/// Shared state: path of the in-progress request file. The caller of
/// `log_llm_request` writes the request JSON to a file and stashes the path
/// here; when `log_llm_response` runs (same process, sequential), it reads
/// the file back, merges the response, and writes the final JSON in place.
///
/// Single-threaded in atomcode (one agent turn at a time), so a plain Mutex
/// is enough — no race. If future parallelism is added, this needs to become
/// a per-task thread-local or passed explicitly.
fn pending_request_path() -> &'static Mutex<Option<PathBuf>> {
    static P: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(None))
}

/// Log the LLM request. Writes a JSON file containing the `request` section
/// under `<working_dir>/datalog/llm/<timestamp>.json` and stashes the path
/// for the subsequent `log_llm_response` call to append to.
///
/// If `enabled` is false, this function is a no-op.
pub fn log_llm_request(
    working_dir: &Path,
    messages: &[Message],
    tool_defs: &[ToolDef],
    model: &str,
    context_window: usize,
    step: usize,
    enabled: bool,
) {
    if !enabled { return; }
    use std::io::Write;

    let log_dir = working_dir.join("datalog").join("llm");
    let _ = std::fs::create_dir_all(&log_dir);

    let ts = timestamp();
    let path = log_dir.join(format!("{}.json", ts));

    let msgs_json = serde_json::to_value(messages).unwrap_or(serde_json::json!([]));
    let tools_json: Vec<serde_json::Value> = tool_defs.iter().map(|td| {
        serde_json::json!({
            "name": td.name,
            "description": td.description,
            "parameters": td.parameters,
        })
    }).collect();
    let total_tokens: usize = messages.iter().map(|m| m.estimate_tokens()).sum();

    let log = serde_json::json!({
        "timestamp": ts,
        "model": model,
        "context_window": context_window,
        "step": step,
        "request": {
            "message_count": messages.len(),
            "estimated_tokens": total_tokens,
            "tool_count": tool_defs.len(),
            "messages": msgs_json,
            "tools": tools_json,
        },
        // `response` key is inserted by log_llm_response.
    });

    let tmp = path.with_extension("json.tmp");
    if let Ok(mut f) = std::fs::File::create(&tmp) {
        let _ = f.write_all(serde_json::to_string_pretty(&log).unwrap_or_default().as_bytes());
        let _ = std::fs::rename(&tmp, &path);
    }

    // Remember for log_llm_response.
    if let Ok(mut guard) = pending_request_path().lock() {
        *guard = Some(path);
    }
}

/// Log the LLM response by reading the pending request file, adding a
/// `response` section, and writing the merged JSON back. Also appends a
/// one-line summary to `calls.log`.
///
/// If `enabled` is false, this function is a no-op.
pub fn log_llm_response(
    working_dir: &Path,
    text: &str,
    tool_calls: &[ToolCall],
    model: &str,
    step: usize,
    duration_ms: u64,
    enabled: bool,
) {
    if !enabled { return; }
    use std::io::Write;

    let log_dir = working_dir.join("datalog").join("llm");
    let _ = std::fs::create_dir_all(&log_dir);

    let path = pending_request_path()
        .lock().ok()
        .and_then(|mut g| g.take());

    let tools_json: Vec<serde_json::Value> = tool_calls.iter().map(|tc| {
        serde_json::json!({
            "id": tc.id,
            "name": tc.name,
            "arguments": tc.arguments,
        })
    }).collect();
    let response_value = serde_json::json!({
        "duration_ms": duration_ms,
        "text": text,
        "tool_calls": tools_json,
    });

    // Determine target path: prefer the stashed pending request so we
    // merge into the same file. Fallback: standalone orphan file, marked
    // so the reader knows the pairing was lost (shouldn't happen in normal
    // operation but we don't want to drop data on the floor).
    let (target_path, merged) = match path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()) {
        Some(existing) => {
            let mut val: serde_json::Value = serde_json::from_str(&existing)
                .unwrap_or_else(|_| serde_json::json!({}));
            if let Some(obj) = val.as_object_mut() {
                obj.insert("response".into(), response_value);
            }
            (path.unwrap(), val)
        }
        None => {
            let ts = timestamp();
            let orphan = log_dir.join(format!("{}_orphan_response.json", ts));
            let val = serde_json::json!({
                "timestamp": ts,
                "model": model,
                "step": step,
                "warning": "no matching request found for this response",
                "response": response_value,
            });
            (orphan, val)
        }
    };

    let tmp = target_path.with_extension("json.tmp");
    if let Ok(mut f) = std::fs::File::create(&tmp) {
        let _ = f.write_all(serde_json::to_string_pretty(&merged).unwrap_or_default().as_bytes());
        let _ = std::fs::rename(&tmp, &target_path);
    }

    // One-line summary to calls.log. Example:
    //   2026-04-14_12-50-54_123  glm-5  step=3  msgs=20/15000tok  →  4200ms  tools=2 [read_file, grep]
    let ts_for_log = merged.get("timestamp").and_then(|v| v.as_str()).unwrap_or("?").to_string();
    let msg_count = merged.pointer("/request/message_count").and_then(|v| v.as_u64()).unwrap_or(0);
    let est_tokens = merged.pointer("/request/estimated_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let tool_names: Vec<&str> = tool_calls.iter().map(|tc| tc.name.as_str()).collect();
    let tools_str = if tool_names.is_empty() {
        "text_only".to_string()
    } else {
        format!("[{}]", tool_names.join(", "))
    };
    let calls_path = log_dir.join("calls.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&calls_path) {
        let _ = writeln!(
            f,
            "{}  {}  step={}  msgs={}/{}tok  →  {}ms  tools={} {}",
            ts_for_log, model, step, msg_count, est_tokens, duration_ms, tool_calls.len(), tools_str,
        );
    }
}

fn timestamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let millis = now.subsec_millis();
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    let days = secs / 86400;
    let (y, mo, d) = epoch_days_to_ymd(days);
    format!("{:04}-{:02}-{:02}_{:02}-{:02}-{:02}_{:03}", y, mo, d, h, m, s, millis)
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
    use crate::tool::{ToolCall, ToolDef};

    #[test]
    fn test_request_response_merged_into_single_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let messages = vec![
            Message::new(Role::System, "You are helpful."),
            Message::new(Role::User, "Hello"),
        ];
        let tools = vec![ToolDef {
            name: "bash",
            description: "Run a command".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }];

        log_llm_request(tmp.path(), &messages, &tools, "test-model", 16000, 3, true);
        log_llm_response(
            tmp.path(),
            "hi back",
            &[ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            }],
            "test-model",
            3,
            123,
            true,
        );

        let log_dir = tmp.path().join("datalog").join("llm");
        let json_files: Vec<_> = std::fs::read_dir(&log_dir).unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().map_or(false, |ext| ext == "json"))
            .collect();
        assert_eq!(json_files.len(), 1, "expected one merged file, got {}", json_files.len());

        let content = std::fs::read_to_string(&json_files[0]).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["model"], "test-model");
        assert_eq!(v["request"]["message_count"], 2);
        assert_eq!(v["request"]["tool_count"], 1);
        assert_eq!(v["response"]["duration_ms"], 123);
        assert_eq!(v["response"]["text"], "hi back");
        assert_eq!(v["response"]["tool_calls"][0]["name"], "bash");

        // calls.log should have exactly one line for this round.
        let calls = std::fs::read_to_string(log_dir.join("calls.log")).unwrap();
        assert_eq!(calls.lines().count(), 1);
        assert!(calls.contains("test-model"));
        assert!(calls.contains("step=3"));
    }

    #[test]
    fn test_orphan_response_when_no_matching_request() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Wipe any stashed pending path from previous tests (single static).
        if let Ok(mut g) = pending_request_path().lock() { *g = None; }

        log_llm_response(tmp.path(), "bare text", &[], "solo-model", 7, 50, true);

        let log_dir = tmp.path().join("datalog").join("llm");
        let orphans: Vec<_> = std::fs::read_dir(&log_dir).unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.file_name().map_or(false, |n| n.to_string_lossy().contains("orphan")))
            .collect();
        assert_eq!(orphans.len(), 1);
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&orphans[0]).unwrap()).unwrap();
        assert!(v["warning"].as_str().unwrap().contains("no matching request"));
    }

    #[test]
    fn test_epoch_days_to_ymd() {
        assert_eq!(epoch_days_to_ymd(0), (1970, 1, 1));
        assert_eq!(epoch_days_to_ymd(1), (1970, 1, 2));
        assert_eq!(epoch_days_to_ymd(10957), (2000, 1, 1));
        let (y, m, _d) = epoch_days_to_ymd(19783);
        assert_eq!(y, 2024);
        assert_eq!(m, 3);
    }
}
