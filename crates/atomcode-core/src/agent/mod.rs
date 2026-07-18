//! Agent-facing shared types (commands, events, phases) and helpers used by
//! both the terminal and daemon engines. The legacy in-process `AgentLoop`
//! engine has been retired; the v2 kernel/bridge stack is the only engine.

pub mod compression;
pub mod goal;
pub mod goal_evaluator;
pub mod loop_state;
pub mod parallel_edit;
pub mod session_title;

use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::conversation::{Conversation, ConversationSnapshot};
use crate::skill::SkillRegistry;
use crate::tool::{ToolCall, ToolRegistry};

/// The unified execution mode. Single source of truth across TUI, daemon,
/// webui. `Build` = interactive approval; `Auto` = auto-approve all tools
/// (bypass); `Plan` = read-only (PlanModeGate blocks writes/commands).
///
/// Wire format stays `build`/`plan`/`bypass` (Auto ↔ "bypass") so the daemon
/// `/live/mode` + `/approval_mode` endpoints and the webui stay compatible.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Build,
    /// Auto-accept file edits (edit/write tools auto-approve), while bash and
    /// sensitive paths still prompt — the safe middle tier between Build and Auto.
    #[serde(rename = "accept_edits")]
    AcceptEdits,
    #[serde(rename = "bypass")]
    Auto,
    Plan,
}

impl Mode {
    /// Forward cycle: Build → AcceptEdits → Auto → Plan → Build (escalating
    /// permissiveness, then Plan to close the loop).
    pub fn next(self) -> Self {
        match self {
            Mode::Build => Mode::AcceptEdits,
            Mode::AcceptEdits => Mode::Auto,
            Mode::Auto => Mode::Plan,
            Mode::Plan => Mode::Build,
        }
    }
    /// Reverse cycle: Build → Plan → Auto → AcceptEdits → Build.
    pub fn prev(self) -> Self {
        match self {
            Mode::Build => Mode::Plan,
            Mode::Plan => Mode::Auto,
            Mode::Auto => Mode::AcceptEdits,
            Mode::AcceptEdits => Mode::Build,
        }
    }
    pub fn is_plan(self) -> bool { matches!(self, Mode::Plan) }
    pub fn is_auto(self) -> bool { matches!(self, Mode::Auto) }
    pub fn is_accept_edits(self) -> bool { matches!(self, Mode::AcceptEdits) }
    /// The `(plan_mode, bypass_mode, accept_edits)` enforcement flags this mode
    /// maps to — the authoritative decomposition the runtime applies to its
    /// atomics. Exhaustive so a new variant forces an update here.
    pub fn to_flags(self) -> (bool, bool, bool) {
        match self {
            Mode::Build => (false, false, false),
            Mode::AcceptEdits => (false, false, true),
            Mode::Auto => (false, true, false),
            Mode::Plan => (true, false, false),
        }
    }
    /// Canonical wire string (`build`/`accept_edits`/`plan`/`bypass`). Must match
    /// the serde rename on the enum above; `mode_wire_matches_serde` locks that.
    pub fn wire(self) -> &'static str {
        match self {
            Mode::Build => "build",
            Mode::AcceptEdits => "accept_edits",
            Mode::Auto => "bypass",
            Mode::Plan => "plan",
        }
    }
    /// Stable ASCII label (status/telemetry). UI display uses i18n glyphs.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Build => "Build",
            Mode::AcceptEdits => "AcceptEdits",
            Mode::Auto => "Auto",
            Mode::Plan => "Plan",
        }
    }
}

