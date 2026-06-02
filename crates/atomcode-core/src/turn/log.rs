use crate::conversation::message::Message;
use crate::tool::{ToolCall, ToolDef};
use std::path::{Path, PathBuf};

/// Per-round LLM log files live under `<datalog_dir>/llm/` where
/// `<datalog_dir>` is the per-project directory resolved by
/// `DatalogWriter::resolve_log_dir` (typically
/// `~/.atomcode/datalog/<project-slug>/`). The caller is responsible for
/// passing in the resolved dir — see `runner.rs`. This keeps the JSONL dump
/// in lockstep with the markdown writer's `[datalog].dir` config; the prior
/// hard-coded `<working_dir>/datalog/llm/` would silently ignore the user's
/// `dir` override.
///
/// One file per LLM round-trip, containing both `request` and `response`
/// sections. Filename = timestamp. calls.log is a one-line-per-round index.
///
/// Split-file layout (prior design) produced two JSONs per round plus a CSV
/// entry per half — hard to read and review. One-file-per-round is both
/// AI-friendly (single JSON to grep/diff/feed back) and human-friendly.
///
/// Request / response pairing: `log_llm_request` returns the path of the
/// JSON file it wrote; the caller holds that `PathBuf` locally and passes
/// it to `log_llm_response` so the response section merges into the SAME
/// file. No process-wide shared state — safe for concurrent `chat_stream`
/// handlers in the daemon (prior design used a `static Mutex<Option<PathBuf>>`
/// that bled across sessions).

