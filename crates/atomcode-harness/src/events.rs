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
use crate::session::Committed;

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
    /// This round answers a nudge the harness wrote rather than anything the
    /// person sent. A recovery policy reads it to know that "no content" is an
    /// answer here — the model has nothing to add — and not a provider failing.
    pub answering_a_nudge: bool,
}

/// Why a model request failed, with the structure a recovery policy needs.
///
/// The provider already classifies its own failures — `retryable`, an HTTP
/// status, a structured code, a real `Retry-After`. Flattening that to a string
/// at the seam would force every recovery plugin to re-derive it by matching on
/// message text, which is exactly how a rate-limit handler ends up mistaking a
/// Why a rate-limited turn stopped rather than failed, and when to come back.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RateLimitPause {
    pub reset_at_display: String,
    pub reset_label: String,
    #[serde(default)]
    pub secs_until_reset: Option<u64>,
    /// The provider's own reason, for a pause that is not a plan window.
    #[serde(default)]
    pub server_message: Option<String>,
}

/// 400 for a 429.
#[derive(Clone, Debug)]
pub struct RequestError {
    pub message: String,
    /// The provider's own verdict on whether trying again could help.
    pub retryable: bool,
    pub http_status: Option<u16>,
    /// Structured provider code (`context_length_exceeded`, …), when given.
    pub code: Option<String>,
    /// From a real `Retry-After` header. Authoritative — preferred over any
    /// backoff a client would otherwise guess.
    pub retry_after: Option<std::time::Duration>,
    /// The history no longer fits. Compacting and retrying is the only thing
    /// that helps, and retrying without compacting is guaranteed to fail again.
    pub context_overflow: bool,
    /// The provider answered with no content and no tool calls. Distinct from
    /// an error: nothing failed, the turn simply cannot proceed on it.
    pub empty_response: bool,
    /// A rate-limit policy decided this 429 is a pause, not a failure: the turn
    /// ends cleanly as rate-limited and the person is told when it resets.
    pub rate_limit_pause: Option<RateLimitPause>,
    /// What the stream had already produced when it broke.
    ///
    /// A stream that fails after emitting real work is not the same failure as
    /// one that never opened: the tokens were paid for, the tool calls may
    /// already describe work worth keeping, and discarding them means the retry
    /// starts from nothing. Carried here so a recovery policy can preserve it.
    pub partial: Option<Box<ModelResponse>>,
}

impl RequestError {
    pub fn message(text: impl Into<String>) -> Self {
        Self {
            message: text.into(),
            retryable: false,
            http_status: None,
            code: None,
            retry_after: None,
            context_overflow: false,
            empty_response: false,
            rate_limit_pause: None,
            partial: None,
        }
    }

    /// Carry the provider's own classification across the seam intact.
    pub fn from_provider(error: &atomcode_kernel::stream::ProviderError) -> Self {
        Self {
            message: error.message.clone(),
            retryable: error.retryable,
            http_status: error.http_status,
            code: error.code.clone(),
            retry_after: error.retry_after_secs.map(std::time::Duration::from_secs),
            context_overflow: error.is_context_overflow(),
            empty_response: false,
            rate_limit_pause: None,
            partial: None,
        }
    }

    /// Attach what the stream had produced before it broke.
    pub fn with_partial(mut self, partial: ModelResponse) -> Self {
        if !partial.text.is_empty()
            || !partial.tool_calls.is_empty()
            || !partial.reasoning.is_empty()
        {
            self.partial = Some(Box::new(partial));
        }
        self
    }

    pub fn empty() -> Self {
        Self {
            empty_response: true,
            retryable: true,
            ..Self::message("the provider returned no content and no tool calls")
        }
    }

    pub fn is_rate_limited(&self) -> bool {
        self.http_status == Some(429)
    }

    /// Auth and bad-request failures never improve on a retry; burning a budget
    /// on them costs time and, on a paid endpoint, money.
    pub fn is_fatal(&self) -> bool {
        matches!(self.http_status, Some(400 | 401 | 403 | 404)) && !self.context_overflow
    }
}

impl std::fmt::Display for RequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

#[derive(Clone, Debug, Default)]
pub struct ModelResponse {
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Option<TokenUsage>,
    /// The provider cut the response at its output-token limit
    /// (`finish_reason=length`). What was produced is real but unfinished, and
    /// anything still streaming when it was cut — a tool call's arguments in
    /// particular — is incomplete and unsafe to act on.
    pub truncated: bool,
}

/// A round's tool calls, on their way to being scheduled.
#[derive(Clone, Debug)]
pub struct ToolBatch {
    pub calls: Vec<ToolCall>,
    pub turn: u64,
    pub step: u32,
    /// Where the tools run. Carried here so a scheduling listener is
    /// self-contained instead of reaching back into the loop.
    pub working_dir: std::path::PathBuf,
    /// The agent's token, so a listener can skip calls that had not started
    /// when the turn was cancelled.
    pub cancel: tokio_util::sync::CancellationToken,
}

/// One tool call on its way to execution. `call` is `&mut` in the waterfall, so
/// a repair listener fixes arguments *before* a policy listener inspects them —
/// registration order is the contract.
#[derive(Clone, Debug)]
pub struct ToolExec {
    pub call: ToolCall,
    pub turn: u64,
    pub round: u32,
    /// How far this call has been authorized, and by whom.
    ///
    /// This is how cooperating waterfall listeners settle one decision between
    /// them: an upstream listener marks the shared object and delegates, rather
    /// than short-circuiting and taking the downstream transforms with it.
    ///
    /// It says WHO because the answer differs by gate. A convenience gate exists
    /// to stop asking twice, so any settled answer will do. A security boundary
    /// exists because the person asked to be stopped, and only that same person
    /// can lift it — see [`Authorization::by_person`].
    pub authorization: Authorization,
}