#[cfg(test)]
mod mode_tests {
    use super::Mode;
    #[test]
    fn mode_cycles_forward_and_back() {
        // Forward: Build → AcceptEdits → Auto → Plan → Build.
        assert_eq!(Mode::Build.next(), Mode::AcceptEdits);
        assert_eq!(Mode::AcceptEdits.next(), Mode::Auto);
        assert_eq!(Mode::Auto.next(), Mode::Plan);
        assert_eq!(Mode::Plan.next(), Mode::Build);
        // Reverse is the exact inverse.
        assert_eq!(Mode::Build.prev(), Mode::Plan);
        assert_eq!(Mode::Plan.prev(), Mode::Auto);
        assert_eq!(Mode::Auto.prev(), Mode::AcceptEdits);
        assert_eq!(Mode::AcceptEdits.prev(), Mode::Build);
        assert_eq!(Mode::default(), Mode::Build);
    }
    #[test]
    fn mode_wire_is_build_accept_plan_bypass() {
        assert_eq!(serde_json::to_string(&Mode::Build).unwrap(), "\"build\"");
        assert_eq!(serde_json::to_string(&Mode::AcceptEdits).unwrap(), "\"accept_edits\"");
        assert_eq!(serde_json::to_string(&Mode::Auto).unwrap(), "\"bypass\"");
        assert_eq!(serde_json::to_string(&Mode::Plan).unwrap(), "\"plan\"");
        assert_eq!(serde_json::from_str::<Mode>("\"bypass\"").unwrap(), Mode::Auto);
        assert_eq!(serde_json::from_str::<Mode>("\"accept_edits\"").unwrap(), Mode::AcceptEdits);
    }
    #[test]
    fn mode_flags_and_predicates() {
        // (plan, bypass, accept_edits)
        assert_eq!(Mode::Build.to_flags(), (false, false, false));
        assert_eq!(Mode::AcceptEdits.to_flags(), (false, false, true));
        assert_eq!(Mode::Auto.to_flags(), (false, true, false));
        assert_eq!(Mode::Plan.to_flags(), (true, false, false));
        assert!(Mode::Auto.is_auto() && !Mode::Auto.is_plan());
        assert!(Mode::Plan.is_plan() && !Mode::Plan.is_auto());
        assert!(Mode::AcceptEdits.is_accept_edits() && !Mode::AcceptEdits.is_auto());
        assert!(!Mode::Build.is_auto() && !Mode::Build.is_plan() && !Mode::Build.is_accept_edits());
    }
    #[test]
    fn mode_wire_matches_serde() {
        for m in [Mode::Build, Mode::AcceptEdits, Mode::Auto, Mode::Plan] {
            assert_eq!(format!("\"{}\"", m.wire()), serde_json::to_string(&m).unwrap());
        }
    }
}

