//! The AgentLoop — a standalone agent that processes user messages,
//! calls LLM providers, executes tools, and communicates with the UI
//! via channels. Decoupled from any TUI concerns.

pub mod background;
pub mod git_auto_commit;
pub mod git_checkpoint;
pub mod compression;
pub mod goal;
pub mod goal_evaluator;
pub mod parallel_edit;
pub mod session_title;
pub mod subtask_driver;

mod diagnose;
mod discipline;
pub mod execute;
mod local_shell;
mod prompt;
mod services;
mod tool_dispatch;
mod verify;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::conversation::{Conversation, ConversationSnapshot};
use crate::hook::HookEngine;
use crate::provider::LlmProvider;
use crate::skill::SkillRegistry;
use crate::tool::use_skill::UseSkillTool;
use crate::tool::{PermissionDecision, PermissionStore, ToolCall, ToolContext, ToolRegistry};
use crate::turn::event::{TurnEvent, TurnResult};
use crate::turn::runner::TurnRunner;

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
        #[allow(dead_code)] // used in 2026-05-09 vision-preprocessor retry; agent reflects on Failed
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
    /// Reload config from TUI (the single source of truth for in-memory config,
    /// including ephemeral OAuth providers). Switches to the new default provider.
    ReloadConfig(crate::config::Config),
    /// Change working directory.
    ChangeDir(String),
    /// Append input during streaming — queued and injected before next LLM call.
    AppendInput(String),
    /// Clear conversation history.
    ClearConversation,
    /// Set conversation state from a resumed session.
    SetConversation(ConversationSnapshot),
    /// Bind the per-conversation session id (the session file's id) so the
    /// `x-atomcode-session-id` header tracks the persistent conversation
    /// identity. Sent by the UI whenever the current session is established
    /// or switched (startup, /session, /resume, -c continue), so resuming a
    /// saved session reuses its original id for gateway prefix-cache
    /// affinity.
    SetSessionId(String),
    /// Set plan mode (read-only exploration, no edits).
    SetPlanMode(bool),
    /// Manually compact conversation history. `prompt` is accepted for
    /// forward-compat with an eventual LLM-backed summarize-with-instruction
    /// path; currently unused — this is the mechanical path only.
    Compact {
        prompt: Option<String>,
    },
    Remember {
        content: String,
        global: bool,
    },
    Forget {
        keyword: String,
    },
    ShowMemory,
    /// Run a one-shot task in an isolated background context (read-only-ish
    /// tool subset, independent conversation, capped turns + timeout).
    /// Result is returned via `AgentEvent::BackgroundComplete`.
    Background {
        task: String,
    },
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
    UndoToPrompt { nth: Option<usize> },
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

/// UI lifecycle of a single compaction (see `AgentEvent::CompactionUi`).
#[derive(Clone, Debug)]
pub enum CompactionUiKind {
    /// A slow LLM-summary compaction is about to run — take over the spinner.
    Begin,
    /// The compaction did not commit (no-op / refused) — release the spinner,
    /// draw no marker.
    End,
    /// The compaction committed — release the spinner and draw this localized
    /// marker line in scrollback.
    Mark(String),
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
    TurnCancelled {
        snapshot: ConversationSnapshot,
    },
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
    MessagesSync {
        snapshot: ConversationSnapshot,
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
    },
    /// Unified compaction UI lifecycle, decoupled from the kernel's byte-level
    /// `Compacted` audit. Drives the TUI's spinner takeover (`Begin`/`End`) and
    /// the dim scrollback marker (`Mark`, label already localized by the
    /// producer). Same for auto-compaction and manual `/compact`. Daemon/web
    /// drivers ignore it via their catch-all arms.
    CompactionUi(CompactionUiKind),
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
    VisionPreprocessSuccess {
        vl_key: String,
        char_count: usize,
    },
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
    /// `/background` task finished. `summary` is the final assistant text
    /// (truncated if long). `success` is false on error / timeout / cancel.
    BackgroundComplete {
        summary: String,
        files_edited: Vec<String>,
        turns: usize,
        success: bool,
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
    /// 同步会话的另一视图（webui）创建了新会话。TUI 据此跟随切换到新会话。
    SessionSwitched(String),
    /// daemon 给当前活动会话改名。TUI 据此更新会话选择器与头部显示。
    SessionRenamed { name: String },
}

/// The current phase of the agent (for UI display).
#[derive(Debug, Clone, PartialEq)]
pub enum AgentPhase {
    Idle,
    Thinking,            // LLM generating text
    CallingTool(String), // Executing a tool (with name)
    WaitingApproval,     // Waiting for user to approve
}

/// Discipline tracking state — counters for loop detection, stagnation,
/// error streaks, and tool usage patterns. Extracted from AgentLoop to
/// keep the God Object manageable.
#[derive(Default)]
pub(crate) struct DisciplineState {
    pub consecutive_reads: usize,
    pub stagnant_turns: usize,
    pub last_known_files: usize,
    pub targeted_read_count: usize,
    pub last_targeted_reads: usize,
    pub verify_injected: bool,
    pub model_produced_text: bool,
    pub silent_tool_rounds: usize,
    pub is_negative_feedback: bool,
    pub build_fail_count: usize,
    pub scouting_count: usize,
    pub api_confirmed_working: bool,
    pub consecutive_edits_file: Option<String>,
    pub consecutive_edits_count: usize,
    pub sleep_count: usize,
    pub consecutive_verify_count: usize,
    pub recent_errors: Vec<String>,
    pub executed_cmds: std::collections::HashMap<String, usize>,
    pub category_fail_streak: std::collections::HashMap<String, usize>,
    pub last_bash_cmd: String,
    pub last_diagnosed_error: String,
}

/// The agent loop state.
pub struct AgentLoop {
    // Core components
    pub conversation: Conversation,
    pub tool_registry: std::sync::Arc<ToolRegistry>,
    /// TurnRunner owns the provider, tools, and context.
    pub turn_runner: TurnRunner,
    pub permission_store: std::sync::Arc<std::sync::RwLock<PermissionStore>>,
    /// `--dangerously-skip-permissions` / `-y`: auto-approve every tool without
    /// prompting. Stored here (not just on the decider) so `AgentRuntimeFactory`
    /// can read it back via `from_initial_loop` and propagate the bypass to every
    /// runtime it re-spawns (/session, /resume, /bg). Without it, a re-spawned
    /// runtime silently re-enabled approval prompts while the ⚠ BYPASS badge
    /// stayed on (issue #714).
    pub dangerously_skip_permissions: bool,
    pub config: Config,
    /// Context construction strategy for the active provider. Selected
    /// at construction via `ctx::for_provider` and rebuilt on
    /// `AgentCommand::ReloadConfig` when the provider changes.
    ///
    /// `Arc` (not `Box`) — shared with `turn_runner.ctx` so datalog's
    /// `build_messages` call and runner's actual send go through the
    /// same instance. Rebuilds on `ReloadConfig` update both clones
    /// (see the reload handler below).
    pub ctx: std::sync::Arc<dyn crate::ctx::CtxBuilder>,

    /// Session-start environment snapshot — git branch / HEAD / status.
    /// Captured once in `new()`, refreshed on `ChangeDir` (new working
    /// tree ⇒ new repo). Stale-by-design: rendered with a disclaimer
    /// in `build_system_prompt` so the model knows it's not live.
    /// See `crate::ctx::env`.
    pub env_snapshot: crate::ctx::EnvSnapshot,

    // Execution state
    pub phase: AgentPhase,
    pub turn_tokens: usize,
    pub total_tokens: usize,
    pub turn_start: Option<Instant>,

    /// Session identifier for hook context.
    /// Generated once at construction and survives ReloadConfig.
    pub session_id: String,

    // Per-turn counters
    tool_call_count: usize,
    /// LLM round-trip count (standard "turn" metric).
    /// Each iteration of run_turn_loop = 1 turn, regardless of how many
    /// tools were called in that iteration.
    turn_count: usize,
    /// Optional hard cap on turn_count. When Some(n), run_turn_loop exits
    /// via finish_turn(TurnStopReason::TurnLimit) before starting turn n+1.
    /// None = unbounded (historical behavior — loop stops naturally when the
    /// LLM returns no tool calls, or when the step budget is hit).
    max_turns: Option<usize>,
    retry_count: usize,
    /// Independent retry budget for empty-response failures (provider
    /// returned 200 OK with no text and no tool calls). Kept separate from
    /// `retry_count` because an empty response is an instant, transient
    /// upstream hiccup that usually recovers on a quick resend — it gets
    /// more attempts and a far shorter backoff than the generic error path.
    /// Reset alongside `retry_count` on each new user message / goal round.
    empty_response_retries: usize,
    /// Tool-call IDs already forwarded to the renderer in the current
    /// user turn. Cleared at the start of each new user message (in
    /// `process_user_input` per-turn reset block).
    ///
    /// Dedupes the case where 429 / stream-ended retries cause the
    /// runner to re-emit `TurnEvent::ToolCallStarted` with the same
    /// provider-assigned tool_call_id. Without this, every retry adds
    /// a duplicate `▸ Bash(...)` row in scrollback — at extreme rate-
    /// limit scenarios users see the same command 30+ times.
    emitted_tool_ids: std::collections::HashSet<String>,

    // Approval channel endpoints for InteractivePermissionDecider
    /// Receives approval requests from InteractivePermissionDecider
    approval_req_rx: mpsc::UnboundedReceiver<crate::turn::permission::ApprovalRequest>,
    /// Sends approval decisions back to InteractivePermissionDecider
    approval_resp_tx: mpsc::UnboundedSender<PermissionDecision>,
    /// Last approval request (for ApproveToolAlways — need to know which tool)
    last_approval_request: Option<crate::turn::permission::ApprovalRequest>,

    // Cancellation token for the current turn
    cancel_token: CancellationToken,

    /// Cancellation token for the background code-graph indexer.
    /// Fresh-cancelled-then-rebuilt on every `/cd` so a prior indexer
    /// (still parsing files) yields CPU instead of racing the new one.
    indexer_cancel: CancellationToken,

    /// Guard against concurrent `/background` tasks. Set on dispatch,
    /// cleared by the spawned task when it completes. Acquire/Release
    /// ordering so the cleared write is visible to the next dispatcher
    /// check on a different thread.
    background_running: std::sync::Arc<AtomicBool>,

    /// Discipline tracking — all counters for loop detection, stagnation,
    /// error streaks, and tool usage patterns. Extracted from AgentLoop to
    /// reduce God Object complexity (was 22 fields inline).
    pub(crate) discipline_state: DisciplineState,

    /// Files read this turn (for tracking read-but-not-edit waste)
    files_read_this_turn: Vec<String>,
    /// Files edited/written this turn
    files_edited_this_turn: Vec<String>,
    /// The user's original task message for this turn (re-injected as reminders).
    current_task: String,
    /// Name of the tool currently being executed (for smart truncation).
    current_tool_name: String,

    /// Last git checkpoint ref (SHA) for /undo rollback.
    pub last_checkpoint: Option<String>,

    /// Most recently edited file (absolute path). Injected as full content in system prompt
    /// so the model doesn't need to re-read it next turn. Capped at ~6K tokens.
    active_file: Option<PathBuf>,

    /// Pending user input appended during streaming. Injected before next LLM call.
    pending_input: Option<String>,
    /// Session-level file tracker: all files read/edited across the entire session.
    /// Used to build the "working set" — tree-sitter skeletons injected before each LLM call.
    /// This replaces the old recent_file_cache with a smarter, budget-aware approach.
    session_files: std::collections::HashMap<String, PathBuf>,
    /// Whether planning phase is active (first LLM call without tools to force a plan).
    planning_phase: bool,
    /// Remaining read-only turns for diagnosis tasks. When > 0, only read-only tools are available.
    /// Decremented each turn. Forces the model to read code before curl/edit.
    diagnosis_read_only_turns: usize,
    /// Plan mode: restrict to read-only tools and inject planning instructions.
    /// Toggled via `/plan` command or `SetPlanMode` agent command.
    pub plan_mode: bool,
    /// Current task type — drives dynamic prompt selection and planning.
    /// ATLAS-style subtask driver: decomposes plan into per-file subtasks.
    subtask_driver: subtask_driver::SubtaskDriver,
    /// Original plan text from model's first response — used for plan adherence reminders.
    plan_text: Option<String>,

    /// Completion detection: model indicated task is done.
    /// Set when text contains completion marker AND recent tool results all succeeded.
    /// Next turn: if model only does read/grep → stop (unnecessary verification).
    /// If model does edit/write/bash → cancel grace, continue (more substantive work).
    #[allow(dead_code)]
    completion_grace: bool,

    /// Track whether all tool results in the last turn were successful.
    /// Used by completion detection: only trigger grace when tools succeeded.
    #[allow(dead_code)]
    last_turn_tools_all_success: bool,

    // Skill registry — provides descriptions for system prompt and powers use_skill tool
    skill_registry: std::sync::Arc<std::sync::RwLock<SkillRegistry>>,

    // Code graph background indexer channel
    reindex_tx: Option<mpsc::UnboundedSender<PathBuf>>,

    // Datalog writer — writes per-turn markdown logs to datalog/ directory.
    datalog: crate::turn::datalog::DatalogWriter,

    /// SystemPrompt hook extensions cached before each turn.
    /// Populated by `refresh_hook_extensions` before calling `build_system_prompt`.
    /// Vec<String> — empty means no extensions. Reset at session start and on reload.
    cached_system_prompt_extensions: Vec<String>,

    /// Session-frozen system prompt (LLM prefix-cache stability — the system
    /// message is messages[0], so any byte change zeroes the whole cache).
    /// Built once via `build_system_prompt`, reused verbatim every turn.
    /// `None` = needs (re)build. Invalidated only at explicit contract
    /// boundaries: SetPlanMode, ClearConversation, ReloadConfig, change_dir.
    cached_system_prompt: Option<String>,

    /// Active /goal state. Default is inactive (no goal set).
    goal: goal::GoalState,

    /// Goal evaluator — built lazily in `run()` from evaluator_provider config.
    goal_evaluator: Option<goal_evaluator::GoalEvaluator>,

    /// Stop reason from the most recent turn loop iteration. Updated by
    /// `finish_turn` so `run_turn_loop` can return it.
    last_stop_reason: TurnStopReason,

    /// Set by the inner-turn Shutdown handler so the outer command loop
    /// can break after `handle_send_message` / `handle_local_shell`
    /// returns. Without this, Shutdown consumed inside a turn (e.g. /quit
    /// during Streaming) only cancels the current LLM call and the agent
    /// stays alive waiting for the next command — the user has to type
    /// /quit a second time to actually exit.
    shutdown_requested: std::sync::atomic::AtomicBool,

    // Channels
    cmd_rx: mpsc::UnboundedReceiver<AgentCommand>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
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

#[derive(Clone)]
pub struct AgentRuntimeFactory {
    pub config: Config,
    pub working_dir: PathBuf,
    pub telemetry: std::sync::Arc<atomcode_telemetry::Telemetry>,
    pub lsp: Option<std::sync::Arc<crate::lsp::manager::LspManager>>,
    pub shared_tools: std::sync::Arc<ToolRegistry>,
    pub skill_registry: std::sync::Arc<std::sync::RwLock<SkillRegistry>>,
    pub max_turns: Option<usize>,
    /// Propagated to every runtime this factory spawns so a `/session` /
    /// `/resume` / `/bg` re-spawn inherits the launch-time `-y` bypass (issue #714).
    pub dangerously_skip_permissions: bool,
    runtime_counter: std::sync::Arc<AtomicU64>,
}

impl AgentRuntimeFactory {
    pub fn set_config(&mut self, config: Config) {
        self.config = config;
    }

    pub fn set_working_dir(&mut self, working_dir: PathBuf) {
        self.working_dir = working_dir;
    }

    fn next_runtime_label(&self) -> String {
        let id = self.runtime_counter.fetch_add(1, Ordering::Relaxed) + 1;
        format!("runtime-{id}")
    }

    pub fn build_provider(&self) -> Box<dyn LlmProvider> {
        let Some(provider_config) = self.config.providers.get(&self.config.default_provider) else {
            return crate::provider::unavailable_provider(
                "未配置 provider。请使用 /provider 添加 provider 后再试。",
            );
        };

        match crate::provider::create_provider(provider_config) {
            Ok(provider) => provider,
            Err(e) => crate::provider::unavailable_provider(format!("provider 初始化失败: {e:#}")),
        }
    }

    pub fn spawn_runtime(
        &self,
        conversation: Conversation,
    ) -> (
        AgentClient,
        tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
    ) {
        let provider = self.build_provider();
        let mut tool_context = ToolContext::with_telemetry(
            self.working_dir.clone(),
            "default",
            self.telemetry.clone(),
        );
        let runtime_label = self.next_runtime_label();
        tool_context.file_history = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::tool::file_history::FileHistory::new(&runtime_label),
        ));
        tool_context.lsp = self.lsp.clone();

        let (mut loop_, handle) = AgentLoop::new_with_shared_parts_skip(
            self.config.clone(),
            provider,
            self.shared_tools.clone(),
            self.skill_registry.clone(),
            Some(runtime_label),
            tool_context,
            conversation,
            self.dangerously_skip_permissions,
        );
        loop_.set_max_turns(self.max_turns);

        let ctx = atomcode_telemetry::CurrentContext::current();
        tokio::spawn(async move {
            atomcode_telemetry::CurrentContext::scope(ctx, || loop_.run()).await
        });

        (handle.client, handle.event_rx)
    }

    pub fn from_initial_loop(agent_loop: &AgentLoop, max_turns: Option<usize>) -> Self {
        let working_dir = agent_loop
            .turn_runner
            .context
            .working_dir
            .try_read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| PathBuf::from("."));

        Self {
            config: agent_loop.config.clone(),
            working_dir,
            telemetry: agent_loop.turn_runner.context.telemetry.clone(),
            lsp: agent_loop.turn_runner.context.lsp.clone(),
            shared_tools: agent_loop.tool_registry.clone(),
            skill_registry: agent_loop.skill_registry.clone(),
            max_turns,
            dangerously_skip_permissions: agent_loop.dangerously_skip_permissions,
            runtime_counter: std::sync::Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn new_for_test(
        config: Config,
        working_dir: PathBuf,
        shared_tools: std::sync::Arc<ToolRegistry>,
        skill_registry: std::sync::Arc<std::sync::RwLock<SkillRegistry>>,
    ) -> Self {
        let telemetry = ToolContext::new(working_dir.clone()).telemetry;
        Self {
            config,
            working_dir,
            telemetry,
            lsp: None,
            shared_tools,
            skill_registry,
            max_turns: None,
            dangerously_skip_permissions: false,
            runtime_counter: std::sync::Arc::new(AtomicU64::new(1)),
        }
    }
}


impl AgentLoop {
    /// Create a new agent loop and its corresponding UI handle.
    pub fn new(
        config: Config,
        provider: Box<dyn LlmProvider>,
        tool_registry: ToolRegistry,
        tool_context: ToolContext,
        conversation: Conversation,
    ) -> (Self, AgentHandle) {
        Self::new_with_skip_permissions(
            config,
            provider,
            tool_registry,
            tool_context,
            conversation,
            false,
        )
    }

    /// Create a new agent loop with an optional `--dangerously-skip-permissions`
    /// flag. When `skip_permissions` is true, all tool calls are auto-approved
    /// without prompting the user.
    pub fn new_with_skip_permissions(
        config: Config,
        provider: Box<dyn LlmProvider>,
        mut tool_registry: ToolRegistry,
        tool_context: ToolContext,
        conversation: Conversation,
        skip_permissions: bool,
    ) -> (Self, AgentHandle) {
        // Load skills from disk and register the use_skill tool.
        let working_dir = tool_context
            .working_dir
            .try_read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| std::path::PathBuf::from("."));

