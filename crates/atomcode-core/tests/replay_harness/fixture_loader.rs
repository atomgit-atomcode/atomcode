//! Parse a session jsonl into a replayable form.
//!
//! A session jsonl contains one event per LLM request. Each event is a
//! snapshot `{messages: [...], ...}` of the conversation state JUST
//! BEFORE that LLM call. The LLM's response for event K materializes as
//! a new `Assistant` message that appears in event K+1 (or the final
//! conversation state emitted at session end).
//!
//! This loader extracts the RESPONSE SEQUENCE (what the LLM returned
//! over the whole session) by walking the final event's `messages` and
//! picking out Assistant messages in order.

use serde_json::Value;

/// Everything needed to drive a replay of a recorded session.
pub struct RecordedSession {
    /// The initial user message (messages[1], after the system prompt).
    pub initial_user_message: String,
    /// LLM responses in order. Each corresponds to one `chat_stream` call
    /// the framework will make during replay.
    pub responses: Vec<RecordedResponse>,
    /// ToolResults in order. Used by mock tools to return recorded
    /// outputs indexed by call_id (or iteration order if call_id absent).
    pub tool_results: Vec<RecordedToolResult>,
}

#[derive(Debug, Clone)]
pub struct RecordedResponse {
    /// Assistant-emitted text (may be empty if the turn was pure
    /// tool-call dispatch with no narration).
    pub text: String,
    /// Tool calls the LLM asked for in this response. Zero → the LLM
    /// decided to terminate this turn with just text.
    pub tool_calls: Vec<RecordedToolCall>,
    /// Mirrors `StreamEvent::Done.truncated` — true means finish_reason
    /// was "length" (hit max_tokens). For clean sessions this is false.
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct RecordedToolCall {
    pub id: String,
    pub name: String,
    /// JSON string of arguments, as the LLM serialized them.
    pub arguments: String,
}

#[derive(Debug, Clone)]
pub struct RecordedToolResult {
    pub call_id: String,
    pub output: String,
    pub success: bool,
}

/// Load a jsonl fixture and extract the replay sequence.
pub fn load(path: &std::path::Path) -> RecordedSession {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("fixture read {}: {}", path.display(), e));
    let last_event: Value = text.lines().filter(|l| !l.trim().is_empty()).last()
        .map(|l| serde_json::from_str(l).expect("last event is valid json"))
        .unwrap_or_else(|| panic!("fixture {} is empty", path.display()));

    let messages = last_event.get("messages").and_then(|v| v.as_array())
        .unwrap_or_else(|| panic!("fixture {}: missing messages array", path.display()));

    // Find the initial user message — first `Text` content whose role is
    // "User". Sessions start with System at index 0 and User at index 1
    // in the patterns observed so far, but don't hardcode the index —
    // walk until found.
    let initial_user_message = messages
        .iter()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("User"))
        .and_then(|m| m.get("content").and_then(|c| c.get("Text")).and_then(|t| t.as_str()))
        .map(|s| s.to_string())
        .unwrap_or_else(|| panic!("fixture {}: no User message found", path.display()));

    // Extract responses: every Assistant message in order. Each one
    // becomes a `RecordedResponse`.
    let mut responses = Vec::new();
    for m in messages {
        let Some(content) = m.get("content") else { continue };
        // `AssistantWithToolCalls { text: Option<String>, tool_calls: Vec<_> }`
        if let Some(a) = content.get("AssistantWithToolCalls") {
            let text = a.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let tool_calls = a.get("tool_calls").and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter().filter_map(|tc| {
                        Some(RecordedToolCall {
                            id: tc.get("id")?.as_str()?.to_string(),
                            name: tc.get("name")?.as_str()?.to_string(),
                            arguments: tc.get("arguments")?.as_str()?.to_string(),
                        })
                    }).collect()
                })
                .unwrap_or_default();
            responses.push(RecordedResponse { text, tool_calls, truncated: false });
        }
        // Pure Text Assistant message (final summary with no tool calls).
        else if m.get("role").and_then(|r| r.as_str()) == Some("Assistant") {
            if let Some(t) = content.get("Text").and_then(|v| v.as_str()) {
                responses.push(RecordedResponse {
                    text: t.to_string(),
                    tool_calls: Vec::new(),
                    truncated: false,
                });
            }
        }
    }

    // Extract tool results in order. Mock tools will dispatch on call_id
    // (or iteration order if call_id is empty in the fixture).
    let mut tool_results = Vec::new();
    for m in messages {
        if let Some(tr) = m.get("content").and_then(|c| c.get("ToolResult")) {
            let call_id = tr.get("call_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let output = tr.get("output").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let success = tr.get("success").and_then(|v| v.as_bool()).unwrap_or(false);
            tool_results.push(RecordedToolResult { call_id, output, success });
        }
    }

    RecordedSession {
        initial_user_message,
        responses,
        tool_results,
    }
}
