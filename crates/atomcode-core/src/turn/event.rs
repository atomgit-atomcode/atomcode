use std::path::PathBuf;
use std::time::Duration;

/// Live-session turn events broadcast to daemon, web, and TUI consumers.
#[derive(Debug, Clone)]
pub enum TurnEvent {
    /// LLM streaming text output
    TextDelta(String),
    /// LLM reasoning/thinking content (e.g., DeepSeek-R1, MiniMax-M2.7, o1-series).
    /// Emitted when the model produces thinking content separately from the final response.
    /// UI can optionally display this in verbose mode (Ctrl+O).
    ReasoningDelta(String),
    /// LLM has started emitting a tool call — name is known, arguments still streaming.
    /// Fires once per tool call, BEFORE the full args have arrived. Lets the UI surface
    /// the tool name immediately so users see "⠋ Write File…" instead of an opaque
    /// "Generating…" while the model spends seconds streaming args.
    ToolCallStreaming { name: String, hint: String },
    /// Tool call fully assembled, about to execute.
    /// `id` is the provider-supplied call id — pairs with the matching `ToolCallResult.call_id`.
    ToolCallStarted {
        id: String,
        name: String,
        arguments: String,
    },
    /// Multiple tool calls fan out from one assistant message. Emitted
    /// BEFORE the per-call `ToolCallStarted` events, only when the
    /// runner is about to dispatch ≥ 2 non-duplicate calls. Lets the
    /// UI render a single grouped block instead of N independent rows.
    /// Per-call `ToolCallStarted` events STILL fire — UIs that don't
    /// care about batches can ignore the batch events and render each
    /// call as today.
    ToolBatchStarted {
        batch_id: String,
        calls: Vec<ToolBatchCall>,
    },
    /// Closes the batch opened by `ToolBatchStarted`. UI uses it to
    /// finalize the group header with `ok / total / elapsed` summary.
    ToolBatchCompleted {
        batch_id: String,
        ok: usize,
        total: usize,
        elapsed_ms: u64,
    },
    /// Real-time output chunk from a running tool (e.g., bash command).
    /// Sent during tool execution before ToolCallResult.
    ToolOutputChunk { call_id: String, chunk: String },
    /// Tool call completed.
    /// `call_id` must equal the `id` emitted with the corresponding `ToolCallStarted`.
    ToolCallResult {
        call_id: String,
        name: String,
        output: String,
        success: bool,
        duration: Duration,
    },
    /// Non-fatal error during execution
    Error(String),
    /// Non-fatal advisory surfaced from a provider or other subsystem.
    /// TUI renders this as a one-line yellow banner; no turn failure.
    /// Currently used for "provider may be truncating input" detection.
    Warning(String),
    /// A 429 rate-limit PAUSE — driver renders a non-error pause line with the
    /// reset time. Empty strings / None when no usage data was available.
    RateLimited {
        reset_at_display: String,
        reset_label: String,
        secs_until_reset: Option<u64>,
        /// `true` = WaitAndRetry (kernel will sleep then retry automatically);
        /// `false` = Pause (kernel stopped the turn, user must act).
        auto_resuming: bool,
        /// The provider's own 429 message (extracted, no `HTTP …:` prefix) for the
        /// generic (non-CodingPlan) pause. `None` otherwise. See `AgentEvent::RateLimited`.
        server_message: Option<String>,
    },
    /// Token usage update
    TokenUsage {
        prompt_tokens: usize,
        completion_tokens: usize,
        total_tokens: usize,
        cached_tokens: usize,
    },
    /// Context budget stats for logging
    ContextStats {
        system_tokens: usize,
        sent_tokens: usize,
        dropped_tokens: usize,
        working_set_tokens: usize,
        total_messages: usize,
    },
    /// Emitted when a tool mutated `ctx.working_dir` (e.g. `change_dir`
    /// or a `bash` call starting with `cd`). Lets the TUI footer track
    /// the current cwd without polling the shared `Arc<RwLock<PathBuf>>`.
    WorkingDirChanged(PathBuf),
    /// A tool requires user approval. Carries a snapshot of
    /// conversation state so the TUI can persist mid-turn session
    /// state (e.g. for `/bg`). The approval itself is
    /// handled by `PermissionDecider`; this event is purely
    /// informational.
    ApprovalRequested {
        tool_name: String,
        reason: String,
        call: crate::tool::ToolCall,
        snapshot: crate::conversation::ConversationSnapshot,
    },
    /// 任一端批准/拒绝了工具审批，广播给所有视图同步卡片状态。
    ApprovalResolved { call_id: String, decision: String },
    /// A tool (currently `request_user_input`) raised a structured question a view must
    /// render and answer through the native runtime response seam. `request_id` is
    /// the correlation id passed to `CodingRuntimeHandle::respond`.
    ///
    /// `options` are raw `{ label, description? }` objects kept as opaque
    /// `serde_json::Value` so core carries no capabilities coupling.
    UserInputRequested {
        request_id: u64,
        header: String,
        question: String,
        mode: String,
        options: Vec<serde_json::Value>,
    },
}

/// One call inside a `ToolBatchStarted` payload. Carries everything the UI
/// needs to render a child row in the group block (name + abbreviated detail).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolBatchCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
    /// True if this call may run concurrently (read-only); false → serialized
    /// behind the write-lock. Drives the UI's honest "in parallel" label.
    pub parallel_safe: bool,
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Shape test for the `request_user_input` webui event: fields round-trip
    /// through construction/pattern-match and `options` stays opaque JSON so
    /// core needs no capabilities coupling. `request_id` pairs with the
    /// native runtime's `DriverCommand::Respond { id, .. }`.
    #[test]
    fn user_input_requested_carries_opaque_options() {
        let ev = TurnEvent::UserInputRequested {
            request_id: 42,
            header: "Choose".into(),
            question: "Which one?".into(),
            mode: "select".into(),
            options: vec![
                serde_json::json!({ "label": "A", "description": "first" }),
                serde_json::json!({ "label": "B" }),
            ],
        };
        match ev {
            TurnEvent::UserInputRequested {
                request_id,
                header,
                question,
                mode,
                options,
            } => {
                assert_eq!(request_id, 42);
                assert_eq!(header, "Choose");
                assert_eq!(question, "Which one?");
                assert_eq!(mode, "select");
                assert_eq!(options.len(), 2);
                assert_eq!(options[0]["label"], serde_json::json!("A"));
                assert_eq!(options[0]["description"], serde_json::json!("first"));
                assert_eq!(options[1]["label"], serde_json::json!("B"));
                assert!(options[1].get("description").is_none());
            }
            _ => panic!("expected UserInputRequested"),
        }
    }
}