        let mut registry = SkillRegistry::new();
        let _ = registry.reload(&working_dir);
        let skill_registry = std::sync::Arc::new(std::sync::RwLock::new(registry));
        let disabled_internal: std::collections::HashSet<String> =
            std::env::var("ATOMCODE_DISABLE_TOOLS")
                .ok()
                .map(|v| {
                    v.split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default();
        let internal_enabled = |name: &str| !disabled_internal.contains(name);

        // Always register use_skill — the tool itself gracefully reports
        // "no skills available" when the registry is empty. Gating on
        // has_skills breaks Windows where skills are installed via plugins
        // that may not be present at compile time. The model only wastes
        // a turn calling use_skill with an empty registry, not 5+ turns
        // re-describing the task that a skill would have covered.
        if internal_enabled("use_skill") {
            tool_registry.register_sync(Box::new(UseSkillTool {
                registry: skill_registry.clone(),
            }));
        }

        // Graph query tools: not exposed to model (adds 5 tool definitions that
        // weak models never use correctly). Graph data is still injected automatically
        // via grep's graph header and auto_inject_graph_context — the model benefits
        // from graph without needing to call these tools directly.
        // To re-enable: set ATOMCODE_GRAPH_TOOLS=1
        if std::env::var("ATOMCODE_GRAPH_TOOLS")
            .map(|v| v == "1")
            .unwrap_or(false)
        {
            if internal_enabled("trace_callers") {
                tool_registry.register_sync(Box::new(crate::tool::trace_callers::TraceCallersTool));
            }
            if internal_enabled("trace_callees") {
                tool_registry.register_sync(Box::new(crate::tool::trace_callees::TraceCalleesTool));
            }
            if internal_enabled("trace_chain") {
                tool_registry.register_sync(Box::new(crate::tool::trace_chain::TraceChainTool));
            }
            if internal_enabled("file_dependencies") {
                tool_registry.register_sync(Box::new(crate::tool::file_deps::FileDependenciesTool));
            }
            if internal_enabled("blast_radius") {
                tool_registry.register_sync(Box::new(crate::tool::blast_radius::BlastRadiusTool));
            }
        }

        let shared_tools = std::sync::Arc::new(tool_registry);
        Self::new_from_shared_bootstrap(
            config,
            provider,
            shared_tools,
            skill_registry,
            None,
            tool_context,
            conversation,
            skip_permissions,
        )
    }

    /// Create a new runtime using the already-shared tool and skill registries.
    /// This path intentionally does not reload skills or register `use_skill` /
    /// graph tools; the initial runtime owns that one-time setup.
    pub fn new_with_shared_parts(
        config: Config,
        provider: Box<dyn LlmProvider>,
        shared_tools: std::sync::Arc<ToolRegistry>,
        skill_registry: std::sync::Arc<std::sync::RwLock<SkillRegistry>>,
        runtime_label: Option<String>,
        tool_context: ToolContext,
        conversation: Conversation,
    ) -> (Self, AgentHandle) {
        Self::new_with_shared_parts_skip(
            config,
            provider,
            shared_tools,
            skill_registry,
            runtime_label,
            tool_context,
            conversation,
            false,
        )
    }

    /// Same as [`new_with_shared_parts`] but carries the
    /// `--dangerously-skip-permissions` / `-y` bypass. Used by
    /// [`AgentRuntimeFactory::spawn_runtime`] so a runtime re-spawned for
    /// `/session` / `/resume` / `/bg` keeps the launch-time bypass instead of
    /// silently re-enabling approval prompts (issue #714).
    #[allow(clippy::too_many_arguments)]
    pub fn new_with_shared_parts_skip(
        config: Config,
        provider: Box<dyn LlmProvider>,
        shared_tools: std::sync::Arc<ToolRegistry>,
        skill_registry: std::sync::Arc<std::sync::RwLock<SkillRegistry>>,
        runtime_label: Option<String>,
        tool_context: ToolContext,
        conversation: Conversation,
        skip_permissions: bool,
    ) -> (Self, AgentHandle) {
        Self::new_from_shared_bootstrap(
            config,
            provider,
            shared_tools,
            skill_registry,
            runtime_label,
            tool_context,
            conversation,
            skip_permissions,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_from_shared_bootstrap(
        config: Config,
        provider: Box<dyn LlmProvider>,
        shared_tools: std::sync::Arc<ToolRegistry>,
        skill_registry: std::sync::Arc<std::sync::RwLock<SkillRegistry>>,
        runtime_label: Option<String>,
        mut tool_context: ToolContext,
        conversation: Conversation,
        skip_permissions: bool,
    ) -> (Self, AgentHandle) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let working_dir = tool_context
            .working_dir
            .try_read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| std::path::PathBuf::from("."));

        // Load persisted code graph from disk and share with ToolContext.
        let graph_path = working_dir.join(".atomcode").join("graph.bin");
        let code_graph = crate::graph::persist::load(&graph_path);
        let graph = std::sync::Arc::new(tokio::sync::RwLock::new(code_graph));
        tool_context.graph = graph;

        // Build approval channels for interactive permission flow
        let (approval_req_tx, approval_req_rx) = mpsc::unbounded_channel();
        let (approval_resp_tx, approval_resp_rx) = mpsc::unbounded_channel();

        let permission_store = std::sync::Arc::new(std::sync::RwLock::new(PermissionStore::new()));

        let interactive_permission =
            Box::new(crate::turn::permission::InteractivePermissionDecider::new_with_skip_permissions(
                approval_req_tx,
                approval_resp_rx,
                permission_store.clone(),
                skip_permissions,
            ));

        // Hand the registry handle to ToolContext so active-dispatch tools
        // (parallel_edit_files) can read it at execute time without
        // creating a Tool ↔ Registry Arc cycle.
        tool_context.tool_registry = Some(shared_tools.clone());
        // Convert Box → Arc so provider can be shared with sub-agents.
        let provider: std::sync::Arc<dyn LlmProvider> = std::sync::Arc::from(provider);

        // Build the datalog writer before `config` is moved into the agent below.
        let mut datalog = match runtime_label.as_deref() {
            Some(label) => crate::turn::datalog::DatalogWriter::new_with_filename_tag(
                &working_dir,
                &config.datalog,
                label,
            ),
            None => crate::turn::datalog::DatalogWriter::new(&working_dir, &config.datalog),
        };

        // Select the context-construction strategy once for this session.
        // Rebuilds on ReloadConfig when the provider changes.
        let ctx: std::sync::Arc<dyn crate::ctx::CtxBuilder> =
            match config.providers.get(&config.default_provider) {
                Some(pc) => crate::ctx::for_provider(pc),
                // Fallback for first-run / broken-config path: synthesize a
                // minimal provider so `for_provider` still gets its hands on
                // a context_window. Matches Config::default_context_window()
                // behavior (128_000) so sessions without a provider don't
                // panic before the user runs /login or /model.
                None => crate::ctx::for_provider(&crate::config::provider::ProviderConfig {
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

}),
            };

        // Initialize unified HookEngine
        let mut hook_engine = HookEngine::new();
        hook_engine.load_all(&working_dir);
        let hook_engine = std::sync::Arc::new(hook_engine);

        let turn_runner = TurnRunner {
            provider,
            tools: shared_tools.clone(),
            context: tool_context.clone(),
            config: config.clone(),
            ctx: ctx.clone(),
            permission: interactive_permission,
            hook_engine: hook_engine.clone(),
            recently_edited_files: Vec::new(),
            loop_guard: Default::default(),
            current_turn_number: 0,
        };

        // Capture session-start env snapshot (git status, branch, HEAD).
        // Blocking I/O here is fine: `new()` runs once at startup, the
        // capture is ~tens of ms for typical repos, and it's required
        // before the first turn's system prompt is assembled.
        let env_snapshot = crate::ctx::EnvSnapshot::capture(&working_dir);

        // Bootstrap session id. Reuses the single id generator
        // (`SessionId::new`, the same one Session/telemetry use) rather than
        // minting a second raw uuid here. This is only the value used before
        // the UI sends `SetSessionId` with the actual session-file id (the
        // agent is constructed before the Session exists); from then on every
        // consumer — header, datalog, telemetry, hooks — shares that id.
        let session_id = crate::session::SessionId::new().as_str().to_string();
        turn_runner.provider.set_session_id(&session_id);
        // Bootstrap the datalog + telemetry with the same id (UI's
        // SetSessionId updates all three once the real session is
        // established). This is what keeps header / datalog / event-reporting
        // on ONE id even in headless, where there's no UI bind.
        datalog.set_session_id(&session_id);
        if let Ok(uuid) = uuid::Uuid::parse_str(&session_id) {
            turn_runner.context.telemetry.set_session_id(uuid);
        }

        let agent = Self {
            conversation,
            tool_registry: shared_tools.clone(),
            turn_runner,
            permission_store,
            dangerously_skip_permissions: skip_permissions,
            config,
            ctx,
            env_snapshot,
            phase: AgentPhase::Idle,
            turn_tokens: 0,
            total_tokens: 0,
            turn_start: None,
            session_id,
            tool_call_count: 0,
            turn_count: 0,
            max_turns: None,
            retry_count: 0,
            empty_response_retries: 0,
            emitted_tool_ids: std::collections::HashSet::new(),
            approval_req_rx,
            approval_resp_tx,
            last_approval_request: None,
            cancel_token: CancellationToken::new(),
            indexer_cancel: CancellationToken::new(),
            background_running: std::sync::Arc::new(AtomicBool::new(false)),
            discipline_state: DisciplineState::default(),
            files_read_this_turn: Vec::new(),
            files_edited_this_turn: Vec::new(),
            current_task: String::new(),
            current_tool_name: String::new(),
            last_checkpoint: None,
            active_file: None,
            pending_input: None,
            planning_phase: false,
            diagnosis_read_only_turns: 0,
            plan_mode: false,
            completion_grace: false,
            last_turn_tools_all_success: false,
            subtask_driver: subtask_driver::SubtaskDriver::new(),
            plan_text: None,
            session_files: std::collections::HashMap::new(),
            skill_registry,
            reindex_tx: None,
            datalog,
            cached_system_prompt_extensions: Vec::new(),
            cached_system_prompt: None,
            goal: goal::GoalState::default(),
            goal_evaluator: None,
            last_stop_reason: TurnStopReason::Natural,
            shutdown_requested: std::sync::atomic::AtomicBool::new(false),
            cmd_rx,
            event_tx,
        };

        let client = AgentClient {
            cmd_tx,
            tool_registry: shared_tools.clone(),
            skill_registry: agent.skill_registry.clone(),
        };
        let handle = AgentHandle { client, event_rx };

        (agent, handle)
    }

    /// Set an optional hard cap on the number of LLM turns this agent will
    /// run. When the cap is reached, run_turn_loop exits via
    /// finish_turn(TurnStopReason::TurnLimit). `None` (the default) is
    /// unbounded. Used by the CLI `--max-turns` flag.
    pub fn set_max_turns(&mut self, max: Option<usize>) {
        self.max_turns = max;
    }

    /// Run the agent loop. This is the main entry point — call from a tokio task.
    /// The loop processes commands from the UI and emits events back.
    pub async fn run(mut self) {
        // Active-dispatch tool registration. The model invokes
        // `parallel_edit_files` explicitly when it judges parallel edit
        // is the right move; the framework no longer infers from text.
        // Gated on `subagent.enabled` so users can disable fork
        // dispatch via `/config subagent.enabled false` without code
        // changes — the tool simply isn't advertised to the model.
        // Registered here (not in `new()`) because `register_arc` is
        // async and `new()` is sync.
        if self.config.subagent.enabled {
            let tool = crate::tool::parallel_edit::ParallelEditTool {
                provider: self.turn_runner.provider.clone(),
                config: self.config.clone(),
                event_tx: self.event_tx.clone(),
            };
            self.tool_registry
                .register_arc("parallel_edit_files".to_string(), std::sync::Arc::new(tool))
                .await;
        }

        // Spawn background code graph indexer
        {
            let working_dir = self.turn_runner.context.working_dir.read().await.clone();
            let graph = self.turn_runner.context.graph.clone();
            let (reindex_tx, mut reindex_rx) = mpsc::unbounded_channel::<PathBuf>();
            let wd_for_indexer = working_dir.clone();
            let cancel = self.indexer_cancel.clone();
            tokio::spawn(async move {
                let mut indexer =
                    crate::graph::indexer::GraphIndexer::new(graph.clone(), wd_for_indexer.clone());
                indexer.index_all(cancel).await;
                // Persist after initial indexing
                let gp = wd_for_indexer.join(".atomcode").join("graph.bin");
                if let Ok(g) = graph.try_read() {
                    let _ = crate::graph::persist::save(&g, &gp);
                }
                // Listen for reindex requests
                while let Some(path) = reindex_rx.recv().await {
                    indexer.reindex_file(&path).await;
                }
            });
            self.reindex_tx = Some(reindex_tx);
        }

        // --- SessionStart Hook ---
        let session_ctx = crate::hook::SessionContext {
            session_id: self.session_id.clone(),
            working_dir: self
                .turn_runner
                .context
                .working_dir
                .try_read()
                .map(|g| g.to_string_lossy().to_string())
                .unwrap_or_default(),
            model_name: self.turn_runner.provider.model_name().to_string(),
            provider_name: self.config.default_provider.clone(),
        };
        let session_msgs = self.turn_runner.hook_engine.trigger_session_start(&session_ctx).await;
        if !session_msgs.is_empty() {
            // SessionStart hook Modified(msg) are collected and merged into the
            // cached system-prompt extensions so the LLM sees them.  Unlike
            // SystemPromptHook (which runs every turn), these are injected once
            // at session start and persist for the session lifetime.
            self.cached_system_prompt_extensions.extend(session_msgs);
        }

        // Build goal evaluator from evaluator_provider config (or fall back
        // to the main provider when not configured).
        //
        // `create_provider` may run blocking I/O (OAuth refresh via
        // `reqwest::blocking::Client`) when the provider config doesn't
        // carry a static `api_key` (e.g. atomgit endpoints). Calling that
        // from this async `run()` context panics with "Cannot drop a
        // runtime in a context where blocking is not allowed". Wrap the
        // construction in `spawn_blocking` so the blocking client lives
        // and dies on a blocking-allowed worker thread.
        //
        // Failure of an *explicitly configured* evaluator_provider is
        // surfaced to the TUI via AgentEvent::Error rather than silently
        // falling back to the main model — otherwise the user thinks they
        // are paying for a cheap Haiku eval but is actually paying for
        // the main (Opus-class) model to evaluate every round (CR C6).
        {
            let eval_provider: std::sync::Arc<dyn crate::provider::LlmProvider> = match self
                .config
                .evaluator_provider
                .clone()
            {
                None => self.turn_runner.provider.clone(),
                Some(key) => match self.config.providers.get(&key).cloned() {
                    None => {
                        let msg = format!(
                            "[goal] evaluator_provider '{key}' not found in [providers] — using main model. Add `[providers.{key}]` to config or remove `evaluator_provider` to silence this warning."
                        );
                        eprintln!("{msg}");
                        let _ = self.event_tx.send(AgentEvent::Error {
                            error: msg,
                            snapshot: self.conversation.snapshot(),
                        });
                        self.turn_runner.provider.clone()
                    }
                    Some(pc) => {
                        let build_result = tokio::task::spawn_blocking({
                            let pc = pc.clone();
                            move || crate::provider::create_provider(&pc)
                        })
                        .await;
                        match build_result {
                            Ok(Ok(p)) => std::sync::Arc::from(p),
                            Ok(Err(e)) => {
                                let msg = format!(
                                    "[goal] evaluator_provider '{key}' failed to initialise: {e:#} — using main model for goal evaluation"
                                );
                                eprintln!("{msg}");
                                let _ = self.event_tx.send(AgentEvent::Error {
                            error: msg,
                            snapshot: self.conversation.snapshot(),
                        });
                                self.turn_runner.provider.clone()
                            }
                            Err(join_err) if join_err.is_panic() => {
                                let msg = format!(
                                    "[goal] evaluator_provider '{key}' construction panicked: {join_err} — using main model"
                                );
                                eprintln!("{msg}");
                                let _ = self.event_tx.send(AgentEvent::Error {
                            error: msg,
                            snapshot: self.conversation.snapshot(),
                        });
                                self.turn_runner.provider.clone()
                            }
                            Err(join_err) => {
                                let msg = format!(
                                    "[goal] evaluator_provider '{key}' construction cancelled: {join_err} — using main model"
                                );
                                eprintln!("{msg}");
                                let _ = self.event_tx.send(AgentEvent::Error {
                            error: msg,
                            snapshot: self.conversation.snapshot(),
                        });
                                self.turn_runner.provider.clone()
                            }
                        }
                    }
                },
            };
            self.goal_evaluator = Some(goal_evaluator::GoalEvaluator::new(eval_provider));
        }

        while let Some(cmd) = self.cmd_rx.recv().await {
            crate::ctrace!("AGT", "outer cmd_rx pop: {:?}", std::mem::discriminant(&cmd));
            match cmd {
                AgentCommand::SendMessage { text, images, image_markers } => {
                    self.handle_send_message(text, images, image_markers).await;
                    if self.shutdown_requested.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                }
                AgentCommand::LocalShell { cmd } => {
                    self.handle_local_shell(cmd).await;
                    if self.shutdown_requested.load(std::sync::atomic::Ordering::Acquire) {
                        break;
                    }
                }
                AgentCommand::Cancel => {
                    crate::ctrace!("AGT", "outer Cancel -> cancel_token.cancel() (was_cancelled={})", self.cancel_token.is_cancelled());
                    self.cancel_token.cancel();
                    self.cancel_token = CancellationToken::new();
                    self.phase = AgentPhase::Idle;
                    // Cancel the current turn — preserve completed content, backfill
                    // (cancelled) for unpaired tool calls, and mark turn as Completed.
                    self.conversation.cancel_current_turn();
                    // Ctrl+C/Esc must also halt any /goal auto-continuation
                    // — otherwise the evaluator schedules a new turn right
                    // after this cancel and the user sees endless
                    // "(cancelled)" cycles (CR C3).
                    if self.goal.active {
                        self.finalize_goal_cancelled();
                    }
                    // Sync the preserved messages to TUI
                    let snapshot = self.conversation.snapshot();
                    let _ = self.event_tx.send(AgentEvent::TurnCancelled { snapshot });
                }
                AgentCommand::ApproveTool => {
                    // Approval handled inside run_turn_loop via channels
                }
                AgentCommand::ApproveToolAlways => {
                    // Approval handled inside run_turn_loop via channels
                }
                AgentCommand::DenyTool => {
                    // Denial handled inside run_turn_loop via channels
                }
                AgentCommand::ReloadConfig(new_config) => {
                    let old_provider_name = self.config.default_provider.clone();
                    let old_type = self
                        .config
                        .providers
                        .get(&old_provider_name)
                        .map(|p| p.provider_type.clone());
                    self.config = new_config;
                    // Config/instructions/model may have changed → rebuild the
                    // frozen system prompt on the next turn.
                    self.cached_system_prompt = None;
                    // Rebuild hook engine from config files.
                    let wd = self
                        .turn_runner
                        .context
                        .working_dir
                        .try_read()
                        .map(|g| g.clone())
                        .unwrap_or_else(|_| std::path::PathBuf::from("."));
                    let mut new_engine = HookEngine::new();
                    new_engine.load_all(&wd);
                    self.turn_runner.hook_engine = std::sync::Arc::new(new_engine);
                    let new_provider_name = self.config.default_provider.clone();
                    let new_type = self
                        .config
                        .providers
                        .get(&new_provider_name)
                        .map(|p| p.provider_type.clone());

                    let should_clear = reload_should_clear_conversation(
                        &old_provider_name,
                        old_type.as_deref(),
                        &new_provider_name,
                        new_type.as_deref(),
                    );
                    if should_clear {
                        self.conversation.messages.clear();
                        self.conversation.turn_tracker =
                            crate::conversation::turn::TurnTracker::new();
                        self.session_files.clear();
                    }

                    if let Some(provider_config) = self.config.providers.get(&new_provider_name) {
                        // Rebuild the context strategy for the new provider.
                        // Selected once per provider; per-model customizations
                        // (e.g. Ollama schema trimming, Claude cache markers)
                        // take effect from the next turn. Assign the same
                        // `Arc` to both `self.ctx` and `self.turn_runner.ctx`
                        // so datalog and the send path stay locked together.
                        let new_ctx = crate::ctx::for_provider(provider_config);
                        self.ctx = new_ctx.clone();
                        self.turn_runner.ctx = new_ctx;
                        match crate::provider::create_provider(provider_config) {
                            Ok(new_provider) => {
                                self.turn_runner.provider = std::sync::Arc::from(new_provider);
                                self.turn_runner.config = self.config.clone();
                            }
                            Err(e) => {
                                let msg = format!("{:#}", e);
                                let is_auth_gap = msg.contains("Not logged in")
                                    || msg.contains("Invalid auth.toml")
                                    || msg.contains("Token expired")
                                    || msg.contains("Token refresh failed");
                                if is_auth_gap {
                                    self.turn_runner.provider = std::sync::Arc::from(
                                        crate::provider::unavailable_provider(format!(
                                            "Provider 凭证不可用：{}。请使用 /login 完成配置后再试。",
                                            msg
                                        )),
                                    );
                                    self.turn_runner.config = self.config.clone();
                                } else {
                                    let _ = self.event_tx.send(AgentEvent::TextDelta(format!(
                                        "**Warning: failed to reload provider: {}**\n\n",
                                        e
                                    )));
                                }
                            }
                        }
                    } else {
                        self.turn_runner.provider =
                            std::sync::Arc::from(crate::provider::unavailable_provider(
                                "No active provider configured. Use /provider to add one.",
                            ));
                        self.turn_runner.config = self.config.clone();
                    }
                }
                AgentCommand::ChangeDir(path) => {
                    self.change_dir(&path).await;
                }
                AgentCommand::AppendInput(text) => {
                    // Queue user input to be injected before the next LLM call.
                    if let Some(ref mut existing) = self.pending_input {
                        existing.push('\n');
                        existing.push_str(&text);
                    } else {
                        self.pending_input = Some(text);
                    }
                }
                AgentCommand::ClearConversation => {
                    // Clear the conversation history in the agent loop.
                    self.conversation = Conversation::new();
                    self.datalog.clear();
                    // New session → re-snapshot the system prompt next turn.
                    self.cached_system_prompt = None;
                    // session_id is NOT reset here: /session pairs this with
                    // a SetSessionId carrying the new session file's id, and
                    // that's the single source of truth for the header.
                }
                AgentCommand::SetSessionId(id) => {
                    self.session_id = id;
                    self.turn_runner
                        .provider
                        .set_session_id(&self.session_id);
                    self.datalog.set_session_id(&self.session_id);
                    if let Ok(uuid) = uuid::Uuid::parse_str(&self.session_id) {
                        self.turn_runner.context.telemetry.set_session_id(uuid);
                    }
                }
                AgentCommand::SetConversation(snapshot) => {
                    // Set conversation state from a resumed session.
                    self.conversation = Conversation::from_snapshot(snapshot);
                    // NOTE: deliberately do NOT regenerate session_id here.
                    // SetConversation also fires on `-c`/`--continue` auto-restore
                    // and `/resume` of the current session — those CONTINUE an
                    // existing conversation, so a new id would fragment one
                    // logical session into two on the gateway. Only an explicit
                    // fresh start (ClearConversation: /session, /clear) resets.
                }
                AgentCommand::UndoToPrompt { nth } => {
                    let available = self.conversation.prompt_count();
                    let target = nth.unwrap_or(available);
                    match self.conversation.undo_to_prompt(target) {
                        Some(restored_prompt) => {
                            let _ = self.event_tx.send(AgentEvent::ConversationTruncated {
                                snapshot: self.conversation.snapshot(),
                                restored_prompt,
                                target_n: target,
                                prompts_before: available,
                            });
                        }
                        None => {
                            let _ = self.event_tx.send(AgentEvent::UndoFailed {
                                requested: target,
                                available,
                            });
                        }
                    }
                }
                AgentCommand::SetPlanMode(enabled) => {
                    let changed = self.plan_mode != enabled;
                    self.plan_mode = enabled;
                    // Plan mode is communicated to the model via a ONE-TIME
                    // synthetic history message, NOT the system prompt — keeping
                    // it out of messages[0] is what stops a plan-mode toggle from
                    // zeroing the whole prefix cache. The read-only tool gating
                    // (see `use_read_only`) enforces the constraint every turn
                    // regardless. We do NOT touch cached_system_prompt here.
                    if changed {
                        let note = if enabled {
                            "[PLAN MODE ACTIVATED] You are now in plan mode. Only read-only \
                             tools are available — you MUST NOT edit, create, or delete any \
                             files. Explore and analyze the codebase, then present a detailed \
                             implementation plan for the user to review before making any changes."
                        } else {
                            "[PLAN MODE ENDED] Plan mode is off. You may now edit files and \
                             carry out the plan."
                        };
                        self.conversation.add_synthetic_user_message(note);
                    }
                }
                AgentCommand::Compact { prompt } => {
                    self.run_compact(prompt).await;
                }
                AgentCommand::Remember { content, global } => {
                    use crate::config::memory::MemoryStore;
                    let store = if global {
                        MemoryStore::global()
                    } else {
                        let wd = self
                            .turn_runner
                            .context
                            .working_dir
                            .try_read()
                            .map(|g| g.clone())
                            .unwrap_or_default();
                        MemoryStore::project(&wd)
                    };
                    match store.append(&content) {
                        Ok(_) => {
                            let scope = if global { "global" } else { "project" };
                            let _ = self.event_tx.send(AgentEvent::TextDelta(format!(
                                "(remembered in {} memory: {})\n",
                                scope, content
                            )));
                        }
                        Err(e) => {
                            let _ = self.event_tx.send(AgentEvent::TextDelta(format!(
                                "(failed to save memory: {})\n",
                                e
                            )));
                        }
                    }
                }
                AgentCommand::Forget { keyword } => {
                    use crate::config::memory::MemoryStore;
                    let wd = self
                        .turn_runner
                        .context
                        .working_dir
                        .try_read()
                        .map(|g| g.clone())
                        .unwrap_or_default();
                    let global = MemoryStore::global();
                    let project = MemoryStore::project(&wd);
                    let g_matches = global.find_matching(&keyword);
                    let p_matches = project.find_matching(&keyword);
                    if g_matches.is_empty() && p_matches.is_empty() {
                        let _ = self.event_tx.send(AgentEvent::TextDelta(format!(
                            "(no memory entries matching '{}')\n",
                            keyword
                        )));
                    } else {
                        let mut msg = String::new();
                        for entry in &g_matches {
                            msg.push_str(&format!("  [global] - {}\n", entry));
                        }
                        for entry in &p_matches {
                            msg.push_str(&format!("  [project] - {}\n", entry));
                        }
                        let g_result = global.remove_matching(&keyword);
                        let p_result = project.remove_matching(&keyword);
                        if g_result.is_err() || p_result.is_err() {
                            msg.push_str(
                                "(warning: some entries could not be removed from disk)\n",
                            );
                        }
                        let total = g_matches.len() + p_matches.len();
                        msg.push_str(&format!(
                            "(removed {} matching entr{})\n",
                            total,
                            if total == 1 { "y" } else { "ies" }
                        ));
                        let _ = self.event_tx.send(AgentEvent::TextDelta(msg));
                    }
                }
                AgentCommand::ShowMemory => {
                    use crate::config::memory::MemoryStore;
                    let wd = self
                        .turn_runner
                        .context
                        .working_dir
                        .try_read()
                        .map(|g| g.clone())
                        .unwrap_or_default();
                    let global = MemoryStore::global();
                    let project = MemoryStore::project(&wd);
                    let g_entries = global.load();
                    let p_entries = project.load();
                    if g_entries.is_empty() && p_entries.is_empty() {
                        let _ = self.event_tx.send(AgentEvent::TextDelta(
                            "(no memories saved yet — use /remember <fact> to add one)\n"
                                .to_string(),
                        ));
                    } else {
                        let mut msg = String::new();
                        if !g_entries.is_empty() {
                            msg.push_str(&format!("  [Global] ({})\n", global.path().display()));
                            for e in &g_entries {
                                msg.push_str(&format!("    - {}\n", e));
                            }
                        }
                        if !p_entries.is_empty() {
                            msg.push_str(&format!("  [Project] ({})\n", project.path().display()));
                            for e in &p_entries {
                                msg.push_str(&format!("    - {}\n", e));
                            }
                        }
                        let _ = self.event_tx.send(AgentEvent::TextDelta(msg));
                    }
                }
                AgentCommand::Background { task } => {
                    // AcqRel: pair with the spawned task's Release store on
                    // completion so the next dispatcher sees the cleared flag.
                    if self.background_running.swap(true, Ordering::AcqRel) {
                        let _ = self.event_tx.send(AgentEvent::Error {
                            error: "A background task is already running. Wait for it to finish."
                                .to_string(),
                            snapshot: self.conversation.snapshot(),
                        });
                    } else {
                        let provider = self.turn_runner.provider.clone();
                        let tools = self.turn_runner.tools.clone();
                        let context = self.turn_runner.context.clone();
                        let context_for_commit = context.clone();
                        let config = self.config.clone();
                        let ctx = self.ctx.clone();
                        let event_tx = self.event_tx.clone();
                        let flag = self.background_running.clone();
                        tokio::spawn(async move {
                            let result = background::run_background_task(
                                &task,
                                provider,
                                tools,
                                context,
                                config,
                                ctx,
                                event_tx.clone(),
                            )
                            .await;
                            if let AgentEvent::BackgroundComplete {
                                files_edited,
                                success: true,
                                ..
                            } = &result
                            {
                                if !files_edited.is_empty() {
                                    let wd = context_for_commit
                                        .working_dir
                                        .try_read()
                                        .map(|g| g.clone())
                                        .unwrap_or_default();
                                    match git_auto_commit::auto_commit_edited_files(
                                        &wd,
                                        files_edited,
                                    ) {
                                        git_auto_commit::AutoCommitOutcome::Committed {
                                            sha,
                                            message,
                                        } => {
                                            let _ = event_tx.send(AgentEvent::TextDelta(format!(
                                                "\n[auto-commit {sha}] {message}\n"
                                            )));
                                        }
                                        git_auto_commit::AutoCommitOutcome::Failed { reason } => {
                                            let _ = event_tx.send(AgentEvent::TextDelta(format!(
                                                "\n[auto-commit skipped] {reason}\n"
                                            )));
                                        }
                                        git_auto_commit::AutoCommitOutcome::Skipped { .. } => {}
                                    }
                                }
                            }
                            let _ = event_tx.send(result);
                            flag.store(false, Ordering::Release);
                        });
                    }
                }
                AgentCommand::RefreshContextStats => {
                    let system_prompt = self.build_system_prompt();
                    let (msgs, _) = self
                        .ctx
                        .build_messages(&self.conversation, &system_prompt, "");
                    self.emit_rich_context_stats(&self.conversation, &msgs)
                        .await;
                }
                AgentCommand::ReloadHooks => {
                    // Triggered by /plugin install|uninstall in the TUI so
                    // newly-contributed hooks (especially UserPromptSubmit)
                    // fire on the very next user message instead of waiting
                    // for /cd or restart.
                    let wd = self
                        .turn_runner
                        .context
                        .working_dir
                        .try_read()
                        .map(|g| g.clone())
                        .unwrap_or_else(|_| std::path::PathBuf::from("."));
                    let mut new_engine = HookEngine::new();
                    new_engine.load_all(&wd);
                    self.turn_runner.hook_engine = std::sync::Arc::new(new_engine);
                }
                AgentCommand::SyncMessages => {
                    let snapshot = self.conversation.snapshot();
                    let _ = self.event_tx.send(AgentEvent::MessagesSync { snapshot });
                }
                AgentCommand::SetGoal { condition } => {
                    let max_rounds = std::env::var("ATOMCODE_GOAL_MAX_ROUNDS")
                        .ok()
                        .and_then(|s| s.trim().parse::<u32>().ok())
                        .filter(|&n| n > 0);
                    let max_duration = std::env::var("ATOMCODE_GOAL_MAX_DURATION_SECS")
                        .ok()
                        .and_then(|s| s.trim().parse::<u64>().ok())
                        .filter(|&n| n > 0)
                        .map(std::time::Duration::from_secs);
                    self.goal = goal::GoalState::new_with_limits(condition, max_rounds, max_duration);
                    self.emit_goal_update(true, None);
                }
                AgentCommand::ClearGoal => {
                    let prev_active = self.goal.active;
                    self.goal.clear();
                    if prev_active {
                        self.emit_goal_update(false, Some("cleared by user".into()));
                    }
                }
                AgentCommand::Shutdown => {
                    // --- SessionEnd Hook ---
                    let session_ctx = crate::hook::SessionContext {
                        session_id: self.session_id.clone(),
                        working_dir: self
                            .turn_runner
                            .context
                            .working_dir
                            .try_read()
                            .map(|g| g.to_string_lossy().to_string())
                            .unwrap_or_default(),
                        model_name: self.turn_runner.provider.model_name().to_string(),
                        provider_name: self.config.default_provider.clone(),
                    };
                    self.turn_runner.hook_engine.trigger_session_end(&session_ctx).await;
                    break;
                }
            }
        }
    }

    // -------------------------------------------------------------------------
    // Core agent logic
    // -------------------------------------------------------------------------

    async fn handle_send_message(
        &mut self,
        mut content: String,
        images: Vec<crate::conversation::message::ImagePart>,
        image_markers: Vec<usize>,
    ) {
        self.current_task = content.clone();

        if let Some(reason) = self.turn_runner.provider.availability_error() {
            let _ = self.event_tx.send(AgentEvent::Error {
                error: reason.to_string(),
                snapshot: self.conversation.snapshot(),
            });
            self.finish_turn(TurnStopReason::Error);
            return;
        }

        // ── UserPromptSubmit hooks ──
        // Run before any preprocessing so plugin hooks see the raw user
        // input. A hook can either block the turn (CC `decision: "block"`
        // or non-zero exit) or inject extra context that we splice into
        // the user message before the LLM sees it.
        let cwd = self
            .turn_runner
            .context
            .working_dir
            .try_read()
            .map(|g| g.display().to_string())
            .unwrap_or_default();
        match self
            .turn_runner
            .hook_engine
            .trigger_user_prompt_submit(&content, &self.session_id, &cwd)
            .await
        {
            crate::hook::UserPromptHookResult::Continue => {}
            crate::hook::UserPromptHookResult::Inject(extra) => {
                content.push_str("\n\n");
                content.push_str(&extra);
            }
            crate::hook::UserPromptHookResult::Block(reason) => {
                let _ = self.event_tx.send(AgentEvent::Error {
                    error: format!("hook blocked: {}", reason),
                    snapshot: self.conversation.snapshot(),
                });
                self.finish_turn(TurnStopReason::Error);
                return;
            }
            crate::hook::UserPromptHookResult::Warning(msg) => {
                // Non-fatal: show inline warning + status-bar hint, continue turn.
                let _ = self.event_tx.send(AgentEvent::Warning(
                    format!("Hook skipped (error): {}", msg),
                ));
                let _ = self.event_tx.send(AgentEvent::HookWarningHint(
                    format!("Hook error: {}", msg),
                ));
            }
        }
        // Detect negative feedback — user is unhappy with previous turn's work.
        let lower = content.to_lowercase();
        let negative_keywords = [
            "改错",
            "不对",
            "错了",
            "还是不行",
            "没用",
            "不是这样",
            "搞错",
            "又错",
            "白做",
            "越改越差",
            "恢复",
            "回滚",
            "撤销",
            "不行",
            "wrong",
            "not right",
            "still broken",
            "doesn't work",
            "undo",
            "revert",
            "go back",
            "that's worse",
            "stop",
            "broken",
        ];
        self.discipline_state.is_negative_feedback =
            content.chars().count() < 80 && negative_keywords.iter().any(|kw| lower.contains(kw));

        // Git checkpoint: snapshot working tree before agent starts editing.
        let wd = self
            .turn_runner
            .context
            .working_dir
            .try_read()
            .map(|g| g.clone())
            .unwrap_or_default();
        self.last_checkpoint = git_checkpoint::create_checkpoint(&wd);

        // Reset ctx_budget_hint to full window at start of each user message.
        // Without this, the first tool call in a new turn reads the stale budget
        // from the previous turn's last LLM call (when ctx was full), causing
        // 670-line files to skeleton when there's plenty of room.
        //
        // Read from `self.ctx` not `self.config` — ctx applies defensive
        // clamps (e.g. OllamaCtx floors at 4K) that config's raw
        // `context_window` doesn't reflect. Using config would tell
        // read_file "you have 128K" when actual budget is 4K.
        self.turn_runner
            .context
            .ctx_budget_hint
            .store(self.ctx.ctx_window(), std::sync::atomic::Ordering::Relaxed);

        // Auto-diagnose: if user mentions error keywords, scan logs and attach findings.
        // This gives the model the real error from Turn 1, instead of spending 3-5 turns grepping.
        let enriched = self.auto_diagnose_errors(&content).await;
        // Extract and store exception signature for recurrence detection across turns.
        if let Some(pos) = enriched.find("<!-- diag_exception:") {
            let rest = &enriched[pos + 20..];
            if let Some(end) = rest.find(" -->") {
                self.discipline_state.last_diagnosed_error = rest[..end].to_string();
            }
        }
        // Strip the hidden marker before adding to conversation
        let clean = if let Some(pos) = enriched.find("\n<!-- diag_exception:") {
            enriched[..pos].to_string()
        } else {
            enriched
        };

        // ── Task boundary cleanup ──
        // New user message = new task. Compress stale context from the
        // previous task ONLY under real budget pressure — gauged by the
        // same threshold as auto-compaction (`needs_compression`), which
        // tracks `ctx_window()`.
        //
        // This used to fire unconditionally whenever messages exceeded
        // KEEP_MESSAGES, ignoring the window. On a large (e.g. 1M) window
        // that has ample room, every follow-up message then rewrote the
        // prompt prefix — collapsing provider prompt-prefix cache to ~10%
        // on the first request of each follow-up — and discarded context
        // the model could still use. Gating on budget keeps the prefix
        // byte-stable across follow-ups when there's room to spare.
        let system_prompt = self.build_system_prompt();
        let sys_tokens = system_prompt.len() / 4 + 4;
        if self.ctx.needs_compression(&self.conversation, sys_tokens) {
            // Task-boundary compression goes through the active ctx strategy.
            // No LLM call — the compressed content is already
            // one-line-per-round summaries (DefaultCtx) compact enough
            // for cold zone.
            let keep_ceiling = compression::compaction_keep_ceiling(
                &*self.ctx,
                &system_prompt,
                &self.turn_runner.tools,
                &self.conversation.cold_summaries,
            )
            .await;
            if let Some((content, n_msgs)) =
                self.ctx.compression_plan(&self.conversation, keep_ceiling)
            {
                let _ = compression::try_apply_compression(
                    &*self.ctx,
                    &mut self.conversation,
                    &system_prompt,
                    n_msgs,
                    content,
                    None, // no post-compress state at task boundary
                );
            }
        }

        // Vision preprocessing: when the active provider can't accept images
        // and the user pasted some, run them through the configured VL model
        // first and turn the result into plain text. See
        // `vision_preprocessor` module doc for the data-flow contract.
        let mut vision_warning: Option<String> = None;
        let (clean, images) = if !images.is_empty() {
            use crate::vision_preprocessor::{maybe_preprocess, PreprocessOutcome};
            match maybe_preprocess(&self.config, &*self.turn_runner.provider, &clean, &images).await {
                PreprocessOutcome::Skipped => (clean, images),
                PreprocessOutcome::Replaced { text, vl_key } => {
                    // Surface a one-line success notice (provider key in
                    // muted gray, char count for sanity-check). The full
                    // description is intentionally NOT shown in the UI —
                    // it would either be redundant with what the main
                    // model proceeds to discuss or, on bad VL output,
                    // mislead the user that "success" means useful
                    // content. Description still rides into conversation
                    // history below.
                    let _ = self.event_tx.send(AgentEvent::VisionPreprocessSuccess {
                        vl_key: vl_key.clone(),
                        char_count: text.chars().count(),
                    });
                    let merged = if clean.is_empty() {
                        format!("[图片内容（由 {vl_key} 识别）]\n{text}")
                    } else {
                        format!("{clean}\n\n[图片内容（由 {vl_key} 识别）]\n{text}")
                    };
                    (merged, Vec::new())
                }
                PreprocessOutcome::Failed { reason } => {
                    vision_warning = Some(format!(
                        "VL 预处理失败：{reason} · 图片已自动保留，可直接重试",
                    ));
                    // Layer-1 retry support: hand the image bytes back to
                    // TUIX so the user doesn't have to re-paste from
                    // clipboard. Without this the bytes are gone after
                    // submit and Ctrl+V is the only way to re-attach.
                    let _ = self.event_tx.send(AgentEvent::RestorePendingImages {
                        images: images.clone(),
                        markers: image_markers.clone(),
                    });
                    let merged = if clean.is_empty() {
                        "[图片识别失败]".to_string()
                    } else {
                        format!("{clean}\n\n[图片识别失败]")
                    };
                    (merged, Vec::new())
                }
            }
        } else {
            (clean, images)
        };
        if let Some(w) = vision_warning {
            let _ = self.event_tx.send(AgentEvent::Warning(w));
        }

        if images.is_empty() {
            self.conversation.add_user_message(&clean);
        } else {
            use crate::conversation::message::{Message, MessageContent, Role};
            let msg = Message {
                role: Role::User,
                content: MessageContent::MultiPart {
                    text: if clean.is_empty() { None } else { Some(clean.clone()) },
                    images,
                },
                synthetic: false,
            };
            let idx = self.conversation.messages.len();
            self.conversation.messages.push(msg);
            self.conversation.turn_tracker.on_user_message(idx);
        }
        self.turn_tokens = 0;
        self.tool_call_count = 0;
        self.turn_count = 0;
        self.retry_count = 0;
        self.empty_response_retries = 0;
        self.emitted_tool_ids.clear();
        self.files_read_this_turn.clear();
        self.files_edited_this_turn.clear();
        self.turn_runner.recently_edited_files.clear();
        // Cross-batch loop guard is scoped to a single user-message
        // turn — every new user message = fresh slate. See
        // `turn::loop_guard` for why this clear() is the entire
        // per-turn-only contract on the caller side.
        self.turn_runner.loop_guard.clear();
        self.discipline_state.consecutive_reads = 0;
        self.discipline_state.verify_injected = false;
        self.discipline_state.model_produced_text = false;
        self.discipline_state.silent_tool_rounds = 0;
        // Note: is_negative_feedback is set above, do not reset here.
        self.discipline_state.build_fail_count = 0;
        self.discipline_state.scouting_count = 0;
        self.discipline_state.api_confirmed_working = false;
        self.discipline_state.consecutive_edits_file = None;
        self.discipline_state.consecutive_edits_count = 0;
        self.discipline_state.sleep_count = 0;
        self.discipline_state.consecutive_verify_count = 0;
        self.discipline_state.recent_errors.clear();
        self.discipline_state.executed_cmds.clear();
        self.discipline_state.category_fail_streak.clear();
        // Reset stagnation tracking — new user message = fresh turn,
        // previous stagnation state must not carry over.
        self.discipline_state.stagnant_turns = 0;
        self.discipline_state.last_known_files = 0;
        self.discipline_state.last_targeted_reads = 0;
        self.discipline_state.targeted_read_count = 0;
        // Reset subtask driver and plan — previous turn's plan must not
        // bleed into the new turn. Without this, a text-only Q&A response
        // that mentions file names (e.g. as examples) triggers extract_from_plan,
        // and the plan completion guard then forces the loop to continue
        // editing files that were never part of the user's actual request.
        self.subtask_driver = subtask_driver::SubtaskDriver::new();
        self.plan_text = None;
        // Clear session_files on each new user message.
        // Working Set only tracks files from the CURRENT task.
        self.session_files.clear();
        self.turn_start = Some(Instant::now());
        self.cancel_token = CancellationToken::new();

        // Initialize datalog for this turn
        {
            let model_name = self.turn_runner.provider.model_name().to_string();
            // Use ctx's effective window so datalog matches what build_messages
            // actually renders with (OllamaCtx 4K floor, etc).
            self.datalog
                .begin_turn(&content, &model_name, self.ctx.ctx_window());
        }

        // State-based decisions (replaces keyword-based task_classifier).
        // Two facts, not guesses:

        // 1. Has the model read any files this session? If not → read-only first turn.
        let has_file_context =
            !self.files_read_this_turn.is_empty() || !self.files_edited_this_turn.is_empty();
        self.diagnosis_read_only_turns = if has_file_context { 0 } else { 1 };
        self.planning_phase = !has_file_context;

        // Unified prepend — no task classification, no auto-build injection.
        // Build command detection deferred to Phase 5 (LLM-inferred project config).
        let _content = format!(
            "Read the relevant code first, then plan and implement.\n\n{}",
            content
        );

        self.phase = AgentPhase::Thinking;
        let _ = self
            .event_tx
            .send(AgentEvent::PhaseChange(AgentPhase::Thinking));

        let mut stop_reason = self.run_turn_loop().await;

        // ── Goal auto-continuation loop ──
        // If user cancelled (Esc/Ctrl+C) mid-turn, halt the goal and tell
        // the TUI so the indicator clears. Previously this clear was silent
        // and left the indicator spinning forever (CR C3).
        if matches!(stop_reason, TurnStopReason::Cancelled) {
            self.finalize_goal_cancelled();
            return;
        }

        while self.goal.active && !self.goal.is_evaluator_exhausted() {
            if let Some(why) = self.goal.cap_reached() {
                self.goal.active = false;
                self.emit_goal_update(false, Some(format!("stopped: {why} — goal not met")));
                break;
            }
            if self.goal.is_unproductive_exhausted() {
                self.goal.active = false;
                self.emit_goal_update(false, Some("stopped: too many failed rounds".into()));
                break;
            }

            let summary = goal::summarize_for_goal(
                &self.conversation.messages,
                self.goal.last_eval_reason.as_deref(),
            );

            // Evaluator call. Pass our cancel_token so Esc fires immediately
            // rather than waiting the full 30s evaluator timeout (CR C5).
            let outcome = if let Some(ref evaluator) = self.goal_evaluator {
                evaluator
                    .evaluate(&self.goal.condition, &summary, &self.cancel_token)
                    .await
            } else {
                goal_evaluator::EvalOutcome {
                    result: goal::GoalResult::Error(anyhow::anyhow!(
                        "no evaluator configured"
                    )),
                    usage: None,
                }
            };

            // Accumulate evaluator cost so /goal status shows the full bill.
            let (prompt_tok, completion_tok) = match &outcome.usage {
                Some(u) => (Some(u.prompt_tokens), Some(u.completion_tokens)),
                None => (None, None),
            };
            if let Some(u) = outcome.usage.as_ref() {
                self.goal
                    .add_tokens((u.prompt_tokens + u.completion_tokens) as u64);
            }

            // Audit trail: evaluator calls happen between main turns and
            // therefore aren't covered by the per-turn datalog. Without this,
            // users debugging "why did the goal stop / why does it keep
            // looping" have no record of what the evaluator was asked or
            // what it said (CR M4).
            let (verdict_for_log, reason_for_log) = match &outcome.result {
                goal::GoalResult::Met { reason } => ("yes", reason.as_str()),
                goal::GoalResult::NotMet { reason } => ("no", reason.as_str()),
                goal::GoalResult::Error(_) => ("error", "(see TUI for details)"),
            };
            self.datalog.log_evaluator_round(
                self.goal.round + 1,
                verdict_for_log,
                reason_for_log,
                prompt_tok,
                completion_tok,
            );

            // Cancellation can fire during evaluate() — bail before injecting
            // any continuation prompt (CR C5).
            if self.cancel_token.is_cancelled() {
                self.finalize_goal_cancelled();
                return;
            }

            match outcome.result {
                goal::GoalResult::Met { reason } => {
                    self.goal.active = false;
                    self.goal.last_eval_reason = Some(reason.clone());
                    // TUI renders the "✓ Goal met: ..." banner from the
                    // GoalUpdate event itself (active=false + reason).
                    // Avoid sending an extra TextDelta — the alt-screen
                    // markdown path drops it after a TurnSeparator and the
                    // user sees nothing.
                    self.emit_goal_update(false, Some(reason));
                    break;
                }
                goal::GoalResult::NotMet { reason } => {
                    self.goal.round += 1;
                    self.goal.last_eval_reason = Some(reason.clone());
                    self.goal.evaluator_consecutive_failures = 0;
                    self.emit_goal_update(true, Some(reason.clone()));

                    // Wrap condition in a fence so negative-keyword heuristics
                    // ("stop"/"cancel"/"wrong") in the literal goal text don't
                    // trip discipline injections meant for real user feedback
                    // (CR M6).
                    let msg = goal::goal_continuation_message(&reason, &self.goal.condition);
                    self.conversation.add_user_message(&msg);

                    // Reset per-turn state for the next iteration.
                    self.turn_count = 0;
                    self.tool_call_count = 0;
                    self.retry_count = 0;
                    self.empty_response_retries = 0;
                    self.cancel_token = CancellationToken::new();
                    self.files_edited_this_turn.clear();
                    self.files_read_this_turn.clear();

                    stop_reason = self.run_turn_loop().await;
                    match stop_reason {
                        TurnStopReason::Natural => self.goal.note_productive(),
                        TurnStopReason::Cancelled => {} // handled just below
                        _ => self.goal.note_unproductive(),
                    }
                    if matches!(stop_reason, TurnStopReason::Cancelled) {
                        self.finalize_goal_cancelled();
                        return;
                    }
                }
                goal::GoalResult::Error(e) => {
                    self.goal.evaluator_consecutive_failures += 1;
                    let public = format!("{:#}", e);
                    // Surface every failure so the user sees activity rather
                    // than ~90s of silence before the final "unavailable"
                    // message lands (CR M12).
                    self.emit_goal_update(
                        true,
                        Some(format!(
                            "evaluator failed ({}/{}): {}",
                            self.goal.evaluator_consecutive_failures,
                            goal::MAX_EVAL_FAILURES,
                            public
                        )),
                    );
                    if self.goal.is_evaluator_exhausted() {
                        self.goal.active = false;
                        // TUI renders "⚠ Goal stopped: evaluator unavailable …"
                        // from GoalUpdate's last_reason — no extra TextDelta.
                        self.emit_goal_update(
                            false,
                            Some(format!("evaluator unavailable: {public}")),
                        );
                        break;
                    }
                    // Backoff so a transient blip can't burn all 3 strikes
                    // in milliseconds; cooperate with cancel during the wait.
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(2)) => {}
                        _ = self.cancel_token.cancelled() => {
                            self.finalize_goal_cancelled();
                            return;
                        }
                    }
                }
            }
        }
    }

