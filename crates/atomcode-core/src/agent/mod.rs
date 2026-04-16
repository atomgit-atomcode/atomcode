//! The AgentLoop — a standalone agent that processes user messages,
//! calls LLM providers, executes tools, and communicates with the UI
//! via channels. Decoupled from any TUI concerns.

pub mod git_checkpoint;
// pub mod knowledge; // removed: low-value cross-session memory, see git history
pub mod sub_agent;
pub mod subtask_driver;
// task_classifier removed — replaced by state-based decisions in handle_send_message.
// pub mod task_classifier;

pub mod execute;
mod diagnose;
mod discipline;
mod prompt;
mod services;
mod tool_dispatch;
mod verify;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::conversation::Conversation;
use crate::provider::LlmProvider;
use crate::skill::SkillRegistry;
use crate::tool::{
    PermissionDecision, PermissionStore, ToolCall, ToolContext, ToolRegistry,
};
use crate::tool::use_skill::UseSkillTool;
use crate::turn::event::{TurnEvent, TurnResult};
use crate::turn::runner::TurnRunner;

/// Commands sent FROM the UI TO the agent loop.
#[derive(Debug)]
pub enum AgentCommand {
    /// User sent a message (may include attached file content).
    SendMessage(String),
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
    /// Set messages from a resumed session.
    SetMessages(Vec<crate::conversation::message::Message>),
    /// Set plan mode (read-only exploration, no edits).
    SetPlanMode(bool),
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

/// Events sent FROM the agent loop TO the UI.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// LLM text delta (streaming).
    TextDelta(String),
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
    },
/// Turn was cancelled by user before completion.
    /// The conversation has been cleaned up - partial messages removed.
    /// Contains the cleaned message list for TUI to sync.
    TurnCancelled { messages: Vec<crate::conversation::message::Message> },
    /// An error occurred.
    Error(String),
    /// Sub-agent progress (real-time parallel task display).
    SubAgentProgress {
        file: String,
        status: String,
    },
    /// Working directory changed.
    WorkingDirChanged(PathBuf),
    /// Context budget stats for logging (not displayed, only written to datalog).
    ContextStats {
        system_tokens: usize,
        sent_tokens: usize,
        dropped_tokens: usize,
        working_set_tokens: usize,
        total_messages: usize,
    },
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
    pub recent_calls: Vec<(String, u64)>,
    pub build_fail_count: usize,
    pub file_read_counts: std::collections::HashMap<String, usize>,
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
    pub config: Config,

    // Execution state
    pub phase: AgentPhase,
    pub turn_tokens: usize,
    pub total_tokens: usize,
    pub turn_start: Option<Instant>,

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

    // Approval channel endpoints for InteractivePermissionDecider
    /// Receives approval requests from InteractivePermissionDecider
    approval_req_rx: mpsc::UnboundedReceiver<crate::turn::permission::ApprovalRequest>,
    /// Sends approval decisions back to InteractivePermissionDecider
    approval_resp_tx: mpsc::UnboundedSender<PermissionDecision>,
    /// Last approval request (for ApproveToolAlways — need to know which tool)
    last_approval_request: Option<crate::turn::permission::ApprovalRequest>,

    // Cancellation token for the current turn
    cancel_token: CancellationToken,

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

    /// Files edited in the previous turn — injected into system prompt so the model
    /// knows where to start when the user reports the same issue again.
    prev_turn_edited_files: Vec<String>,

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

    /// Discovered service URLs extracted from tool outputs (e.g., "http://localhost:3002").
    /// Persisted across turns so the model knows which ports are active.
    /// Key: label (e.g., "frontend", "backend"), Value: URL.
    active_services: std::collections::HashMap<String, String>,

    // Skill registry — provides descriptions for system prompt and powers use_skill tool
    skill_registry: std::sync::Arc<std::sync::RwLock<SkillRegistry>>,

    // Code graph background indexer channel
    reindex_tx: Option<mpsc::UnboundedSender<PathBuf>>,

    // Datalog writer — writes per-turn markdown logs to datalog/ directory.
    datalog: crate::turn::datalog::DatalogWriter,

    // Channels
    cmd_rx: mpsc::UnboundedReceiver<AgentCommand>,
    event_tx: mpsc::UnboundedSender<AgentEvent>,
}

/// Handle for the UI to communicate with the agent.
pub struct AgentHandle {
    pub cmd_tx: mpsc::UnboundedSender<AgentCommand>,
    pub event_rx: mpsc::UnboundedReceiver<AgentEvent>,
}

