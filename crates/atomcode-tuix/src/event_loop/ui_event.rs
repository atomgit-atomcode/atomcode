use std::path::PathBuf;
use std::time::Duration;

use atomcode_kernel::event::ToolBatchCall;
use atomcode_kernel::message::ImageContent;
use atomcode_kernel::message::SessionSnapshot;
use atomcode_kernel::stream::TokenUsage;
use atomcode_kernel::tool::ToolCall;

/// TUI-only presentation events. Runtime and live-session inputs are projected
/// into this type at the driver boundary; it is not an engine protocol.
#[derive(Debug, Clone)]
pub enum UiEvent {
    TextDelta(String),
    ReasoningDelta(String),
    ToolCallStreaming {
        name: String,
        hint: String,
        /// Char length of THIS streamed argument fragment — accumulated into
        /// `turn_output_chars` so the spinner's `↑ N tokens` indicator ticks
        /// while the model emits a long tool call (the fragment content itself
        /// isn't shown; only `hint`'s first chars are).
        arg_chars: usize,
    },
    ToolCallStarted {
        id: String,
        name: String,
        arguments: String,
    },
    ToolBatchStarted {
        batch_id: String,
        calls: Vec<ToolBatchCall>,
    },
    ToolBatchCompleted {
        batch_id: String,
        ok: usize,
        total: usize,
        elapsed_ms: u64,
    },
    ToolOutputChunk {
        call_id: String,
        chunk: String,
    },
    ToolCallResult {
        call_id: String,
        name: String,
        output: String,
        success: bool,
        duration: Duration,
    },
    ApprovalNeeded {
        tool_name: String,
        reason: String,
        call: ToolCall,
        snapshot: SessionSnapshot,
    },
    TokenUsage(TokenUsage),
    PhaseChange(UiAgentPhase),
    TurnComplete {
        duration: Duration,
        total_tokens: usize,
        turn_count: usize,
        tool_call_count: usize,
        stop_reason: UiTurnStopReason,
        snapshot: SessionSnapshot,
    },
    TurnCancelled {
        snapshot: SessionSnapshot,
    },
    ConversationTruncated {
        snapshot: SessionSnapshot,
        restored_prompt: String,
        target_n: usize,
        prompts_before: usize,
    },
    UndoFailed {
        requested: usize,
        available: usize,
    },
    MessagesSync {
        snapshot: SessionSnapshot,
    },
    ConversationRestored {
        restore_id: u64,
        snapshot: SessionSnapshot,
    },
    ConversationRestoreFailed {
        restore_id: u64,
        error: String,
    },
    Error {
        error: String,
        snapshot: SessionSnapshot,
    },
    Warning(String),
    RateLimited {
        reset_at_display: String,
        reset_label: String,
        secs_until_reset: Option<u64>,
        auto_resuming: bool,
        server_message: Option<String>,
    },
    HookWarningHint(String),
    RestorePendingImages {
        images: Vec<ImageContent>,
        markers: Vec<usize>,
    },
    VisionPreprocessSuccess {
        vl_model: String,
        char_count: usize,
    },
    SubAgentDispatchStart {
        tasks: Vec<SubAgentTaskInfo>,
    },
    SubAgentDispatchEnd,
    SubAgentTaskStarted {
        index: usize,
    },
    SubAgentTaskDone {
        index: usize,
        elapsed_ms: u64,
        turns: usize,
        summary: String,
    },
    SubAgentTaskFailed {
        index: usize,
        elapsed_ms: u64,
        turns: usize,
        reason: String,
    },
    GoalUpdate {
        active: bool,
        terminal: Option<atomcode_coding::GoalTerminal>,
        round: u32,
        elapsed_secs: u64,
        condition: String,
        last_reason: Option<String>,
    },
    LoopUpdate {
        active: bool,
        round: u32,
        elapsed_secs: u64,
        label: String,
        last_reason: Option<String>,
    },
    WorkingDirChanged(PathBuf),
    ProjectSwitched(PathBuf),
    RemoteSlashCommand(String),
    ContextStats {
        system_tokens: usize,
        sent_tokens: usize,
        dropped_tokens: usize,
        working_set_tokens: usize,
        total_messages: usize,
        tool_defs_tokens: usize,
        cold_zone_tokens: usize,
        ctx_window: usize,
        ctx_name: String,
        system_prompt: String,
    },
    UserEcho(String),
    PeerBusy(bool),
    ProviderChanged(String),
    ModeChanged {
        plan: bool,
        bypass: bool,
    },
    SessionRenamed {
        name: String,
    },
    Steered {
        count: usize,
        inputs: Vec<atomcode_kernel::event::SteeredInput>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum UiAgentPhase {
    Idle,
    Thinking,
    CallingTool(String),
    WaitingApproval,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiTurnStopReason {
    Natural,
    TurnLimit,
    StepLimit,
    Cancelled,
    Error,
}

impl UiTurnStopReason {
    pub fn as_tag(&self) -> &'static str {
        match self {
            Self::Natural => "natural",
            Self::TurnLimit => "turn_limit",
            Self::StepLimit => "step_limit",
            Self::Cancelled => "cancelled",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SubAgentTaskInfo {
    pub path: String,
    pub dedup_suffix: String,
}

#[cfg(test)]
mod tests {
    use super::UiEvent;

    #[test]
    fn tool_batch_payload_is_kernel_native() {
        let calls: Vec<atomcode_kernel::event::ToolBatchCall> =
            vec![atomcode_kernel::event::ToolBatchCall {
                id: "call-1".into(),
                name: "read_file".into(),
                arguments: "{}".into(),
                parallel_safe: true,
            }];

        let event = UiEvent::ToolBatchStarted {
            batch_id: "batch-1".into(),
            calls,
        };
        assert!(matches!(event, UiEvent::ToolBatchStarted { calls, .. } if calls.len() == 1));
    }
}