    // needs_planning replaced by task_classifier::TaskType::needs_planning()

    // auto_diagnose_errors → diagnose.rs
    // find_file_in_project → diagnose.rs

    /// Finalize a user-cancelled turn (Esc / stop button).
    ///
    /// Shared by two paths: (1) `run_with_filter` returning
    /// `TurnResult::Cancelled` because the in-flight LLM stream was interrupted
    /// mid-turn, and (2) the top-of-loop guard in `run_turn_loop` catching a
    /// cancellation that landed *between* turns (the previous round finished as
    /// `UsedTools`/`Responded` before the cancel was observed). Both must leave
    /// the conversation in a valid state and emit `TurnCancelled` exactly once,
    /// WITHOUT emitting `TurnComplete` (which would buffer a stale "✓ done · N
    /// rounds" line that fires on the user's next submission).
    ///
    /// Sets `last_stop_reason = Cancelled` FIRST so callers (notably the /goal
    /// wrapper) see Cancelled and break out of their auto-continuation loop.
    fn finalize_cancelled(&mut self) -> TurnStopReason {
        self.last_stop_reason = TurnStopReason::Cancelled;
        // If AgentCommand::Cancel already marked the turn Completed (the outer
        // run() handler path calls cancel_current_turn itself), there is nothing
        // left to backfill — just return.
        if self.conversation.turn_tracker.active_turn().is_none() {
            return self.last_stop_reason;
        }
        // Preserve completed content + backfill (cancelled) for unpaired tool calls.
        self.conversation.cancel_current_turn();
        let snapshot = self.conversation.snapshot();
        let _ = self.event_tx.send(AgentEvent::TurnCancelled { snapshot });
        // finish_turn's bookkeeping without emitting TurnComplete (see doc above).
        self.datalog.end_turn(self.turn_tokens, self.tool_call_count);
        self.turn_start = None;
        self.phase = AgentPhase::Idle;
        let _ = self
            .event_tx
            .send(AgentEvent::PhaseChange(AgentPhase::Idle));
        self.conversation.save(&Conversation::history_path());
        self.last_stop_reason
    }