/// Commands sent FROM the UI TO the agent loop.
#[derive(Debug)]
pub enum AgentCommand {
    /// User sent a message (may include attached file content and/or images).
    /// `image_markers[i]` is the `[Image #N]` number printed for `images[i]`
    /// at paste time. Round-tripped through `AgentEvent::RestorePendingImages`
    /// so that on VL preprocess failure the TUI can re-attach images with
    /// their ORIGINAL markers — otherwise an UP-recalled `[Image #5]` text
    /// wouldn't match a freshly-renumbered restored image. Empty when the
    /// caller has no images (slash commands, queued text from streaming,
    /// CLI single-shot).
    SendMessage {
        text: String,
        images: Vec<crate::conversation::message::ImagePart>,
        #[allow(dead_code)]
        // used in 2026-05-09 vision-preprocessor retry; agent reflects on Failed
        image_markers: Vec<usize>,
    },
    /// Cancel current operation.
    Cancel,
    /// Approve a pending tool call.
    ApproveTool,
    /// Approve and always allow this tool for the session.
    ApproveToolAlways,
    /// Deny a pending tool call.
    DenyTool,
    /// A driver's answer to a pending generic `AgentEvent::Request` (matched by `id`).
    Respond { id: u64, value: serde_json::Value },
    /// Reload config from TUI (the single source of truth for in-memory config,
    /// including ephemeral OAuth providers). Switches to the new default provider.
    ReloadConfig(atomcode_config::config::Config),
    /// Change working directory.
    ChangeDir(String),
    /// Append input during streaming — queued and injected before next LLM call.
    AppendInput(String),
    /// Clear conversation history.
    ClearConversation,
    /// Set conversation state from a resumed session.
    ///
    /// `restore_id` identifies callers that need an exact, asynchronous
    /// acknowledgement. The runtime answers those requests with either
    /// [`AgentEvent::ConversationRestored`] or
    /// [`AgentEvent::ConversationRestoreFailed`] carrying the same id.
    SetConversation {
        snapshot: ConversationSnapshot,
        restore_id: Option<u64>,
    },
    /// Bind the per-conversation session id (the session file's id) so the
    /// `x-atomcode-session-id` header tracks the persistent conversation
    /// identity. Sent by the UI whenever the current session is established
    /// or switched (startup, /session, /resume, -c continue), so resuming a
    /// saved session reuses its original id for gateway prefix-cache
    /// affinity.
    SetSessionId(String),
    /// Set plan mode (read-only exploration, no edits).
    SetPlanMode(bool),
    /// Set the unified execution mode (replaces SetPlanMode; SetPlanMode is
    /// removed in a later task once all producers migrate).
    SetMode(crate::agent::Mode),
    /// Recompute and re-emit a rich ContextStats snapshot. `/context` sends
    /// this before rendering so the user never sees a stale cache — the
    /// cache is only refreshed on LLM round-trips, so between turns (or
    /// after out-of-turn mutations like `inject_post_compress_state`) the
    /// snapshot can lag the actual conversation state.
    RefreshContextStats,
    /// Rebuild the hook executor from disk after a `/plugin install|uninstall`
    /// or other change to plugin state. Cheap (just re-reads JSON files);
    /// does NOT touch provider/model state, unlike ReloadConfig.
    ReloadHooks,
    /// Request a snapshot of the current conversation state.
    /// The agent responds with `AgentEvent::MessagesSync`. Used by the TUI
    /// before `/bg` to ensure the session has up-to-date history even when
    /// a turn is still in progress (e.g. waiting for tool approval).
    SyncMessages,
    /// Roll conversation memory back to just before the `nth` real user
    /// prompt (1-based). `None` targets the last prompt (bare `/undo`). The
    /// agent replies with `AgentEvent::ConversationTruncated` on success or
    /// `AgentEvent::UndoFailed` when `nth` is out of range.
    UndoToPrompt {
        nth: Option<usize>,
    },
    /// User invoked `!cmd` bash mode. Runs the shell command locally and
    /// injects `<bash-input>/<bash-stdout>/<bash-stderr>` into the
    /// conversation as a User message — WITHOUT starting an LLM turn.
    /// The model sees it on the user's next real message.
    LocalShell {
        cmd: String,
    },
    /// Set a goal condition — agent will loop until evaluator says met.
    SetGoal {
        condition: String,
    },
    /// Clear the active goal.
    ClearGoal,
    /// Start a self-paced /loop with the given prompt.
    SetLoop {
        prompt: String,
    },
    /// Stop the active /loop.
    ClearLoop,
    /// Shutdown the agent.
    Shutdown,
}

/// Reason the agent's turn loop stopped. Carried on TurnComplete so downstream
/// consumers (CLI [done] line, eval harness) can distinguish natural completion
/// from budget-enforced truncation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnStopReason {
    /// Model responded with text only — no more tool calls, conversation done.
    Natural,
    /// Turn budget (AgentLoop.max_turns) was reached.
    TurnLimit,
    /// Step budget (check_step_limit tool-call cap) was reached.
    StepLimit,
    /// User cancelled the turn.
    Cancelled,
    /// API or internal error terminated the loop.
    Error,
}

pub use compression::CompressionOutcome;

impl TurnStopReason {
    /// Short machine-parseable tag (snake_case) for logs / CLI output.
    pub fn as_tag(&self) -> &'static str {
        match self {
            TurnStopReason::Natural => "natural",
            TurnStopReason::TurnLimit => "turn_limit",
            TurnStopReason::StepLimit => "step_limit",
            TurnStopReason::Cancelled => "cancelled",
            TurnStopReason::Error => "error",
        }
    }
}

