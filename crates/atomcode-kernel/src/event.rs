use crate::message::{ImageContent, MessageMeta, SessionSnapshot};
use crate::tool::{ToolCall, ToolResult};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};

/// Driver round-trip `kind` for the round-cap checkpoint (kernel-initiated:
/// the fuse pauses the turn and asks the driver "continue past the cap?").
/// The driver answers `{"continue": bool}`; any non-object / missing / Null
/// response degrades to `false` (stop). Distinct from `request_user_input`
/// (model-initiated, in atomcode-capabilities).
pub const ROUND_CAP_CHECKPOINT_KIND: &str = "round_cap_checkpoint";

/// Driver round-trip `kind` emitted after the bounded automatic recovery for an
/// output-token truncation has been exhausted. Interactive drivers may offer a
/// continue/stop choice; unknown or unattended drivers must answer fail-closed.
pub const OUTPUT_TRUNCATION_CHECKPOINT_KIND: &str = "output_truncation_checkpoint";

pub type RequestId = u64;

/// Stable machine-readable reason for a hard policy intervention. Drivers use
/// this code to select trusted, localized presentation; it never carries model
/// input, rejected command bytes, or credentials.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PolicyInterventionCode {
    CredentialShellBlocked,
}

/// Recovery actions a driver may safely offer after a hard policy terminal.
/// None of these actions authorizes the rejected generic-shell operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum PolicyRecoveryAction {
    CompleteExternally,
    SkipStep,
    ViewSafeInstructions,
    EndTask,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyIntervention {
    /// Process-unique correlation id. Drivers must echo this id when resolving
    /// the intervention so a delayed response cannot acknowledge a newer one.
    pub id: u64,
    pub code: PolicyInterventionCode,
    pub actions: Vec<PolicyRecoveryAction>,
}

impl PolicyIntervention {
    pub fn credential_shell_blocked() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            code: PolicyInterventionCode::CredentialShellBlocked,
            actions: vec![
                PolicyRecoveryAction::CompleteExternally,
                PolicyRecoveryAction::SkipStep,
                PolicyRecoveryAction::ViewSafeInstructions,
                PolicyRecoveryAction::EndTask,
            ],
        }
    }
}

/// A user input that was authoritatively folded into an already-running turn.
/// Kept separate from persisted [`crate::message::Message`]: this is a transient
/// acknowledgement payload for drivers correlating their local pending UI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteeredInput {
    pub text: String,
    #[serde(default)]
    pub images: Vec<ImageContent>,
}

/// WHY a turn ended (FAILURE PERCEPTION). Carried by the terminal
/// `AgentEvent::TurnComplete { reason }` and aggregated into `Outcome::stop`, so a
/// driver (TUI / SWE-bench grader / CI) can ALWAYS tell a clean stop from a
/// failure — a failed turn can never look like an empty SUCCESS.
///
/// `#[non_exhaustive]` so new terminal causes don't break downstream matches.
/// `Stopped` is the NORMAL terminal (the model emitted no tool calls and the
/// `offer_continuation` hook did not continue), and is the `Default` so `Outcome::default()`
/// still compiles.
///
/// **The only one.** The harness used to keep a second `StopReason` for the
/// session log's `TurnEnd`, and its pump translated one into the other, folding
/// three causes into `MaxRounds` / `ProviderError` on the way — so a front end
/// read one reason in the log and another on the handle (`docs/adr/0021` §6).
/// Serialized by variant name, which is also how the log stored the harness's
/// copy; `InputRejected` is that copy's name for [`Self::PromptRejected`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum StopReason {
    /// Normal completion: model produced no tool calls and `offer_continuation` returned None.
    #[default]
    Stopped,
    /// The round budget ran out: the `max_rounds` cap was reached and nobody
    /// granted more (a person at a round-cap checkpoint, or no one to ask).
    MaxRounds,
    /// The `max_continuations` safety fuse tripped (a `offer_continuation` hook
    /// kept injecting continuations with no model agency to stop — a runaway loop).
    MaxContinuations,
    /// The always-on coarse repetition fuse observed the same model-emitted tool
    /// call pattern for too many consecutive rounds, even though exact results may
    /// have varied or the opt-in exact guard was disabled.
    RepeatLoop,
    /// The opt-in exact tool-loop guard reached its configured stop threshold for
    /// the same call (or all-read-only batch), model-visible result(s), and success
    /// state after a warning failed to make the model change course.
    ToolLoopDetected,
    /// The provider failed to open the stream OR errored mid-stream.
    ProviderError,
    /// A liveness `stream_timeout` elapsed waiting for the next stream event.
    Timeout,
    /// The turn was cooperatively cancelled (`AgentCommand::Cancel`).
    Cancelled,
    /// The input was refused before a step ran: a `user_prompt_submit` hook
    /// rejected the prompt, or a pre-step listener rejected the claimed input.
    #[serde(alias = "InputRejected")]
    PromptRejected,
    /// A tool middleware enforced a hard policy boundary. The blocked tool
    /// result was persisted before the turn terminated, so provider pairing is valid.
    PolicyDenied,
    /// The provider returned 429 and the host chose to PAUSE (reset too far to
    /// wait out). Not a failure — already-produced content is preserved.
    RateLimited,
    /// A stopping policy other than the round budget ended the turn — a
    /// deadline, a cost ceiling.
    StoppedByPolicy,
    /// The loop's own coarse runaway fuse. Not a policy: it exists so a loop with
    /// no stopping policy at all still terminates.
    RunawayFuse,
    /// Something reached the model that the session log cannot explain. The turn
    /// is stopped rather than continued: a prompt nobody can reconstruct makes
    /// resume, fork and compaction unsound from here on.
    InvariantViolated,
}