    /// Multi-turn execution loop using TurnRunner.
    /// Each iteration calls TurnRunner.run() for one LLM turn, then applies
    /// discipline (reminders, step limits) and decides whether to continue.
    async fn run_turn_loop(&mut self) -> TurnStopReason {
        loop {
            // User-cancellation guard. The cancel token stays cancelled across
            // iterations (the inner Cancel handler no longer resets it), so a
            // stop that landed while the previous round was finishing as
            // UsedTools/Responded — i.e. *between* turns, before the cancel was
            // observed by the LLM stream — is caught here instead of silently
            // starting another reasoning/tool round. Without this the user had
            // to press stop once per round to fully halt a multi-step task.
            // The token is reset to a fresh one by handle_send_message (and the
            // /goal loop) before the next run_turn_loop, so this is only true
            // for a genuine in-progress cancellation.
            if self.cancel_token.is_cancelled() {
                return self.finalize_cancelled();
            }
            // Turn budget check BEFORE incrementing, so the reported
            // turn_count equals the number of turns actually executed
            // (not including the "would-be" next turn we refuse to run).
            // The stop reason is propagated via TurnComplete.stop_reason;
            // the CLI [done] line surfaces it as `stopped=turn_limit`.
            if self.check_turn_limit() {
                self.finish_turn(TurnStopReason::TurnLimit);
                return self.last_stop_reason;
            }
            self.turn_count += 1;

            // --- 统一 TurnStart Hook (fire-and-forget) ---
            {
                let wd = self
                    .turn_runner
                    .context
                    .working_dir
                    .try_read()
                    .map(|g| g.display().to_string())
                    .unwrap_or_default();
                let turn_ctx = crate::hook::TurnStartContext {
                    turn_number: self.turn_count as u32,
                    session_id: Some(self.session_id.clone()),
                    working_dir: wd,
                    phase: format!("{:?}", self.phase).to_lowercase(),
                    has_file_context: !self.files_edited_this_turn.is_empty(),
                };
                self.turn_runner.hook_engine.trigger_on_turn_start(&turn_ctx).await;
            }

            // Decrement diagnosis read-only counter each turn.
            if self.diagnosis_read_only_turns > 0 {
                self.diagnosis_read_only_turns -= 1;
            }

            // Inject any pending user input appended during streaming.
            if let Some(input) = self.pending_input.take() {
                self.conversation
                    .add_synthetic_user_message(&format!("[Additional context from user]: {}", input));
            }

            // Planning phase: inject planning reminder on turn 3.
            // Turn 1-2: model reads files to understand the task.
            // Planning phase injection: REMOVED.
            // Was injecting "[PLAN NOW]" at turn 3, but this is arbitrary timing.
            // The system prompt WORKFLOW section already guides planning.

            // NOTE: Negative feedback injection disabled — adds a System message that
            // confuses weak models and wastes context. The model sees the user's complaint
            // directly; no extra injection needed.

            // DIAGNOSTIC STRATEGY injection removed — the model decides its own
            // debugging approach. System prompt PLAN FIRST section is sufficient.

            // Stagnation detection: REMOVED.
            // Was injecting "[STAGNATION WARNING]" after 3 turns without edits.
            // Bug: triggered after model output a completion summary (pure text,
            // no edits), preventing it from stopping. The warning was interpreted
            // as "keep working" by the model. Stagnation detection was harmful —
            // the prompt guides the model to work efficiently.

            // Refresh hook system-prompt extensions before building the prompt.
            // NOTE: this still runs every turn (unchanged), but once the
            // session-level system prompt is frozen (see build_system_prompt)
            // the refreshed value is only consumed on a cold cache — i.e. the
            // first build and after an explicit invalidation. Keeping the
            // refresh unconditional avoids an ordering hazard where a
            // build_system_prompt call on another path (e.g. RefreshContextStats)
            // could freeze a prompt before SystemPromptHook output was collected.
            self.cached_system_prompt_extensions = self
                .turn_runner
                .hook_engine
                .collect_system_prompt_extensions()
                .await;

            let system_prompt = self.build_system_prompt();
            // Per-turn reminder: empty in normal (build) mode — the verbatim
            // task rides the cadence reflection checkpoint (see
            // agent::discipline::reflection_prompt). In PLAN mode it carries a
            // standing instruction so the model keeps planning instead of
            // dumping the implementation inline (the read-only tool gate blocks
            // file writes, but not writing code into the reply).
            let turn_reminder = plan_mode_turn_reminder(self.plan_mode);
            let cancel = self.cancel_token.clone();

            // Context compression: when > 70% budget, pause and compress
            // old turns via LLM call. Keeps last 5 turns full, compressed
            // history goes to cold zone (FIFO, max 3 entries).
            {
                let state = build_post_compress_state(
                    &self.current_task,
                    &self.files_edited_this_turn,
                    &self.files_read_this_turn,
                );
                compression::maybe_compress_history(
                    &*self.ctx,
                    &mut self.conversation,
                    &*self.turn_runner.provider,
                    &self.turn_runner.tools,
                    &system_prompt,
                    state.as_deref(),
                )
                .await;
            }

            // Batch reminder: REMOVED.
            // Was injecting fake user messages ("[Batch reminder: call MULTIPLE tools...]")
            // every turn after turn 3 when last turn was single-tool. In a 24-turn session,
            // this injected 19 fake user messages that disrupted model's diagnostic focus.
            // The system prompt already contains batch guidance — injecting mid-conversation
            // user messages is counterproductive.

            // Move conversation out to avoid borrow conflicts with self in select!
            let mut conv = std::mem::take(&mut self.conversation);

            // Datalog: mark the start of a new LLM round-trip
            self.datalog.log_llm_call();

            // Rich ContextStats for `/context` + inline datalog dump.
            // The file-level request log (`log_llm_request`) now lives
            // inside `TurnRunner::run_with_filter`, paired with
            // `log_llm_response`, so any caller — AgentLoop or daemon —
            // gets symmetric request/response files. This block only
            // feeds UI state + datalog md inline debug.
            {
                let context_window = self.ctx.ctx_window();
                // Same `Arc` instance as `self.turn_runner.ctx`, so
                // `build_messages` here and in the runner produce
                // byte-identical output (same system prompt, same
                // per-model directives, same reminder placement).
                let (msgs, _) = self
                    .ctx
                    .build_messages(&conv, &system_prompt, &turn_reminder);
                let tool_defs = self.turn_runner.tools.get_definitions().await;
                // Dump request to datalog for inline debugging
                self.datalog.log_llm_dump(
                    &msgs,
                    tool_defs.len(),
                    self.turn_runner.provider.model_name(),
                    context_window,
                );

                self.emit_rich_context_stats(&conv, &msgs).await;
            }

            // Run the turn in a scoped block so all borrows of self.turn_runner
            // end before we use self.conversation again.
            let (result, mut turn_rx, context_collapsed) = {
                let (turn_tx, mut turn_rx) = mpsc::unbounded_channel::<TurnEvent>();

                // Destructure self to get split borrows — the borrow checker needs to see
                // that turn_runner and the other fields are disjoint borrows.
                let mut context_collapsed = false;
                let context_collapsed = &mut context_collapsed;
                let runner = &mut self.turn_runner;
                let cmd_rx = &mut self.cmd_rx;
                let approval_req_rx = &mut self.approval_req_rx;
                let event_tx = &self.event_tx;
                let approval_resp_tx = &self.approval_resp_tx;
                let permission_store = &self.permission_store;
                let cancel_token = &mut self.cancel_token;
                let last_approval_request = &mut self.last_approval_request;
                let pending_input = &mut self.pending_input;
                let phase = &mut self.phase;
                let model_produced_text = &mut self.discipline_state.model_produced_text;
                let current_tool_name = &mut self.current_tool_name;
                let datalog = &mut self.datalog;
                let files_edited_this_turn = &mut self.files_edited_this_turn;
                let active_file = &mut self.active_file;
                let files_read_this_turn = &mut self.files_read_this_turn;
                let consecutive_reads = &mut self.discipline_state.consecutive_reads;
                let targeted_read_count = &mut self.discipline_state.targeted_read_count;
                let last_bash_cmd = &mut self.discipline_state.last_bash_cmd;
                let session_files = &mut self.session_files;
                let reindex_tx = &self.reindex_tx;
                let emitted_tool_ids = &mut self.emitted_tool_ids;

                // Tool filtering: diagnosis phase uses read-only tools.
                // All other turns have full tool access (including edit_file).
                // EXECUTE thinking is applied INSIDE edit_file (fresh file read,
                // ±5 lines context return, fuzzy match, delta validation) —
                // not by blocking tools at the agent loop level.
                let read_only_tools: &[&str] = &[
                    "read_file",
                    "grep",
                    "glob",
                    "list_directory",
                    "web_search",
                    "web_fetch",
                    "trace_callees",
                    "trace_callers",
                    "trace_chain",
                    "file_dependencies",
                    "blast_radius",
                ];
                let use_read_only = self.plan_mode || self.diagnosis_read_only_turns > 0;
                let tool_filter: Option<&[&str]> = if use_read_only {
                    Some(read_only_tools)
                } else {
                    None // Full tool access — model can read, edit, bash, search_replace
                };
                // Sync current turn number to TurnRunner so ToolCallStartContext
                // gets the correct turn index (not a 0 placeholder).
                runner.current_turn_number = self.turn_count as u32;

                let turn_fut = runner.run_with_filter(
                    &mut conv,
                    &system_prompt,
                    &turn_reminder,
                    &turn_tx,
                    cancel,
                    tool_filter,
                );
                tokio::pin!(turn_fut);

                // Accumulate text deltas for datalog (flushed on tool call or turn end)
                let mut datalog_text_accum = String::new();

                let result = loop {
                    tokio::select! {
                        biased;

                        result = &mut turn_fut => break result,

                        Some(event) = turn_rx.recv() => {
                            // Inline forward_turn_event to avoid borrowing self
                            match event {
                                TurnEvent::TextDelta(text) => {
                                    *model_produced_text = true;
                                    datalog_text_accum.push_str(&text);
                                    let _ = event_tx.send(AgentEvent::TextDelta(text));
                                }
                                TurnEvent::ReasoningDelta(text) => {
                                    let _ = event_tx.send(AgentEvent::ReasoningDelta(text));
                                }
                                TurnEvent::ToolBatchStarted { ref batch_id, ref calls } => {
                                    let _ = event_tx.send(AgentEvent::ToolBatchStarted {
                                        batch_id: batch_id.clone(),
                                        calls: calls.clone(),
                                    });
                                }
                                TurnEvent::ToolBatchCompleted { ref batch_id, ok, total, elapsed_ms } => {
                                    let _ = event_tx.send(AgentEvent::ToolBatchCompleted {
                                        batch_id: batch_id.clone(),
                                        ok,
                                        total,
                                        elapsed_ms,
                                    });
                                }
                                TurnEvent::ToolCallStarted { ref id, ref name, ref arguments } => {
                                    // Dedupe across retries: the same provider-assigned tool_call_id
                                    // arrives again whenever a 429 / stream-ended attempt is retried.
                                    // Without this guard, every retry paints another `▸ Bash(...)` row.
                                    // Skip ALL downstream side effects (datalog, phase, file tracking,
                                    // event emission) for the duplicate — the first emission has
                                    // already accounted for them.
                                    if !emitted_tool_ids.insert(id.clone()) {
                                        continue;
                                    }
                                    // Forward tool name immediately for UI spinner
                                    let _ = event_tx.send(AgentEvent::ToolCallStreaming { name: name.clone(), hint: String::new() });
                                    // Flush accumulated model text to datalog before logging tool call accumulated model text to datalog before logging tool call
                                    if !datalog_text_accum.is_empty() {
                                        datalog.log_model_text(&datalog_text_accum);
                                        datalog_text_accum.clear();
                                    }
                                    datalog.log_tool_call(name, arguments);

                                    *current_tool_name = name.clone();
                                    *phase = AgentPhase::CallingTool(name.clone());
                                    let _ = event_tx.send(AgentEvent::PhaseChange(phase.clone()));

                                    if name == "bash" {
                                        if let Ok(args) = serde_json::from_str::<serde_json::Value>(arguments) {
                                            *last_bash_cmd = args
                                                .get("command")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("")
                                                .to_string();
                                        }
                                    }

                                    // Track files for Working Set + read counts
                                    if matches!(name.as_str(), "read_file" | "edit_file" | "create_file" | "search_replace" | "glob" | "grep") {
                                        if let Ok(args) = serde_json::from_str::<serde_json::Value>(arguments) {
                                            // Try file_path first, then path (glob/grep use path)
                                            let fp = args.get("file_path").and_then(|v| v.as_str())
                                                .or_else(|| args.get("path").and_then(|v| v.as_str()));
                                            if let Some(fp) = fp {
                                                let short = std::path::Path::new(fp)
                                                    .file_name()
                                                    .map(|n| n.to_string_lossy().to_string())
                                                    .unwrap_or_else(|| fp.to_string());
                                                session_files.insert(short.clone(), std::path::PathBuf::from(fp));
                                                if name == "read_file" {
                                                    if !files_read_this_turn.contains(&short) {
                                                        files_read_this_turn.push(short);
                                                    }
                                                    // Targeted reads (offset/limit) are always progress
                                                    let has_offset = args.get("offset").is_some() || args.get("limit").is_some();
                                                    if has_offset {
                                                        *targeted_read_count += 1;
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    let _ = event_tx.send(AgentEvent::ToolCallStarted { id: id.clone(), name: name.clone(), arguments: arguments.clone() });
                                }
                                TurnEvent::ToolOutputChunk { call_id, chunk } => {
                                    // Forward real-time tool output to UI
                                    let _ = event_tx.send(AgentEvent::ToolOutputChunk { call_id, chunk });
                                }
                                TurnEvent::ToolCallResult { call_id, name, output, success, duration } => {
                                    // Track files for discipline
                                    if let Some(pos) = output.find("Edited ") {
                                        let rest = &output[pos + 7..];
                                        let fp_end = rest.find(|c: char| c == ' ' || c == '\n' || c == '(').unwrap_or(rest.len());
                                        let fp = rest[..fp_end].trim();
                                        if !fp.is_empty() {
                                            *active_file = Some(PathBuf::from(fp));
                                        }
                                        if !fp.is_empty() {
                                            let file = fp.to_string();
                                            if !files_edited_this_turn.contains(&file) {
                                                files_edited_this_turn.push(file);
                                            }
                                        }
                                    }
                                    if let Some(pos) = output.find("Wrote ").or_else(|| output.find("Overwrote ")).or_else(|| output.find("Created new file ")) {
                                        let keyword_len = if output[pos..].starts_with("Overwrote ") { 10 }
                                            else if output[pos..].starts_with("Created new file ") { 17 }
                                            else { 6 };
                                        let rest = &output[pos + keyword_len..];
                                        let fp_end = rest.find(|c: char| c == ' ' || c == '\n' || c == '(').unwrap_or(rest.len());
                                        let fp = rest[..fp_end].trim();
                                        if !fp.is_empty() {
                                            *active_file = Some(PathBuf::from(fp));
                                        }
                                        if !fp.is_empty() {
                                            let file = fp.to_string();
                                            if !files_edited_this_turn.contains(&file) {
                                                files_edited_this_turn.push(file);
                                            }
                                        }
                                    }
                                    if success {
                                        track_tool_modified_files(
                                            &name,
                                            last_bash_cmd,
                                            &output,
                                            files_edited_this_turn,
                                        );
                                    }
                                    if matches!(name.as_str(), "read_file" | "list_directory" | "glob" | "grep") {
                                        *consecutive_reads += 1;
                                    } else if matches!(name.as_str(), "edit_file" | "create_file") {
                                        *consecutive_reads = 0;
                                    }
                                    // Notify background indexer to reindex edited/created files
                                    if matches!(name.as_str(), "edit_file" | "create_file") && success {
                                        if let Some(ref tx) = reindex_tx {
                                            let path_str = output.lines().next().unwrap_or("")
                                                .trim_start_matches("Edited ")
                                                .trim_start_matches("Created new file ")
                                                .trim_start_matches("Created ")
                                                .trim_start_matches("Wrote ")
                                                .trim_start_matches("Overwrote ")
                                                .split_whitespace().next().unwrap_or("");
                                            if !path_str.is_empty() {
                                                let _ = tx.send(PathBuf::from(path_str));
                                            }
                                        }
                                    }
                                    datalog.log_tool_result(&output, success);
                                    let _ = event_tx.send(AgentEvent::ToolCallResult {
                                        call_id, name, output, success, duration,
                                    });
                                }
                                TurnEvent::TokenUsage { prompt_tokens, completion_tokens, total_tokens: _, cached_tokens } => {
                                    datalog.log_token_usage(prompt_tokens, completion_tokens, cached_tokens);
                                    if cached_tokens > 0 {
                                        datalog.log_cache_hit(prompt_tokens, cached_tokens);
                                    }
                                    let _ = event_tx.send(AgentEvent::TokenUsage(
                                        crate::stream::TokenUsage {
                                            prompt_tokens,
                                            completion_tokens,
                                            cached_tokens,
                                        }
                                    ));
                                }
                                TurnEvent::ContextStats { system_tokens, sent_tokens, dropped_tokens, working_set_tokens, total_messages } => {
                                    datalog.log_context_stats(system_tokens, sent_tokens, dropped_tokens, working_set_tokens, total_messages);

                                    // Detect context collapse: if sent tokens drop dramatically,
                                    // model has lost most history. Reset edit tracking so BLOCKED
                                    // doesn't prevent the model from re-reading files it forgot about.
                                    if sent_tokens < 3000 {
                                        *context_collapsed = true;
                                    }

                                    // Narrow stats path — rich fields (tool_defs / cold_zone /
                                    // ctx_window / ctx_name) are sent from the datalog block in
                                    // handle_send_message, which has access to self.ctx.
                                    // TUI side merges both emissions into a single cache.
                                    let _ = event_tx.send(AgentEvent::ContextStats {
                                        system_tokens, sent_tokens, dropped_tokens, working_set_tokens, total_messages,
                                        tool_defs_tokens: 0,
                                        cold_zone_tokens: 0,
                                        ctx_window: 0,
                                        ctx_name: String::new(),
                                        system_prompt: String::new(),
                                    });
                                }
                                TurnEvent::ToolCallStreaming { name, hint } => {
                                    let _ = event_tx.send(AgentEvent::ToolCallStreaming { name, hint });
                                }
                                TurnEvent::Error(e) => {
                                    // Streaming-error forwarder: `conv` is borrowed
                                    // by the in-flight `turn_fut`, so we can't snapshot
                                    // `conv.messages` from here. The terminal-error
                                    // branches in `handle_send_message` fire after
                                    // turn_fut completes with the proper snapshot.
                                    let _ = event_tx.send(AgentEvent::Error {
                                        error: e,
                                        snapshot: ConversationSnapshot::default(),
                                    });
                                }
                                TurnEvent::Warning(w) => {
                                    datalog.log_warning(&w);
                                    let _ = event_tx.send(AgentEvent::Warning(w));
                                }
                                TurnEvent::WorkingDirChanged(new_dir) => {
                                    // A tool (change_dir / bash cd) mutated the shared
                                    // cwd. Surface it so the TUI footer can update.
                                    // Intentionally does not mirror `services.rs::change_dir`
                                    // (which clears the conversation, reloads the code graph,
                                    // respawns the indexer) — those side effects are right for
                                    // a user-initiated `/cd` but would destroy mid-turn state
                                    // when the LLM is just navigating.
                                    let _ = event_tx.send(AgentEvent::WorkingDirChanged(new_dir));
                                }
                                TurnEvent::ApprovalRequested { tool_name, reason, call, snapshot } => {
                                    // Forward approval request to TUI, including
                                    // a snapshot of conversation state so the
                                    // TUI can persist mid-turn session state.
                                    let _ = event_tx.send(AgentEvent::ApprovalNeeded {
                                        tool_name,
                                        reason,
                                        call,
                                        snapshot,
                                    });
                                    *phase = AgentPhase::WaitingApproval;
                                    let _ = event_tx.send(AgentEvent::PhaseChange(AgentPhase::WaitingApproval));
                                }
                                TurnEvent::RateLimited { reset_at_display, reset_label, secs_until_reset, auto_resuming } => {
                                    let _ = event_tx.send(AgentEvent::RateLimited {
                                        reset_at_display,
                                        reset_label,
                                        secs_until_reset,
                                        auto_resuming,
                                    });
                                }
                            }
                        }

                        Some(req) = approval_req_rx.recv() => {
                            // The ApprovalNeeded event was already sent from the
                            // TurnEvent::ApprovalRequested handler above (which
                            // has access to conversation.messages).  Here we
                            // only record the request for later approve/deny.
                            *last_approval_request = Some(req);
                        }

                        Some(cmd) = cmd_rx.recv() => {
                            crate::ctrace!("AGT", "inner cmd_rx pop: {:?}", std::mem::discriminant(&cmd));
                            match cmd {
                                AgentCommand::Cancel => {
                                    crate::ctrace!("AGT", "inner Cancel -> cancel_token.cancel() (was_cancelled={})", cancel_token.is_cancelled());
                                    // Cancel the in-flight turn. Do NOT reset the
                                    // token to a fresh one here: it must stay
                                    // cancelled so the outer turn loop's
                                    // top-of-loop guard halts the *whole* task
                                    // instead of starting the next reasoning/tool
                                    // round. Previously the reset meant one Cancel
                                    // only interrupted the current step — if the
                                    // turn finished as UsedTools/Responded the loop
                                    // continued with a clean token and the user had
                                    // to press stop once per round. A fresh token is
                                    // installed by handle_send_message on the next
                                    // user message.
                                    cancel_token.cancel();
                                }
                                AgentCommand::ApproveTool => {
                                    *phase = AgentPhase::Thinking;
                                    let _ = event_tx.send(AgentEvent::PhaseChange(AgentPhase::Thinking));
                                    let _ = approval_resp_tx.send(PermissionDecision::Allow);
                                }
                                AgentCommand::ApproveToolAlways => {
                                    if let Some(ref req) = last_approval_request {
                                        if let Ok(mut store) = permission_store.write() {
                                            // Path-scoped approvals (sensitive
                                            // reads) remember only this exact
                                            // resource, so re-reading the same
                                            // file stops re-prompting while
                                            // other sensitive paths stay gated.
                                            // Everything else grants the tool.
                                            match &req.scope {
                                                Some(scope) => store.grant_session_scope(scope),
                                                None => store.grant_session(&req.call.name),
                                            }
                                        }
                                    }
                                    *phase = AgentPhase::Thinking;
                                    let _ = event_tx.send(AgentEvent::PhaseChange(AgentPhase::Thinking));
                                    let _ = approval_resp_tx.send(PermissionDecision::Allow);
                                }
                                AgentCommand::DenyTool => {
                                    *phase = AgentPhase::Thinking;
                                    let _ = event_tx.send(AgentEvent::PhaseChange(AgentPhase::Thinking));
                                    let _ = approval_resp_tx.send(PermissionDecision::Deny);
                                }
                                AgentCommand::Shutdown => {
                                    cancel_token.cancel();
                                    self.shutdown_requested.store(true, std::sync::atomic::Ordering::Release);
                                }
                                AgentCommand::AppendInput(text) => {
                                    if let Some(ref mut existing) = pending_input {
                                        existing.push('\n');
                                        existing.push_str(&text);
                                    } else {
                                        *pending_input = Some(text);
                                    }
                                }
                                // SyncMessages is handled in the outer loop
                                // (after turn completes) because `conv` is
                                // mutably borrowed by `turn_fut` here.
                                _ => {} // Other commands ignored during turn
                            }
                        }
                    }
                };

                // Flush any remaining accumulated text to datalog
                if !datalog_text_accum.is_empty() {
                    datalog.log_model_text(&datalog_text_accum);
                }

                // turn_tx drops here (owned by this block), turn_fut also drops
                (result, turn_rx, *context_collapsed)
            };
            // All borrows of self.turn_runner are now released.

            // Handle context collapse: clear edit tracking so model can re-read
            if context_collapsed {
                self.turn_runner.recently_edited_files.clear();
            }

            // Restore conversation
            self.conversation = conv;

            // Drain remaining events
            while let Ok(event) = turn_rx.try_recv() {
                self.forward_turn_event(event);
            }

            // Handle result
            match result {
                TurnResult::Responded {
                    ref text,
                    tokens,
                    truncated,
                } => {
                    // Trigger model response hooks (fire-and-forget)
                    {
                        let wd = self
                            .turn_runner
                            .context
                            .working_dir
                            .try_read()
                            .map(|g| g.display().to_string())
                            .unwrap_or_default();
                        let mctx = crate::hook::TurnStartContext {
                            turn_number: self.turn_count as u32,
                            session_id: Some(self.session_id.clone()),
                            working_dir: wd,
                            phase: format!("{:?}", self.phase).to_lowercase(),
                            has_file_context: !self.files_edited_this_turn.is_empty(),
                        };
                        self.turn_runner.hook_engine.trigger_on_model_response(text, &mctx).await;
                    }
                    self.turn_tokens += tokens;
                    self.total_tokens += tokens;
                    // Log the final assistant text to datalog (TUI used to do this —
                    // absorbed here now that TUI's duplicate TurnLog was removed).
                    if !text.trim().is_empty() {
                        self.datalog.log_text(text);
                    }

                    // ATLAS subtask extraction: if model just output a plan (FeatureDev,
                    // first response with text, no tools used yet), extract subtasks
                    // and drive execution file-by-file.
                    //
                    // Guard: only extract when the model was truncated (it wanted to
                    // continue but hit max_tokens). A Natural stop means the model
                    // considers its response complete — it may be answering a question,
                    // discussing design, or giving examples that mention file names.
                    // Extracting subtasks from such text produces phantom plans
                    // (e.g. "auth.rs" mentioned as an example gets treated as an
                    // edit target, and plan-completion-guard then forces the loop
                    // to keep running).
                    if self.tool_call_count == 0
                        && truncated
                        && !text.trim().is_empty()
                        && !self.subtask_driver.active
                    {
                        self.subtask_driver.extract_from_plan(text);
                        // Store plan text for adherence reminders
                        self.plan_text = Some(text.clone());

                        // Graph: check if plan covers all dependent files.
                        // If the plan mentions router.rs and weather.rs but both depend
                        // on types.rs, warn that types.rs might also need changes.
                        if self.subtask_driver.active {
                            let graph = self.turn_runner.context.graph.read().await;
                            if graph.is_ready() {
                                let plan_files: Vec<&str> = self
                                    .subtask_driver
                                    .subtasks
                                    .iter()
                                    .map(|s| s.file.as_str())
                                    .collect();
                                let mut missing_deps: Vec<String> = Vec::new();
                                let mut seen = std::collections::HashSet::new();

                                for plan_file in &plan_files {
                                    seen.insert(plan_file.to_string());
                                }

                                for plan_file in &plan_files {
                                    // Find this file in graph and get its dependencies
                                    for (path, _) in &graph.file_symbols {
                                        let basename = path
                                            .file_name()
                                            .map(|f| f.to_string_lossy().to_string())
                                            .unwrap_or_default();
                                        if basename == *plan_file {
                                            // Check files this file depends on (callees' files)
                                            let sym_ids = graph.symbols_in_file(path);
                                            if let Some(ids) = sym_ids {
                                                for &sid in ids.iter().take(20) {
                                                    if let Some(edges) = graph.callees(sid) {
                                                        for edge in edges {
                                                            if let Some(node) = graph.node(edge.to)
                                                            {
                                                                let dep_name = node
                                                                    .file
                                                                    .file_name()
                                                                    .map(|f| {
                                                                        f.to_string_lossy()
                                                                            .to_string()
                                                                    })
                                                                    .unwrap_or_default();
                                                                if !dep_name.is_empty()
                                                                    && !seen.contains(&dep_name)
                                                                    && dep_name != basename
                                                                {
                                                                    seen.insert(dep_name.clone());
                                                                    missing_deps.push(dep_name);
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                            break;
                                        }
                                    }
                                }

                                // PLAN CHECK injection: REMOVED. Dependency warnings are not needed —
                                // dependency warnings. Model discovers deps itself.
                                let _ = missing_deps; // suppress unused warning
                            }
                            drop(graph);
                        }

                        // Subtask driver serial execution: REMOVED.
                        // Was injecting "now edit file X" instructions from regex-extracted
                        // plan. Batch prompt now lets model handle multi-file work itself.
                        // Sub-agent dispatch also disabled (try_sub_agent_dispatch returns None).
                    }

                    // finish_reason-based termination dispatch (2026-04-22).
                    //
                    // The previous code injected `(continuing...)` + `Continue.`
                    // when the model returned empty text, under the theory that
                    // empty = "was about to say more". In practice this conflated:
                    //   (a) finish_reason="length" — real max-token cutoff
                    //       mid-generation, retrying does salvage the session
                    //   (b) finish_reason="stop" + no text — model cleanly
                    //       decided to stop after reading tool results
                    //       (e.g. `cargo check` passed, nothing more to say)
                    // and cycled case (b) into meaningless `Continue.` loops.
                    //
                    // CC has no such recovery mechanism — empty-on-stop IS the
                    // natural termination (`project_cc_prompt_philosophy.md`).
                    //
                    // Briefly tried adding an "empty-after-failure" branch
                    // (2026-04-22 20:44) but the hermes 20-41 session showed
                    // the real issue was upstream in edit.rs `find_closest_match_inner`
                    // producing garbage "closest match" hints — the model
                    // gave up because the framework's hint was actively
                    // misleading, not because it needed more nudging.
                    // Reverting to the principled state machine.
                    if truncated && self.retry_count < 1 {
                        self.retry_count += 1;
                        self.conversation.add_synthetic_user_message(
                            "Output limit hit. If the task is already complete, just output a \
                             short summary and stop (no tool calls). Otherwise resume where you left off."
                        );
                        continue;
                    }

                    self.finish_turn(TurnStopReason::Natural);
                    return self.last_stop_reason;
                }
                TurnResult::UsedTools {
                    tool_count,
                    tokens,
                    text,
                } => {
                    self.turn_tokens += tokens;
                    self.total_tokens += tokens;
                    self.tool_call_count += tool_count;
                    // Track silent rounds: model used tools without explaining anything.
                    let had_text = text.as_ref().map(|t| !t.trim().is_empty()).unwrap_or(false);
                    if had_text {
                        self.discipline_state.silent_tool_rounds = 0;
                    } else {
                        self.discipline_state.silent_tool_rounds += 1;
                    }

                    // Fork sub-agent dispatch is no longer driven by parsing this
                    // turn's text. The model invokes `parallel_edit_files`
                    // explicitly when it judges parallel edit is the right move.
                    // See `crate::tool::parallel_edit` for the active-dispatch
                    // tool and `agent/mod.rs::run` for its registration.

                    // Post-process: truncate large outputs + externalize to disk
                    self.post_process_tool_results(tool_count);

                    // ATLAS auto-verify: removed along with the verify module.
                    // Model runs build/lint itself when needed.
                    // See docs/archive/guardian-auto-compile.md if re-introducing.

                    // Safety cap at 200 tool calls — only for runaway cost protection.
                    if self.check_step_limit() {
                        self.finish_turn(TurnStopReason::StepLimit);
                        return self.last_stop_reason;
                    }
                    // Continue to next turn
                    self.phase = AgentPhase::Thinking;
                    let _ = self
                        .event_tx
                        .send(AgentEvent::PhaseChange(AgentPhase::Thinking));
                    continue;
                }
                TurnResult::Failed(e) => {
                    // Retry logic for transient errors
                    let is_rate_limited = is_rate_limited_error(&e);
                    // A 429 whose quota only resets at a fixed future time
                    // (monthly / period quota exhausted) can't recover by
                    // retrying — fail fast and show the upstream reason as-is,
                    // instead of the 5-shot rate-limit backoff (which just
                    // delays the same error by ~45s and re-hammers the gateway).
                    let is_quota_exhausted = crate::provider::is_non_retryable_rate_limit(&e);
                    let is_auth_error = is_auth_error(&e);
                    let is_insufficient_balance = is_insufficient_balance_error(&e);
                    let is_messages_illegal = e.contains("illegal") || e.contains("messages");
                    // Upstream context-length overflow (OpenRouter 400, OpenAI
                    // context_length_exceeded, Anthropic "prompt is too long").
                    // Without this, the error fell through to the generic
                    // retry branch which slept and re-sent the same oversized
                    // request — guaranteed to fail again.
                    let is_context_overflow = is_context_overflow_error(&e);
                    // Open-source build attempted a CodingPlan-signed request.
                    // The signing module isn't compiled in; retrying is
                    // guaranteed to fail again. Fail-fast skips the otherwise-
                    // useless 3-shot retry (3+6+9s of wasted time + 3 spurious
                    // "[API error 请求失败]" lines hardcoded in Chinese that
                    // would also display to English-locale users).
                    let is_official_build_required = is_codingplan_unavailable_error(&e);
                    // Provider returned 200 OK with a completely empty
                    // completion (no text, no tool calls). Transient upstream
                    // hiccup — handled by its own fast-retry branch below.
                    let is_empty_response = is_empty_response_error(&e);

                    if is_official_build_required {
                        self.datalog.log_error(&e);
                        self.report_error("codingplan_unavailable", &e).await;
                        let _ = self.event_tx.send(AgentEvent::Error {
                            error: public_error_message(&e),
                            snapshot: self.conversation.snapshot(),
                        });
                        self.finish_turn(TurnStopReason::Error);
                        return self.last_stop_reason;
                    } else if is_empty_response {
                        // Provider returned a 200 OK with a completely empty
                        // completion (no text, no tool calls). Confirmed
                        // transient on deepseek-v4-flash via the atomgit
                        // gateway, which only forwards to DeepSeek's official
                        // API: datalog shows the SAME request resent recovers
                        // (one session logged empty×3 then success on the 4th
                        // send), and the empty rate was actually HIGHER at
                        // <10K context than at >100K — so this is an upstream/
                        // model hiccup, not a context-size or request problem.
                        // It returns instantly, so the generic 3/6/9s backoff
                        // is pure wasted latency: retry quickly, a few more
                        // times, on an independent budget.
                        if self.empty_response_retries < EMPTY_RESPONSE_MAX_RETRIES {
                            self.empty_response_retries += 1;
                            // Front-loaded short backoff: 1,1,2,2,3s (~9s for
                            // all 5 attempts vs 18s for the old 3-shot path).
                            let wait =
                                (((self.empty_response_retries + 1) / 2).min(3)) as u64;
                            let _ = self.event_tx.send(AgentEvent::TextDelta(format!(
                                "\n[模型返回空响应，重试中({}/{})...]\n",
                                self.empty_response_retries, EMPTY_RESPONSE_MAX_RETRIES
                            )));
                            if backoff_or_cancelled(wait, &self.cancel_token).await {
                                return self.finalize_cancelled();
                            }
                            continue;
                        } else {
                            // Exhausted: surface a clear, non-alarming reason
                            // (not the misleading generic "请求失败") and keep
                            // the conversation snapshot so the user can simply
                            // resend.
                            self.datalog.log_error(&e);
                            self.report_error("empty_response", &e).await;
                            let _ = self.event_tx.send(AgentEvent::Error {
                                error: format!(
                                    "模型连续 {} 次返回空响应（上游偶发，与上下文长度无关）。可直接回车重试，或稍后再试。",
                                    EMPTY_RESPONSE_MAX_RETRIES
                                ),
                                snapshot: self.conversation.snapshot(),
                            });
                            self.finish_turn(TurnStopReason::Error);
                            return self.last_stop_reason;
                        }
                    } else if (is_messages_illegal || is_context_overflow) && self.retry_count < 2 {
                        self.retry_count += 1;
                        let sys_prompt = self.build_system_prompt();
                        // Auto-discover the proxy's actually-enforced limit
                        // from the error body. Self-built proxies for
                        // open-weight models often enforce far less than
                        // the configured ctx_window — without parsing the
                        // rejection we'd compact toward the wrong target.
                        let limit = extract_provider_ctx_limit(&e)
                            .unwrap_or_else(|| self.ctx.ctx_window());
                        // 5K safety buffer — leaves room for the streaming
                        // response and one round of tool results before the
                        // next compact would be needed.
                        let target = limit.saturating_sub(5_000);
                        let recovered = self
                            .emergency_compact_to_target(target, &sys_prompt)
                            .await;
                        let msg = if recovered {
                            "\n[Context overflow — recovered via layered compact, retrying...]\n"
                                .to_string()
                        } else {
                            format!(
                                "\n[Context overflow — compacted toward {}T but still over, \
                                 retrying anyway...]\n",
                                target
                            )
                        };
                        let _ = self.event_tx.send(AgentEvent::TextDelta(msg));
                        continue;
                    } else if is_quota_exhausted {
                        // Non-retryable: surface the upstream message verbatim
                        // (e.g. "当前月度额度已用完，请于 … 后继续使用。") so the
                        // user sees the real reason and reset time, not a
                        // generic "请稍后再试".
                        self.datalog.log_error(&e);
                        self.report_error("rate_limit_exhausted", &e).await;
                        let shown = e.strip_prefix("[429] ").unwrap_or(&e).to_string();
                        let _ = self.event_tx.send(AgentEvent::Error {
                            error: shown,
                            snapshot: self.conversation.snapshot(),
                        });
                        self.finish_turn(TurnStopReason::Error);
                        return self.last_stop_reason;
                    } else if is_insufficient_balance {
                        // Non-retryable: no balance means every retry of this
                        // exact request fails identically. Fail-fast like the
                        // quota/auth branches and surface the real reason
                        // ("Insufficient Balance" / 余额不足) instead of 3
                        // misleading "请求失败，重试" lines.
                        self.datalog.log_error(&e);
                        self.report_error("insufficient_balance", &e).await;
                        let _ = self.event_tx.send(AgentEvent::Error {
                            error: public_error_message(&e),
                            snapshot: self.conversation.snapshot(),
                        });
                        self.finish_turn(TurnStopReason::Error);
                        return self.last_stop_reason;
                    } else if is_rate_limited && self.retry_count < 5 {
                        self.retry_count += 1;
                        // 指数退避基线（3/6/9/12/15s）。但若网关在错误体里明确给了冷却
                        // 时长（如 litellm 的 "No deployments available… Try again in 10
                        // seconds"），就采纳它——否则更早重试只会打到仍在冷却的部署池，
                        // 反而再次触发/延长冷却（用户侧表现为"明明有额度却一直 429"）。
                        // 取二者较大值并封顶 30s。
                        let computed = (self.retry_count as u64 * 3).min(30);
                        let wait = parse_retry_after_hint(&e)
                            .unwrap_or(0)
                            .max(computed)
                            .min(30);
                        let _ = self.event_tx.send(AgentEvent::TextDelta(format!(
                            "\n[Rate limited — retrying in {}s...]\n",
                            wait
                        )));
                        if backoff_or_cancelled(wait, &self.cancel_token).await {
                            return self.finalize_cancelled();
                        }
                        continue;
                    } else if is_auth_error {
                        self.datalog.log_error(&e);
                        self.report_error("auth_error", &e).await;
                        let _ = self.event_tx.send(AgentEvent::Error {
                            error: public_error_message(&e),
                            snapshot: self.conversation.snapshot(),
                        });
                        self.finish_turn(TurnStopReason::Error);
                        return self.last_stop_reason;
                    } else if self.retry_count < 3 {
                        self.retry_count += 1;
                        let wait = (self.retry_count as u64 * 3).min(15);
                        let reason = public_error_reason(&e);
                        let _ = self.event_tx.send(AgentEvent::TextDelta(format!(
                            "\n[API error {}，{} 秒后重试({}/3)...]\n",
                            reason, wait, self.retry_count
                        )));
                        if backoff_or_cancelled(wait, &self.cancel_token).await {
                            return self.finalize_cancelled();
                        }
                        continue;
                    } else {
                        self.datalog.log_error(&e);
                        self.report_error("api_error", &e).await;
                        let _ = self.event_tx.send(AgentEvent::Error {
                            error: public_error_message(&e),
                            snapshot: self.conversation.snapshot(),
                        });
                        self.finish_turn(TurnStopReason::Error);
                        return self.last_stop_reason;
                    }
                }
                TurnResult::Cancelled => {
                    // In-flight stream interrupted mid-turn. Shared finalize:
                    // marks Cancelled first (so the /goal wrapper breaks out of
                    // its auto-continuation loop instead of scheduling another
                    // "(cancelled)" round), backfills unpaired tool calls, and
                    // emits TurnCancelled without a stale TurnComplete.
                    return self.finalize_cancelled();
                }
            }
        }
        // run_turn_loop's body is `loop { ... }` — every match arm returns,
        // so the post-loop expression below is unreachable. Kept as an
        // explicit `unreachable!` (rather than removed) so any future
        // change that introduces a `break` will fail loudly instead of
        // silently returning a stale `last_stop_reason`.
        #[allow(unreachable_code)]
        {
            unreachable!("run_turn_loop must return via a match arm");
        }
    }

    // forward_turn_event → tool_dispatch.rs
    // post_process_tool_results → tool_dispatch.rs

    /// Pro-active context compaction. Two-stage:
    ///
    /// 1. **Tier 1 (cheap, mechanical):** collapse old `ToolResult`
    ///    bodies into stubs (`compact_old_tool_results_in_place`, using
    ///    the same `build_compact_stub` format shared with the normal-path
    ///    `collapse_committed`; keeps the last 3 turns full). Zero LLM
    ///    calls. Cheap to fire, easy to revert if model needs the bytes
    ///    back via re-read.
    ///
    /// 2. **Tier 2 (expensive, LLM-driven):** if Tier 1 didn't bring
    ///    the context under threshold, fall through to LLM-summarize
    ///    older turns into the cold zone (existing path).
    ///
    /// Buffer was retuned 2026-05-06: small windows (≤100K, e.g.
    /// self-hosted GLM 65K) now trigger at 60K instead of 52K, so
    /// the 5K runway above the trigger lets Tier 1 absorb hits
    /// before the proxy 65K wall. Datalog 2026-05-06_19-06-50: 4
    /// reactive emergency compactions, each dropping 18-30K
    /// catastrophically. With proactive Tier 1 firing 5K below the
    /// wall, expected pattern is 3-4 mild Tier 1 events dropping
    /// 5-10K each, model retains skeleton + recent turns.
    /// Max tokens the kept conversation tail may occupy after compaction:
    /// the window minus the per-request overhead that shares it (system
    /// prompt, tool definitions, cold-zone summaries, output reservation).
    /// This overhead is roughly CONSTANT w.r.t. window size, so subtracting
    /// it scales correctly from 128K to 1M — a fixed fraction would not
    /// (it would waste ~460K on a 1M model, the same class of bug as the
    /// old 60K drop cap).
    /// Emit a full ContextStats snapshot for the `/context` command.
    /// Callers pass the conversation and the already-built `msgs` (from
    /// `self.ctx.build_messages`) so the estimate reflects exactly what
    /// the model would see on the next turn — directives and all. Used by
    /// both `handle_send_message` (once per turn, post-build_messages) and
    /// `run_compact` (to refresh the cached stats TUI reads for `/context`
    /// after an out-of-turn compaction).
    async fn emit_rich_context_stats(
        &self,
        conv: &Conversation,
        msgs: &[crate::conversation::message::Message],
    ) {
        let tool_defs = self.turn_runner.tools.get_definitions().await;
        let tool_defs_tokens: usize = tool_defs
            .iter()
            .map(|d| {
                let params = serde_json::to_string(&d.parameters).unwrap_or_default();
                (d.name.len() + d.description.len() + params.len()) / 4
            })
            .sum();
        let cold_zone_tokens: usize = conv.cold_summaries.iter().map(|s| s.len() / 4 + 4).sum();
        let actual_system_prompt = msgs
            .iter()
            .find(|m| matches!(m.role, crate::conversation::message::Role::System))
            .and_then(|m| m.text().map(|s| s.to_string()))
            .unwrap_or_default();
        let system_tokens_local = msgs
            .iter()
            .find(|m| matches!(m.role, crate::conversation::message::Role::System))
            .map(|m| m.estimate_tokens())
            .unwrap_or(0);
        let sent_tokens_local: usize = msgs
            .iter()
            .map(|m| m.estimate_tokens())
            .sum::<usize>()
            .saturating_sub(system_tokens_local);
        let total_messages_local = msgs.len();
        let _ = self.event_tx.send(AgentEvent::ContextStats {
            system_tokens: system_tokens_local,
            sent_tokens: sent_tokens_local,
            dropped_tokens: 0,
            working_set_tokens: 0,
            total_messages: total_messages_local,
            tool_defs_tokens,
            cold_zone_tokens,
            ctx_window: self.ctx.ctx_window(),
            ctx_name: self.ctx.name().to_string(),
            system_prompt: actual_system_prompt,
        });
    }

    /// D2 emergency compact — layered, measured, never combines destructive
    /// ops. Replaces the previous "LLM-compress + blind truncate(len-4)"
    /// path that destroyed last-turn context (datalog atomgr-2d99b47d/
    /// 2026-05-06_08-43-12: 65K → 8516 tokens because compression THEN a
    /// 4-message truncate ran back-to-back, and the truncate dropped
    /// exactly the recent file reads the user needed for "继续").
    ///
    /// Each tier checks budget against `target` and breaks at the first
    /// sufficient tier. Returns true if any tier reached the target.
    ///
    /// Tiers (least → most destructive):
    /// 1. Collapse old tool_results (keep last 3 turns full).
    /// 2. LLM-summarize older turns into cold zone.
    /// 3. Hard token-driven truncate (drops oldest until under target,
    ///    snapping to safe boundaries; the last user message is sacred).
    async fn emergency_compact_to_target(
        &mut self,
        target_tokens: usize,
        system_prompt: &str,
    ) -> bool {
        let sys_tokens = system_prompt.len() / 4 + 4;
        let estimate = |conv: &Conversation| -> usize {
            sys_tokens + conv.messages.iter().map(|m| m.estimate_tokens()).sum::<usize>()
        };

        if estimate(&self.conversation) <= target_tokens {
            return true;
        }

        // Tier 1: collapse heavy tool results in older turns.
        crate::ctx::render::compact_old_tool_results_in_place(
            &mut self.conversation,
            /* keep_recent_turns */ 3,
            false,
        );
        if estimate(&self.conversation) <= target_tokens {
            return true;
        }

        // Tier 2: LLM-summarize older turns into the cold zone. This is
        // the most expensive tier (it makes a network round trip), so
        // we only reach it after Tier 1 already failed.
        // Use a minimal task-only hint — emergency compaction needs max
        // compression rate; full file lists would consume precious tokens.
        {
            let minimal_state = if !self.current_task.is_empty() {
                let task_short: String = self.current_task.chars().take(120).collect();
                Some(format!("TASK: {}", task_short))
            } else {
                None
            };
            compression::maybe_compress_history(
                &*self.ctx,
                &mut self.conversation,
                &*self.turn_runner.provider,
                &self.turn_runner.tools,
                system_prompt,
                minimal_state.as_deref(),
            )
            .await;
        }
        if estimate(&self.conversation) <= target_tokens {
            return true;
        }

        // Tier 3: hard truncate to fit. Token-driven, not message-count
        // driven. The previous code did `truncate(len - 4)` blindly
        // which is what produced the 8516-token catastrophe.
        hard_truncate_to_target(&mut self.conversation, target_tokens, sys_tokens);
        estimate(&self.conversation) <= target_tokens
    }

    /// Manual `/compact` entry point. Mechanical only — reuses the active
    /// ctx strategy's `compression_plan` (same path as the task-boundary
    /// cleanup in `handle_send_message`) so behavior stays consistent with
    /// the rest of the codebase. `_prompt` is accepted for forward-compat
    /// with a future LLM-guided summarize path and ignored today.
    ///
    /// Net-savings guard: on terse conversations the cold-zone summary
    /// header + `inject_post_compress_state` inject can weigh more than
    /// the dropped messages, so compaction would silently inflate the
    /// prompt. We measure before/after token totals via `build_messages`
    /// (post all render-pipeline effects — `clean_message_pipeline`,
    /// `collapse_committed`, final-byte ceiling, etc.) and roll the
    /// conversation back if the operation didn't actually shrink the wire
    /// payload. Analytical projection was tried first but too many
    /// render-pipeline branches made it unreliable.
    async fn run_compact(&mut self, prompt: Option<String>) {
        let system_prompt = self.build_system_prompt();
        let keep_ceiling = compression::compaction_keep_ceiling(
            &*self.ctx,
            &system_prompt,
            &self.turn_runner.tools,
            &self.conversation.cold_summaries,
        )
        .await;
        let Some((mechanical_content, n_msgs)) =
            self.ctx.compression_plan(&self.conversation, keep_ceiling)
        else {
            let _ = self.event_tx.send(AgentEvent::TextDelta(
                crate::i18n::t(crate::i18n::Msg::CompactNothingShort).into_owned(),
            ));
            return;
        };

        let _ = self.event_tx.send(AgentEvent::TextDelta(
            crate::i18n::t(crate::i18n::Msg::CompactStarting).into_owned(),
        ));

        // Try LLM summarization (with optional custom prompt)
        let summarize_prompt = if let Some(ref custom) = prompt {
            format!(
                "Summarize this conversation history, focusing on: {}.\n\
                 Keep: file names, what was changed, key decisions, errors encountered.\n\
                 Drop: exact code content, tool arguments, line numbers.\n\n{}",
                custom, mechanical_content
            )
        } else {
            compression::default_summarize_prompt(&mechanical_content)
        };

        let summary = self.run_llm_summary_with_telemetry(&summarize_prompt).await;
        let content = if summary.trim().is_empty() {
            mechanical_content
        } else {
            summary
        };

        let state = build_post_compress_state(
            &self.current_task,
            &self.files_edited_this_turn,
            &self.files_read_this_turn,
        );
        let outcome = compression::try_apply_compression(
            &*self.ctx,
            &mut self.conversation,
            &system_prompt,
            n_msgs,
            content,
            state.as_deref(),
        );

        if !outcome.applied {
            let before = fmt_k_tokens(outcome.before_tokens);
            let after = fmt_k_tokens(outcome.after_tokens);
            let _ = self.event_tx.send(AgentEvent::TextDelta(
                crate::i18n::t(crate::i18n::Msg::CompactNothingNoSavings {
                    before: &before,
                    after: &after,
                })
                .into_owned(),
            ));
            let (msgs, _) =
                self.ctx
                    .build_messages(&self.conversation, &system_prompt, "");
            self.emit_rich_context_stats(&self.conversation, &msgs).await;
            return;
        }

        let before = fmt_k_tokens(outcome.before_tokens);
        let after = fmt_k_tokens(outcome.after_tokens);
        let _ = self.event_tx.send(AgentEvent::TextDelta(
            crate::i18n::t(crate::i18n::Msg::CompactDropped {
                messages: outcome.removed_messages,
                before: &before,
                after: &after,
            })
            .into_owned(),
        ));

        let (msgs, _) = self
            .ctx
            .build_messages(&self.conversation, &system_prompt, "");
        self.emit_rich_context_stats(&self.conversation, &msgs)
            .await;
    }

    /// Run LLM summary with telemetry reporting.
    /// Thin wrapper around [`compression::run_llm_summary`] that adds
    /// provider/host/model attribution and tracks the call in telemetry.
    async fn run_llm_summary_with_telemetry(&self, prompt: &str) -> String {
        let sys = compression::COMPRESSION_SYSTEM_PROMPT;
        let started = std::time::Instant::now();

        let (summary, tel_input, tel_output, tel_cached, had_error) =
            compression::run_llm_summary(&*self.turn_runner.provider, prompt).await;

        // Build the same envelope context a normal turn would carry.
        let pcfg = self.config.providers.get(&self.config.default_provider);
        let vendor = pcfg.map(|p| p.provider_type.clone());
        let host = pcfg.and_then(|p| {
            atomcode_telemetry::resolve_provider_host(&p.provider_type, p.base_url.as_deref())
        });
        let parent = atomcode_telemetry::CurrentContext::current();
        let scope_ctx = atomcode_telemetry::CurrentContext {
            turn_id: Some(uuid::Uuid::new_v4()),
            provider: parent.provider.clone().or(vendor),
            provider_host: parent.provider_host.clone().or(host),
            model: parent
                .model
                .clone()
                .or_else(|| Some(self.turn_runner.provider.model_name().to_string())),
            ..parent
        };
        let event = atomcode_telemetry::Event::LlmChat {
            duration_ms: started.elapsed().as_millis() as u32,
            tool_calls_count: 0,
            input_tokens: tel_input,
            output_tokens: tel_output,
            cached_tokens: tel_cached,
            had_error,
            context_window: self.ctx.ctx_window() as u32,
            system_tokens: (sys.len() / 4 + 4) as u32,
            tool_def_tokens: 0,
            tool_result_tokens: 0,
            message_tokens: (prompt.len() / 4) as u32,
            messages_count: 2, // system + user
            error_kind: None,
            error_data: None,
        };
        let tel = self.turn_runner.context.telemetry.clone();
        atomcode_telemetry::CurrentContext::scope(scope_ctx, || async move {
            tel.track(event);
        })
        .await;

        summary
    }

    /// Emit a `GoalUpdate` event reflecting current `GoalState`. Centralises
    /// the 6-field event construction that was duplicated 6 times across the
    /// agent loop — adding/renaming a field now only touches this one place
    /// and reduces "I forgot one path" bugs.
    fn emit_goal_update(&self, active: bool, last_reason: Option<String>) {
        let _ = self.event_tx.send(AgentEvent::GoalUpdate {
            active,
            round: self.goal.round,
            elapsed_secs: self.goal.elapsed_secs(),
            condition: self.goal.condition.clone(),
            last_reason,
        });
    }

    /// Clear the goal AND notify the TUI in one shot. Used by Esc/Cancel
    /// paths — historically these clears were silent and left the TUI
    /// indicator running even after the agent had given up (see CR C3).
    fn finalize_goal_cancelled(&mut self) {
        if !self.goal.active && self.goal.condition.is_empty() {
            return; // nothing to clear
        }
        self.goal.clear();
        self.emit_goal_update(false, Some("cancelled by user".to_owned()));
    }

    fn finish_turn(&mut self, stop_reason: TurnStopReason) {
        self.last_stop_reason = stop_reason;
        // Error exits must not leave the user's message in the history
        // as an "orphan turn" (user message with no assistant reply).
        // The next send_message would then stack another user message
        // on top of it — an API call with two consecutive user turns
        // and no intervening assistant, which weak models respond to
        // with 0 tokens (see test 3 / 4: MiniMax-M2.7 returns empty
        // after a failed localhost turn). Cancel the turn instead so
        // the next user message starts from a clean transcript.
        //
        // Counters (turn_count / turn_tokens / tool_call_count) stay
        // UNTOUCHED here so the TurnComplete event below still carries
        // accurate stats for the UI's "✓ Nailed it · N rounds · M tok"
        // line. `start_turn` resets them for the next message.
        if matches!(stop_reason, TurnStopReason::Error) {
            self.conversation.cancel_current_turn_including_user();
        } else {
            self.conversation.turn_tracker.complete_current();
        }

        // Auto-commit edited files if enabled
        if self.config.auto_commit
            && !matches!(stop_reason, TurnStopReason::Error)
            && !self.files_edited_this_turn.is_empty()
        {
            let wd = self
                .turn_runner
                .context
                .working_dir
                .try_read()
                .map(|g| g.clone())
                .unwrap_or_default();
            match git_auto_commit::auto_commit_edited_files(&wd, &self.files_edited_this_turn) {
                git_auto_commit::AutoCommitOutcome::Committed { sha, message } => {
                    let notice = format!("\n[auto-commit {sha}] {message}\n");
                    self.datalog.log_model_text(&notice);
                    let _ = self.event_tx.send(AgentEvent::TextDelta(notice));
                }
                git_auto_commit::AutoCommitOutcome::Failed { reason } => {
                    let notice = format!("\n[auto-commit skipped] {reason}\n");
                    self.datalog.log_error(&notice);
                    let _ = self.event_tx.send(AgentEvent::TextDelta(notice));
                }
                git_auto_commit::AutoCommitOutcome::Skipped { reason } => {
                    self.datalog
                        .log_model_text(&format!("[auto-commit skipped] {reason}"));
                }
            }
        }

        // Flush datalog with final stats
        self.datalog
            .end_turn(self.turn_tokens, self.tool_call_count);

        let duration = self.turn_start.map(|t| t.elapsed()).unwrap_or_default();
        self.turn_start = None;
        self.phase = AgentPhase::Idle;

        // --- 统一 TurnComplete Hook (fire-and-forget) ---
        {
            let stop_str = format!("{:?}", stop_reason).to_lowercase();
            let turn_complete_ctx = crate::hook::TurnCompleteContext {
                turn_number: self.turn_count as u32,
                result_type: stop_str,
                tokens_used: self.turn_tokens,
                tool_calls: self.tool_call_count,
                duration_ms: duration.as_millis() as u64,
                truncated: false,
                edited_files: self.files_edited_this_turn.clone(),
            };
            // fire-and-forget: spawn to avoid blocking the sync finish_turn
            let engine = self.turn_runner.hook_engine.clone();
            let ctx = turn_complete_ctx.clone();
            tokio::spawn(async move {
                engine.trigger_on_turn_complete(&ctx).await;
            });
        }

        let _ = self.event_tx.send(AgentEvent::TurnComplete {
            duration,
            total_tokens: self.turn_tokens,
            turn_count: self.turn_count,
            tool_call_count: self.tool_call_count,
            stop_reason,
            snapshot: self.conversation.snapshot(),
        });
        let _ = self
            .event_tx
            .send(AgentEvent::PhaseChange(AgentPhase::Idle));
        self.conversation.save(&Conversation::history_path());
    }

    // store_tool_result → tool_dispatch.rs

    // change_dir → services.rs

    // try_sub_agent_dispatch → REMOVED. Fork sub-agent dispatch is now
    // ACTIVE: the model invokes `parallel_edit_files` (see
    // `crate::tool::parallel_edit`) when it judges parallel edit is the
    // right move. The framework no longer parses plan text or guesses
    // intent — eliminating ~250 lines of heuristics, ~70 hardcoded
    // intent-keywords across two iterations of failed gate logic, and
    // an entire class of mis-fire failures (read-only turns dispatching
    // 6 fork sub-agents that fake edits or no-op).

    /// Fire-and-forget error reporting through the unified hook engine.
    /// Consolidates ErrorContext construction for terminal error paths
    /// (auth, api, codingplan_unavailable) to avoid duplication.
    async fn report_error(&self, error_type: &str, error_message: &str) {
        let ctx = crate::hook::ErrorContext {
            error_type: error_type.into(),
            error_message: error_message.into(),
            phase: format!("{:?}", self.phase).to_lowercase(),
            turn_number: Some(self.turn_count as u32),
        };
        self.turn_runner.hook_engine.trigger_on_error(&ctx).await;
    }
}


fn track_tool_modified_files(
    tool_name: &str,
    bash_command: &str,
    output: &str,
    edited_files: &mut Vec<String>,
) {
    if tool_name == "bash" {
        track_bash_modified_files(bash_command, output, edited_files);
    } else if tool_name == "search_replace" {
        track_search_replace_files(output, edited_files);
    }
}

fn track_bash_modified_files(command: &str, output: &str, edited_files: &mut Vec<String>) {
    let Some(cwd) = bash_output_cwd(output) else {
        return;
    };

    for file in rm_file_targets(command, &cwd) {
        push_edited_file(edited_files, file);
    }
    for file in bash_workspace_modified_files(output, &cwd) {
        push_edited_file(edited_files, file);
    }
}

fn bash_output_cwd(output: &str) -> Option<PathBuf> {
    output.lines().rev().find_map(|line| {
        line.strip_prefix("[cwd: ")
            .and_then(|rest| rest.strip_suffix(']'))
            .map(PathBuf::from)
    })
}

fn bash_workspace_modified_files(output: &str, cwd: &std::path::Path) -> Vec<String> {
    let Some(line) = output
        .lines()
        .find(|line| line.starts_with("[workspace modified via bash: "))
    else {
        return Vec::new();
    };
    let Some(rest) = line.strip_prefix("[workspace modified via bash: ") else {
        return Vec::new();
    };
    let changed = rest.split(". If ").next().unwrap_or(rest);
    changed
        .split(',')
        .map(str::trim)
        .filter(|file| !file.is_empty() && !file.starts_with('+'))
        .map(|file| {
            let path = std::path::Path::new(file);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            }
            .to_string_lossy()
            .to_string()
        })
        .collect()
}

fn track_search_replace_files(output: &str, edited_files: &mut Vec<String>) {
    for line in output.lines() {
        let trimmed = line.trim_start();
        let Some((path, _summary)) = trimmed.split_once(" (") else {
            continue;
        };
        if path.is_empty() {
            continue;
        }
        push_edited_file(edited_files, path.to_string());
    }
}

fn rm_file_targets(command: &str, cwd: &std::path::Path) -> Vec<String> {
    let tokens = shell_words(command);
    let mut targets = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if tokens[i] != "rm" {
            i += 1;
            continue;
        }

        i += 1;
        let mut rm_targets = Vec::new();
        let mut recursive = false;
        while i < tokens.len() {
            let token = &tokens[i];
            if matches!(token.as_str(), "&&" | "||" | ";" | "|") {
                break;
            }
            if token.starts_with('-') {
                if token.contains('r') || token.contains('R') {
                    recursive = true;
                }
                i += 1;
                continue;
            }

            let path = std::path::Path::new(token);
            let full_path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            };
            rm_targets.push(full_path.to_string_lossy().to_string());
            i += 1;
        }

        if !recursive {
            targets.extend(rm_targets);
        }
    }
    targets
}

fn push_edited_file(edited_files: &mut Vec<String>, file: String) {
    if !edited_files.contains(&file) {
        edited_files.push(file);
    }
}

fn shell_words(raw: &str) -> Vec<String> {
    raw.split_whitespace()
        .map(|token| {
            token.trim_matches(|c| {
                matches!(
                    c,
                    '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | ','
                )
            })
        })
        .filter(|token| !token.is_empty())
        .map(|token| token.to_string())
        .collect()
}

/// Whether a `ReloadConfig` should wipe the existing conversation history.
///
/// Prior behavior cleared whenever the `default_provider` name changed.
/// That was too aggressive: CodingPlan registers one provider entry per
/// model, so a user swapping Kimi ↔ GLM via `/model` lost all context
/// every time — even though both entries are the same `openai` type and
/// all known cross-model differences (reasoning_content echo policy,
/// DeepSeek content-field requirement, tool_call args JSON repair) are
/// now handled in the per-provider send path.
///
/// Current policy:
/// - Same `provider_type` on both sides → keep history. This covers the
///   common Kimi/GLM/DeepSeek-through-AtomGit swap.
/// - Different `provider_type` (e.g. openai → claude) → clear, because
///   tool_call id formats and tool_use block translation between the
///   OpenAI-shaped and Anthropic-shaped messages haven't been proven
///   round-trip clean.
/// - Can't resolve the old type (old provider was removed from config)
///   → clear when the name changed, matching the pre-existing safe
///   default.
fn reload_should_clear_conversation(
    old_name: &str,
    old_type: Option<&str>,
    new_name: &str,
    new_type: Option<&str>,
) -> bool {
    match (old_type, new_type) {
        (Some(a), Some(b)) => a != b,
        _ => old_name != new_name,
    }
}

/// D2 Tier 1: replace the `output` of every ToolResult in turns older
/// than the last `keep_recent_turns` with a one-line stub. Cheapest
/// destructive tier — preserves the conversation skeleton (assistant
/// text, tool-call shapes, paired result IDs) so the model can still
/// reason about *what was attempted*, just not the heavy outputs.
///
/// The previous emergency path (`truncate(len - 4)`) destroyed the
/// skeleton too. Keeping it intact is what lets the model resume
/// after compaction without re-exploring.
/// D2 Tier 3: drop oldest messages until total tokens (incl. system) <=
/// `target_tokens`. Token-driven — never drops a fixed number of
/// messages, since that's how `truncate(len - 4)` corrupted state.
///
/// Sacred invariants (won't violate even if it means staying over budget):
/// 1. The FIRST real user message is kept — it's the conversation
///    anchor that `/resume` display starts from. Dropping it leaves
///    the user with a session that opens on a tool_call with no
///    visible reason for it.
/// 2. The LAST real user message is kept — it's the current question
///    the model is answering. Dropping it breaks the in-flight turn.
/// 3. Both filters skip `synthetic` user messages — agent-authored
///    injections like `[Context was compressed]` / `Output limit hit`
///    / `[Additional context]` are plumbing, NOT user anchors. The
///    pre-`synthetic`-field code matched any `Role::User` here and
///    routinely picked a synthetic injection as the "last user",
///    leaving the original prompt unprotected for the drain.
/// 4. The drop boundary snaps to a turn boundary so we never split a
///    `tool_call` from its paired `tool_result`.
fn hard_truncate_to_target(
    conv: &mut crate::conversation::Conversation,
    target_tokens: usize,
    sys_tokens: usize,
) {
    use crate::conversation::message::{Message, MessageContent, Role};
    if conv.messages.is_empty() {
        return;
    }
    let total_budget = target_tokens.saturating_sub(sys_tokens);

    let is_real_user = |m: &Message| m.role == Role::User && !m.synthetic;
    let first_real_user_idx = conv.messages.iter().position(|m| is_real_user(m));
    let last_real_user_idx = conv.messages.iter().rposition(|m| is_real_user(m));

    let mut kept_tokens = 0usize;
    let mut keep_from = conv.messages.len();
    for i in (0..conv.messages.len()).rev() {
        let mt = conv.messages[i].estimate_tokens();
        // Sacred set: first AND last real user messages. Both pass
        // through regardless of remaining budget.
        let is_sacred =
            Some(i) == first_real_user_idx || Some(i) == last_real_user_idx;
        if !is_sacred && kept_tokens + mt > total_budget && keep_from < conv.messages.len() {
            break;
        }
        kept_tokens += mt;
        keep_from = i;
    }

    // Snap forward: don't start at a ToolResult orphan (its paired
    // assistant tool_call would be in the dropped section). Keep
    // walking forward until we land on a User or AssistantText message.
    while keep_from < conv.messages.len() {
        match &conv.messages[keep_from].content {
            MessageContent::ToolResult(_) | MessageContent::ToolResultRef(_) => {
                keep_from += 1;
            }
            _ => break,
        }
    }
    // Don't skip past either sacred anchor. Better to ship over budget
    // than to drop the original prompt or the current question.
    if let Some(lu) = last_real_user_idx {
        keep_from = keep_from.min(lu);
    }
    if let Some(fr) = first_real_user_idx {
        keep_from = keep_from.min(fr);
    }

    if keep_from > 0 {
        conv.messages.drain(0..keep_from);
        conv.turn_tracker = crate::conversation::turn::TurnTracker::rebuild(&conv.messages);
    }
}

/// True when an upstream API error string indicates the request exceeded
/// the model's context-length budget. Covers OpenRouter's verbose 400
/// message, OpenAI's `context_length_exceeded` code, and Anthropic's
/// "prompt is too long". Used by the retry path to route into the
/// compression branch instead of blindly re-sending the same oversized
/// request.
fn is_context_overflow_error(e: &str) -> bool {
    e.contains("context length")
        || e.contains("context_length_exceeded")
        || e.contains("maximum context")
        || e.contains("prompt is too long")
        || e.contains("reduce the length")
}

/// Extract the provider's actually-enforced context limit from a 400/
/// overflow error message, if it's discoverable. Used by D2 emergency
/// compaction so we compact toward the *real* limit (proxy-enforced)
/// rather than the configured ctx_window — which can be much larger
/// than what the upstream actually accepts.
///
/// Self-built proxies for open-weight models are the worst offender:
/// the model is nominally 128K but the proxy enforces 64K, and the
/// framework can't know that without parsing the rejection.
///
/// Recognised shapes (case-sensitive, all observed in real datalogs):
/// - OpenAI / GLM proxy: `maximum context length is 65536 tokens`
/// - OpenRouter: `This endpoint's maximum context length is 200000 tokens`
/// - Generic: `context length of 32768`
/// - Anthropic-ish: `prompt is too long: 200000 tokens > 200000 maximum`
fn extract_provider_ctx_limit(e: &str) -> Option<usize> {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        // Three anchors, any of them satisfies. Number captured is the
        // smallest plausible limit token count (≥ 1024 — drop very small
        // numbers that appear in unrelated parts of error bodies).
        regex::Regex::new(
            r"(?:maximum context length (?:is|of)|context length of|context length limit (?:is|of)|tokens? > (?P<rhs>\d+))\s*(?P<lhs>\d+)?",
        )
        .expect("valid regex")
    });
    for caps in re.captures_iter(e) {
        let n = caps
            .name("lhs")
            .or_else(|| caps.name("rhs"))
            .and_then(|m| m.as_str().parse::<usize>().ok());
        // Filter out tiny numbers that aren't real ctx limits — every
        // realistic context window is at least a few thousand tokens.
        if let Some(n) = n {
            if n >= 1024 {
                return Some(n);
            }
        }
    }
    None
}

fn is_rate_limited_error(e: &str) -> bool {
    // English / HTTP standard patterns.
    if e.contains("429") || e.contains("rate") || e.contains("Too Many") {
        return true;
    }
    // Chinese / gateway-side patterns. GitCode's litellm proxy on
    // glm-5.1 returns the user-facing 「模型「X」的请求负载过高，
    // 请稍后再试」 message via in-stream SSE (then closes the
    // connection without [DONE], surfaced as StreamEvent::Error by
    // openai.rs's abrupt-close discriminator). Without these
    // patterns the error fell through to the generic 3-shot retry
    // branch — proper rate-limit handling (5 retries, 3-30s
    // exponential backoff) only fires when this matches.
    e.contains("请求负载过高")
        || e.contains("请求过于频繁")
        || e.contains("服务繁忙")
        || e.contains("限流")
}

/// Parse a gateway-suggested cooldown (in seconds) out of a rate-limit error
/// body, e.g. litellm's `"No deployments available for selected model. Try
/// again in 10 seconds."` Returns `None` when no explicit hint is present, in
/// which case the caller falls back to its exponential backoff. ASCII-only
/// lowercasing keeps byte offsets aligned with the original so the digit slice
/// is valid even when the message contains earlier multibyte (e.g. Chinese)
/// text.
fn parse_retry_after_hint(e: &str) -> Option<u64> {
    let lower = e.to_ascii_lowercase();
    let idx = lower.find("try again in ")?;
    let rest = &e[idx + "try again in ".len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u64>().ok()
}

/// Back off for `secs`, but wake the instant the user cancels. Returns `true` if
/// a cancellation landed during the wait (the caller must stop and finalize),
/// `false` if the full backoff elapsed. Every retry backoff in `run_turn_loop`
/// routes through this instead of a bare `sleep().await`: otherwise a cancel
/// pressed during a backoff (up to 30s for the rate-limit path) isn't observed
/// until the sleep finishes, which the user perceives as "cancel not working"
/// even though the loop-top guard would eventually catch it. Mirrors the
/// cancel-cooperative wait used by the /goal evaluator backoff.
async fn backoff_or_cancelled(secs: u64, cancel: &CancellationToken) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(secs)) => false,
        _ = cancel.cancelled() => true,
    }
}

fn is_auth_error(e: &str) -> bool {
    e.contains("401 ")
        || e.contains("403 ")
        || e.contains("Unauthorized")
        || e.contains("Forbidden")
        || e.contains("invalid_api_key")
        || e.contains("incorrect_api_key")
}

/// True when the upstream error means the account is out of funds — a
/// **non-retryable** condition for the current request: retrying the same
/// call cannot make money appear, it only delays the real error behind
/// misleading "请求失败，重试" lines. Mirrors the `is_quota_exhausted`
/// fail-fast branch, but for payment/balance rather than rate quota.
///
/// HTTP 402 (Payment Required) is the standard code, but providers are
/// inconsistent: DeepSeek returns `{"error":{"message":"Insufficient
/// Balance"}}`, OpenAI uses `insufficient_quota`, and Chinese gateways say
/// 余额不足/欠费. Match the message text plus the canonical status
/// reason rather than a bare "402" substring (which would false-positive on
/// token counts in the body).
fn is_insufficient_balance_error(e: &str) -> bool {
    e.contains("Insufficient Balance")
        || e.contains("insufficient_balance")
        || e.contains("insufficient_quota")
        || e.contains("Payment Required")
        || e.contains("余额不足")
        || e.contains("欠费")
}

/// True when the error came from `build_codingplan_headers` failing
/// with `SignError::Unavailable` — i.e. an open-source AtomCode build
/// tried to issue a request that requires the closed-source signing
/// module. This is **terminal**: no amount of retry will produce a
/// valid signature in this binary; the user must install the official
/// release. The retry classifier short-circuits to fail-fast on this
/// to avoid the otherwise pointless 3-shot retry cycle.
///
/// Match on the official releases URL substring — both the English
/// (`Msg::CpOfficialBuildRequired`) and Chinese variants embed it
/// verbatim, and the URL is not localised, so a single substring
/// match handles both locales without coupling to translation strings.
fn is_codingplan_unavailable_error(e: &str) -> bool {
    e.contains("atomgit_atomcode/atomcode/releases")
}

/// Max quick-retries for an empty provider response before giving up.
/// Empty responses are instant and transient (see `is_empty_response_error`),
/// so more attempts with a short backoff recover the common bursty case far
/// better than the generic 3-shot / 18s path.
const EMPTY_RESPONSE_MAX_RETRIES: usize = 5;

/// True when the turn failed because the provider returned a 200 OK with a
/// completely empty completion — no text and no tool calls (per datalog,
/// usually no reasoning either). Emitted verbatim by `turn::runner` when both
/// `text_buf` and `tool_calls_buf` are empty.
///
/// Confirmed transient on deepseek-v4-flash (atomgit gateway → DeepSeek's
/// official API): the identical request resent recovers — one logged session
/// returned empty×3 then succeeded on the 4th send — and the empty rate was
/// higher at <10K context than at >100K, ruling out a context-size cause.
/// The retry classifier routes this to a dedicated fast-retry branch instead
/// of the generic backoff, so the common hiccup recovers in ~1-2s rather than
/// stalling 18s and then mislabeling itself "请求失败".
fn is_empty_response_error(e: &str) -> bool {
    e.contains("Provider returned an empty response")
}

fn should_show_raw_api_error() -> bool {
    !matches!(
        std::env::var("ATOMCODE_SHOW_RAW_API_ERROR").as_deref(),
        Ok("0") | Ok("false") | Ok("FALSE") | Ok("no") | Ok("NO")
    )
}

fn public_error_reason(e: &str) -> &'static str {
    if is_context_overflow_error(e) {
        "上下文过长"
    } else if is_insufficient_balance_error(e) {
        "余额不足"
    } else if is_auth_error(e) {
        "认证失败或无权限"
    } else if is_rate_limited_error(e) {
        "请求过于频繁或额度已用尽"
    } else if e.contains("Stream timeout") || e.contains("no event for") {
        "模型响应超时"
    } else if e.contains("Connection failed")
        || e.contains("dns")
        || e.contains("TLS")
        || e.contains("certificate")
        || e.contains("connect")
    {
        "网络连接失败"
    } else if e.contains("500")
        || e.contains("502")
        || e.contains("503")
        || e.contains("504")
        || e.contains("Internal Server Error")
        || e.contains("Bad Gateway")
        || e.contains("Service Unavailable")
        || e.contains("Gateway Timeout")
    {
        "上游服务暂时不可用"
    } else if e.contains("400") {
        "请求参数无效"
    } else {
        "请求失败"
    }
}

fn public_error_message(e: &str) -> String {
    if should_show_raw_api_error() {
        return e.to_string();
    }

    match public_error_reason(e) {
        "上下文过长" => {
            "请求超过了模型上下文长度限制。请减少附加内容或缩短会话历史后重试。".to_string()
        }
        "余额不足" => {
            "账户余额不足，请前往模型提供方控制台充值后重试。".to_string()
        }
        "认证失败或无权限" => {
            "认证失败或当前账号无权限访问该模型。请检查 API Key 和提供方权限配置。".to_string()
        }
        "请求过于频繁或额度已用尽" => {
            "请求过于频繁，或当前额度已用尽。请稍后再试。".to_string()
        }
        "模型响应超时" => "模型响应超时，请稍后重试。".to_string(),
        "网络连接失败" => "连接模型服务失败，请检查网络后重试。".to_string(),
        "上游服务暂时不可用" => "模型服务暂时不可用，请稍后重试。".to_string(),
        "请求参数无效" => "请求被模型服务拒绝，请调整输入后重试。".to_string(),
        _ => e.to_string(),
    }
}

/// Build the post-compaction status note injected into the conversation so
/// the model can resume without re-exploring. Returns `None` when there is
/// nothing worth saying (all inputs empty) — caller skips the injection then.
///
/// Extracted as a free function so the truncation / formatting is testable
/// without building a full `AgentLoop`.
/// Per-turn reminder injected into the last user message while plan mode is
/// active (empty otherwise). Plan mode is deliberately kept OUT of the system
/// prompt to preserve the prefix cache (see `prompt.rs`), and the one-time
/// toggle notice scrolls out of recency fast — so without a per-turn nudge the
/// model drifts and writes the whole implementation inline (even as a code
/// block) instead of presenting a plan and stopping. Riding `turn_reminder`
/// keeps the constraint fresh every turn without touching the cached system
/// prompt.
fn plan_mode_turn_reminder(plan_mode: bool) -> String {
    if !plan_mode {
        return String::new();
    }
    "<system-reminder>\n\
     PLAN MODE is active. Do NOT create, edit, or delete files, and do NOT write out the \
     implementation — not even as code blocks in your reply. Investigate with read-only tools, \
     then present a concise implementation plan and STOP, waiting for the user to review and \
     switch to build mode. Writing the full solution now defeats the purpose of plan mode.\n\
     </system-reminder>"
        .to_string()
}

fn build_post_compress_state(
    current_task: &str,
    files_edited: &[String],
    files_read: &[String],
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if !current_task.is_empty() {
        // chars().take — must be char-boundary safe for multi-byte (CJK)
        // user messages. A byte-slice truncation here would panic or
        // produce invalid UTF-8.
        let task_short: String = current_task.chars().take(200).collect();
        parts.push(format!("TASK: {}", task_short));
    }
    if !files_edited.is_empty() {
        parts.push(format!("FILES EDITED: {}", files_edited.join(", ")));
    }
    if !files_read.is_empty() {
        let recent: Vec<&str> = files_read
            .iter()
            .rev()
            .take(5)
            .map(|s| s.as_str())
            .collect();
        parts.push(format!("RECENTLY READ: {}", recent.join(", ")));
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!(
        "[Context was compressed. Here is your current state:]\n{}",
        parts.join("\n")
    ))
}

/// Format a token count for user-facing banners: `9800` → `"9.8K"`,
/// `137` → `"137"`. Mirrors the `k(...)` closure in the TUI's
/// `format_context_report` so `/compact` output reads the same units
/// as `/context`.
fn fmt_k_tokens(t: usize) -> String {
    if t >= 1000 {
        format!("{:.1}K", t as f64 / 1000.0)
    } else {
        format!("{}", t)
    }
}

#[cfg(test)]
mod backoff_cancel_tests {
    use super::backoff_or_cancelled;
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn already_cancelled_short_circuits_the_backoff() {
        let token = CancellationToken::new();
        token.cancel();
        // A 30s backoff must NOT be waited out when cancel is already set.
        assert!(
            backoff_or_cancelled(30, &token).await,
            "pre-cancelled token must return true immediately"
        );
    }

    #[tokio::test]
    async fn elapsed_backoff_returns_false_when_not_cancelled() {
        let token = CancellationToken::new();
        // 0s sleep elapses at once; token never cancels → caller should retry.
        assert!(
            !backoff_or_cancelled(0, &token).await,
            "a completed backoff must return false"
        );
    }

    #[tokio::test]
    async fn cancel_landing_mid_wait_wins_over_the_sleep() {
        let token = CancellationToken::new();
        let canceller = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            canceller.cancel();
        });
        // 30s sleep must lose to a cancel that lands 20ms in.
        assert!(
            backoff_or_cancelled(30, &token).await,
            "a cancel fired during the wait must win"
        );
    }
}

#[cfg(test)]
mod agent_handle_tests {
    use super::{AgentClient, AgentHandle, AgentRuntimeFactory};