/// Log the LLM request. Writes a JSON file containing the `request` section
/// under `<datalog_dir>/llm/<timestamp>.json`.
///
/// Returns the path so the caller can pass it to `log_llm_response` for
/// in-place merge. Returns `None` if `enabled` is false or the write failed.
pub fn log_llm_request(
    datalog_dir: &Path,
    messages: &[Message],
    tool_defs: &[ToolDef],
    model: &str,
    context_window: usize,
    step: usize,
    session_id: &str,
    enabled: bool,
) -> Option<PathBuf> {
    if !enabled {
        return None;
    }
    use std::io::Write;

    let log_dir = datalog_dir.join("llm");
    let _ = std::fs::create_dir_all(&log_dir);

    let ts = timestamp();
    let path = log_dir.join(format!("{}.json", ts));

    let msgs_json = serde_json::to_value(messages).unwrap_or(serde_json::json!([]));
    let tools_json: Vec<serde_json::Value> = tool_defs
        .iter()
        .map(|td| {
            serde_json::json!({
                "name": td.name,
                "description": td.description,
                "parameters": td.parameters,
            })
        })
        .collect();
    let total_tokens: usize = messages.iter().map(|m| m.estimate_tokens()).sum();

    let log = serde_json::json!({
        "timestamp": ts,
        "session_id": session_id,
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
    match std::fs::File::create(&tmp) {
        Ok(mut f) => {
            if f.write_all(
                serde_json::to_string_pretty(&log)
                    .unwrap_or_default()
                    .as_bytes(),
            )
            .is_err()
            {
                return None;
            }
            if std::fs::rename(&tmp, &path).is_err() {
                return None;
            }
            Some(path)
        }
        Err(_) => None,
    }
}

/// Log the LLM response by merging it into the request file identified by
/// `pending_request`. If `pending_request` is `None` (no prior request, or
/// request write failed), writes a standalone `*_orphan_response.json`
/// marked with a warning. Also appends a one-line summary to `calls.log`.
///
/// If `enabled` is false, this function is a no-op.
pub fn log_llm_response(
    datalog_dir: &Path,
    pending_request: Option<PathBuf>,
    text: &str,
    tool_calls: &[ToolCall],
    reasoning: &str,
    model: &str,
    step: usize,
    duration_ms: u64,
    enabled: bool,
) {
    if !enabled {
        return;
    }
    use std::io::Write;

    let log_dir = datalog_dir.join("llm");
    let _ = std::fs::create_dir_all(&log_dir);

    let path = pending_request;

    let tools_json: Vec<serde_json::Value> = tool_calls
        .iter()
        .map(|tc| {
            serde_json::json!({
                "id": tc.id,
                "name": tc.name,
                "arguments": tc.arguments,
            })
        })
        .collect();
    // Record reasoning_content emitted by thinking models (Moonshot Kimi,
    // DeepSeek-R1, MiniMax-M2, etc.). Without this, bugs like issue #165
    // ("reasoning_content missing in assistant tool call message") are
    // impossible to diagnose from datalog alone.
    let response_value = serde_json::json!({
        "duration_ms": duration_ms,
        "text": text,
        "reasoning_content": reasoning,
        "tool_calls": tools_json,
    });

    // Determine target path: prefer the stashed pending request so we
    // merge into the same file. Fallback: standalone orphan file, marked
    // so the reader knows the pairing was lost (shouldn't happen in normal
    // operation but we don't want to drop data on the floor).
    let (target_path, merged) = match path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()) {
        Some(existing) => {
            let mut val: serde_json::Value =
                serde_json::from_str(&existing).unwrap_or_else(|_| serde_json::json!({}));
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
        let _ = f.write_all(
            serde_json::to_string_pretty(&merged)
                .unwrap_or_default()
                .as_bytes(),
        );
        let _ = std::fs::rename(&tmp, &target_path);
    }

    // One-line summary to calls.log. Example:
    //   2026-04-14_12-50-54_123  glm-5  step=3  msgs=20/15000tok  →  4200ms  tools=2 [read_file, grep]
    let ts_for_log = merged
        .get("timestamp")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let msg_count = merged
        .pointer("/request/message_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let est_tokens = merged
        .pointer("/request/estimated_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let tool_names: Vec<&str> = tool_calls.iter().map(|tc| tc.name.as_str()).collect();
    let tools_str = if tool_names.is_empty() {
        "text_only".to_string()
    } else {
        format!("[{}]", tool_names.join(", "))
    };
    let calls_path = log_dir.join("calls.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&calls_path)
    {
        let _ = writeln!(
            f,
            "{}  {}  step={}  msgs={}/{}tok  →  {}ms  tools={} {}",
            ts_for_log,
            model,
            step,
            msg_count,
            est_tokens,
            duration_ms,
            tool_calls.len(),
            tools_str,
        );
    }
}

/// Filename timestamp: `YYYY-MM-DD_HH-MM-SS_sss` (UTC).
/// Format preserved bit-for-bit from the prior hand-rolled implementation.
fn timestamp() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%d_%H-%M-%S_%3f")
        .to_string()
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

        let pending =
            log_llm_request(tmp.path(), &messages, &tools, "test-model", 16000, 3, "sess-test", true);
        assert!(pending.is_some(), "request log should return its path");
        log_llm_response(
            tmp.path(),
            pending,
            "hi back",
            &[ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            }],
            "",
            "test-model",
            3,
            123,
            true,
        );

        let log_dir = tmp.path().join("llm");
        let json_files: Vec<_> = std::fs::read_dir(&log_dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().map_or(false, |ext| ext == "json"))
            .collect();
        assert_eq!(
            json_files.len(),
            1,
            "expected one merged file, got {}",
            json_files.len()
        );

        let content = std::fs::read_to_string(&json_files[0]).unwrap();
        let v: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(v["model"], "test-model");
        assert_eq!(v["session_id"], "sess-test");
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

        log_llm_response(
            tmp.path(),
            None,
            "bare text",
            &[],
            "",
            "solo-model",
            7,
            50,
            true,
        );

        let log_dir = tmp.path().join("llm");
        let orphans: Vec<_> = std::fs::read_dir(&log_dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .map_or(false, |n| n.to_string_lossy().contains("orphan"))
            })
            .collect();
        assert_eq!(orphans.len(), 1);
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&orphans[0]).unwrap()).unwrap();
        assert!(v["warning"]
            .as_str()
            .unwrap()
            .contains("no matching request"));
    }

    /// Regression: prior design used a process-wide `static Mutex<Option<PathBuf>>`
    /// to pair request/response. Two concurrent sessions would clobber each
    /// other's pending path — one session's response would merge into the
    /// OTHER session's request file. This test interleaves two sessions
    /// and asserts each response lands in its own request file.
    #[test]
    fn test_concurrent_sessions_do_not_mix_request_response() {
        let tmp_a = tempfile::TempDir::new().unwrap();
        let tmp_b = tempfile::TempDir::new().unwrap();
        let msgs_a = vec![Message::new(Role::User, "alpha")];
        let msgs_b = vec![Message::new(Role::User, "beta")];

        // Interleaved: A-req, B-req, A-resp, B-resp — the exact pattern
        // that would corrupt results under the old static-Mutex design.
        let pending_a =
            log_llm_request(tmp_a.path(), &msgs_a, &[], "model-a", 16000, 0, "sess-a", true);
        let pending_b =
            log_llm_request(tmp_b.path(), &msgs_b, &[], "model-b", 16000, 0, "sess-b", true);
        log_llm_response(
            tmp_a.path(),
            pending_a,
            "reply-A",
            &[],
            "",
            "model-a",
            0,
            10,
            true,
        );
        log_llm_response(
            tmp_b.path(),
            pending_b,
            "reply-B",
            &[],
            "",
            "model-b",
            0,
            20,
            true,
        );

        let read_merged = |dir: &Path| -> serde_json::Value {
            let log_dir = dir.join("llm");
            let files: Vec<_> = std::fs::read_dir(&log_dir)
                .unwrap()
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| {
                    p.extension().map_or(false, |ext| ext == "json")
                        && !p
                            .file_name()
                            .map_or(false, |n| n.to_string_lossy().contains("orphan"))
                })
                .collect();
            assert_eq!(files.len(), 1, "each session gets its own merged file");
            serde_json::from_str(&std::fs::read_to_string(&files[0]).unwrap()).unwrap()
        };

        let a = read_merged(tmp_a.path());
        let b = read_merged(tmp_b.path());
        assert_eq!(a["model"], "model-a");
        assert_eq!(a["response"]["text"], "reply-A");
        assert_eq!(b["model"], "model-b");
        assert_eq!(b["response"]["text"], "reply-B");
    }

    #[test]
    fn timestamp_format_is_stable() {
        // Guard: filename pattern YYYY-MM-DD_HH-MM-SS_sss. Callers of
        // log_llm_request rely on this shape (daemon / tests glob `*.json`
        // and orphan_response detection splits on this layout).
        let ts = timestamp();
        assert_eq!(ts.len(), 23, "expected 23-char timestamp, got {:?}", ts);
        assert_eq!(&ts[4..5], "-");
        assert_eq!(&ts[7..8], "-");
        assert_eq!(&ts[10..11], "_");
        assert_eq!(&ts[13..14], "-");
        assert_eq!(&ts[16..17], "-");
        assert_eq!(&ts[19..20], "_");
        assert!(ts[..4].chars().all(|c| c.is_ascii_digit()));
        assert!(ts[20..].chars().all(|c| c.is_ascii_digit()));
    }
}