/// One descriptor per sub-agent in a `SubAgentDispatchStart` batch.
/// Mirrored 1:1 with the `tasks` vector built in `parallel_edit::execute`
/// so callers can reuse the index across the lifecycle events.
#[derive(Debug, Clone)]
pub struct SubAgentTaskInfo {
    /// Workspace-relative file path the sub-agent will edit. Renderer
    /// shows this in full (not basename-only) so multi-component paths
    /// like `src/server/tunnel.rs` vs `src/client/tunnel.rs` stay
    /// visibly distinct.
    pub path: String,
    /// User-facing duplicate-instance qualifier. Empty when the path
    /// is unique within this dispatch; `" (#2)"`, `" (#3)"` when the
    /// dispatcher is forking >1 sub-agent against the same path.
    pub dedup_suffix: String,
}

/// Events sent FROM the agent loop TO the UI.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// LLM text delta (streaming).
    TextDelta(String),
    /// LLM reasoning/thinking content (e.g., DeepSeek-R1, MiniMax-M2.7, o1-series).
    /// Emitted when the model produces thinking content separately from the final response.
    /// UI can optionally display this in verbose mode (Ctrl+O).
    ReasoningDelta(String),
    /// LLM has started emitting a tool call — only the name is known so far,
    /// arguments are still streaming. UI uses this to display the tool name
    /// immediately instead of waiting for the full args.
    ToolCallStreaming { name: String, hint: String },
    /// A tool call is about to execute (for display).
    /// `id` pairs with `ToolCallResult.call_id` so the UI can match start→result
    /// across parallel or interleaved calls without reconstructing ids from counters.
    ToolCallStarted {
        id: String,
        name: String,
        arguments: String,
    },
    /// Multiple tool calls fan out from one assistant message. Fires BEFORE
    /// the per-call `ToolCallStarted` events, only when ≥ 2 non-duplicate
    /// calls are about to dispatch. UI uses this to render a single
    /// grouped block (`▸ Reading 4 files (parallel)` + child rows) rather
    /// than N independent `▸` rows. Per-call events still fire for
    /// backward compat — UI dedupes via `batch_id` membership.
    ToolBatchStarted {
        batch_id: String,
        calls: Vec<crate::turn::event::ToolBatchCall>,
    },
    /// Closes the batch opened by `ToolBatchStarted`. UI finalizes the
    /// group header with `· N/M ok · Xs wall` summary.
    ToolBatchCompleted {
        batch_id: String,
        ok: usize,
        total: usize,
        elapsed_ms: u64,
    },
    /// Real-time output chunk from a running tool (e.g., bash command).
    /// Sent during tool execution before ToolCallResult.
    ToolOutputChunk { call_id: String, chunk: String },
    /// A tool call completed with a result.
    ToolCallResult {
        call_id: String,
        name: String,
        output: String,
        success: bool,
        duration: Duration,
    },
    /// Waiting for user approval of a tool call.
    ApprovalNeeded {
        tool_name: String,
        reason: String,
        call: ToolCall,
        /// Snapshot of conversation state at the time the approval
        /// request was raised. Lets the TUI persist mid-turn session
        /// state (e.g. when `/bg` backgrounds a session that is waiting
        /// for approval).
        snapshot: ConversationSnapshot,
    },
    /// Generic driver round-trip: the runtime raised an opaque interactive request
    /// (`kind` + `payload`) that a driver must render and answer with
    /// `AgentCommand::Respond { id, .. }`. Core is agnostic to `kind`/`payload` — the
    /// domain-level mirror of the kernel's own `Request`, so interactive tools (e.g.
    /// `request_user_input`) need no tool-specific core code.
    Request { id: u64, kind: String, payload: serde_json::Value },
    /// Token usage update.
    TokenUsage(crate::stream::TokenUsage),
    /// The agent's current phase changed.
    PhaseChange(AgentPhase),
    /// Turn completed successfully.
    TurnComplete {
        duration: Duration,
        total_tokens: usize,
        /// LLM round-trips (standard agent metric).
        turn_count: usize,
        /// Total individual tool calls.
        tool_call_count: usize,
        /// Why the loop stopped. `Natural` for ordinary completion; see
        /// TurnStopReason for budget / cancel / error variants.
        stop_reason: TurnStopReason,
        /// Snapshot of the conversation state at the moment the turn
        /// ended. Mirrors `TurnCancelled.messages` so UIs have one uniform
        /// path for persisting session state on either terminal event.
        snapshot: ConversationSnapshot,
    },
    /// Turn was cancelled by user before completion.
    /// The conversation has been cleaned up - partial messages removed.
    /// Contains the cleaned conversation state for TUI to sync.
    TurnCancelled { snapshot: ConversationSnapshot },
    /// Conversation memory was rolled back by `/undo`. Carries the truncated
    /// message list (for the TUI to persist + replay), the removed prompt's
    /// text (to restore into the input box), and turn numbers for the
    /// confirmation line (`target_n..=prompts_before` were removed).
    ConversationTruncated {
        snapshot: ConversationSnapshot,
        restored_prompt: String,
        target_n: usize,
        prompts_before: usize,
    },
    /// `/undo` could not be honored: `requested` turn is out of range.
    /// `available` real prompts exist (0 = nothing to undo).
    UndoFailed { requested: usize, available: usize },
    /// Response to `AgentCommand::SyncMessages`. Carries a snapshot of
    /// conversation state at the time the agent processed the command.
    /// Used by the TUI to sync session state before backgrounding a session
    /// that is mid-turn (e.g. waiting for tool approval).
    MessagesSync { snapshot: ConversationSnapshot },
    /// The runtime has restored a requested conversation and then read the
    /// installed state back from its own conversation owner. `snapshot` is the
    /// read-back value, not an echo of the command payload.
    ConversationRestored {
        restore_id: u64,
        snapshot: ConversationSnapshot,
    },
    /// A requested conversation restore could not be installed or verified.
    ConversationRestoreFailed {
        restore_id: u64,
        error: String,
    },
    /// An error occurred. Carries a snapshot of `conversation.messages`
    /// so the TUI can persist mid-turn state even when the turn dies
    /// before TurnComplete/TurnCancelled fire — without this, a
    /// first-turn LLM failure silently drops the user's typed message
    /// from disk and `/resume` shows nothing for that conversation.
    /// Producers that don't hold the conversation (the inline
    /// streaming-error forwarder in `run_turn_loop`) send `messages:
    /// Vec::new()`; the terminal error path captured at
    /// `handle_send_message` provides the full snapshot.
    Error {
        error: String,
        snapshot: ConversationSnapshot,
    },
    /// Non-fatal advisory from a provider or other subsystem. UI renders
    /// this as a one-line yellow banner; does not abort the turn.
    /// Currently sourced from the OpenAI provider's truncation detector
    /// when the proxy reports implausibly few prompt_tokens.
    Warning(String),
    /// A 429 rate-limit PAUSE — driver renders a non-error pause line with the
    /// reset time. Empty strings / None when no usage data was available.
    /// Forwarded from the kernel's `AgentEvent::RateLimited` via the bridge.
    RateLimited {
        reset_at_display: String,
        reset_label: String,
        secs_until_reset: Option<u64>,
        /// `true` = WaitAndRetry (kernel will sleep then retry automatically);
        /// `false` = Pause (kernel stopped the turn, user must act).
        auto_resuming: bool,
        /// The provider's OWN 429 message (extracted, no `HTTP …:` prefix) when the
        /// 429 carried an actionable body (e.g. an external model's `余额不足…请充值`).
        /// `None` for CodingPlan-window pauses and auto-retry. Drivers surface it ONLY
        /// on the generic (non-CodingPlan) pause.
        server_message: Option<String>,
    },
    /// A UserPromptSubmit hook failed due to an environment issue (missing
    /// dependency, crash, etc.) rather than an explicit block. The turn
    /// continues but the status-bar hint should surface the error so the
    /// user can fix their hook configuration.
    HookWarningHint(String),
    /// VL preprocessing failed; the agent is returning the user's pending
    /// images so the TUI can re-attach them to the input state. Lets the
    /// user retry the same image without re-pasting from clipboard. Hashes
    /// are TUI-side state, so the renderer recomputes them from the
    /// returned base64 bytes (best-effort; clipboard-equality dedup may
    /// fire on a fresh paste of the same image — minor UX, not breaking).
    RestorePendingImages {
        images: Vec<crate::conversation::message::ImagePart>,
        /// Original `[Image #N]` numbers, parallel to `images`. Round-tripped
        /// from `AgentCommand::SendMessage::image_markers` so the TUI can
        /// re-attach with the SAME marker numbers — keeps UP-recalled
        /// caption text matching after retry.
        markers: Vec<usize>,
    },
    /// VL preprocessing succeeded — surface a one-line success notice
    /// without dumping the (possibly long, sometimes uninformative) VL
    /// description into the UI. The description still rides into
    /// conversation history for the main model. `vl_key` is the provider
    /// key from config; `char_count` is `text.chars().count()` so users
    /// can spot zero/near-zero outputs that would mislead the main model.
    VisionPreprocessSuccess { vl_key: String, char_count: usize },
    /// Sub-agent batch began. `tasks` is the ordered list of children
    /// the dispatcher is about to fork — same order as the resulting
    /// `SubAgentTaskDone`/`SubAgentTaskFailed` events will arrive in,
    /// so the UI can pre-allocate one display slot per child and
    /// disambiguate same-basename tasks via the index.
    SubAgentDispatchStart {
        /// Per-task descriptors. `path` is the workspace-relative file
        /// path (preserved as the model wrote it — no basename-only
        /// truncation). `dedup_suffix` is the user-facing `(#2)`,
        /// `(#3)` qualifier when the same path appears N times in one
        /// dispatch; empty for unique entries.
        tasks: Vec<SubAgentTaskInfo>,
    },
    /// Sub-agent batch ended (all tasks settled or pool returned). UI
    /// clears the override so subsequent thinks/tools resume normal
    /// label behaviour.
    SubAgentDispatchEnd,
    /// One sub-agent has been claimed from the pool and is now running.
    /// `index` indexes into the `tasks` vector emitted with the
    /// matching DispatchStart so the UI can locate its slot.
    SubAgentTaskStarted { index: usize },
    /// Sub-agent finished successfully. `summary` is a one-sentence
    /// human-readable result, already truncated to a reasonable length
    /// by the agent loop.
    SubAgentTaskDone {
        index: usize,
        elapsed_ms: u64,
        turns: usize,
        summary: String,
    },
    /// Sub-agent failed (error, timeout, no-edit). `reason` is one
    /// short phrase, not a stack trace.
    SubAgentTaskFailed {
        index: usize,
        elapsed_ms: u64,
        turns: usize,
        reason: String,
    },
    /// Goal evaluator update — TUI shows progress.
    GoalUpdate {
        active: bool,
        round: u32,
        elapsed_secs: u64,
        condition: String,
        last_reason: Option<String>,
    },
    /// Self-paced /loop progress — TUI mirrors round/elapsed/label.
    LoopUpdate {
        active: bool,
        round: u32,
        elapsed_secs: u64,
        label: String,
        last_reason: Option<String>,
    },
    /// Working directory changed.
    WorkingDirChanged(PathBuf),
    /// Another client (e.g. the webui) switched the project working directory.
    /// Unlike `WorkingDirChanged` — which is an in-place cwd change from the
    /// agent's own `cd`/`change_dir` tool and keeps the conversation — this
    /// means "switch project, start fresh here": the receiving view changes cwd
    /// AND opens a new session. Delivered over the live-sync channel
    /// (`LiveEvent::WorkingDirChanged` → here) so a same-process TUI follows a
    /// webui directory switch. Kept distinct from `WorkingDirChanged` precisely
    /// so an agent-driven `cd` mid-task never wipes the conversation.
    ProjectSwitched(PathBuf),
    /// The mobile app asked the desktop TUI to run a slash command (e.g.
    /// `/status`). Whitelist-checked on the TUI side; the textual output is
    /// broadcast back via `LiveEvent::CommandOutput`. Delivered over the
    /// live-sync channel (`LiveEvent::RemoteCommand` → here).
    RemoteSlashCommand(String),
    /// 手机 App / webui 切换了审批模式（build / plan）→ TUI 跟随更新。
    /// Context budget stats — piped into datalog and cached by the TUI
    /// for `/context`. Emitted after every turn's `ctx.build_messages`
    /// call, so stats reflect the snapshot the model actually saw.
    ///
    /// The rich breakdown (tool defs / cold zone / ctx window / ctx name)
    /// only appears on the second emission path in
    /// `handle_send_message` — the first path (TurnEvent forwarding) uses
    /// the narrow stats from the ctx::render output. TUI merges both.
    ContextStats {
        system_tokens: usize,
        sent_tokens: usize,
        dropped_tokens: usize,
        working_set_tokens: usize,
        total_messages: usize,
        /// Total bytes of tool definitions / 4. 0 when not yet computed.
        tool_defs_tokens: usize,
        /// Tokens used by cold-zone compressed summaries.
        cold_zone_tokens: usize,
        /// Effective token budget from the active ctx strategy
        /// (`ctx.ctx_window()`), including any defensive clamping.
        ctx_window: usize,
        /// Ctx strategy name — `default` / `ollama` / future impls.
        ctx_name: String,
        /// Full assembled system prompt for the turn — lets the TUI's
        /// `/context prompt` show the exact bytes sent. Empty on the
        /// narrow TurnEvent-forwarded path; only the rich emission in
        /// `handle_send_message` fills this.
        system_prompt: String,
    },
    /// 另一视图（webui/其他 TUI）发起的用户消息回显，用于同步模式下本端渲染用户气泡。
    UserEcho(String),
    /// 同步会话的对端正在进行 turn（true=进行中），用于禁用/恢复本端输入。
    PeerBusy(bool),
    /// 同步会话的另一视图（webui 下拉框）切换了模型。TUI 据此更新头部显示与活动 provider。
    ProviderChanged(String),
    /// 同步会话的另一视图（webui 模式下拉框）切换了审批模式。
    ModeChanged { plan: bool, bypass: bool },
    /// 同步会话的另一视图（webui）创建了新会话。TUI 据此跟随切换到新会话。
    SessionSwitched(String),
    /// daemon 给当前活动会话改名。TUI 据此更新会话选择器与头部显示。
    SessionRenamed { name: String },
    /// One or more user prompts were folded ("steered") into the running turn at
    /// a round boundary. `count` is the number folded this round. Drivers surface
    /// a running "folded into current turn" count in the footer while streaming.
    Steered { count: usize },
}