    #[test]
    fn agent_client_clones_command_sender_and_registries() {
        let (cmd_tx, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let tool_registry = std::sync::Arc::new(crate::tool::ToolRegistry::new());
        let skill_registry =
            std::sync::Arc::new(std::sync::RwLock::new(crate::skill::SkillRegistry::new()));

        let client = AgentClient {
            cmd_tx,
            tool_registry: tool_registry.clone(),
            skill_registry: skill_registry.clone(),
        };
        let handle = AgentHandle {
            client: client.clone(),
            event_rx,
        };

        assert!(std::sync::Arc::ptr_eq(
            &client.tool_registry,
            &handle.client.tool_registry
        ));
        assert!(std::sync::Arc::ptr_eq(
            &client.skill_registry,
            &handle.client.skill_registry
        ));
    }

    #[test]
    fn runtime_factory_reports_missing_provider_as_unavailable() {
        let mut config = crate::config::Config::default();
        config.default_provider = "missing".to_string();
        config.providers.clear();

        let factory = AgentRuntimeFactory::new_for_test(
            config,
            std::path::PathBuf::from("/tmp/project"),
            std::sync::Arc::new(crate::tool::ToolRegistry::new()),
            std::sync::Arc::new(std::sync::RwLock::new(crate::skill::SkillRegistry::new())),
        );

        let provider = factory.build_provider();

        assert_eq!(
            provider.availability_error(),
            Some("未配置 provider。请使用 /provider 添加 provider 后再试。")
        );
    }

    #[test]
    fn runtime_factory_setters_update_snapshots() {
        let mut factory = AgentRuntimeFactory::new_for_test(
            crate::config::Config::default(),
            std::path::PathBuf::from("/tmp/old"),
            std::sync::Arc::new(crate::tool::ToolRegistry::new()),
            std::sync::Arc::new(std::sync::RwLock::new(crate::skill::SkillRegistry::new())),
        );

        let mut config = crate::config::Config::default();
        config.default_provider = "fresh".to_string();
        factory.set_config(config);
        factory.set_working_dir(std::path::PathBuf::from("/tmp/new"));

        assert_eq!(factory.config.default_provider, "fresh");
        assert_eq!(factory.working_dir, std::path::PathBuf::from("/tmp/new"));
    }

    /// Issue #714: launching with `-y` / `--dangerously-skip-permissions` must
    /// keep the bypass across runtime re-spawns (/session, /resume, /bg). The
    /// factory is built from the initial loop, so it must read the flag back
    /// and a runtime it spawns must inherit it — otherwise approval prompts
    /// silently return while the ⚠ BYPASS badge stays on.
    #[test]
    fn factory_propagates_skip_permissions_from_initial_loop() {
        let mut config = crate::config::Config::default();
        config.providers.clear();
        let tool_context = crate::tool::ToolContext::new(std::path::PathBuf::from("/tmp/p714"));

        // Initial loop launched WITH the bypass (mirrors `atomcode -y`).
        let (loop_, _handle) = super::AgentLoop::new_with_skip_permissions(
            config,
            crate::provider::unavailable_provider("test"),
            crate::tool::ToolRegistry::new(),
            tool_context,
            crate::conversation::Conversation::new(),
            true,
        );
        assert!(
            loop_.dangerously_skip_permissions,
            "the initial loop must record the launch-time -y bypass"
        );

        // The factory built from that loop must carry the flag forward...
        let factory = AgentRuntimeFactory::from_initial_loop(&loop_, None);
        assert!(
            factory.dangerously_skip_permissions,
            "factory must inherit the bypass so re-spawns honor -y (issue #714)"
        );

        // ...and the runtime it constructs must, too. `will_auto_approve` is the
        // observable signal: under bypass it returns true for a normally-gated call.
        let (spawned, _h) = super::AgentLoop::new_with_shared_parts_skip(
            factory.config.clone(),
            factory.build_provider(),
            factory.shared_tools.clone(),
            factory.skill_registry.clone(),
            Some("runtime-1".to_string()),
            crate::tool::ToolContext::new(factory.working_dir.clone()),
            crate::conversation::Conversation::new(),
            factory.dangerously_skip_permissions,
        );
        let call = crate::tool::ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        };
        assert!(
            spawned.turn_runner.permission.will_auto_approve(
                &call,
                &crate::tool::ApprovalRequirement::RequireApprovalAlways("sudo".into()),
            ),
            "a factory-spawned runtime must auto-approve under -y, even RequireApprovalAlways"
        );
    }