impl StopReason {
    /// The reason as drivers of the coding runtime's own protocol have always
    /// seen it: the three causes the harness pump used to fold away are folded
    /// the same way here.
    ///
    /// Transitional. Those drivers (tuix, daemon, ACP, clix) and the lifecycle
    /// hooks behind them match on the folded set; applying this where the runtime
    /// takes a reason off the tree keeps their behaviour unchanged while the tree
    /// and its new front end carry the real cause. Delete with the runtime's
    /// driver protocol (`docs/tui-replaces-tuix-plan.md` M6).
    pub fn folded_for_runtime_drivers(self) -> Self {
        match self {
            Self::StoppedByPolicy | Self::RunawayFuse => Self::MaxRounds,
            Self::InvariantViolated => Self::ProviderError,
            other => other,
        }
    }
}

/// Driver → agent. Serializable so it crosses process/network boundaries
/// (web/daemon), not just in-process (TUI/desktop). `#[non_exhaustive]` so new
/// variants don't break downstream drivers.
#[non_exhaustive]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AgentCommand {
    /// The user's next prompt. `images` carries optional multimodal attachments;
    /// ADDITIVE (`#[serde(default)]`) so an older `{text}`-only command still
    /// deserializes (→ no images). Empty `images` is exactly the text-only path.
    SendMessage {
        text: String,
        #[serde(default)]
        images: Vec<ImageContent>,
    },
    /// One real user prompt with host-owned synthetic context prepended to the
    /// SAME turn. The context is stored as `Message::synthetic_user`, then the
    /// real prompt is stored normally; one command therefore has one turn and one
    /// terminal. Used for deterministic resume context that must not become a
    /// second automated turn or leak into user-facing prompt projections.
    SendMessageWithContext {
        text: String,
        #[serde(default)]
        images: Vec<ImageContent>,
        context: String,
    },
    /// Host-injected synthetic prompt (e.g. an automated goal-mode continuation).
    /// Same execution path as `SendMessage` (user_prompt_submit hook, task-boundary
    /// compaction, mid-turn FIFO queueing), but the conversation message is pushed
    /// via `Message::synthetic_user`, so `sacred_floor` skips it and hosts can hide
    /// it from user-facing projections.
    SendSyntheticMessage {
        text: String,
    },
    /// Answer a pending AgentEvent::Request, correlated by id.
    Respond {
        id: RequestId,
        value: serde_json::Value,
    },
    /// Ask the agent to emit a snapshot of per-message execution stats.
    Snapshot,
    /// MANUAL compaction (e.g. a user `/compact`). Runs the injected
    /// `CompactionStrategy` REGARDLESS of any auto `compact_threshold` (a manual
    /// request is always honored). `focus` optionally steers the strategy toward a
    /// topic. A net-loss/no-op plan is still refused by `apply_plan` (no epoch
    /// burn). Serializable so a web/daemon driver can request it over the wire.
    Compact {
        focus: Option<String>,
    },
    Cancel,
    Shutdown,
    /// Any command, carrying an id the driver chose, so the agent can answer
    /// *this* command: [`AgentEvent::Accepted`] or [`AgentEvent::Rejected`] with
    /// the same id (`docs/adr/0021` §7).
    ///
    /// An envelope rather than a field on every command, so a driver that does
    /// not want receipts sends what it always sent. The id never reaches the
    /// session log; an ACP JSON-RPC request id maps onto it directly.
    Tagged {
        id: CommandId,
        command: Box<AgentCommand>,
    },
    /// Start receiving a session's facts as [`AgentEvent::Fact`]: every fact
    /// from `from` on that is already in its log, then each new one as it is
    /// committed — in order, none twice, none skipped (`docs/adr/0022` §1).
    ///
    /// Any session this agent can reach by id: its own, or a team member's.
    /// Content reaches a front end only this way; events carry status.
    Subscribe {
        session: String,
        #[serde(default)]
        from: crate::session::SeqNo,
    },
    /// Stop receiving a session's facts.
    Unsubscribe {
        session: String,
    },
    /// Run a command from a session's catalog (`docs/adr/0021` §10). `id` is
    /// the receipt: `Accepted` or `Rejected` names it, and so does the
    /// `Invoked` that carries the result. `session` is what the command acts
    /// on — the session, or one agent in it, as the catalog entry says.
    Invoke {
        id: CommandId,
        session: String,
        name: String,
        #[serde(default)]
        args: String,
    },
    /// `command`, for the agent behind `session` rather than the one this
    /// connection drives — a team member the person is talking to, cancelling
    /// or compacting (`docs/adr/0021` §9, `docs/adr/0023` §4, §8). An envelope,
    /// like [`Self::Tagged`], so no command grows an address field it only
    /// sometimes needs.
    To {
        session: String,
        command: Box<AgentCommand>,
    },
}