/// The current phase of the agent (for UI display).
#[derive(Debug, Clone, PartialEq)]
pub enum AgentPhase {
    Idle,
    Thinking,            // LLM generating text
    CallingTool(String), // Executing a tool (with name)
    WaitingApproval,     // Waiting for user to approve
}

/// Cloneable sender side for UI/runtime code to communicate with the agent.
#[derive(Clone)]
pub struct AgentClient {
    pub cmd_tx: mpsc::UnboundedSender<AgentCommand>,
    /// Shared tool registry for dynamic MCP tool registration.
    pub tool_registry: std::sync::Arc<ToolRegistry>,
    /// Loaded skills, shared with the agent loop. The TUI uses this
    /// to populate the slash-command palette with `user_invocable()`
    /// entries, and to expand the template when a user picks one.
    /// Same `Arc` the agent loop holds — reload(...) calls there are
    /// visible here without extra plumbing.
    pub skill_registry: std::sync::Arc<std::sync::RwLock<SkillRegistry>>,
}

/// Handle for the UI to communicate with the agent.
pub struct AgentHandle {
    pub client: AgentClient,
    pub event_rx: mpsc::UnboundedReceiver<AgentEvent>,
}

/// 上下文预算明细（纯计算，无副作用）。TUI 的 emit_rich_context_stats 与
/// daemon 的 /context 共用同一 token 口径。
#[derive(Debug, Clone)]
pub struct RichContextStats {
    pub system_tokens: usize,
    pub sent_tokens: usize,
    pub total_messages: usize,
    pub tool_defs_tokens: usize,
    pub cold_zone_tokens: usize,
    pub ctx_window: usize,
    pub ctx_name: String,
}