    /// Part-2 regression (system-prompt prefix-cache stability): the system
    /// prompt is messages[0], so any byte change zeroes the entire prefix
    /// cache. It must stay byte-identical across turns even when the model
    /// changes the working directory (the most common mid-session mutation).
    /// Only an explicit cache invalidation (plan-mode toggle, /clear, reload,
    /// /cd) may rebuild it.
    #[tokio::test]
    async fn system_prompt_is_frozen_across_model_cwd_change() {
        let old_wd =
            std::path::PathBuf::from(format!("/tmp/atomcode_sysfreeze_old_{}", std::process::id()));
        let new_wd =
            std::path::PathBuf::from(format!("/tmp/atomcode_sysfreeze_new_{}", std::process::id()));

        let tool_context = crate::tool::ToolContext::new(old_wd.clone());
        let (mut loop_, _handle) = super::AgentLoop::new_with_shared_parts(
            crate::config::Config::default(),
            crate::provider::unavailable_provider("test"),
            std::sync::Arc::new(crate::tool::ToolRegistry::new()),
            std::sync::Arc::new(std::sync::RwLock::new(crate::skill::SkillRegistry::new())),
            Some("test".to_string()),
            tool_context,
            crate::conversation::Conversation::new(),
        );

        // Turn 1: build + freeze.
        let sys1 = loop_.build_system_prompt();
        assert!(
            sys1.contains(&old_wd.display().to_string()),
            "sanity: the working directory IS interpolated into the system prompt"
        );

        // Model runs its `cd` tool — writes working_dir directly, exactly like
        // tool/cd.rs / tool/bash.rs do (does NOT go through change_dir).
        *loop_.turn_runner.context.working_dir.write().await = new_wd.clone();

        // Turn 2+: system prompt MUST be byte-identical despite the cwd change.
        let sys2 = loop_.build_system_prompt();
        assert_eq!(
            sys1, sys2,
            "system prompt must be frozen across a model cwd change (prefix cache)"
        );
        assert!(
            !sys2.contains(&new_wd.display().to_string()),
            "the frozen prompt must NOT pick up the new cwd"
        );

        // Explicit boundary: invalidating the cache (as plan-mode toggle,
        // /clear, reload, or /cd do) rebuilds and DOES reflect the new cwd —
        // proving the freeze above was real, not a coincidence.
        loop_.cached_system_prompt = None;
        let sys3 = loop_.build_system_prompt();
        assert_ne!(sys1, sys3, "after invalidation the prompt should rebuild");
        assert!(
            sys3.contains(&new_wd.display().to_string()),
            "the rebuilt prompt should reflect the new cwd"
        );
    }