/// Who settled a tool call, for a gate deciding whether that answer is good
/// enough for the question it asks.
///
/// This was a `bool` until 2026-09-16, and the bug that split it is worth
/// remembering: a `[permissions] allow` rule someone wrote to stop being asked
/// about `curl` marked the call exactly as a person's live "yes" does, so the
/// strict credential shell read it as consent and let a token leave the machine.
/// Convenience and consent are not the same authority, and a `bool` could not
/// tell a gate which one it was holding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Authorization {
    /// Nobody has settled this call yet.
    #[default]
    No,
    /// Settled without asking anyone: a `[permissions] allow` rule, a
    /// `PreToolUse` hook's `allow`, or a gate that judged the call benign on its
    /// own (a write inside the workspace, a read inside the workspace).
    ///
    /// Enough to skip a prompt. Never enough to lift a boundary — all three are
    /// things a person set up once, not an answer to this call.
    Presumed,
    /// A person answered yes to THIS call: an approval prompt they just
    /// answered, a plan-mode grant, or a grant they stored earlier for this
    /// same scope.
    ByPerson,
}

impl Authorization {
    /// Someone settled it — enough for a gate that only exists to avoid asking
    /// the same question twice.
    pub fn settled(self) -> bool {
        !matches!(self, Authorization::No)
    }

    /// The person themselves settled it — the only answer that may lift a
    /// security boundary. A gate that guards one asks this, never
    /// [`settled`](Self::settled).
    pub fn by_person(self) -> bool {
        matches!(self, Authorization::ByPerson)
    }
}

/// What one step was given, before it becomes model-visible history.
#[derive(Clone, Debug)]
pub struct StepDecision {
    pub turn: u64,
    /// 1-based index of this step within the turn.
    pub step: u32,
    /// The message this step answers, if the inbox had one.
    pub message: Option<String>,
    /// Who asked for it. A listener that rewrites the message keeps the
    /// provenance unless it means to change who is speaking.
    pub origin: crate::agent::MessageOrigin,
    /// Attachments the message carried.
    pub images: Vec<atomcode_kernel::message::ImageContent>,
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
    /// Whether the turn would take another round: tools ran, the answer was cut
    /// off, or work is waiting. A budget stops a turn that wants more; a turn
    /// ending on its own at the budget has finished, not been cut off.
    pub continuing: bool,
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
    /// Every fact appended to a session log, broadcast as it commits.
    /// Persistence, telemetry, transcripts and UIs are all listeners here —
    /// none of them is wired into the loop. The payload names its session,
    /// because a listener above two logs receives facts from both.
    SessionEventCommitted, "session/event", Emit, Committed
);
plexus_event!(TurnStart, "turn/start", Emit, TurnStarted);
plexus_event!(
    /// The turn is over and not yet recorded as over — awaited.
    ///
    /// `turn/end` fires after `TurnEnd` is committed, and committing it is what
    /// tells a driver the turn finished. A listener whose work must be done
    /// before anyone acts on that — a store writing the turn durably, so that an
    /// undo issued the moment the driver sees the end reads the new state, or a
    /// fail-closed check that looks for a failed write — has no moment there:
    /// `turn/end` is synchronous, and a spawned write races the driver.
    ///
    /// Every listener runs, concurrently, and the turn waits for all of them.
    /// Nothing here can change the outcome; it is the last await before the
    /// boundary is committed.
    TurnFinishing, "turn/finishing", Parallel, TurnOutcome
);
plexus_event!(TurnEnd, "turn/end", Emit, TurnOutcome);
plexus_event!(AssistantChunk, "assistant/chunk", Emit, Chunk);
plexus_event!(AssistantMessage, "assistant/message", Emit, Message);
plexus_event!(ToolResultEvent, "tool/result", Emit, ToolResult);

plexus_event!(
    /// Around the model call. Delegating runs the provider; returning without
    /// delegating replaces the response entirely (a cache hit, a canned reply, a
    /// replay fixture).
    AgentRequest, "agent/request", Waterfall, ModelRequest => Result<ModelResponse, RequestError>
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
    /// Something reached an agent's inbox.
    ///
    /// What wakes a driver whose agent is idle. Before this, a turn began only
    /// on a driver's command or because the previous turn left work queued —
    /// so a message from a peer, a timer or a goal controller sat in the inbox
    /// until a person happened to type. Emitted on the agent's own context, so
    /// its driver hears it and so does anything supervising from above.
    InboxInserted, "agent/inbox/inserted", Emit, AgentInfo
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
    /// Around a whole round's worth of tool calls.
    ///
    /// Separate from `tools/execute` because it answers a different question:
    /// that one is "may this call run, and what does it return", this one is
    /// "how are these calls scheduled against each other". The loop's own
    /// terminal runs them one at a time; a listener can overlap them, cap them,
    /// or ship them elsewhere — and removing every listener leaves working
    /// serial execution rather than a hole.
    ToolsExecuteBatch, "tools/execute-batch", Waterfall, ToolBatch => Vec<ToolResult>
);

plexus_event!(
    /// Around one tool execution. Denying is *returning a result* rather than
    /// raising: the model must see why its call did not run, and history must
    /// stay pairable.
    ToolsExecute, "tools/execute", Waterfall, ToolExec => ToolResult
);
