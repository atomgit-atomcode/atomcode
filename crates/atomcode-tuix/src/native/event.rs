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

use std::time::Duration;

use atomcode_core::agent::{AgentPhase, TurnStopReason};
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
}