    /// Regression for the user-reported web_search "wrong year" bug: with no
    /// current-date anchor the model fell back to its training-era year and
    /// built "2025 latest news" queries in 2026. The current date (day-granular,
    /// frozen per session — see `system_prompt_is_frozen_across_model_cwd_change`)
    /// must be present in the system prompt's env metadata.
    #[tokio::test]
    async fn system_prompt_includes_current_date() {
        let wd = std::path::PathBuf::from(format!("/tmp/atomcode_date_{}", std::process::id()));
        let tool_context = crate::tool::ToolContext::new(wd);
        let (mut loop_, _handle) = super::AgentLoop::new_with_shared_parts(
            crate::config::Config::default(),
            crate::provider::unavailable_provider("test"),
            std::sync::Arc::new(crate::tool::ToolRegistry::new()),
            std::sync::Arc::new(std::sync::RwLock::new(crate::skill::SkillRegistry::new())),
            Some("test".to_string()),
            tool_context,
            crate::conversation::Conversation::new(),
        );
        let sys = loop_.build_system_prompt();
        assert!(
            sys.contains("Today's date:"),
            "system prompt must carry a current-date anchor for time-sensitive queries"
        );
        let year = chrono::Local::now().format("%Y").to_string();
        assert!(
            sys.contains(&year),
            "system prompt must contain the current year ({year}) so the model stops using a training-era year for web_search"
        );
    }

    /// Regression: in plan mode the read-only tool gate blocks file writes, but
    /// not the model writing the implementation into its reply. A per-turn
    /// reminder must carry the "plan only, don't implement, STOP" constraint so
    /// the model doesn't drift into dumping the full solution inline. Build mode
    /// must carry no such reminder (keeps the last user turn clean + cacheable).
    #[test]
    fn plan_mode_turn_reminder_present_only_in_plan_mode() {
        assert!(
            super::plan_mode_turn_reminder(false).is_empty(),
            "build mode must not inject a plan reminder"
        );
        let r = super::plan_mode_turn_reminder(true);
        assert!(r.contains("PLAN MODE"), "plan reminder must name plan mode: {r:?}");
        let low = r.to_lowercase();
        // The exact failure mode: writing the implementation inline as a code block.
        assert!(low.contains("code block"), "must forbid inline code-block dumps: {r:?}");
        assert!(low.contains("stop"), "must tell the model to stop after the plan: {r:?}");
    }

    /// Part-2 (systemA): plan mode must NOT live in the system prompt. Toggling
    /// it would otherwise rewrite messages[0] and zero the prefix cache. It's
    /// announced via a synthetic history message instead (see SetPlanMode) and
    /// enforced by read-only tool gating.
    #[tokio::test]
    async fn plan_mode_is_not_in_system_prompt() {
        let wd = std::path::PathBuf::from(format!("/tmp/atomcode_planmode_{}", std::process::id()));
        let (mut loop_, _handle) = super::AgentLoop::new_with_shared_parts(
            crate::config::Config::default(),
            crate::provider::unavailable_provider("test"),
            std::sync::Arc::new(crate::tool::ToolRegistry::new()),
            std::sync::Arc::new(std::sync::RwLock::new(crate::skill::SkillRegistry::new())),
            Some("test".to_string()),
            crate::tool::ToolContext::new(wd),
            crate::conversation::Conversation::new(),
        );

        loop_.plan_mode = false;
        let sys_off = loop_.build_system_prompt();

        // Toggle plan mode ON and rebuild from a cold cache.
        loop_.cached_system_prompt = None;
        loop_.plan_mode = true;
        let sys_on = loop_.build_system_prompt();

        assert!(
            !sys_on.contains("PLAN MODE"),
            "plan mode must NOT appear in the system prompt"
        );
        assert_eq!(
            sys_off, sys_on,
            "plan_mode must not change the system prompt at all (it's frozen; mode rides in history)"
        );
    }

    #[test]
    fn cloned_runtime_factories_allocate_unique_labels() {
        let factory = AgentRuntimeFactory::new_for_test(
            crate::config::Config::default(),
            std::path::PathBuf::from("/tmp/project"),
            std::sync::Arc::new(crate::tool::ToolRegistry::new()),
            std::sync::Arc::new(std::sync::RwLock::new(crate::skill::SkillRegistry::new())),
        );
        let cloned = factory.clone();

        assert_eq!(factory.next_runtime_label(), "runtime-2");
        assert_eq!(cloned.next_runtime_label(), "runtime-3");
    }
}

#[cfg(test)]
mod classifier_tests {
    use super::{
        extract_provider_ctx_limit, is_auth_error, is_codingplan_unavailable_error,
        is_context_overflow_error, is_empty_response_error, is_insufficient_balance_error,
        is_rate_limited_error, parse_retry_after_hint, public_error_message, public_error_reason,
        reload_should_clear_conversation,
    };

    // ── reload_should_clear_conversation ──

    #[test]
    fn reload_same_type_different_name_keeps_history() {
        // The common CodingPlan case: one provider entry per model, all
        // `openai`-typed. User swaps Kimi ↔ GLM via `/model` — history MUST
        // survive, otherwise every model switch is a brand-new session.
        assert!(!reload_should_clear_conversation(
            "AtomGit-kimi-k2.6",
            Some("openai"),
            "AtomGit-glm5",
            Some("openai"),
        ));
    }

    #[test]
    fn reload_different_type_clears() {
        // Cross-type (openai → claude) is not proven round-trip clean:
        // tool_call id formats differ, tool_use block translation is
        // non-trivial. Stay safe and clear.
        assert!(reload_should_clear_conversation(
            "kimi",
            Some("openai"),
            "claude-sonnet",
            Some("claude"),
        ));
    }

    #[test]
    fn reload_missing_old_type_falls_back_to_name_change() {
        // Old provider was removed from new_config (rename, delete, config
        // rewritten by wizard). We can't tell whether types match, so fall
        // back to the historical safe default: clear when the name flips.
        assert!(reload_should_clear_conversation(
            "old-gone",
            None,
            "new-arrival",
            Some("openai"),
        ));
        assert!(!reload_should_clear_conversation(
            "same",
            None,
            "same",
            Some("openai"),
        ));
    }

    #[test]
    fn reload_same_name_never_clears() {
        // A no-op ReloadConfig (same default, same type) is a noop here too.
        // Sanity — should not accidentally wipe history.
        assert!(!reload_should_clear_conversation(
            "kimi",
            Some("openai"),
            "kimi",
            Some("openai"),
        ));
    }

    #[test]
    fn openrouter_400_is_overflow() {
        let msg = "API error (400 Bad Request): This endpoint's maximum context \
                   length is 204800 tokens. However, you requested about 745279 \
                   tokens... Please reduce the length of either one.";
        assert!(is_context_overflow_error(msg));
    }

    #[test]
    fn openai_context_length_exceeded_is_overflow() {
        assert!(is_context_overflow_error(
            "{\"error\":{\"code\":\"context_length_exceeded\"}}"
        ));
    }

    #[test]
    fn anthropic_prompt_too_long_is_overflow() {
        assert!(is_context_overflow_error(
            "prompt is too long: 250000 tokens"
        ));
    }

    #[test]
    fn generic_rate_limit_is_not_overflow() {
        assert!(!is_context_overflow_error("429 Too Many Requests"));
    }

    #[test]
    fn empty_provider_response_is_classified() {
        // Exact string emitted by turn::runner when text_buf + tool_calls
        // are both empty (crates/atomcode-core/src/turn/runner.rs).
        let msg = "Provider returned an empty response (no text, no tool calls).";
        assert!(is_empty_response_error(msg));
    }

    #[test]
    fn empty_response_not_confused_with_other_errors() {
        let msg = "Provider returned an empty response (no text, no tool calls).";
        // Must NOT be stolen by (or fail-fast'd in) any other classifier:
        // the dedicated fast-retry branch has to win.
        assert!(!is_context_overflow_error(msg));
        assert!(!is_rate_limited_error(msg));
        assert!(!is_auth_error(msg));
        assert!(!is_insufficient_balance_error(msg));
        assert!(!is_codingplan_unavailable_error(msg));
        // The inline `is_messages_illegal` guard keys off "illegal"/"messages";
        // neither appears in the empty-response string, so it can't steal it.
        assert!(!msg.contains("illegal") && !msg.contains("messages"));
        // And real errors are not misread as empty responses.
        assert!(!is_empty_response_error("429 Too Many Requests"));
        assert!(!is_empty_response_error("prompt is too long: 250000 tokens"));
    }

    #[test]
    fn auth_error_is_not_overflow() {
        assert!(!is_context_overflow_error("401 Unauthorized"));
    }

    #[test]
    fn extract_glm_proxy_ctx_limit() {
        // From the actual datalog that motivated D2.
        let msg = "API error (400 Bad Request) at `http://115.120.18.212:18005/v1/chat/completions`: \
                   {\"error\":{\"message\":\"This model's maximum context length is 65536 tokens. \
                   However, you requested 15210 output tokens and your prompt contains at least \
                   50327 input tokens, for a total of at least 65537 tokens.\"}}";
        assert_eq!(extract_provider_ctx_limit(msg), Some(65536));
    }

    #[test]
    fn extract_openrouter_ctx_limit() {
        let msg = "API error (400): This endpoint's maximum context length is 204800 tokens. \
                   However, you requested about 745279 tokens";
        assert_eq!(extract_provider_ctx_limit(msg), Some(204800));
    }

    #[test]
    fn extract_anthropic_prompt_too_long() {
        let msg = "prompt is too long: 200000 tokens > 200000 maximum";
        assert_eq!(extract_provider_ctx_limit(msg), Some(200000));
    }

    #[test]
    fn extract_no_limit_returns_none_for_non_overflow_errors() {
        assert_eq!(extract_provider_ctx_limit("429 Too Many Requests"), None);
        assert_eq!(extract_provider_ctx_limit("401 Unauthorized"), None);
        assert_eq!(extract_provider_ctx_limit(""), None);
    }

    #[test]
    fn extract_filters_out_implausibly_small_numbers() {
        // Status codes and small ints in error bodies must not be
        // mistaken for context limits.
        let msg = "Error 400: maximum context length is 200 tokens";
        assert_eq!(extract_provider_ctx_limit(msg), None);
    }

    // ── D2 emergency compact tier helpers ──

    use crate::conversation::{Conversation, message::MessageContent};
    use crate::tool::{ToolCall, ToolResult};

    /// Build a synthetic conversation with `n_turns` turns, each carrying
    /// one user message + one assistant tool_call + one tool_result of
    /// `result_size` chars.
    fn build_conv(n_turns: usize, result_size: usize) -> Conversation {
        let mut conv = Conversation::new();
        for t in 0..n_turns {
            conv.add_user_message(&format!("turn {} request", t));
            conv.add_assistant_tool_calls(
                None,
                vec![ToolCall {
                    id: format!("call_{}", t),
                    name: "read_file".into(),
                    arguments: r#"{"file_path":"/x"}"#.into(),
                }],
                None,
            );
            conv.add_tool_result(ToolResult {
                call_id: format!("call_{}", t),
                output: "x".repeat(result_size),
                success: true,
            });
        }
        conv
    }

    fn count_collapsed_results(conv: &Conversation) -> usize {
        // New unified stub format: `[<tool> <ok|FAILED>: N lines, first: …]`.
        // The substring " lines, first:" is unique to the stub shape and
        // robust whether the tool name is "bash", "grep", or "tool".
        conv.messages
            .iter()
            .filter(|m| match &m.content {
                MessageContent::ToolResult(tr) => tr.output.contains(" lines, first:"),
                _ => false,
            })
            .count()
    }