/// A driver's name for one command, echoed back on its receipt.
pub type CommandId = String;

impl AgentCommand {
    /// The command inside any [`AgentCommand::Tagged`] envelopes, for a loop that
    /// sends no receipts and so has no use for the id.
    pub fn untagged(self) -> Self {
        match self {
            Self::Tagged { command, .. } => command.untagged(),
            other => other,
        }
    }
}

/// Why a tagged command was refused on the spot (`docs/adr/0021` §8).
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandError {
    /// The question being answered is no longer waiting for an answer.
    StaleQuestion,
    /// There is no turn running for this to act on.
    NotRunning,
    /// The agent cannot take commands now, or at all any more.
    Unavailable,
    /// Not now: something the agent is doing has to finish first.
    Busy { reason: String },
    /// Nothing by that id is here — a session this agent cannot reach.
    NotFound,
    /// A command this agent has no answer for.
    Unsupported,
}

/// One call inside a `ToolBatchStarted` payload — everything the driver/UI
/// needs to render a child row in the group block.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolBatchCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
    /// True if this call may run concurrently (read-only); false → serialized
    /// behind the write-lock. Drives the UI's honest "in parallel" label.
    pub parallel_safe: bool,
}

/// Where a piece of model-visible context came from, when it was not typed by
/// the person.
///
/// Typed rather than a label, for the reason the recovery events are typed: a
/// driver that has to parse prose to decide how to draw something is a driver
/// that is wrong in every language nobody tested.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSource {
    /// Another agent — a team member reporting to its lead, a lead steering a
    /// member. `from` is the sender's session id, which outlives both of them.
    Peer { from: String },
    /// A persistent memory store.
    Memory,
    /// A runtime note the engine adds to the request.
    Reminder,
    /// The engine asked for another round on its own.
    Continuation,
    /// A summary standing in for history that was dropped.
    CompactionSummary,
    /// What the person said directly to a team member, shown to the lead.
    PersonToMember { member: String },
    /// Something about a team member the lead should know without being woken.
    TeamNote { member: String },
}