/// 计算上下文预算明细。`msgs` 应为 `ctx.build_messages(conv, system_prompt, "")` 的产物。
pub async fn compute_rich_context_stats(
    conv: &Conversation,
    msgs: &[crate::conversation::message::Message],
    tools: &ToolRegistry,
    ctx: &dyn crate::ctx::CtxBuilder,
) -> RichContextStats {
    use crate::conversation::message::Role;
    let tool_defs = tools.get_definitions().await;
    let tool_defs_tokens: usize = tool_defs
        .iter()
        .map(|d| {
            let params = serde_json::to_string(&d.parameters).unwrap_or_default();
            (d.name.len() + d.description.len() + params.len()) / 4
        })
        .sum();
    let cold_zone_tokens: usize = conv.cold_summaries.iter().map(|s| s.len() / 4 + 4).sum();
    let system_tokens = msgs
        .iter()
        .find(|m| matches!(m.role, Role::System))
        .map(|m| m.estimate_tokens())
        .unwrap_or(0);
    let sent_tokens = msgs
        .iter()
        .map(|m| m.estimate_tokens())
        .sum::<usize>()
        .saturating_sub(system_tokens);
    RichContextStats {
        system_tokens,
        sent_tokens,
        total_messages: msgs.len(),
        tool_defs_tokens,
        cold_zone_tokens,
        ctx_window: ctx.ctx_window(),
        ctx_name: ctx.name().to_string(),
    }
}

