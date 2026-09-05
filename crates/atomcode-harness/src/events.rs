//! The turn's extension points, as typed events.
//!
//! ```text
//! turn/start
//!   loop:
//!     assemble system prompt + tool schemas from the registries
//!     agent/request     (waterfall)  wrap, rewrite, cache, or replace the model call
//!       assistant/chunk (emit)       live stream, for whoever renders
//!     assistant/message (emit)
//!     for each tool call:
//!       tools/execute   (waterfall)  gate, repair args, run, transform the result
//!       tool/result     (emit)
//!     no tool calls -> stop
//! turn/end
//! ```
//!
//! The loop dispatches these; it does not know who listens. Approval, argument
//! repair, output capping, tracing and wire logging are all listeners — and
//! removing their rows from the config removes their behaviour, with no branch
//! left behind in the loop.

use atomcode_kernel::message::Message;
use atomcode_kernel::provider::ChatOptions;
use atomcode_kernel::stream::TokenUsage;
use atomcode_kernel::tool::{ToolCall, ToolDef, ToolResult};
use atomcode_plexus::plexus_event;

use crate::seams::{StopReason, TurnOutcome};
use crate::session::LoggedEvent;

/// Everything that goes on the wire for one model call. A listener may rewrite
/// any of it before delegating.
#[derive(Clone, Debug)]
pub struct ModelRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
    pub options: ChatOptions,
    /// 1-based turn number within the session.
    pub turn: u64,
    /// 1-based index of this call within the turn.
    pub round: u32,
}

#[derive(Clone, Debug, Default)]
pub struct ModelResponse {
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<TokenUsage>,
}

/// One tool call on its way to execution. `call` is `&mut` in the waterfall, so
/// a repair listener fixes arguments *before* a policy listener inspects them —
/// registration order is the contract.
#[derive(Clone, Debug)]
pub struct ToolExec {
    pub call: ToolCall,
    pub turn: u64,
    pub round: u32,
    /// Set by a listener that has already authorized this call, so a gate
    /// further down does not ask again.
    ///
    /// This is how cooperating waterfall listeners settle one decision between
    /// them: an upstream rule marks the shared object and delegates, rather than
    /// short-circuiting and taking the downstream transforms with it.
    pub pre_approved: bool,
}

/// What one step was given, before it becomes model-visible history.
#[derive(Clone, Debug)]
pub struct StepDecision {
    pub turn: u64,
    /// 1-based index of this step within the turn.
    pub step: u32,
    /// The message this step answers, if the inbox had one.
    pub message: Option<String>,
    /// Context claimed alongside it.
    pub injections: Vec<(String, crate::session::InjectionOrigin)>,
    /// Set by a listener to refuse this input. The turn ends without a step.
    pub rejected: Option<String>,
    /// Start a fresh model message series rather than appending to the existing
    /// prefix. A listener that invalidates history (a compactor, a model swap)
    /// sets it so the request header records the break.
    pub starts_request_series: bool,
}

impl StepDecision {
    /// Nothing to say to the model. A first claim that ends up empty closes the
    /// turn rather than sending a request with no new input.
    pub fn is_empty(&self) -> bool {
        self.message.is_none() && self.injections.is_empty()
    }
}

/// A newly created agent.
#[derive(Clone, Debug)]
pub struct AgentInfo {
    pub id: crate::agent::AgentId,
}

/// Where a turn stands, offered to whoever decides whether it continues.
#[derive(Clone, Debug)]
pub struct TurnProgress {
    pub turn: u64,
    /// Steps completed so far this turn.
    pub rounds: u32,
    pub tool_calls: u32,
    /// Prompt tokens the last request reported, `0` when unknown.
    pub used_tokens: u32,
    /// Wall-clock since the turn opened.
    pub elapsed: std::time::Duration,
}

#[derive(Clone, Debug)]
pub struct TurnStarted {
    pub prompt: String,
    pub turn: u64,
}

#[derive(Clone, Debug)]
pub struct Chunk {
    pub text: String,
    pub reasoning: bool,
}

plexus_event!(
    /// Every fact appended to the session log, broadcast as it commits.
    /// Persistence, telemetry, transcripts and UIs are all listeners here —
    /// none of them is wired into the loop.
    SessionEventCommitted, "session/event", Emit, LoggedEvent
);
plexus_event!(TurnStart, "turn/start", Emit, TurnStarted);
plexus_event!(TurnEnd, "turn/end", Emit, TurnOutcome);
plexus_event!(AssistantChunk, "assistant/chunk", Emit, Chunk);
plexus_event!(AssistantMessage, "assistant/message", Emit, Message);
plexus_event!(ToolResultEvent, "tool/result", Emit, ToolResult);

plexus_event!(
    /// Around the model call. Delegating runs the provider; returning without
    /// delegating replaces the response entirely (a cache hit, a canned reply, a
    /// replay fixture).
    AgentRequest, "agent/request", Waterfall, ModelRequest => Result<ModelResponse, String>
);

plexus_event!(
    /// Decides what the model sees this step.
    ///
    /// Listeners receive the input just claimed from the agent's inbox and may
    /// rewrite it, add to it, or reject it outright. A rejection on the first
    /// claim closes the turn with no step at all — and the attempt is still
    /// logged, because a turn that was refused is a fact about the session.
    PreStep, "agent/pre-step", Waterfall, StepDecision => StepDecision
);

plexus_event!(
    /// An agent was created. A UI, a scheduler or a supervisor listens here
    /// rather than being told by whoever created it.
    AgentCreated, "agent/created", Emit, AgentInfo
);

plexus_event!(
    /// Asked at the end of every round: should the turn stop here?
    ///
    /// The first listener with an opinion wins. This is how a round budget, a
    /// wall-clock deadline or a cost ceiling becomes a row instead of a branch
    /// in the loop — remove every listener and only the loop's own runaway fuse
    /// remains.
    TurnStopping, "agent/turn-stopping", Serial, TurnProgress => StopReason
);

plexus_event!(
    /// Around one tool execution. Denying is *returning a result* rather than
    /// raising: the model must see why its call did not run, and history must
    /// stay pairable.
    ToolsExecute, "tools/execute", Waterfall, ToolExec => ToolResult
);