/// Agent → driver. Serializable for the same reason. The id-correlated
/// Request/Respond pair replaces any in-process oneshot, so the round-trip
/// works identically in-process and over the wire.
#[non_exhaustive]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AgentEvent {
    /// A turn began (perception granularity).
    ///
    /// `turn` is the session log's turn number when the agent keeps a log — the
    /// same number as the [`Self::TurnComplete`] that closes it and the
    /// [`Self::Accepted`] of every message it answered. `None` from a source that
    /// numbers no turns.
    TurnStarted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn: Option<u64>,
    },
    /// A tagged command was taken (`docs/adr/0021` §7).
    ///
    /// For a message: `turn` is the turn that answers it, and `steered` says it
    /// was folded into that turn while it was already running rather than
    /// starting it. Commands that belong to no turn (a compaction, a snapshot, an
    /// answer) are accepted with `turn: None`.
    Accepted {
        command: CommandId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn: Option<u64>,
        #[serde(default)]
        steered: bool,
    },
    /// A tagged command was refused on the spot. Nothing it asked for happened.
    Rejected {
        command: CommandId,
        error: CommandError,
    },
    /// One fact of a session this driver subscribed to
    /// ([`AgentCommand::Subscribe`]). The session log is the content; a screen is
    /// a fold over these (`docs/adr/0022` §1).
    ///
    /// Boxed, like the descriptions below: a fact is the largest thing this
    /// enum carries, and every event on every channel would otherwise be as
    /// large as one.
    Fact(Box<crate::session::Committed>),
    /// The agent behind a session just subscribed to, described. Sent first
    /// on every subscription (`docs/adr/0022` §5).
    Described {
        description: Box<crate::agent::AgentDescription>,
    },
    /// A member joined a subscribed session. Followed by its status; also sent
    /// for each member already there when the subscription starts.
    AgentAdded {
        description: Box<crate::agent::AgentDescription>,
    },
    /// What an `Invoke` produced, for a person to read.
    Invoked {
        id: CommandId,
        output: String,
        /// Whether the command also left the model something to do.
        ///
        /// Some catalog commands only answer (`policy`, `queue`); some answer
        /// **and** queue a message a turn will pick up (`worklog`, `init`). A
        /// front end that draws facts as they arrive never had to tell those
        /// apart — a turn either happens or it does not, and either way the
        /// screen draws what comes. One that answers a *request* with a turn
        /// does: it has to know whether the exchange is over here or whether a
        /// turn is starting. Both guesses are wrong in a way that bites later —
        /// end too early and the turn's facts land on the next request, wait
        /// for a turn that never comes and the request hangs.
        ///
        /// Answered rather than inferred because only the side that ran the
        /// command can see the inbox it queued into.
        queued: bool,
    },
    /// A member of a subscribed session is gone.
    AgentRemoved {
        session: String,
    },
    /// A subscribed session's agent, or one of its members, changed status.
    StatusChanged {
        session: String,
        status: crate::agent::AgentStatus,
    },
    TextDelta(String),
    /// **Model-visible context the person did not type.**
    ///
    /// A team member's report, a continuation the engine asked for, a memory
    /// block, a compaction summary. It is in the model's request and in the
    /// session log; without this event it is in neither the screen nor anything
    /// a driver could render, and the person and the model end up reading two
    /// different conversations. That is not hypothetical — it shipped: a lead
    /// answered its user with "your previous message was actually the
    /// subagent's words", because the report had reached the model and nothing
    /// else.
    ///
    /// A driver MUST NOT draw this as the user speaking. It is evidence the
    /// agent was handed, and the difference matters most exactly when the text
    /// reads like an instruction.
    ContextAdded {
        text: String,
        source: ContextSource,
    },
    /// A STREAMING fragment of a tool call the model is still emitting — live display of
    /// the tool name / arguments as they arrive. `index` groups fragments of the same
    /// call. Purely observational: the tool is EXECUTED later (see `ToolStarted` + the
    /// complete call); a driver may render the partial args or ignore this entirely.
    ToolCallStreaming {
        index: u32,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
    /// Multiple tool calls fan out from one assistant message. Fires BEFORE
    /// the per-call `ToolStarted` events, only when ≥ 2 non-duplicate calls
    /// are about to dispatch. Driver/UI uses this to render a single grouped
    /// block rather than N independent rows. Per-call events still fire for
    /// backward compat — driver dedupes via `batch_id` membership.
    ToolBatchStarted {
        batch_id: String,
        calls: Vec<ToolBatchCall>,
    },
    /// Closes the batch opened by `ToolBatchStarted`. Driver/UI finalizes
    /// the group header with `· N/M ok · Xs wall` summary.
    ToolBatchCompleted {
        batch_id: String,
        ok: usize,
        total: usize,
        elapsed_ms: u64,
    },
    ToolStarted {
        call: ToolCall,
    },
    /// Live progress from a long-running tool MID-execution (e.g. a sub-agent tool
    /// reporting a per-task update). `call_id` is the executing call's id; `message` is
    /// the tool's free-form status. Purely observational — a driver may render or ignore it.
    ToolProgress {
        call_id: String,
        message: String,
    },
    ToolResult {
        result: ToolResult,
    },
    /// A hard policy boundary stopped the turn, with a driver-safe recovery
    /// contract. Emitted only after every tool call in the batch has a paired
    /// result and immediately before the authoritative PolicyDenied terminal —
    /// UNLESS the turn is concurrently cancelled, in which case the cancel
    /// supersedes: this event and the PolicyDenied terminal are both dropped
    /// together (the turn ends Cancelled) and drivers surface nothing. The hard
    /// block itself still stands — its paired blocked ToolResult is persisted.
    PolicyIntervention {
        intervention: PolicyIntervention,
    },
    /// Generic middleware ↔ driver round-trip. Kernel is agnostic to kind/payload.
    Request {
        id: RequestId,
        kind: String,
        payload: serde_json::Value,
    },
    /// Per-LLM-call execution stats (perception side; mirrors the message sidecar).
    Usage(MessageMeta),
    /// Whole-conversation snapshot (reply to Snapshot command). Carries the
    /// LOSSLESS, VERSIONED `SessionSnapshot` — full `Vec<Message>` (role / text /
    /// tool_calls / tool_call_id / meta), suitable for persist + resume.
    Snapshot {
        snapshot: SessionSnapshot,
    },
    /// TERMINAL turn event. `reason` (FAILURE PERCEPTION) says WHY the turn ended —
    /// `Stopped` (normal) vs a failure/fuse (`ProviderError`/`Timeout`/`MaxRounds`/
    /// `MaxContinuations`/`RepeatLoop`/`ToolLoopDetected`/`Cancelled`/
    /// `PromptRejected`/`PolicyDenied`). A driver can no longer mistake a failed turn for an empty
    /// success.
    ///
    /// Exactly one per [`Self::TurnStarted`], with the same `turn` — cancelled,
    /// failed and shut-down turns included.
    TurnComplete {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn: Option<u64>,
        reason: StopReason,
    },
    /// A failure (failed open / mid-stream / timeout / max-rounds / prompt-rejected /
    /// tool error). `message` is the human-readable cause; `http_status` + `code` are
    /// the STRUCTURED error code for provider failures (`None` for kernel-internal ones).
    Error {
        message: String,
        #[serde(default)]
        http_status: Option<u16>,
        #[serde(default)]
        code: Option<String>,
        /// Provider-classified retryability. `None` means this is an internal
        /// error or came from an older serialized event that did not carry the
        /// classification.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retryable: Option<bool>,
    },
    /// The turn was cooperatively cancelled (AgentCommand::Cancel mid-turn).
    /// Emitted immediately before the terminal TurnComplete on a cancel path;
    /// any dangling tool_calls have been backfilled with synthetic results so
    /// the conversation stays API-valid.
    Cancelled,
    /// Model thinking/reasoning channel. The reasoning is BOTH emitted live here
    /// (perception side) AND accumulated + stored on `Message.reasoning` (claim 29),
    /// and is transformable per-chunk via `LifecycleHooks::on_reasoning_delta`
    /// (symmetric to visible text via `on_text_delta`) — so a redaction reaches both
    /// the live channel and storage consistently.
    Reasoning(String),
    /// Non-fatal advisory (e.g. a truncated response). The turn still completes.
    Warning(String),
    /// A partial response timed out after replay-unsafe output had already been
    /// observed. The kernel preserved that output and opened a fresh continuation
    /// inside the SAME turn instead of replaying the original request.
    StreamRecovery {
        attempt: u32,
        max_attempts: u32,
        recovered: bool,
    },
    /// A retryable provider OPEN failure is backing off before reopening the
    /// same logical round. Structured so non-interactive drivers do not parse a
    /// localized warning string to recover attempt metadata.
    ProviderRetry {
        attempt: u32,
        max_attempts: u32,
        backoff_secs: u64,
        reason: String,
    },
    /// The provider ended a text response with `finish_reason=length`, and the
    /// kernel is using one of its bounded automatic continuation attempts.
    /// Purely observational: drivers may update transient progress UI.
    OutputTruncationRecovery {
        attempt: u32,
        max_attempts: u32,
    },
    /// A 429 rate-limit PAUSE (host decided the reset is too far to auto-wait).
    /// A driver renders this as a non-error pause line with the reset time, NOT
    /// as a red error. `secs_until_reset`/`reset_at_display` may be empty when the
    /// host had no usage data.
    RateLimited {
        reset_at_display: String,
        reset_label: String,
        #[serde(default)]
        secs_until_reset: Option<u64>,
        /// `true` = WaitAndRetry (kernel will sleep then retry automatically);
        /// `false` = Pause (kernel stopped the turn, user must act).
        #[serde(default)]
        auto_resuming: bool,
        /// The provider's OWN 429 message (already extracted, no `HTTP …:` prefix),
        /// when the 429 carried an actionable body — e.g. a user's external model
        /// replying `余额不足或无可用资源包,请充值`. `None` for CodingPlan-window
        /// pauses (they carry `reset_*` instead) and for auto-retry. A driver surfaces
        /// it ONLY on the generic (non-CodingPlan) pause so an external-model 429 shows
        /// its real reason instead of a bare "HTTP 429".
        #[serde(default)]
        server_message: Option<String>,
    },
    /// One or more user prompts were folded ("steered") into the running turn at
    /// a round boundary. `count` folded this round. Drivers relabel their
    /// type-ahead indicator from "queued" to "folded into current turn".
    Steered {
        /// The running turn the inputs were folded into.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn: Option<u64>,
        count: usize,
        /// Exact inputs folded at this boundary. Additive for wire
        /// compatibility: older events deserialize with an empty list.
        #[serde(default)]
        inputs: Vec<SteeredInput>,
    },
    /// A compaction is ABOUT TO RUN — emitted before the strategy plans/summarizes
    /// (a manual `/compact` may make a slow one-shot LLM summary call here). Lets a
    /// driver show a "compacting…" progress line before the possibly multi-second
    /// work; the outcome (sizes / committed) is not known yet — see `Compacted`.
    CompactionStarted {
        trigger: crate::message::CompactTrigger,
    },
    /// A compaction was ATTEMPTED (mirrors `message::CompactReport`). `committed`
    /// distinguishes a real shrink (history rewritten, `epoch` bumped to the NEW
    /// generation, `bytes_after < bytes_before`) from a REFUSED one (net-loss guard
    /// or no-op plan: history byte-identical, `epoch` unchanged, `removed == 0`).
    /// Emitted on BOTH the auto task-boundary trigger and the manual `Compact`
    /// command. Serializable for web/daemon drivers.
    Compacted {
        /// WHY this compaction ran — `Auto` (task-boundary pressure), `Manual` (`/compact`),
        /// or `Overflow { attempt }` (hard context-overflow recovery). Lets a telemetry sink
        /// distinguish normal-path compaction from emergency overflow recovery.
        trigger: crate::message::CompactTrigger,
        epoch: u64,
        removed: usize,
        bytes_before: usize,
        bytes_after: usize,
        committed: bool,
        /// Exact post-compaction working set. Present for a committed manual
        /// compaction so driver-owned session mirrors can persist the same bytes
        /// before reporting success; absent for no-op/auto/overflow attempts.
        #[serde(default)]
        snapshot: Option<SessionSnapshot>,
    },
    /// A prepared manual compaction could not be durably checkpointed. The live
    /// conversation and cache epoch are unchanged.
    CompactionFailed {
        trigger: crate::message::CompactTrigger,
        error: crate::checkpoint::CompactionCheckpointError,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tagged command crosses the wire whole, and the events that answer it
    /// read without the fields an older peer does not send (`docs/adr/0021` §7).
    #[test]
    fn receipts_and_turn_numbers_cross_the_wire() {
        let command = AgentCommand::Tagged {
            id: "req-7".into(),
            command: Box::new(AgentCommand::SendMessage {
                text: "hi".into(),
                images: Vec::new(),
            }),
        };
        let back: AgentCommand =
            serde_json::from_str(&serde_json::to_string(&command).unwrap()).unwrap();
        assert!(matches!(
            back.clone(),
            AgentCommand::Tagged { id, command } if id == "req-7"
                && matches!(*command, AgentCommand::SendMessage { ref text, .. } if text == "hi")
        ));
        assert!(matches!(back.untagged(), AgentCommand::SendMessage { .. }));

        let old: AgentEvent =
            serde_json::from_str(r#"{"TurnComplete":{"reason":"Stopped"}}"#).unwrap();
        assert!(matches!(
            old,
            AgentEvent::TurnComplete {
                turn: None,
                reason: StopReason::Stopped
            }
        ));
        let accepted = AgentEvent::Accepted {
            command: "req-7".into(),
            turn: Some(3),
            steered: true,
        };
        let json = serde_json::to_string(&accepted).unwrap();
        assert!(matches!(
            serde_json::from_str::<AgentEvent>(&json).unwrap(),
            AgentEvent::Accepted {
                turn: Some(3),
                steered: true,
                ..
            }
        ));
    }

    /// What a subscriber is told about an agent crosses the wire whole, and a
    /// description written before a field existed still reads.
    #[test]
    fn agent_descriptions_and_status_cross_the_wire() {
        use crate::agent::{AgentDescription, AgentStatus, MemberIdentity};
        use crate::provider::ReasoningEffort;

        let description = AgentDescription {
            session: "lead/scout".into(),
            parent: Some("lead".into()),
            member: Some(MemberIdentity {
                name: "scout".into(),
                role: "explorer".into(),
            }),
            model: Some("glm-5".into()),
            supports_vision: true,
            reasoning_effort: Some(ReasoningEffort::Low),
            compaction: true,
            commands: vec![crate::agent::CommandDescription {
                name: "stop".into(),
                usage: Some("<member>".into()),
                summary: "stop a member".into(),
                target: crate::agent::CommandTarget::Agent,
            }],
        };
        for event in [
            AgentEvent::Described {
                description: Box::new(description.clone()),
            },
            AgentEvent::AgentAdded {
                description: Box::new(description.clone()),
            },
            AgentEvent::AgentRemoved {
                session: "lead/scout".into(),
            },
            AgentEvent::StatusChanged {
                session: "lead/scout".into(),
                status: AgentStatus::Stopping,
            },
            AgentEvent::Invoked {
                id: "i-1".into(),
                output: "stopped: scout".into(),
                queued: false,
            },
        ] {
            let json = serde_json::to_string(&event).unwrap();
            let back =
                serde_json::to_string(&serde_json::from_str::<AgentEvent>(&json).unwrap()).unwrap();
            assert_eq!(json, back);
        }

        for command in [
            AgentCommand::Invoke {
                id: "i-1".into(),
                session: "lead/scout".into(),
                name: "stop".into(),
                args: "scout".into(),
            },
            AgentCommand::To {
                session: "lead/scout".into(),
                command: Box::new(AgentCommand::Tagged {
                    id: "m-1".into(),
                    command: Box::new(AgentCommand::SendMessage {
                        text: "use the other file".into(),
                        images: Vec::new(),
                    }),
                }),
            },
        ] {
            let json = serde_json::to_string(&command).unwrap();
            assert_eq!(
                json,
                serde_json::to_string(&serde_json::from_str::<AgentCommand>(&json).unwrap())
                    .unwrap()
            );
        }

        let sparse: AgentDescription = serde_json::from_str(r#"{"session":"s"}"#).unwrap();
        assert_eq!(
            sparse,
            AgentDescription {
                session: "s".into(),
                ..Default::default()
            }
        );
    }

    /// The harness's own copy logged a refused input as `InputRejected`; the one
    /// enum still reads that name.
    #[test]
    fn a_stop_reason_logged_under_the_harness_name_still_reads() {
        let reason: StopReason = serde_json::from_str("\"InputRejected\"").unwrap();
        assert_eq!(reason, StopReason::PromptRejected);
        assert_eq!(
            serde_json::to_string(&StopReason::RunawayFuse).unwrap(),
            "\"RunawayFuse\""
        );
    }

    /// What the coding runtime's drivers are handed is exactly what the harness
    /// pump used to hand them: the three causes it folded, folded the same way,
    /// and every other reason untouched.
    #[test]
    fn runtime_drivers_see_the_causes_the_pump_used_to_fold_folded_the_same_way() {
        use StopReason::*;
        for (reason, seen) in [
            (StoppedByPolicy, MaxRounds),
            (RunawayFuse, MaxRounds),
            (InvariantViolated, ProviderError),
            (Stopped, Stopped),
            (MaxRounds, MaxRounds),
            (MaxContinuations, MaxContinuations),
            (RepeatLoop, RepeatLoop),
            (ToolLoopDetected, ToolLoopDetected),
            (ProviderError, ProviderError),
            (Timeout, Timeout),
            (Cancelled, Cancelled),
            (PromptRejected, PromptRejected),
            (PolicyDenied, PolicyDenied),
            (RateLimited, RateLimited),
        ] {
            assert_eq!(reason.folded_for_runtime_drivers(), seen, "{reason:?}");
        }
    }

    #[test]
    fn send_message_serde_is_additive_for_images() {
        // An OLD {text}-only command (no `images`) must still deserialize → no images.
        let cmd: AgentCommand = serde_json::from_str(r#"{"SendMessage":{"text":"hi"}}"#).unwrap();
        match cmd {
            AgentCommand::SendMessage { text, images } => {
                assert_eq!(text, "hi");
                assert!(images.is_empty());
            }
            _ => panic!("expected SendMessage"),
        }
    }

    #[test]
    fn send_synthetic_message_serde_roundtrip() {
        let cmd = AgentCommand::SendSyntheticMessage {
            text: "continue".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: AgentCommand = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, AgentCommand::SendSyntheticMessage { text } if text == "continue"));
    }

    #[test]
    fn send_message_with_context_serde_roundtrip() {
        let cmd = AgentCommand::SendMessageWithContext {
            text: "continue".into(),
            images: vec![],
            context: "hidden recovery".into(),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: AgentCommand = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back,
            AgentCommand::SendMessageWithContext { text, context, images }
                if text == "continue" && context == "hidden recovery" && images.is_empty()
        ));
    }

    #[test]
    fn send_message_wire_format_unchanged_by_synthetic_variant() {
        // 旧 JSON 形态与 Rust 构造均不受新变体影响(additive API)。
        let cmd: AgentCommand = serde_json::from_str(r#"{"SendMessage":{"text":"hi"}}"#).unwrap();
        match cmd {
            AgentCommand::SendMessage { text, images } => {
                assert_eq!(text, "hi");
                assert!(images.is_empty());
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn steered_serde_defaults_missing_inputs_and_roundtrips_payload() {
        let old: AgentEvent = serde_json::from_str(r#"{"Steered":{"count":1}}"#).unwrap();
        assert!(matches!(
            old,
            AgentEvent::Steered {
                count: 1,
                inputs, .. } if inputs.is_empty()
        ));

        let event = AgentEvent::Steered {
            turn: None,
            count: 1,
            inputs: vec![SteeredInput {
                text: "follow up".into(),
                images: Vec::new(),
            }],
        };
        let json = serde_json::to_string(&event).unwrap();
        let roundtrip: AgentEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            roundtrip,
            AgentEvent::Steered {
                count: 1,
                inputs, .. } if inputs == vec![SteeredInput {
                text: "follow up".into(),
                images: Vec::new(),
            }]
        ));
    }

    #[test]
    fn compacted_serde_defaults_missing_snapshot_to_none() {
        let event: AgentEvent = serde_json::from_str(
            r#"{"Compacted":{"trigger":{"Manual":{"focus":null}},"epoch":1,"removed":2,"bytes_before":100,"bytes_after":50,"committed":true}}"#,
        )
        .unwrap();
        assert!(matches!(
            event,
            AgentEvent::Compacted { snapshot: None, .. }
        ));
    }

    #[test]
    fn error_retryability_is_additive_and_round_trips() {
        let old: AgentEvent = serde_json::from_str(
            r#"{"Error":{"message":"network","http_status":null,"code":null}}"#,
        )
        .unwrap();
        assert!(matches!(
            old,
            AgentEvent::Error {
                retryable: None,
                ..
            }
        ));

        let event = AgentEvent::Error {
            message: "network".into(),
            http_status: None,
            code: None,
            retryable: Some(true),
        };
        let json = serde_json::to_string(&event).unwrap();
        let decoded: AgentEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            AgentEvent::Error {
                retryable: Some(true),
                ..
            }
        ));
    }

    #[test]
    fn compaction_failure_round_trips_with_typed_error() {
        let event = AgentEvent::CompactionFailed {
            trigger: crate::message::CompactTrigger::Manual { focus: None },
            error: crate::checkpoint::CompactionCheckpointError::new("disk full"),
        };
        let json = serde_json::to_string(&event).unwrap();
        let decoded: AgentEvent = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            AgentEvent::CompactionFailed { error, .. } if error.message() == "disk full"
        ));
    }
}