#[cfg(test)]
mod generic_round_trip_tests {
    use super::{AgentCommand, AgentEvent};

    #[test]
    fn core_generic_request_respond_variants_exist() {
        let ev = AgentEvent::Request { id: 7, kind: "request_user_input".into(), payload: serde_json::json!({"q":1}) };
        match ev { AgentEvent::Request { id, kind, .. } => { assert_eq!(id, 7); assert_eq!(kind, "request_user_input"); } _ => panic!() }
        let cmd = AgentCommand::Respond { id: 7, value: serde_json::json!({"ok":true}) };
        match cmd { AgentCommand::Respond { id, .. } => assert_eq!(id, 7), _ => panic!() }
    }
}

#[cfg(test)]
mod compute_rich_context_stats_tests {
    use super::{compute_rich_context_stats, Conversation};
    use crate::tool::ToolRegistry;

    #[tokio::test]
    async fn compute_rich_context_stats_counts_system_and_sent() {
        use crate::conversation::message::{Message, Role};
        // 一个空工具表 + default ctx，专注验证 system/sent/total 口径。
        let conv = Conversation::new();
        let msgs = vec![
            Message::new(Role::System, "system prompt here"),
            Message::new(Role::User, "hello world"),
        ];
        let tools = ToolRegistry::new();
        let ctx = crate::ctx::for_provider(&atomcode_config::config::provider::ProviderConfig {
            provider_type: String::new(),
            api_key: None,
            model: String::new(),
            base_url: None,
            system_prompt: None,
            user_agent: None,
            context_window: 128_000,
            max_tokens: None,
            thinking_type: None,
            thinking_keep: None,
            reasoning_history: None,
            reasoning_effort: None,
            thinking_enabled: None,
            thinking_budget: None,
            skip_tls_verify: false,
            ephemeral: true,
            capable_model: None,
        });
        let s = compute_rich_context_stats(&conv, &msgs, &tools, &*ctx).await;
        assert_eq!(s.total_messages, 2);
        assert!(s.system_tokens > 0);
        assert!(s.sent_tokens > 0);
        assert_eq!(s.ctx_window, 128_000);
        assert_eq!(s.tool_defs_tokens, 0); // 空工具表
    }
}