    /// Phase 1 proactive compact: Tier 1 (collapse) is enough for the
    /// common case — heavy old tool_result bodies become stubs and
    /// the conversation token total drops below threshold without
    /// invoking the LLM-summary round trip. Tier 2 only fires when
    /// Tier 1 wasn't enough. This test pins the contract that Tier 1
    /// is invoked first; Tier 2 path is covered separately by the
    /// existing emergency-compact tests.
    #[test]
    fn proactive_tier1_collapses_old_tool_results_only() {
        // Build a conversation heavy with old, large tool_results
        // (typical bash/cargo session shape). After Tier 1 with
        // keep_recent_turns=3, the 3 OLDEST turns' tool_results
        // should be stubs while the 3 RECENT turns retain full
        // payload. Pins the "older=collapsed, newer=intact" split.
        let mut conv = build_conv(/* n_turns */ 6, /* result_size */ 4_000);
        crate::ctx::render::compact_old_tool_results_in_place(&mut conv, 3, false);

        // Walk the messages: each turn pushes (User, AssistantToolCall,
        // ToolResult). 6 turns × 3 msgs = 18 msgs. The first 3 turns
        // are "old"; turns 4-6 are "recent".
        let mut tr_sizes: Vec<usize> = Vec::new();
        for m in &conv.messages {
            if let MessageContent::ToolResult(tr) = &m.content {
                tr_sizes.push(tr.output.len());
            }
        }
        assert_eq!(tr_sizes.len(), 6, "expected 6 tool_results");
        // Old: index 0, 1, 2 — must be stubs (small).
        for &s in &tr_sizes[..3] {
            assert!(
                s < 200,
                "old tool_result must collapse to stub; got len={}",
                s
            );
        }
        // Recent: index 3, 4, 5 — must remain full (4_000 chars + the
        // 'x' chars).
        for &s in &tr_sizes[3..] {
            assert!(
                s >= 4_000,
                "recent tool_result must remain full; got len={}",
                s
            );
        }
    }

    #[test]
    fn collapse_keeps_last_n_turns_full() {
        let mut conv = build_conv(5, 1024);
        crate::ctx::render::compact_old_tool_results_in_place(&mut conv, 2, false);
        // 5 turns, keep last 2 → first 3 should have stubbed tool_results.
        assert_eq!(count_collapsed_results(&conv), 3);
    }

    #[test]
    fn collapse_skips_already_tiny_results() {
        // Tool results under 200 chars aren't worth collapsing — the stub
        // would weigh more than the original.
        let mut conv = build_conv(5, 50);
        crate::ctx::render::compact_old_tool_results_in_place(&mut conv, 2, false);
        assert_eq!(count_collapsed_results(&conv), 0);
    }

    #[test]
    fn collapse_no_op_when_under_keep_threshold() {
        let mut conv = build_conv(2, 1024);
        crate::ctx::render::compact_old_tool_results_in_place(&mut conv, 3, false);
        // Only 2 turns total, keep 3 — nothing to collapse.
        assert_eq!(count_collapsed_results(&conv), 0);
    }

    #[test]
    fn collapse_preserves_call_id_and_success_flag() {
        let mut conv = build_conv(3, 1024);
        crate::ctx::render::compact_old_tool_results_in_place(&mut conv, 1, false);
        // Verify call_0's tool_result still has the right call_id even
        // though its body was stubbed — preserves tool_call/tool_result
        // pairing for OpenAI-style providers.
        let tr = conv
            .messages
            .iter()
            .find_map(|m| match &m.content {
                MessageContent::ToolResult(tr) if tr.call_id == "call_0" => Some(tr),
                _ => None,
            })
            .expect("call_0 result must still exist");
        // New unified stub format: `[<tool> <ok|FAILED>: N lines, first: …]`.
        assert!(tr.output.contains(" lines, first:"));
        assert!(tr.output.starts_with("["));
        assert!(tr.success);
    }

    #[test]
    fn hard_truncate_keeps_last_user_message_even_under_budget() {
        // Tight budget that forces aggressive drops; sacred invariant
        // says the last user msg must survive regardless. This is the
        // structural guarantee the previous `truncate(len-4)` code
        // *violated*, producing the 8516-token catastrophe.
        let mut conv = build_conv(10, 2048);
        super::hard_truncate_to_target(&mut conv, /* target */ 100, /* sys */ 50);
        let has_user = conv
            .messages
            .iter()
            .any(|m| matches!(m.role, crate::conversation::message::Role::User));
        assert!(has_user, "last user message must survive even at tight budget");
    }

    #[test]
    fn hard_truncate_does_not_start_with_orphan_tool_result() {
        // After truncate the first surviving message must NOT be a
        // ToolResult — that would orphan it from its paired assistant
        // tool_call, which OpenAI-style APIs reject with 400.
        let mut conv = build_conv(8, 1024);
        super::hard_truncate_to_target(&mut conv, /* target */ 2000, /* sys */ 100);
        if let Some(first) = conv.messages.first() {
            assert!(
                !matches!(
                    first.content,
                    MessageContent::ToolResult(_) | MessageContent::ToolResultRef(_)
                ),
                "first surviving message must not be an orphan tool_result"
            );
        }
    }

    #[test]
    fn hard_truncate_no_op_when_already_under_target() {
        let mut conv = build_conv(3, 100);
        let before = conv.messages.len();
        super::hard_truncate_to_target(&mut conv, /* target */ 100_000, /* sys */ 100);
        assert_eq!(conv.messages.len(), before);
    }

    #[test]
    fn hard_truncate_rebuilds_turn_tracker() {
        // After draining messages from the front, the turn_tracker must
        // be rebuilt so its Turn entries point at valid indices. Without
        // this, the next build_messages crashes or silently emits wrong
        // boundaries (the bug the old `truncate(len-4)` path also
        // patched, but inconsistently — see the rebuild call there).
        let mut conv = build_conv(10, 2048);
        super::hard_truncate_to_target(&mut conv, /* target */ 1000, /* sys */ 100);
        // Every turn's start_idx must be a valid index into messages.
        for t in &conv.turn_tracker.turns {
            assert!(
                t.start_idx <= conv.messages.len(),
                "turn start_idx {} out of bounds (messages.len()={})",
                t.start_idx,
                conv.messages.len()
            );
        }
    }

    #[test]
    fn stream_timeout_is_summarized() {
        // public_error_message defers to ATOMCODE_SHOW_RAW_API_ERROR (raw by
        // default), so the user-facing string can't be tested deterministically
        // without env-var manipulation that races other parallel tests.
        // public_error_reason covers the routing logic regardless of env state.
        assert_eq!(
            public_error_reason("Stream timeout: no event for 300s"),
            "模型响应超时"
        );
        // The provider-layer byte-idle timeout (now 300s, env-configurable via
        // ATOMCODE_IDLE_TIMEOUT_SECS) emits a different phrasing — it must still
        // route to the same reason so the retry/UX path is unchanged.
        assert_eq!(
            public_error_reason("Stream timeout: no data received for 300s"),
            "模型响应超时"
        );
    }

    #[test]
    fn upstream_5xx_is_summarized() {
        assert_eq!(
            public_error_reason(
                "API error (503 Service Unavailable) at `https://x`:\nbackend trace"
            ),
            "上游服务暂时不可用"
        );
    }

    #[test]
    fn auth_error_is_detected() {
        assert!(is_auth_error(
            "API error (401 Unauthorized): invalid_api_key"
        ));
    }

    /// CpOfficialBuildRequired (English variant) — surfaces from
    /// build_codingplan_headers in open-source builds when an
    /// AtomGit-bound request is attempted.
    #[test]
    fn codingplan_unavailable_detected_in_english_message() {
        let en = "This feature requires the official AtomCode build. \
                  Download it from https://atomgit.com/atomgit_atomcode/atomcode/releases.";
        assert!(is_codingplan_unavailable_error(en));
    }

    /// Same error, Chinese locale. The Releases URL is the substring
    /// match — it's not localised, so the same classifier handles
    /// both en and zh-CN without coupling to translation text.
    #[test]
    fn codingplan_unavailable_detected_in_chinese_message() {
        let zh = "此功能需要官方 AtomCode 构建，请前往 \
                  https://atomgit.com/atomgit_atomcode/atomcode/releases 下载安装。";
        assert!(is_codingplan_unavailable_error(zh));
    }

    /// Negative: an unrelated network error must NOT trip the
    /// classifier. Verifies the URL anchor is narrow enough to avoid
    /// false positives.
    #[test]
    fn codingplan_unavailable_does_not_match_unrelated_errors() {
        assert!(!is_codingplan_unavailable_error(
            "API error (500 Internal Server Error) at `https://api.openai.com/v1/chat/completions`"
        ));
        assert!(!is_codingplan_unavailable_error("Stream timeout: no event for 300s"));
        assert!(!is_codingplan_unavailable_error(""));
    }

    #[test]
    fn rate_limit_error_is_detected() {
        assert!(is_rate_limited_error("API error (429 Too Many Requests)"));
    }

    /// 余额不足 / 402 Payment Required 必须 fail-fast(归类为「余额不足」),
    /// 不能落入通用重试分支被笼统地报「请求失败」。
    #[test]
    fn insufficient_balance_is_classified() {
        assert_eq!(
            public_error_reason(
                "API error (402 Payment Required) at `https://api.deepseek.com/chat/completions`: Insufficient Balance"
            ),
            "余额不足"
        );
        assert_eq!(public_error_reason("insufficient_quota"), "余额不足");
        assert_eq!(public_error_reason("账户余额不足"), "余额不足");
        // 反例:普通限流不应被误判为余额不足。
        assert_ne!(
            public_error_reason("API error (429 Too Many Requests)"),
            "余额不足"
        );
    }

    /// Chinese gateway-side rate-limit blobs streamed in-band by
    /// GitCode litellm (and similar proxies) must route to the
    /// proper rate-limit retry path (5 attempts × 3-30s backoff),
    /// not the generic 3-shot fallback. Without this the
    /// abrupt-close discriminator in openai.rs converts the SSE
    /// blob to StreamEvent::Error but the agent then mis-retries
    /// it.
    #[test]
    fn rate_limit_error_detects_chinese_gateway_patterns() {
        assert!(is_rate_limited_error("模型「GLM-5.1」的请求负载过高，请稍后再试。"));
        assert!(is_rate_limited_error("请求过于频繁，请稍后再试"));
        assert!(is_rate_limited_error("服务繁忙"));
        assert!(is_rate_limited_error("当前已被限流"));
        // Negative: a vanilla error must NOT be classified as rate
        // limit just because it mentions "请稍后再试" alone
        // (which is generic Chinese "try again later").
        assert!(!is_rate_limited_error("请稍后再试"));
        assert!(!is_rate_limited_error("API error (500 Internal Server Error)"));
    }

    #[test]
    fn retry_after_hint_parsed_from_litellm_no_deployments_body() {
        // The exact shape elegant001 hit: litellm "No deployments available"
        // with an embedded cooldown hint. The retry must honour the 10s so it
        // doesn't hammer the still-cold pool at 3s.
        let e = "[429] No deployments available for selected model, Try again in 10 seconds. \
                 Passed model=deepseek-v4-flash. pre-call-checks=True, cooldown_list=['b5cb...']";
        assert_eq!(parse_retry_after_hint(e), Some(10));
        // Capital-T phrasing + a multibyte prefix must still parse (byte offsets
        // stay aligned under ASCII-only lowercasing).
        assert_eq!(
            parse_retry_after_hint("模型繁忙：Try again in 30 seconds."),
            Some(30)
        );
        // No hint → None (caller keeps its exponential backoff).
        assert_eq!(parse_retry_after_hint("429 Too Many Requests"), None);
        assert_eq!(parse_retry_after_hint("try again in soon"), None);
    }

    #[test]
    fn invalid_request_is_summarized_without_raw_body() {
        let old = std::env::var("ATOMCODE_SHOW_RAW_API_ERROR").ok();
        unsafe { std::env::set_var("ATOMCODE_SHOW_RAW_API_ERROR", "0") };
        let raw = "API error (400 Bad Request) at `https://x`:\nstack=secret detail";
        assert_eq!(public_error_reason(raw), "请求参数无效");
        assert!(!public_error_message(raw).contains("secret detail"));
        if let Some(v) = old {
            unsafe { std::env::set_var("ATOMCODE_SHOW_RAW_API_ERROR", v) };
        } else {
            unsafe { std::env::remove_var("ATOMCODE_SHOW_RAW_API_ERROR") };
        }
    }

    #[test]
    fn raw_error_is_shown_by_default() {
        let old = std::env::var("ATOMCODE_SHOW_RAW_API_ERROR").ok();
        unsafe { std::env::remove_var("ATOMCODE_SHOW_RAW_API_ERROR") };
        let raw = "API error (400 Bad Request) at `https://x`:\nstack=secret detail";
        assert_eq!(public_error_message(raw), raw);
        if let Some(v) = old {
            unsafe { std::env::set_var("ATOMCODE_SHOW_RAW_API_ERROR", v) };
        }
    }
}

#[cfg(test)]
mod post_compress_state_tests {
    use super::build_post_compress_state;

    #[test]
    fn empty_inputs_return_none() {
        assert!(build_post_compress_state("", &[], &[]).is_none());
    }

    #[test]
    fn task_only() {
        let out = build_post_compress_state("fix login bug", &[], &[]).unwrap();
        assert!(out.starts_with("[Context was compressed. Here is your current state:]\n"));
        assert!(out.contains("TASK: fix login bug"));
        assert!(!out.contains("FILES EDITED"));
        assert!(!out.contains("RECENTLY READ"));
    }

    #[test]
    fn task_exact_200_is_unchanged() {
        // chars().take(200) on an exactly-200-char input must pass through.
        let exact: String = "字".repeat(200);
        let out = build_post_compress_state(&exact, &[], &[]).unwrap();
        let line = out.lines().find(|l| l.starts_with("TASK: ")).unwrap();
        let payload = &line["TASK: ".len()..];
        assert_eq!(payload.chars().count(), 200);
        assert_eq!(payload, exact);
    }

    #[test]
    fn task_201_drops_exactly_one_char() {
        // Boundary: 201 → 200, and must land on a char boundary (not split
        // the last 3-byte "字").
        let over: String = "字".repeat(201);
        let out = build_post_compress_state(&over, &[], &[]).unwrap();
        let line = out.lines().find(|l| l.starts_with("TASK: ")).unwrap();
        let payload = &line["TASK: ".len()..];
        assert_eq!(payload.chars().count(), 200);
        assert!(payload.is_char_boundary(payload.len()));
    }

    #[test]
    fn task_long_multibyte_truncates_safely() {
        // Regression guard: byte-slicing here would panic mid-codepoint.
        let long: String = "字".repeat(500);
        let out = build_post_compress_state(&long, &[], &[]).unwrap();
        let line = out.lines().find(|l| l.starts_with("TASK: ")).unwrap();
        let payload = &line["TASK: ".len()..];
        assert_eq!(payload.chars().count(), 200);
    }

    #[test]
    fn files_edited_comma_joined() {
        let edited = vec!["a.rs".to_string(), "b.rs".to_string()];
        let out = build_post_compress_state("", &edited, &[]).unwrap();
        assert!(out.contains("FILES EDITED: a.rs, b.rs"));
    }

    #[test]
    fn files_read_last_five_reversed() {
        // rev().take(5) → newest first, at most 5.
        let read: Vec<String> = (1..=8).map(|i| format!("f{}.rs", i)).collect();
        let out = build_post_compress_state("", &[], &read).unwrap();
        let line = out
            .lines()
            .find(|l| l.starts_with("RECENTLY READ: "))
            .unwrap();
        assert_eq!(line, "RECENTLY READ: f8.rs, f7.rs, f6.rs, f5.rs, f4.rs");
    }

    #[test]
    fn all_three_parts_combined() {
        let out = build_post_compress_state("task x", &["a.rs".to_string()], &["b.rs".to_string()])
            .unwrap();
        assert!(out.contains("TASK: task x"));
        assert!(out.contains("FILES EDITED: a.rs"));
        assert!(out.contains("RECENTLY READ: b.rs"));
    }
}

#[cfg(test)]
mod fmt_k_tokens_tests {
    use super::fmt_k_tokens;

    #[test]
    fn under_1000_no_suffix() {
        assert_eq!(fmt_k_tokens(0), "0");
        assert_eq!(fmt_k_tokens(137), "137");
        assert_eq!(fmt_k_tokens(999), "999");
    }

    #[test]
    fn one_thousand_and_above_use_k_suffix_with_one_decimal() {
        assert_eq!(fmt_k_tokens(1000), "1.0K");
        assert_eq!(fmt_k_tokens(3700), "3.7K");
        assert_eq!(fmt_k_tokens(9800), "9.8K");
        assert_eq!(fmt_k_tokens(64000), "64.0K");
    }
}

#[cfg(test)]
mod bash_deleted_file_tracking_tests {
    use super::{
        bash_workspace_modified_files, rm_file_targets, track_search_replace_files,
        track_tool_modified_files,
    };
    use std::path::Path;

    #[test]
    fn tracks_simple_rm_target_from_cwd() {
        let targets = rm_file_targets("rm numbers.txt", Path::new("/tmp/project"));
        assert_eq!(targets, vec!["/tmp/project/numbers.txt"]);
    }

    #[test]
    fn skips_recursive_rm_targets() {
        let targets = rm_file_targets("rm -rf dist", Path::new("/tmp/project"));
        assert!(targets.is_empty());
    }

    #[test]
    fn tracks_successful_bash_rm_from_output_cwd() {
        let mut edited = Vec::new();
        track_tool_modified_files(
            "bash",
            "rm numbers.txt",
            "[elapsed: 0.0s, exit: 0]\n[cwd: /tmp/project]",
            &mut edited,
        );
        assert_eq!(edited, vec!["/tmp/project/numbers.txt"]);
    }

    #[test]
    fn tracks_workspace_modified_bash_output() {
        let files = bash_workspace_modified_files(
            "[workspace modified via bash: src/a.rs, /tmp/project/b.txt. If you meant to edit source, use edit_file next time]\n[cwd: /tmp/project]",
            Path::new("/tmp/project"),
        );
        assert_eq!(
            files,
            vec![
                "/tmp/project/src/a.rs".to_string(),
                "/tmp/project/b.txt".to_string()
            ]
        );
    }

    #[test]
    fn tracks_search_replace_output_files() {
        let mut edited = Vec::new();
        track_search_replace_files(
            "Replaced 'old' -> 'new': 2 replacements across 2 files.\n  /tmp/project/a.rs (1 replacements)\n  /tmp/project/b.rs (1 replacements)",
            &mut edited,
        );
        assert_eq!(
            edited,
            vec![
                "/tmp/project/a.rs".to_string(),
                "/tmp/project/b.rs".to_string()
            ]
        );
    }
}

#[cfg(test)]
mod hard_truncate_tests {
    use super::hard_truncate_to_target;
    use crate::conversation::Conversation;
    use crate::conversation::message::{Message, MessageContent, Role};
    use crate::tool::{ToolCall, ToolResult};

    /// Build a "real prompt → many tool calls → synthetic injection"
    /// conversation matching the production bug scenario: user asks a
    /// question, agent explores a codebase across many tool calls (each
    /// chewing tokens), then the agent self-injects a synthetic
    /// `[Context was compressed.]` user message before continuing.
    /// Before this fix, `hard_truncate_to_target` would identify the
    /// SYNTHETIC injection as "last user" and drop the original prompt.
    fn build_real_then_synthetic_conv() -> Conversation {
        let mut conv = Conversation::new();
        conv.messages
            .push(Message::new(Role::User, "ORIGINAL PROMPT: please explore the codebase and explain"));
        // Pad with 10 turns of assistant tool_call + tool_result, each
        // ~80-100 chars so total est ≥ ~300 tokens.
        for i in 0..10 {
            conv.messages.push(Message {
                role: Role::Assistant,
                content: MessageContent::AssistantWithToolCalls {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: format!("c{}", i),
                        name: "read_file".into(),
                        arguments: format!(r#"{{"file_path":"src/module_{}.rs"}}"#, i),
                    }],
                    reasoning_content: None,
                    thinking_blocks: Vec::new(),
                },
                synthetic: false,
            });
            conv.messages.push(Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id: format!("c{}", i),
                    output: format!(
                        "lots of file contents here for module {}, including \
                         many lines of code that the agent had to read",
                        i
                    ),
                    success: true,
                }),
                synthetic: false,
            });
        }
        // The synthetic injection that triggered the bug. Role::User,
        // synthetic=true, [...]-bracketed text.
        conv.messages.push(Message::synthetic_user(
            "[Context was compressed. Here is your current state:]\nTASK: explore the codebase",
        ));
        conv
    }

    /// Regression for the bug reported on /resume (2026-05-25): a
    /// session that triggered hard_truncate would lose the original
    /// user prompt because `last_user_idx` matched the synthetic
    /// `[Context was compressed]` injection instead of the real prompt
    /// at index 0. Resume then opened on a tool_call with no visible
    /// reason for it and the JSON had no prompt to display.
    ///
    /// Post-fix: BOTH the first AND last real user messages are sacred.
    /// The synthetic injection is filtered out by `!m.synthetic`, so
    /// the only real user message (at index 0) is now both first AND
    /// last real-user and is preserved no matter how tight the budget.
    #[test]
    fn hard_truncate_preserves_original_prompt_over_synthetic_injection() {
        let mut conv = build_real_then_synthetic_conv();

        // Tight budget: well below the conversation's total tokens. Pre-
        // fix, `last_user_idx` would land on the synthetic injection
        // (because it had Role::User), leaving messages[0] unprotected
        // and the drain would silently delete the original prompt.
        hard_truncate_to_target(&mut conv, /* target */ 80, /* sys */ 0);

        // ── Primary assertion: messages[0] is still the real prompt. ──
        let first = &conv.messages[0];
        assert!(
            !first.synthetic,
            "messages[0] must be the REAL user prompt, not the synthetic injection"
        );
        assert!(
            matches!(first.role, Role::User),
            "messages[0] must be Role::User, got {:?}",
            first.role
        );
        assert!(
            first.text().unwrap_or("").contains("ORIGINAL PROMPT"),
            "messages[0] must literally be the original prompt, got: {:?}",
            first.text()
        );
        // ── Trade-off documented: with the original prompt at index 0
        // both first_real_user AND last_real_user point there, so the
        // sacred set forces `keep_from = 0` — compaction stays over
        // budget in this scenario rather than dropping the anchor. We
        // accept this: tier 1/2 (tool-result stubbing, LLM summary) are
        // already non-destructive ways to reduce tokens; tier 3 only
        // exists to drop OLD TURNS, and a single-turn conversation has
        // none. Better to leave the convo intact than silently lose the
        // prompt. The pre-fix behavior would have shrunk here — at the
        // cost of breaking /resume display, which is the bug.
    }

    /// `last_user_idx` must also filter synthetics: in a multi-turn
    /// conversation where the user sent two real prompts AND the agent
    /// later injected a synthetic, the "last real user" should be the
    /// second human prompt, not the post-synthetic injection. Verifies
    /// the symmetric pole of the sacred set.
    #[test]
    fn hard_truncate_last_real_user_skips_trailing_synthetic() {
        let mut conv = Conversation::new();
        conv.messages.push(Message::new(Role::User, "first real prompt"));
        for i in 0..6 {
            conv.messages.push(Message {
                role: Role::Assistant,
                content: MessageContent::AssistantWithToolCalls {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: format!("a{}", i),
                        name: "bash".into(),
                        arguments: r#"{"command":"echo ok"}"#.into(),
                    }],
                    reasoning_content: None,
                    thinking_blocks: Vec::new(),
                },
                synthetic: false,
            });
            conv.messages.push(Message {
                role: Role::Tool,
                content: MessageContent::ToolResult(ToolResult {
                    call_id: format!("a{}", i),
                    output: "ok and lots of output filler bytes for token weight".into(),
                    success: true,
                }),
                synthetic: false,
            });
        }
        conv.messages
            .push(Message::new(Role::User, "second real follow-up prompt"));
        conv.messages.push(Message::synthetic_user(
            "[Context was compressed.] dropped a bunch of stale state",
        ));

        hard_truncate_to_target(&mut conv, /* target */ 80, /* sys */ 0);

        // Find what survived as the LAST user-role message — it should
        // be the second REAL prompt, not the trailing synthetic.
        let last_user = conv
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, Role::User))
            .expect("at least one user message must survive");
        // We may also see the trailing synthetic if it survived, so
        // iterate further to find the most recent REAL one.
        let last_real = conv
            .messages
            .iter()
            .rev()
            .find(|m| matches!(m.role, Role::User) && !m.synthetic)
            .expect("a real user message must survive");
        assert!(
            last_real
                .text()
                .unwrap_or("")
                .contains("second real follow-up"),
            "last real user must be the second real prompt, got: {:?}",
            last_real.text()
        );
        // The trailing synthetic might or might not have survived
        // depending on budget — what matters is that `last_user` (if it
        // happens to be the synthetic) doesn't shadow the real one.
        let _ = last_user;
    }

    /// Empty conversation: no-op, no panic. Covers the early-return.
    #[test]
    fn hard_truncate_empty_convo_is_noop() {
        let mut conv = Conversation::new();
        hard_truncate_to_target(&mut conv, 100, 0);
        assert_eq!(conv.messages.len(), 0);
    }
}
