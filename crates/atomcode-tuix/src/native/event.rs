//! `UiEvent` — the event vocabulary the TUI renders, owned by the driver.
//!
//! The native runtime adapter (B2) drives a kernel `CodingRuntime` and maps its
//! lean `kernel::AgentEvent`s into this RICH vocab — recovering tool name/duration,
//! synthesizing turn stats / context stats, surfacing orchestration (goal / vision /
//! background) and the live-sync variants. This is the type that replaces
//! `core::agent::AgentEvent` on tuix's fan channel, decoupling the renderer from
//! core's agent module and letting the bridge crate be deleted.
//!
//! Variants are added as the mapping (and, later, the lifecycle + orchestration
//! handling) needs them — kept shape-compatible with the legacy `core::AgentEvent`
//! so the 38-arm `handle_agent_event` re-point is mechanical.

use std::path::PathBuf;
use std::time::Duration;

use atomcode_core::agent::{AgentPhase, SubAgentTaskInfo, TurnStopReason};
use atomcode_core::conversation::message::ImagePart;
use atomcode_core::conversation::ConversationSnapshot;
use atomcode_core::tool::ToolCall;

/// Events sent FROM the runtime adapter TO the UI.
///
/// `dead_code` is allowed while this type is being built: its consumer is the
/// 38-arm `handle_agent_event` renderer, which is re-pointed onto `UiEvent` in a
/// later step (B2 ②). Until then the carried fields have no reader. The allow is
/// removed once the renderer consumes the full payload.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum UiEvent {
    /// LLM text delta (streaming).
    TextDelta(String),
    /// LLM reasoning/thinking content (streamed separately from the final reply).
    ReasoningDelta(String),
    /// LLM has started emitting a tool call — only the name is known so far,
    /// arguments still streaming. `hint` is a truncated argument preview.
    ToolCallStreaming { name: String, hint: String },
    /// A tool call is about to execute (for display). `id` pairs with
    /// `ToolCallResult.call_id`.
    ToolCallStarted {
        id: String,
        name: String,
        arguments: String,
    },
    /// Real-time output chunk from a running tool (e.g. bash), before the result.
    ToolOutputChunk { call_id: String, chunk: String },
    /// A tool call completed. `name`/`duration` are recovered from the matching
    /// `ToolCallStarted` (the kernel result carries neither).
    ToolCallResult {
        call_id: String,
        name: String,
        output: String,
        success: bool,
        duration: Duration,
    },
    /// The agent's current phase changed.
    PhaseChange(AgentPhase),
    /// Multiple tool calls fan out from one assistant message (grouped display).
    ToolBatchStarted {
        batch_id: String,
        calls: Vec<atomcode_core::turn::event::ToolBatchCall>,
    },
    /// Closes the batch opened by `ToolBatchStarted`.
    ToolBatchCompleted {
        batch_id: String,
        ok: usize,
        total: usize,
        elapsed_ms: u64,
    },
    /// Token usage update for the last LLM call.
    TokenUsage(atomcode_core::stream::TokenUsage),
    /// Context-budget stats — synthesized after each `Usage` (the kernel doesn't
    /// break tokens down the way v1's ctx strategy did; most fields are zero).
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
    /// Non-fatal advisory; does not abort the turn.
    Warning(String),
    /// A failure. `snapshot` lets the TUI persist mid-turn state.
    Error {
        error: String,
        snapshot: ConversationSnapshot,
    },
    /// Turn completed. Stats (duration/tokens/turn+tool counts) are synthesized
    /// from the per-turn `TurnStats`; the kernel `TurnComplete` carries only a reason.
    TurnComplete {
        duration: Duration,
        total_tokens: usize,
        turn_count: usize,
        tool_call_count: usize,
        stop_reason: TurnStopReason,
        snapshot: ConversationSnapshot,
    },
    /// Turn cancelled before completion; carries the cleaned conversation state.
    TurnCancelled { snapshot: ConversationSnapshot },
    /// Waiting for user approval of a tool call. The driver answers the kernel's
    /// approval round-trip after the user decides.
    ApprovalNeeded {
        tool_name: String,
        reason: String,
        call: ToolCall,
        snapshot: ConversationSnapshot,
    },

    // ── Orchestration (goal / vision / background / undo / sub-agent) ──
    /// `/undo` rolled the conversation back. Carries the truncated history (persist
    /// + replay), the restored prompt text, and turn numbers for the confirmation.
    ConversationTruncated {
        snapshot: ConversationSnapshot,
        restored_prompt: String,
        target_n: usize,
        prompts_before: usize,
    },
    /// `/undo` out of range: `requested` turn vs `available` real prompts.
    UndoFailed { requested: usize, available: usize },
    /// Reply to a SyncMessages request — a snapshot for the TUI to sync before
    /// backgrounding a mid-turn session.
    MessagesSync { snapshot: ConversationSnapshot },
    /// A UserPromptSubmit hook failed on an environment issue (not an explicit
    /// block); the turn continues but the status bar surfaces the error.
    HookWarningHint(String),
    /// VL preprocessing failed; return the user's pending images for re-attach.
    RestorePendingImages {
        images: Vec<ImagePart>,
        markers: Vec<usize>,
    },
    /// VL preprocessing succeeded — a one-line success notice (the description rides
    /// into history for the main model, not the UI).
    VisionPreprocessSuccess { vl_key: String, char_count: usize },
    /// Sub-agent batch began (ordered child descriptors).
    SubAgentDispatchStart { tasks: Vec<SubAgentTaskInfo> },
    /// Sub-agent batch ended (all settled / pool returned).
    SubAgentDispatchEnd,
    /// One sub-agent was claimed from the pool and is now running.
    SubAgentTaskStarted { index: usize },
    /// Sub-agent finished successfully.
    SubAgentTaskDone {
        index: usize,
        elapsed_ms: u64,
        turns: usize,
        summary: String,
    },
    /// Sub-agent failed (error / timeout / no-edit).
    SubAgentTaskFailed {
        index: usize,
        elapsed_ms: u64,
        turns: usize,
        reason: String,
    },
    /// Goal evaluator update — the TUI shows progress.
    GoalUpdate {
        active: bool,
        round: u32,
        elapsed_secs: u64,
        condition: String,
        last_reason: Option<String>,
    },
    /// `/background` task finished.
    BackgroundComplete {
        summary: String,
        files_edited: Vec<String>,
        turns: usize,
        success: bool,
    },
    /// The agent's own `cd`/`change_dir` tool changed the working directory
    /// (conversation preserved).
    WorkingDirChanged(PathBuf),

    // ── Live-sync (another view — webui — drives this session) ──
    /// Another client switched the project working directory (switch + fresh session).
    ProjectSwitched(PathBuf),
    /// A user message echoed from another view, to render the user bubble locally.
    UserEcho(String),
    /// The synced peer is mid-turn (true = running) — disable/restore local input.
    PeerBusy(bool),
    /// Another view switched the model.
    ProviderChanged(String),
    /// Another view created a new session — follow it.
    SessionSwitched(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The orchestration + live-sync variants are constructible with the expected
    /// shapes and `Clone` (the renderer needs owned events on the fan channel).
    #[test]
    fn orchestration_and_live_sync_variants_construct_and_clone() {
        let events = vec![
            UiEvent::GoalUpdate {
                active: true,
                round: 2,
                elapsed_secs: 9,
                condition: "tests pass".into(),
                last_reason: Some("still failing".into()),
            },
            UiEvent::BackgroundComplete {
                summary: "done".into(),
                files_edited: vec!["a.rs".into()],
                turns: 3,
                success: true,
            },
            UiEvent::UndoFailed { requested: 5, available: 2 },
            UiEvent::UserEcho("hi from webui".into()),
            UiEvent::PeerBusy(true),
            UiEvent::SessionSwitched("sess-1".into()),
        ];
        for ev in &events {
            // Exercises Clone + Debug (derived) so the full mirror stays renderable.
            let _ = format!("{:?}", ev.clone());
        }
        assert!(matches!(events[2], UiEvent::UndoFailed { requested: 5, available: 2 }));
    }
}