impl AgentLoop {
    /// Create a new agent loop and its corresponding UI handle.
    pub fn new(
        config: Config,
        provider: Box<dyn LlmProvider>,
        mut tool_registry: ToolRegistry,
        mut tool_context: ToolContext,
        conversation: Conversation,
    ) -> (Self, AgentHandle) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        // Load skills from disk and register the use_skill tool.
        let working_dir = tool_context.working_dir.try_read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| std::path::PathBuf::from("."));

        // Load persisted code graph from disk and share with ToolContext
        let graph_path = working_dir.join(".atomcode").join("graph.bin");
        let code_graph = crate::graph::persist::load(&graph_path);
        let graph = std::sync::Arc::new(tokio::sync::RwLock::new(code_graph));
        tool_context.graph = graph.clone();
        let mut registry = SkillRegistry::new();
        registry.reload(&working_dir);
        let has_skills = !registry.is_empty();
        let skill_registry = std::sync::Arc::new(std::sync::RwLock::new(registry));
        // Only register use_skill tool when skills are available.
        // Otherwise the model invents skill names and wastes turns.
        // Honour ATOMCODE_DISABLE_TOOLS here too — main.rs filters the base
        // CLI tools at construction time, but AgentLoop::new adds internal
        // tools (graph queries, use_skill) that must respect the same
        // gate so `--disable-tools trace_callers` actually works.
        let disabled_internal: std::collections::HashSet<String> = std::env::var("ATOMCODE_DISABLE_TOOLS")
            .ok()
            .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
            .unwrap_or_default();
        let internal_enabled = |name: &str| !disabled_internal.contains(name);

        if has_skills && internal_enabled("use_skill") {
            tool_registry.register(Box::new(UseSkillTool { registry: skill_registry.clone() }));
        }

        // Graph query tools: not exposed to model (adds 5 tool definitions that
        // weak models never use correctly). Graph data is still injected automatically
        // via grep's graph header and auto_inject_graph_context — the model benefits
        // from graph without needing to call these tools directly.
        // To re-enable: set ATOMCODE_GRAPH_TOOLS=1
        if std::env::var("ATOMCODE_GRAPH_TOOLS").map(|v| v == "1").unwrap_or(false) {
            if internal_enabled("trace_callers") {
                tool_registry.register(Box::new(crate::tool::trace_callers::TraceCallersTool));
            }
            if internal_enabled("trace_callees") {
                tool_registry.register(Box::new(crate::tool::trace_callees::TraceCalleesTool));
            }
            if internal_enabled("trace_chain") {
                tool_registry.register(Box::new(crate::tool::trace_chain::TraceChainTool));
            }
            if internal_enabled("file_dependencies") {
                tool_registry.register(Box::new(crate::tool::file_deps::FileDependenciesTool));
            }
            if internal_enabled("blast_radius") {
                tool_registry.register(Box::new(crate::tool::blast_radius::BlastRadiusTool));
            }
        }
        // Build approval channels for interactive permission flow
        let (approval_req_tx, approval_req_rx) = mpsc::unbounded_channel();
        let (approval_resp_tx, approval_resp_rx) = mpsc::unbounded_channel();

        let permission_store = std::sync::Arc::new(std::sync::RwLock::new(PermissionStore::new()));

        let interactive_permission = Box::new(
            crate::turn::permission::InteractivePermissionDecider::new(
                approval_req_tx, approval_resp_rx, permission_store.clone(),
            )
        );

        // Share tool registry between AgentLoop and TurnRunner via Arc.
        let shared_tools = std::sync::Arc::new(tool_registry);

        // Convert Box → Arc so provider can be shared with sub-agents.
        let provider: std::sync::Arc<dyn LlmProvider> = std::sync::Arc::from(provider);

        let turn_runner = TurnRunner {
            provider,
            tools: shared_tools.clone(),
            context: tool_context.clone(),
            config: config.clone(),
            permission: interactive_permission,
            recently_edited_files: Vec::new(),
        };

        let agent = Self {
            conversation,
            tool_registry: shared_tools,
            turn_runner,
            permission_store,
            config,
            phase: AgentPhase::Idle,
            turn_tokens: 0,
            total_tokens: 0,
            turn_start: None,
            tool_call_count: 0,
            turn_count: 0,
            max_turns: None,
            retry_count: 0,
            approval_req_rx,
            approval_resp_tx,
            last_approval_request: None,
            cancel_token: CancellationToken::new(),
            discipline_state: DisciplineState::default(),
            files_read_this_turn: Vec::new(),
            files_edited_this_turn: Vec::new(),
            current_task: String::new(),
            current_tool_name: String::new(),
            prev_turn_edited_files: Vec::new(),
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
            active_services: std::collections::HashMap::new(),
            skill_registry,
            reindex_tx: None,
            datalog: crate::turn::datalog::DatalogWriter::new(&working_dir),
            cmd_rx,
            event_tx,
        };

        let handle = AgentHandle { cmd_tx, event_rx };

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
        // Detect already-running dev servers on startup.
        self.detect_running_services().await;

        // Spawn background code graph indexer
        {
            let working_dir = self.turn_runner.context.working_dir.read().await.clone();
            let graph = self.turn_runner.context.graph.clone();
            let (reindex_tx, mut reindex_rx) = mpsc::unbounded_channel::<PathBuf>();
            let wd_for_indexer = working_dir.clone();
            tokio::spawn(async move {
                let mut indexer = crate::graph::indexer::GraphIndexer::new(
                    graph.clone(), wd_for_indexer.clone(),
                );
                indexer.index_all().await;
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

        while let Some(cmd) = self.cmd_rx.recv().await {
            match cmd {
                AgentCommand::SendMessage(content) => {
                    self.handle_send_message(content).await;
                }
                AgentCommand::Cancel => {
                    self.cancel_token.cancel();
                    self.cancel_token = CancellationToken::new();
                    self.phase = AgentPhase::Idle;
                    // Cancel the current turn - remove partial messages from conversation
                    self.conversation.cancel_current_turn();
                    // Sync the cleaned messages to TUI
                    let messages = self.conversation.messages.clone();
                    let _ = self.event_tx.send(AgentEvent::TurnCancelled { messages });
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
                    let old_provider = self.config.default_provider.clone();
                    self.config = new_config;
                    let new_provider_name = self.config.default_provider.clone();

                    // If provider/model changed, clear conversation to avoid context pollution
                    if old_provider != new_provider_name {
                        self.conversation.messages.clear();
                        self.conversation.turn_tracker = crate::conversation::turn::TurnTracker::new();
                        self.session_files.clear();
                    }

                    if let Some(provider_config) = self.config.providers.get(&new_provider_name) {
                        match crate::provider::create_provider(provider_config) {
                            Ok(new_provider) => {
                                self.turn_runner.provider = std::sync::Arc::from(new_provider);
                                self.turn_runner.config = self.config.clone();
                            }
                            Err(e) => {
                                let _ = self.event_tx.send(AgentEvent::TextDelta(
                                    format!("**Warning: failed to reload provider: {}**\n\n", e)
                                ));
                            }
                        }
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
                }
                AgentCommand::SetMessages(messages) => {
                    // Set messages from a resumed session.
                    self.conversation.messages = messages;
                }
                AgentCommand::SetPlanMode(enabled) => {
                    self.plan_mode = enabled;
                }
                AgentCommand::Shutdown => break,
            }
        }
    }

    // -------------------------------------------------------------------------
    // Core agent logic
    // -------------------------------------------------------------------------

    async fn handle_send_message(&mut self, content: String) {
        self.current_task = content.clone();

        // Detect negative feedback — user is unhappy with previous turn's work.
        let lower = content.to_lowercase();
        let negative_keywords = [
            "改错", "不对", "错了", "还是不行", "没用", "不是这样", "搞错",
            "又错", "白做", "越改越差", "恢复", "回滚", "撤销", "不行",
            "wrong", "not right", "still broken", "doesn't work", "undo",
            "revert", "go back", "that's worse", "stop", "broken",
        ];
        self.discipline_state.is_negative_feedback = content.chars().count() < 80
            && negative_keywords.iter().any(|kw| lower.contains(kw));

        // Git checkpoint: snapshot working tree before agent starts editing.
        let wd = self.turn_runner.context.working_dir.try_read()
            .map(|g| g.clone())
            .unwrap_or_default();
        self.last_checkpoint = git_checkpoint::create_checkpoint(&wd);

        // Reset ctx_budget_hint to full window at start of each user message.
        // Without this, the first tool call in a new turn reads the stale budget
        // from the previous turn's last LLM call (when ctx was full), causing
        // 670-line files to skeleton when there's plenty of room.
        let ctx_window = self.config.providers
            .get(&self.config.default_provider)
            .map(|p| p.context_window)
            .unwrap_or(128000);
        self.turn_runner.context.ctx_budget_hint.store(
            ctx_window,
            std::sync::atomic::Ordering::Relaxed,
        );

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
        // New user message = new task. If there's old context from the
        // previous task (>12 messages), compress it unconditionally.
        // This prevents dirty-start degradation where 20K+ of stale
        // conversation dilutes the batch prompt for the new task.
        // Unlike maybe_compress_history (which checks the 50% threshold),
        // this fires at every task boundary regardless of token count.
        if self.conversation.messages.len() > 12 {
            let (content, n_msgs) = self.conversation.build_compression_content();
            if !content.is_empty() && n_msgs > 0 {
                // Mechanical compression — no LLM call needed at task boundary.
                // The compressed content from build_compression_content is already
                // one-line-per-round summaries, compact enough for cold zone.
                self.conversation.apply_compression(n_msgs, content);
            }
        }

        self.conversation.add_user_message(&clean);
        self.turn_tokens = 0;
        self.tool_call_count = 0;
        self.turn_count = 0;
        self.retry_count = 0;
        self.discipline_state.recent_calls.clear();
        // Save current turn's edits before clearing — used in next turn's system prompt
        self.prev_turn_edited_files = self.files_edited_this_turn.clone();
        self.files_read_this_turn.clear();
        self.files_edited_this_turn.clear();
        self.turn_runner.recently_edited_files.clear();
        self.discipline_state.consecutive_reads = 0;
        self.discipline_state.verify_injected = false;
        self.discipline_state.model_produced_text = false;
        self.discipline_state.silent_tool_rounds = 0;
        // Note: is_negative_feedback is set above, do not reset here.
        self.discipline_state.build_fail_count = 0;
        self.discipline_state.file_read_counts.clear();
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
            let ctx_window = self.config.providers.get(&self.config.default_provider)
                .map(|p| p.context_window).unwrap_or(128000);
            self.datalog.begin_turn(&content, &model_name, ctx_window);
        }

        // State-based decisions (replaces keyword-based task_classifier).
        // Two facts, not guesses:

        // 1. Has the model read any files this session? If not → read-only first turn.
        let has_file_context = !self.files_read_this_turn.is_empty()
            || !self.files_edited_this_turn.is_empty();
        self.diagnosis_read_only_turns = if has_file_context { 0 } else { 1 };
        self.planning_phase = !has_file_context;

        // Unified prepend — no task classification, no auto-build injection.
        // Build command detection deferred to Phase 5 (LLM-inferred project config).
        let _content = format!("Read the relevant code first, then plan and implement.\n\n{}", content);

        self.phase = AgentPhase::Thinking;
        let _ = self
            .event_tx
            .send(AgentEvent::PhaseChange(AgentPhase::Thinking));

        self.run_turn_loop().await;
    }

    // needs_planning replaced by task_classifier::TaskType::needs_planning()

    // auto_diagnose_errors → diagnose.rs
    // find_file_in_project → diagnose.rs

    /// Multi-turn execution loop using TurnRunner.
    /// Each iteration calls TurnRunner.run() for one LLM turn, then applies
    /// discipline (reminders, step limits) and decides whether to continue.
    async fn run_turn_loop(&mut self) {
        loop {
            // Turn budget check BEFORE incrementing, so the reported
            // turn_count equals the number of turns actually executed
            // (not including the "would-be" next turn we refuse to run).
            // The stop reason is propagated via TurnComplete.stop_reason;
            // the CLI [done] line surfaces it as `stopped=turn_limit`.
            if self.check_turn_limit() {
                self.finish_turn(TurnStopReason::TurnLimit);
                return;
            }
            self.turn_count += 1;

            // Decrement diagnosis read-only counter each turn.
            if self.diagnosis_read_only_turns > 0 {
                self.diagnosis_read_only_turns -= 1;
            }

            // Inject any pending user input appended during streaming.
            if let Some(input) = self.pending_input.take() {
                self.conversation.add_user_message(&format!("[Additional context from user]: {}", input));
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

            let system_prompt = self.build_system_prompt();
            let turn_reminder = self.build_turn_reminder();
            let cancel = self.cancel_token.clone();

            // Context compression: when > 70% budget, pause and compress
            // old turns via LLM call. Keeps last 5 turns full, compressed
            // history goes to cold zone (FIFO, max 3 entries).
            self.maybe_compress_history(&system_prompt).await;

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

            // Log LLM request to <working_dir>/datalog/llm/ — colocated with turn .md files.
            {
                let context_window = self.config
                    .providers
                    .get(&self.config.default_provider)
                    .map(|p| p.context_window)
                    .unwrap_or(128000);
                let (msgs, _) = conv.to_provider_messages_budgeted(&system_prompt, context_window);
                let tool_defs = self.turn_runner.tools.get_definitions();
                let wd = self.turn_runner.context.working_dir
                    .try_read().map(|g| g.clone()).unwrap_or_default();
                crate::turn::log::log_llm_request(
                    &wd,
                    &msgs,
                    &tool_defs,
                    self.turn_runner.provider.model_name(),
                    context_window,
                    self.tool_call_count,
                );
                // Dump request to datalog for inline debugging
                self.datalog.log_llm_dump(
                    &msgs,
                    tool_defs.len(),
                    self.turn_runner.provider.model_name(),
                    context_window,
                );
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
                let file_read_counts = &mut self.discipline_state.file_read_counts;
                let consecutive_reads = &mut self.discipline_state.consecutive_reads;
                let targeted_read_count = &mut self.discipline_state.targeted_read_count;
                let session_files = &mut self.session_files;
                let reindex_tx = &self.reindex_tx;

                // Tool filtering: diagnosis phase uses read-only tools.
                // All other turns have full tool access (including edit_file).
                // EXECUTE thinking is applied INSIDE edit_file (fresh file read,
                // ±5 lines context return, fuzzy match, delta validation) —
                // not by blocking tools at the agent loop level.
                let read_only_tools: &[&str] = &[
                    "read_file", "grep", "glob", "list_directory",
                    "web_search", "web_fetch",
                    "trace_callees", "trace_callers", "trace_chain",
                    "file_dependencies", "blast_radius",
                ];
                let use_read_only = self.plan_mode || self.diagnosis_read_only_turns > 0;
                let tool_filter: Option<&[&str]> = if use_read_only {
                    Some(read_only_tools)
                } else {
                    None // Full tool access — model can read, edit, bash, search_replace
                };
                let turn_fut = runner.run_with_filter(
                    &mut conv, &system_prompt, &turn_reminder, &turn_tx, cancel, tool_filter,
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
                                TurnEvent::ToolCallStarted { ref id, ref name, ref arguments } => {
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
                                                // Track per-file read count for re-read guard
                                                if name == "read_file" {
                                                    *file_read_counts.entry(short.clone()).or_insert(0) += 1;
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
                                TurnEvent::ToolCallResult { call_id, name, output, success, duration } => {
                                    // Track files for discipline
                                    if let Some(pos) = output.find("Edited ") {
                                        let rest = &output[pos + 7..];
                                        let fp_end = rest.find(|c: char| c == ' ' || c == '\n' || c == '(').unwrap_or(rest.len());
                                        let fp = rest[..fp_end].trim();
                                        if !fp.is_empty() {
                                            *active_file = Some(PathBuf::from(fp));
                                        }
                                        if let Some(end) = rest.find(|c: char| c == '\n' || c == '.') {
                                            let file = short_path(&rest[..end]);
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
                                        if let Some(end) = rest.find(|c: char| c == '\n' || c == ' ') {
                                            let file = short_path(&rest[..end]);
                                            if !files_edited_this_turn.contains(&file) {
                                                files_edited_this_turn.push(file);
                                            }
                                        }
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

                                    let _ = event_tx.send(AgentEvent::ContextStats {
                                        system_tokens, sent_tokens, dropped_tokens, working_set_tokens, total_messages,
                                    });
                                }
                                TurnEvent::ToolCallStreaming { name, hint } => {
                                    let _ = event_tx.send(AgentEvent::ToolCallStreaming { name, hint });
                                }
                                TurnEvent::Error(e) => {
                                    let _ = event_tx.send(AgentEvent::Error(e));
                                }
                            }
                        }

                        Some(req) = approval_req_rx.recv() => {
                            // Forward approval request to TUI
                            let _ = event_tx.send(AgentEvent::ApprovalNeeded {
                                tool_name: req.call.name.clone(),
                                reason: req.reason.clone(),
                                call: req.call.clone(),
                            });
                            *phase = AgentPhase::WaitingApproval;
                            let _ = event_tx.send(AgentEvent::PhaseChange(AgentPhase::WaitingApproval));
                            *last_approval_request = Some(req);
                        }

                        Some(cmd) = cmd_rx.recv() => {
                            match cmd {
                                AgentCommand::Cancel => {
                                    cancel_token.cancel();
                                    *cancel_token = CancellationToken::new();
                                }
                                AgentCommand::ApproveTool => {
                                    *phase = AgentPhase::Thinking;
                                    let _ = event_tx.send(AgentEvent::PhaseChange(AgentPhase::Thinking));
                                    let _ = approval_resp_tx.send(PermissionDecision::Allow);
                                }
                                AgentCommand::ApproveToolAlways => {
                                    if let Some(ref req) = last_approval_request {
                                        if let Ok(mut store) = permission_store.write() {
                                            store.grant_session(&req.call.name);
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
                                }
                                AgentCommand::AppendInput(text) => {
                                    if let Some(ref mut existing) = pending_input {
                                        existing.push('\n');
                                        existing.push_str(&text);
                                    } else {
                                        *pending_input = Some(text);
                                    }
                                }
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
                TurnResult::Responded { ref text, tokens, truncated } => {
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
                                let plan_files: Vec<&str> = self.subtask_driver.subtasks
                                    .iter().map(|s| s.file.as_str()).collect();
                                let mut missing_deps: Vec<String> = Vec::new();
                                let mut seen = std::collections::HashSet::new();

                                for plan_file in &plan_files {
                                    seen.insert(plan_file.to_string());
                                }

                                for plan_file in &plan_files {
                                    // Find this file in graph and get its dependencies
                                    for (path, _) in &graph.file_symbols {
                                        let basename = path.file_name()
                                            .map(|f| f.to_string_lossy().to_string())
                                            .unwrap_or_default();
                                        if basename == *plan_file {
                                            // Check files this file depends on (callees' files)
                                            let sym_ids = graph.symbols_in_file(path);
                                            if let Some(ids) = sym_ids {
                                                for &sid in ids.iter().take(20) {
                                                    if let Some(edges) = graph.callees(sid) {
                                                        for edge in edges {
                                                            if let Some(node) = graph.node(edge.to) {
                                                                let dep_name = node.file.file_name()
                                                                    .map(|f| f.to_string_lossy().to_string())
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

                    // Empty response from LLM (common with DeepSeek/SiliconFlow/GLM):
                    // Retry with a nudge — but ONLY if the response was fast (<60s).
                    // Slow empty responses (300s) mean the model spent all max_tokens
                    // on internal reasoning — retrying will produce the same result.
                    let is_empty = text.trim().is_empty() || (text.trim().len() < 5 && tokens < 10);
                    let turn_elapsed = self.turn_start.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                    // Slow empty: model spent all tokens on thinking. Don't retry
                    // with "Continue" (will just burn tokens again). Instead nudge
                    // model to summarize what it did and finish.
                    let is_slow_empty = is_empty && turn_elapsed > 60;
                    if is_slow_empty && self.retry_count < 1 {
                        self.retry_count += 1;
                        self.conversation.messages.push(
                            crate::conversation::message::Message::new(
                                crate::conversation::message::Role::Assistant,
                                "(completed)".to_string(),
                            )
                        );
                        self.conversation.add_user_message(
                            "Summarize what you changed and finish."
                        );
                        continue;
                    }
                    if is_empty && self.retry_count < 2 {
                        self.retry_count += 1;
                        // Ensure valid message alternation: empty LLM response didn't add
                        // an Assistant message, so add one before injecting User message.
                        // Without this: ToolResult → User (invalid) → LLM returns empty.
                        self.conversation.messages.push(
                            crate::conversation::message::Message::new(
                                crate::conversation::message::Role::Assistant,
                                "(continuing...)".to_string(),
                            )
                        );
                        // Empty response retry: one uniform nudge regardless of whether
                        // edits happened. Removed the edit-specific "Summarize what you
                        // changed: <files>" branch — it prompted weak models to re-narrate
                        // work already reflected in tool results.
                        self.conversation.add_user_message("Continue.");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                    // Plan completion guard: REMOVED.
                    // Was injecting "You are NOT done" based on subtask_driver's regex-extracted
                    // file list, which often didn't match actual edited files. This prevented
                    // the model from stopping even when the task was complete.
                    // Model decides when it's done. Same as CC.

                    // Truncation guard: if LLM was cut off by max_tokens (finish_reason="length"),
                    // automatically continue. No keyword heuristics needed — the API tells us.
                    if truncated && self.retry_count < 3 {
                        self.retry_count += 1;
                        self.conversation.add_user_message(
                            "Output limit hit. If the task is already complete, just output a \
                             short summary and stop (no tool calls). Otherwise resume where you left off."
                        );
                        continue;
                    }

                    // Colon guard: REMOVED. End-of-text punctuation check is unnecessary.
                    // If model stops mid-sentence, user can say "继续".

                    self.finish_turn(TurnStopReason::Natural);
                    return;
                }
                TurnResult::UsedTools { tool_count, tokens, text } => {
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

                    // Sub-agent extraction from UsedTools: model may output plan text
                    // alongside tool calls (e.g. "Plan: 1. IdeaCenter.vue 2. ProductCenter.vue"
                    // + read_file in the same turn). Only dispatch on the FIRST tool-use turn
                    // (planning phase). If model has already been editing files, don't
                    // re-dispatch — the text may just mention files it already changed.
                    if let Some(ref plan_text) = text {
                        if self.tool_call_count <= tool_count  // only first tool-use response
                            && !self.subtask_driver.active
                            && !plan_text.trim().is_empty()
                        {
                            self.subtask_driver.extract_from_plan(plan_text);
                            if self.subtask_driver.active && self.subtask_driver.subtasks.len() >= 2 {
                                self.plan_text = Some(plan_text.clone());
                                if let Some(sub_result) = self.try_sub_agent_dispatch(plan_text).await {
                                    let _ = self.event_tx.send(AgentEvent::TextDelta(sub_result.clone()));
                                    self.subtask_driver = subtask_driver::SubtaskDriver::new();

                                    if sub_result.contains("BUILD ERRORS") {
                                        // Build failed — inject error, continue turn loop
                                        self.conversation.add_user_message(&format!(
                                            "[Sub-agent merge build FAILED. Fix the errors below, then summarize.]\n{}",
                                            sub_result
                                        ));
                                    } else {
                                        // Sub-agent results streamed via TextDelta above;
                                        // no extra "Summarize" user-turn — it just triggers
                                        // another round of re-narration. Let the turn stop naturally.
                                        self.finish_turn(TurnStopReason::Natural);
                                        return;
                                    }
                                }
                                // Failed — fall through to serial execution
                            }
                        }
                    }

                    // Post-process: truncate large outputs + externalize to disk
                    self.post_process_tool_results(tool_count);

                    // ATLAS auto-verify: removed along with the verify module.
                    // Model runs build/lint itself when needed.
                    // See docs/archive/guardian-auto-compile.md if re-introducing.

                    // Apply discipline: inject status reminders (no STOP commands).
                    self.apply_post_turn_discipline();
                    // Safety cap at 200 tool calls — only for runaway cost protection.
                    if self.check_step_limit() {
                        self.finish_turn(TurnStopReason::StepLimit);
                        return;
                    }
                    // Continue to next turn
                    self.phase = AgentPhase::Thinking;
                    let _ = self.event_tx.send(AgentEvent::PhaseChange(AgentPhase::Thinking));
                    continue;
                }
                TurnResult::Failed(e) => {
                    // Retry logic for transient errors
                    let is_rate_limited = e.contains("429") || e.contains("rate") || e.contains("Too Many");
                    let is_auth_error = e.contains("401 ") || e.contains("403 ");
                    let is_messages_illegal = e.contains("illegal") || e.contains("messages");

                    if is_messages_illegal && self.retry_count == 0 {
                        self.retry_count += 1;
                        // Try compression first (preserve semantics), fall back to truncation.
                        let sys_prompt = self.build_system_prompt();
                        self.maybe_compress_history(&sys_prompt).await;
                        // If compression didn't help enough, truncate as last resort.
                        let len = self.conversation.messages.len();
                        if len > 10 {
                            self.conversation.messages.truncate(len - 4);
                        }
                        let _ = self.event_tx.send(AgentEvent::TextDelta(
                            "\n[Context overflow — compressed history and retrying...]\n".to_string()
                        ));
                        continue;
                    } else if is_rate_limited && self.retry_count < 5 {
                        self.retry_count += 1;
                        let wait = (self.retry_count as u64 * 3).min(30);
                        let _ = self.event_tx.send(AgentEvent::TextDelta(
                            format!("\n[Rate limited — retrying in {}s...]\n", wait)
                        ));
                        tokio::time::sleep(Duration::from_secs(wait)).await;
                        continue;
                    } else if is_auth_error {
                        self.datalog.log_error(&e);
                        let _ = self.event_tx.send(AgentEvent::Error(e));
                        self.finish_turn(TurnStopReason::Error);
                        return;
                    } else if self.retry_count < 3 {
                        self.retry_count += 1;
                        let wait = (self.retry_count as u64 * 3).min(15);
                        let _ = self.event_tx.send(AgentEvent::TextDelta(
                            format!("\n[API error — retrying in {}s ({}/3)...]\n", wait, self.retry_count)
                        ));
                        tokio::time::sleep(Duration::from_secs(wait)).await;
                        continue;
                    } else {
                        self.datalog.log_error(&e);
                        let _ = self.event_tx.send(AgentEvent::Error(e));
                        self.finish_turn(TurnStopReason::Error);
                        return;
                    }
                }
                TurnResult::Cancelled => {
                    // Check if turn was already cancelled by AgentCommand::Cancel
                    // (which removes the turn from tracker immediately)
                    if self.conversation.turn_tracker.active_turn().is_none() {
                        // Already handled by AgentCommand::Cancel - just return
                        return;
                    }
                    // Remove the current turn's messages before saving
                    self.conversation.cancel_current_turn();
                    // Send TurnCancelled event for TUI to sync
                    let messages = self.conversation.messages.clone();
                    let _ = self.event_tx.send(AgentEvent::TurnCancelled { messages });
                    self.finish_turn(TurnStopReason::Cancelled);
                    return;
                }
            }
        }
    }

    // forward_turn_event → tool_dispatch.rs
    // post_process_tool_results → tool_dispatch.rs

    /// Auto-summarize old turns when context exceeds 70% of budget.
    /// Makes a lightweight LLM call to compress old turn content into a
    /// short summary, so the model retains awareness of prior work without
    /// the full message cost.
    /// Compress old turns when context > 70% budget.
    /// Pauses the task, calls LLM to summarize, stores in cold zone.
    /// Falls back to mechanical compression if LLM fails.
    async fn maybe_compress_history(&mut self, system_prompt: &str) {
        let context_window = self.config
            .providers
            .get(&self.config.default_provider)
            .map(|p| p.context_window)
            .unwrap_or(128000);

        let sys_tokens = system_prompt.len() / 4 + 4;
        if !self.conversation.needs_compression(sys_tokens, context_window) {
            return;
        }

        let (content, n_turns) = self.conversation.build_compression_content();
        if content.is_empty() || n_turns == 0 { return; }

        // Try LLM compression
        let summarize_prompt = format!(
            "Summarize this conversation history in 3-5 concise sentences. \
             Keep: file names, what was changed, key decisions, errors encountered. \
             Drop: exact code content, tool arguments, line numbers.\n\n{}",
            content
        );

        let mut mini_conv = crate::conversation::Conversation::new();
        mini_conv.add_user_message(&summarize_prompt);
        let msgs = mini_conv.to_provider_messages(
            "You are a conversation summarizer. Output ONLY the summary."
        );

        let mut summary = String::new();
        if let Ok(mut stream) = self.turn_runner.provider.chat_stream(&msgs, None) {
            use futures::StreamExt;
            // 30s first token + 30s between tokens. OpenRouter can be slow.
            let first_timeout = std::time::Duration::from_secs(30);
            let stream_timeout = std::time::Duration::from_secs(30);
            let mut got_token = false;
            loop {
                let timeout = if got_token { stream_timeout } else { first_timeout };
                match tokio::time::timeout(timeout, stream.next()).await {
                    Ok(Some(Ok(crate::stream::StreamEvent::Delta(text)))) => {
                        got_token = true;
                        // Strip model thinking tags (compression doesn't go through TurnRunner)
                        let clean = text.replace("<think>", "").replace("</think>", "")
                            .replace("<|im_start|>", "").replace("<|im_end|>", "");
                        summary.push_str(&clean);
                    }
                    Ok(Some(Ok(crate::stream::StreamEvent::Done { .. }))) => break,
                    Ok(Some(Ok(_))) => continue,
                    _ => break,
                }
            }
        }

        // Fallback: if LLM failed, use mechanical compression
        if summary.trim().is_empty() {
            summary = content; // mechanical one-liners from build_compression_content
        }

        self.conversation.apply_compression(n_turns, summary);

        // Post-compression task state restoration:
        // After compression, the model loses track of what it was doing.
        // Inject a brief status message so it can resume without re-exploring.
        let mut state_parts: Vec<String> = Vec::new();
        if !self.current_task.is_empty() {
            let task_short: String = self.current_task.chars().take(200).collect();
            state_parts.push(format!("TASK: {}", task_short));
        }
        if !self.files_edited_this_turn.is_empty() {
            state_parts.push(format!("FILES EDITED: {}", self.files_edited_this_turn.join(", ")));
        }
        if !self.files_read_this_turn.is_empty() {
            let recent: Vec<&str> = self.files_read_this_turn.iter()
                .rev().take(5).map(|s| s.as_str()).collect();
            state_parts.push(format!("RECENTLY READ: {}", recent.join(", ")));
        }
        if !state_parts.is_empty() {
            self.conversation.add_user_message(&format!(
                "[Context was compressed. Here is your current state:]\n{}",
                state_parts.join("\n")
            ));
        }
    }

    #[allow(dead_code)]
    async fn maybe_summarize_old_turns(&mut self, system_prompt: &str) {
        let context_window = self.config
            .providers
            .get(&self.config.default_provider)
            .map(|p| p.context_window)
            .unwrap_or(128000);

        let sys_tokens = system_prompt.len() / 4 + 4;
        let n_turns = self.conversation.turns_needing_summary(sys_tokens, context_window);
        if n_turns == 0 { return; }

        // Build the content to summarize
        let content = self.conversation.build_summary_content(n_turns);
        if content.is_empty() { return; }

        // Make a lightweight LLM call for summarization
        let summarize_prompt = format!(
            "Summarize the following conversation turns in 2-4 concise sentences. \
             Focus on: what the user asked, what files were read/edited, what was the outcome. \
             Keep file names and key decisions. Be brief.\n\n{}",
            content
        );

        let mut mini_conv = crate::conversation::Conversation::new();
        mini_conv.add_user_message(&summarize_prompt);

        let msgs = mini_conv.to_provider_messages(
            "You are a conversation summarizer. Output only the summary, nothing else."
        );

        // Stream the summary (non-streaming would be simpler but we only have chat_stream)
        match self.turn_runner.provider.chat_stream(&msgs, None) {
            Ok(mut stream) => {
                let mut summary = String::new();
                use futures::StreamExt;
                let timeout = std::time::Duration::from_secs(30);
                loop {
                    match tokio::time::timeout(timeout, stream.next()).await {
                        Ok(Some(Ok(crate::stream::StreamEvent::Delta(text)))) => {
                            summary.push_str(&text);
                        }
                        Ok(Some(Ok(crate::stream::StreamEvent::Done { .. }))) => break,
                        Ok(Some(Ok(_))) => continue,
                        _ => break, // timeout, error, or stream ended
                    }
                }

                if !summary.is_empty() {
                    self.conversation.apply_summary(n_turns, summary);
                }
            }
            Err(_) => {} // Summarization failed — proceed without it
        }
    }

    fn finish_turn(&mut self, stop_reason: TurnStopReason) {
        self.conversation.turn_tracker.complete_current();

        // Flush datalog with final stats
        self.datalog.end_turn(self.turn_tokens, self.tool_call_count);

        let duration = self.turn_start.map(|t| t.elapsed()).unwrap_or_default();
        self.turn_start = None;
        self.phase = AgentPhase::Idle;
        let _ = self.event_tx.send(AgentEvent::TurnComplete {
            duration,
            total_tokens: self.turn_tokens,
            turn_count: self.turn_count,
            tool_call_count: self.tool_call_count,
            stop_reason,
        });
        let _ = self
            .event_tx
            .send(AgentEvent::PhaseChange(AgentPhase::Idle));
        self.conversation.save(&Conversation::history_path());
    }

    // store_tool_result → tool_dispatch.rs

    // detect_running_services → services.rs
    // extract_service_urls → services.rs
    // change_dir → services.rs

    /// Try to dispatch sub-agents for parallel multi-file editing.
    /// Returns Some(summary_text) if dispatch succeeded, None if it should
    /// fall back to serial subtask execution.
    async fn try_sub_agent_dispatch(&mut self, _plan_text: &str) -> Option<String> {
        // Sub-agent disabled: 8 次实测全败，等 Phase 4 用 fork 模式重建。
        // 当前 fallback 到 serial subtask execution（主 agent 串行编辑）。
        return None;

        #[allow(unreachable_code)]
        let wd = self.turn_runner.context.working_dir.try_read()
            .map(|g| g.clone())
            .ok()?;

        let subtasks = &self.subtask_driver.subtasks;
        if subtasks.len() < 2 {
            return None;
        }

        // Bug fix tasks should NOT use sub-agents — need serial diagnosis.
        // Only feature development (create/implement/add/beautify) benefits from parallel.
        let task_lower = self.current_task.to_lowercase();
        let is_bugfix = task_lower.contains("报错") || task_lower.contains("错误")
            || task_lower.contains("修复") || task_lower.contains("修一下")
            || task_lower.contains("不行") || task_lower.contains("fix")
            || task_lower.contains("error") || task_lower.contains("broken")
            || task_lower.contains("bug") || task_lower.contains("还是");
        if is_bugfix {
            return None;
        }

        let _ = self.event_tx.send(AgentEvent::TextDelta(
            format!("\n\n**Dispatching {} sub-agents in parallel...**\n", subtasks.len())
        ));

        // Read all target files. If any file can't be found, fall back to serial.
        let mut tasks = Vec::new();
        let mut all_file_contents: Vec<(String, String)> = Vec::new();

        for subtask in subtasks {
            // Try to find the file: first check direct path, then walk the tree.
            let file_path = {
                let direct = wd.join(&subtask.file);
                if direct.exists() {
                    direct
                } else {
                    // Walk directory tree to find the file by name
                    match find_file_recursive(&wd, &subtask.file) {
                        Some(p) => p,
                        None => {
                            let _ = self.event_tx.send(AgentEvent::TextDelta(
                                format!("  Cannot find {}. Falling back to serial mode.\n", subtask.file)
                            ));
                            return None;
                        }
                    }
                }
            };

            let content = match std::fs::read_to_string(&file_path) {
                Ok(c) => c,
                Err(_) => {
                    let _ = self.event_tx.send(AgentEvent::TextDelta(
                        format!("  Cannot read {}. Falling back to serial mode.\n", subtask.file)
                    ));
                    return None;
                }
            };

            all_file_contents.push((
                file_path.to_string_lossy().to_string(),
                content,
            ));
        }

        // Generate sibling skeletons: compact view of other files
        for i in 0..all_file_contents.len() {
            let (ref file_path, ref _content) = all_file_contents[i];
            let mut siblings = String::new();
            for (j, (ref sib_path, ref sib_content)) in all_file_contents.iter().enumerate() {
                if i == j { continue; }
                let short = std::path::Path::new(sib_path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| sib_path.clone());
                // Take first 30 lines as skeleton
                let skeleton: String = sib_content.lines().take(30)
                    .collect::<Vec<_>>().join("\n");
                siblings.push_str(&format!("### {}\n```\n{}\n```\n\n", short, skeleton));
            }

            // Extract the task instruction for this file from the plan
            let file_name = &subtasks[i].file;
            let task_instr = extract_file_instruction(_plan_text, file_name);

            tasks.push(sub_agent::SubAgentTask {
                file_path: file_path.clone(),
                file_content: all_file_contents[i].1.clone(),
                task_instruction: task_instr,
                contract: extract_contract(_plan_text),
                sibling_skeletons: siblings,
            });
        }

        // Dispatch
        let pool = sub_agent::SubAgentPool::new(tasks);
        let provider = self.turn_runner.provider.clone();
        let tools = self.tool_registry.clone();
        let config = self.config.clone();

        let results = pool.execute_all(provider, tools, &config, &wd, &self.event_tx).await;

        // Build summary
        let mut summary = String::from("\n**Sub-agent results:**\n");
        let mut all_success = true;
        for r in &results {
            let status = if r.success { "OK" } else { "FAILED" };
            let short = std::path::Path::new(&r.file_path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| r.file_path.clone());
            summary.push_str(&format!(
                "| {} | {} | {} turns | {} |\n",
                short, status, r.turns_used, r.summary,
            ));
            if !r.success {
                all_success = false;
                for err in &r.errors {
                    summary.push_str(&format!("  Error: {}\n", err));
                }
            }
            // Track edited files
            if r.success {
                let short_name = std::path::Path::new(&r.file_path)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                if !self.files_edited_this_turn.contains(&short_name) {
                    self.files_edited_this_turn.push(short_name);
                }
            }
        }

        if all_success {
            summary.push_str(&format!("\nAll {} sub-agents completed successfully.\n", results.len()));
        } else {
            let failed: Vec<_> = results.iter().filter(|r| !r.success).collect();
            summary.push_str(&format!("\n{}/{} sub-agents failed.\n", failed.len(), results.len()));
        }

        // Merge verification: compile/build to catch cross-file errors.
        // Search up to 2 levels deep for build markers (handles nested project dirs).
        let build_cmd_and_dir = find_build_command(&wd);

        if let Some((cmd, build_dir)) = build_cmd_and_dir {
            let output = tokio::process::Command::new("sh")
                .args(["-c", &cmd])
                .current_dir(&build_dir)
                .output()
                .await;
            if let Ok(out) = output {
                let stdout = String::from_utf8_lossy(&out.stdout);
                let stderr = String::from_utf8_lossy(&out.stderr);
                let combined = format!("{}{}", stdout, stderr);
                if !out.status.success() || combined.to_lowercase().contains("error") {
                    let err_lines: String = combined.lines().take(10).collect::<Vec<_>>().join("\n");
                    summary.push_str(&format!(
                        "\n⚠ BUILD ERRORS after sub-agent merge:\n{}\nFix these errors before proceeding.\n",
                        err_lines
                    ));
                } else {
                    summary.push_str("\n✓ Build verification passed.\n");
                }
            }
        }

        Some(summary)
    }
}

/// Recursively search for a file by name under the given directory.
/// Returns the first match. Skips hidden dirs, node_modules, target, etc.
fn find_file_recursive(dir: &std::path::Path, file_name: &str) -> Option<std::path::PathBuf> {
    let walker = ignore::WalkBuilder::new(dir)
        .hidden(true)       // skip hidden
        .git_ignore(true)   // respect .gitignore
        .max_depth(Some(10))
        .build();

    for entry in walker {
        if let Ok(e) = entry {
            if e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                if let Some(name) = e.path().file_name() {
                    if name.to_string_lossy() == file_name {
                        return Some(e.into_path());
                    }
                }
            }
        }
    }
    None
}

/// Extract the instruction for a specific file from the plan text.
/// Looks for lines mentioning the file name and returns them as context.
fn extract_file_instruction(plan_text: &str, file_name: &str) -> String {
    let mut relevant_lines = Vec::new();
    for line in plan_text.lines() {
        if line.contains(file_name) {
            relevant_lines.push(line.trim().to_string());
        }
    }
    if relevant_lines.is_empty() {
        format!("Edit {} according to the plan.", file_name)
    } else {
        relevant_lines.join("\n")
    }
}

/// Extract contract/interface information from the plan text.
/// Looks for "Contract", "Interface", "API" sections.
fn extract_contract(plan_text: &str) -> String {
    let mut in_contract = false;
    let mut contract_lines = Vec::new();
    for line in plan_text.lines() {
        let lower = line.to_lowercase();
        if lower.contains("contract") || lower.contains("interface") || lower.contains("api") {
            in_contract = true;
        }
        if in_contract {
            contract_lines.push(line.to_string());
            // Stop after a blank line following contract section
            if line.trim().is_empty() && contract_lines.len() > 1 {
                break;
            }
        }
    }
    if contract_lines.is_empty() {
        "No explicit contract defined. Follow the plan.".to_string()
    } else {
        contract_lines.join("\n")
    }
}

/// LEGACY: Hardcoded build marker detection. Used only by sub-agent merge verification.
/// Phase 5 replaces this with LLM-inferred project config (codename: "ProjectSense").
/// See docs/phase5-the-final-five.md
fn find_build_command(wd: &std::path::Path) -> Option<(String, std::path::PathBuf)> {
    let markers: &[(&str, &str)] = &[
        ("package.json", "npm run build 2>&1 | head -30"),
        ("Cargo.toml", "cargo check 2>&1 | tail -20"),
        ("pom.xml", "mvn compile -q 2>&1 | tail -20"),
        ("go.mod", "go build ./... 2>&1 | tail -20"),
    ];

    // Check wd itself first
    for &(marker, cmd) in markers {
        if wd.join(marker).exists() {
            return Some((cmd.to_string(), wd.to_path_buf()));
        }
    }

    // Check immediate subdirectories (depth 1)
    if let Ok(entries) = std::fs::read_dir(wd) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let sub = entry.path();
                // Skip hidden dirs, node_modules, target, etc.
                let name = sub.file_name().unwrap_or_default().to_string_lossy();
                if name.starts_with('.') || name == "node_modules" || name == "target" {
                    continue;
                }
                for &(marker, cmd) in markers {
                    if sub.join(marker).exists() {
                        return Some((cmd.to_string(), sub));
                    }
                }
            }
        }
    }

    None
}

fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.rsplitn(3, '/').collect();
    match parts.len() {
        0 | 1 => path.to_string(),
        2 => format!("{}/{}", parts[1], parts[0]),
        _ => format!(".../{}/{}", parts[1], parts[0]),
    }
}





