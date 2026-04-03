//! The AgentLoop — a standalone agent that processes user messages,
//! calls LLM providers, executes tools, and communicates with the UI
//! via channels. Decoupled from any TUI concerns.

pub mod git_checkpoint;
pub mod knowledge;
pub mod subtask_driver;
pub mod task_classifier;

mod diagnose;
mod discipline;
mod prompt;
mod services;
mod tool_dispatch;
mod verify;

use std::collections::HashSet;
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
    ToolResult,
};
use crate::tool::result_store::ToolResultStore;
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
    /// Switch to a different provider.
    SwitchProvider(String),
    /// Reload config (e.g. after OAuth login) and switch to the new default provider.
    ReloadConfig(crate::config::Config),
    /// Change working directory.
    ChangeDir(String),
    /// Append input during streaming — queued and injected before next LLM call.
    AppendInput(String),
    /// Clear conversation history.
    ClearConversation,
    /// Set messages from a resumed session.
    SetMessages(Vec<crate::conversation::message::Message>),
    /// Shutdown the agent.
    Shutdown,
}

/// Events sent FROM the agent loop TO the UI.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// LLM text delta (streaming).
    TextDelta(String),
    /// A tool call is about to execute (for display).
    ToolCallStarted {
        name: String,
        arguments: String,
    },
    /// A tool call completed with a result.
    ToolCallResult {
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
        /// LLM round-trips (Claude Code-compatible metric).
        turn_count: usize,
        /// Total individual tool calls.
        tool_call_count: usize,
    },
    /// An error occurred.
    Error(String),
    /// Working directory changed.
    WorkingDirChanged(PathBuf),
    /// Context budget stats for logging (not displayed, only written to datalog).
    ContextStats {
        system_tokens: usize,
        hot_tokens: usize,
        cold_tokens: usize,
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
    /// LLM round-trip count (Claude Code-compatible "turn" metric).
    /// Each iteration of run_turn_loop = 1 turn, regardless of how many
    /// tools were called in that iteration.
    turn_count: usize,
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

    // Cached project context (invalidated on working dir change)
    project_context_cache: Option<(PathBuf, String)>,
    /// Absolute paths of descriptor files included in the project context.
    /// Used to intercept redundant read_file calls.
    context_included_files: HashSet<PathBuf>,
    /// Files read this turn (for tracking read-but-not-edit waste)
    files_read_this_turn: Vec<String>,
    /// Files edited/written this turn
    files_edited_this_turn: Vec<String>,
    /// Consecutive read-type calls without an edit (for read budget enforcement)
    consecutive_reads: usize,
    /// Whether verify prompt was already injected this turn (fire at most once)
    verify_injected: bool,
    /// Whether the model produced any text output this turn (if so, skip auto-summary)
    model_produced_text: bool,
    /// Consecutive LLM rounds with tool calls but zero text output.
    /// Reset to 0 whenever the model produces text. Used to inject progress prompts.
    silent_tool_rounds: usize,
    /// True when the user's message is negative feedback on the previous turn's work.
    is_negative_feedback: bool,
    /// Last N tool call signatures for loop detection. (name, args_hash)
    recent_calls: Vec<(String, u64)>,
    /// The user's original task message for this turn (re-injected as reminders).
    current_task: String,
    /// Name of the tool currently being executed (for smart truncation).
    current_tool_name: String,
    /// Pre-read file contents injected as system context (not synthetic tool calls).
    preread_context: String,
    /// Consecutive build/compile failures without a successful build in between.
    build_fail_count: usize,
    /// Per-file read count this turn — detects reading the same file repeatedly.
    file_read_counts: std::collections::HashMap<String, usize>,
    /// Number of scouting commands (curl/lsof/ps/kill) this turn.
    scouting_count: usize,
    /// Set when curl/wget returns valid data (not error) — backend is confirmed working.
    api_confirmed_working: bool,
    /// Consecutive edit_file calls to the same file without any other tool in between.
    consecutive_edits_file: Option<String>,
    consecutive_edits_count: usize,
    /// Count of `sleep` commands this turn — detects sleep polling loops.
    sleep_count: usize,
    /// Consecutive verification-only bash commands (--version, list, status, which, ls).
    consecutive_verify_count: usize,
    /// Normalized bash commands executed this turn → count.
    /// Used to detect repeated execution of the same command.
    executed_cmds: std::collections::HashMap<String, usize>,
    /// Consecutive failures by command category (e.g., "curl", "mysql").
    /// Reset on success. Used to detect "same approach keeps failing" patterns.
    category_fail_streak: std::collections::HashMap<String, usize>,
    /// Last bash command string (set on ToolCallStarted, used on ToolCallResult).
    last_bash_cmd: String,
    /// Exception signature from previous auto_diagnose (e.g. "TransactionRequiredException").
    /// Used to detect when the same error recurs after a "fix" attempt.
    last_diagnosed_error: String,

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
    /// Remaining read-only turns for diagnosis tasks. When > 0, only read_file/grep/glob/
    /// list_directory/find_references/list_symbols/read_symbol are available.
    /// Decremented each turn. Forces the model to read code before curl/edit.
    diagnosis_read_only_turns: usize,
    /// Current task type — drives dynamic prompt selection and planning.
    /// ATLAS-style subtask driver: decomposes plan into per-file subtasks.
    subtask_driver: subtask_driver::SubtaskDriver,
    /// Original plan text from model's first response — used for plan adherence reminders.
    plan_text: Option<String>,

    /// Discovered service URLs extracted from tool outputs (e.g., "http://localhost:3002").
    /// Persisted across turns so the model knows which ports are active.
    /// Key: label (e.g., "frontend", "backend"), Value: URL.
    active_services: std::collections::HashMap<String, String>,

    // Tool result cache (content-addressed disk store)
    result_store: ToolResultStore,

    // Skill registry — provides descriptions for system prompt and powers use_skill tool
    skill_registry: std::sync::Arc<std::sync::RwLock<SkillRegistry>>,

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
        tool_context: ToolContext,
        conversation: Conversation,
    ) -> (Self, AgentHandle) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        // Load skills from disk and register the use_skill tool.
        let working_dir = tool_context.working_dir.try_read()
            .map(|g| g.clone())
            .unwrap_or_else(|_| std::path::PathBuf::from("."));
        let mut registry = SkillRegistry::new();
        registry.reload(&working_dir);
        let skill_registry = std::sync::Arc::new(std::sync::RwLock::new(registry));
        tool_registry.register(Box::new(UseSkillTool { registry: skill_registry.clone() }));

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

        let turn_runner = TurnRunner {
            provider,
            tools: shared_tools.clone(),
            context: tool_context.clone(),
            config: config.clone(),
            permission: interactive_permission,
            result_store: ToolResultStore::new(ToolResultStore::default_dir()),
            recently_edited_files: Vec::new(),
            post_edit_read_counts: std::collections::HashMap::new(),
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
            retry_count: 0,
            approval_req_rx,
            approval_resp_tx,
            last_approval_request: None,
            cancel_token: CancellationToken::new(),
            project_context_cache: None,
            context_included_files: HashSet::new(),
            files_read_this_turn: Vec::new(),
            files_edited_this_turn: Vec::new(),
            consecutive_reads: 0,
            verify_injected: false,
            model_produced_text: false,
            silent_tool_rounds: 0,
            is_negative_feedback: false,
            recent_calls: Vec::new(),
            current_task: String::new(),
            current_tool_name: String::new(),
            preread_context: String::new(),
            build_fail_count: 0,
            file_read_counts: std::collections::HashMap::new(),
            scouting_count: 0,
            api_confirmed_working: false,
            consecutive_edits_file: None,
            consecutive_edits_count: 0,
            sleep_count: 0,
            consecutive_verify_count: 0,
            executed_cmds: std::collections::HashMap::new(),
            category_fail_streak: std::collections::HashMap::new(),
            last_bash_cmd: String::new(),
            last_diagnosed_error: String::new(),
            last_checkpoint: None,
            active_file: None,
            pending_input: None,
            planning_phase: false,
            diagnosis_read_only_turns: 0,
            subtask_driver: subtask_driver::SubtaskDriver::new(),
            plan_text: None,
            session_files: std::collections::HashMap::new(),
            active_services: std::collections::HashMap::new(),
            result_store: ToolResultStore::new(ToolResultStore::default_dir()),
            skill_registry,
            cmd_rx,
            event_tx,
        };

        let handle = AgentHandle { cmd_tx, event_rx };

        (agent, handle)
    }

    /// Run the agent loop. This is the main entry point — call from a tokio task.
    /// The loop processes commands from the UI and emits events back.
    pub async fn run(mut self) {
        // Detect already-running dev servers on startup.
        self.detect_running_services().await;

        while let Some(cmd) = self.cmd_rx.recv().await {
            match cmd {
                AgentCommand::SendMessage(content) => {
                    self.handle_send_message(content).await;
                }
                AgentCommand::Cancel => {
                    self.cancel_token.cancel();
                    self.cancel_token = CancellationToken::new();
                    self.phase = AgentPhase::Idle;
                    let _ = self.event_tx.send(AgentEvent::PhaseChange(AgentPhase::Idle));
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
                AgentCommand::SwitchProvider(provider_name) => {
                    // Reload config from file first (in case new providers were added via /login or /provider)
                    let config_path = Config::default_path();
                    if let Ok(new_config) = Config::load(&config_path) {
                        self.config = new_config;
                    }
                    
                    // Try exact match first, then case-insensitive match
                    let provider_config = self.config.providers.get(&provider_name)
                        .or_else(|| {
                            // Try case-insensitive match
                            self.config.providers.iter()
                                .find(|(k, _)| k.to_lowercase() == provider_name.to_lowercase())
                                .map(|(_, v)| v)
                        });
                    
                    if let Some(provider_config) = provider_config {
                        self.config.default_provider = provider_name.clone();
                        match crate::provider::create_provider(provider_config) {
                            Ok(new_provider) => {
                                let model_name = new_provider.model_name().to_string();
                                self.turn_runner.provider = new_provider;
                                self.turn_runner.config = self.config.clone();
                                let _ = self.event_tx.send(AgentEvent::TextDelta(
                                    format!("**Switched to: {} / {}**\n\n", provider_name, model_name)
                                ));
                            }
                            Err(e) => {
                                let _ = self.event_tx.send(AgentEvent::TextDelta(
                                    format!("**Failed to create provider: {}**\n\n", e)
                                ));
                            }
                        }
                    } else {
                        let available: Vec<_> = self.config.providers.keys().collect();
                        let _ = self.event_tx.send(AgentEvent::TextDelta(
                            format!("**Provider '{}' not found. Available: {:?}**\n\n", provider_name, available)
                        ));
                    }
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
                                self.turn_runner.provider = new_provider;
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
                    self.change_dir(&path);
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
                }
                AgentCommand::SetMessages(messages) => {
                    // Set messages from a resumed session.
                    self.conversation.messages = messages;
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
        self.is_negative_feedback = content.chars().count() < 80
            && negative_keywords.iter().any(|kw| lower.contains(kw));

        // Git checkpoint: snapshot working tree before agent starts editing.
        let wd = self.turn_runner.context.working_dir.try_read()
            .map(|g| g.clone())
            .unwrap_or_default();
        self.last_checkpoint = git_checkpoint::create_checkpoint(&wd);

        self.preread_context = self.build_preread_context(&content).await;

        // Auto-diagnose: if user mentions error keywords, scan logs and attach findings.
        // This gives the model the real error from Turn 1, instead of spending 3-5 turns grepping.
        let enriched = self.auto_diagnose_errors(&content).await;
        // Extract and store exception signature for recurrence detection across turns.
        if let Some(pos) = enriched.find("<!-- diag_exception:") {
            let rest = &enriched[pos + 20..];
            if let Some(end) = rest.find(" -->") {
                self.last_diagnosed_error = rest[..end].to_string();
            }
        }
        // Strip the hidden marker before adding to conversation
        let clean = if let Some(pos) = enriched.find("\n<!-- diag_exception:") {
            enriched[..pos].to_string()
        } else {
            enriched
        };
        self.conversation.add_user_message(&clean);
        self.turn_tokens = 0;
        self.tool_call_count = 0;
        self.turn_count = 0;
        self.retry_count = 0;
        self.recent_calls.clear();
        self.files_read_this_turn.clear();
        self.files_edited_this_turn.clear();
        self.turn_runner.recently_edited_files.clear();
        self.turn_runner.post_edit_read_counts.clear();
        self.consecutive_reads = 0;
        self.verify_injected = false;
        self.model_produced_text = false;
        self.silent_tool_rounds = 0;
        // Note: is_negative_feedback is set above, do not reset here.
        self.build_fail_count = 0;
        self.file_read_counts.clear();
        self.scouting_count = 0;
        self.api_confirmed_working = false;
        self.consecutive_edits_file = None;
        self.consecutive_edits_count = 0;
        self.sleep_count = 0;
        self.consecutive_verify_count = 0;
        self.executed_cmds.clear();
        self.category_fail_streak.clear();
        // Clear session_files on each new user message.
        // Working Set only tracks files from the CURRENT task.
        // Previous files are remembered via cold zone summaries.
        self.session_files.clear();
        self.turn_start = Some(Instant::now());
        self.cancel_token = CancellationToken::new();

        // "记住这个" / "remember this" — save last assistant response as knowledge.
        let lower_content = content.to_lowercase();
        let is_remember = lower_content.contains("记住")
            || lower_content.contains("remember")
            || lower_content.contains("记录一下")
            || lower_content.contains("记下来");
        if is_remember {
            let wd = self.turn_runner.context.working_dir.try_read()
                .map(|g| g.clone()).unwrap_or_default();
            // Find last assistant text
            let last_assistant = self.conversation.messages.iter().rev()
                .find(|m| matches!(m.role, crate::conversation::message::Role::Assistant))
                .and_then(|m| m.text())
                .unwrap_or("")
                .to_string();
            if !last_assistant.is_empty() {
                // Use first 200 chars as value, timestamp as category
                let summary = if last_assistant.chars().count() > 200 {
                    format!("{}...", last_assistant.chars().take(197).collect::<String>())
                } else {
                    last_assistant.clone()
                };
                let category = format!("user_note_{}", chrono::Local::now().format("%Y%m%d_%H%M"));
                knowledge::save_user_knowledge(&wd, &category, &summary);
            }
        }

        // Classify task to decide planning and read-only constraint.
        let has_previous = !self.conversation.messages.is_empty();
        let task_type = task_classifier::classify(&content, has_previous);
        self.planning_phase = task_type.needs_planning();

        // Diagnosis/follow-up tasks: restrict to read-only tools for first 3 turns.
        // Forces the model to read code before curl/edit — prevents the "blind curl" pattern.
        self.diagnosis_read_only_turns = match task_type {
            task_classifier::TaskType::BugFix => 3,
            task_classifier::TaskType::FollowUp => 2,
            _ => 0,
        };

        // Prepend "analyze first" to user message for complex tasks.
        // Data shows this changes model behavior from "pattern match → quick fix (often wrong)"
        // to "systematic diagnosis → correct fix". Placed in user message (not system prompt)
        // because models comply with user instructions more reliably.
        let content = match task_type {
            task_classifier::TaskType::BugFix
            | task_classifier::TaskType::FollowUp => {
                format!("Analyze the root cause before making changes.\n\n{}", content)
            }
            task_classifier::TaskType::FeatureDev => {
                format!("Read the relevant code first, then plan and implement.\n\n{}", content)
            }
            _ => content,
        };

        self.phase = AgentPhase::Thinking;
        let _ = self
            .event_tx
            .send(AgentEvent::PhaseChange(AgentPhase::Thinking));

        self.run_turn_loop().await;
    }

    // needs_planning replaced by task_classifier::TaskType::needs_planning()

    // build_preread_context → prompt.rs

    // auto_diagnose_errors → diagnose.rs
    // find_file_in_project → diagnose.rs

    /// Multi-turn execution loop using TurnRunner.
    /// Each iteration calls TurnRunner.run() for one LLM turn, then applies
    /// discipline (reminders, step limits) and decides whether to continue.
    async fn run_turn_loop(&mut self) {
        loop {
            self.turn_count += 1;

            // Decrement diagnosis read-only counter each turn.
            if self.diagnosis_read_only_turns > 0 {
                self.diagnosis_read_only_turns -= 1;
            }

            // Inject any pending user input appended during streaming.
            if let Some(input) = self.pending_input.take() {
                self.conversation.add_user_message(&format!("[Additional context from user]: {}", input));
            }

            // Planning phase: inject instruction to plan before acting
            if self.planning_phase {
                self.conversation.add_user_message(
                    "[System: This is a complex task. Before using any tools, first output a brief plan \
                     (3-5 steps) of what you'll do. Then proceed to execute it.]"
                );
                self.planning_phase = false; // Only inject once
            }

            // NOTE: Negative feedback injection disabled — adds a System message that
            // confuses weak models and wastes context. The model sees the user's complaint
            // directly; no extra injection needed.

            // Fix 6: Bug-fix diagnostic guidance — inject on first turn when user
            // message contains bug-related keywords.
            if self.tool_call_count == 0 {
                let last_user = self.conversation.messages.iter().rev()
                    .find(|m| matches!(m.role, crate::conversation::message::Role::User))
                    .and_then(|m| m.text())
                    .unwrap_or("")
                    .to_string();
                let lower = last_user.to_lowercase();
                let has_bug_keyword = ["bug", "fix", "broken", "error", "错误", "报错", "不行",
                    "失败", "crash", "wrong", "issue", "problem", "doesn't work", "not working"]
                    .iter().any(|k| lower.contains(k));
                if has_bug_keyword {
                    let has_frontend_keyword = ["页面", "前端", "样式", "css", "html", "vue",
                        "react", "component", "button", "render", "display", "layout", "ui"]
                        .iter().any(|k| lower.contains(k));
                    let strategy = if has_frontend_keyword {
                        "[DIAGNOSTIC STRATEGY: This looks like a frontend bug. \
                         1) Read the relevant component file. \
                         2) Check for CSS/template issues. \
                         3) Make the fix. \
                         Do NOT start a dev server or run build commands until you've read the code.]"
                    } else {
                        "[DIAGNOSTIC STRATEGY: This looks like a bug fix task. \
                         1) Read the most likely source file (check error messages/stack traces for clues). \
                         2) Identify the root cause. \
                         3) Make the fix. \
                         Stay focused — most bugs need only 3-4 steps to fix.]"
                    };
                    self.conversation.messages.push(
                        crate::conversation::message::Message::new(
                            crate::conversation::message::Role::System,
                            strategy,
                        )
                    );
                }
            }

            // Fix 5: Step budget warning — nudge the model to stop reading and start editing.
            if self.tool_call_count >= 6 && self.files_edited_this_turn.is_empty() {
                let warning = format!(
                    "[STEP BUDGET WARNING: You have made {} tool calls with ZERO edits. \
                     You are off track. Most bug fixes need 3-4 steps. \
                     STOP reading more files. Based on what you already know, make your edit NOW. \
                     Files you've read: {}]",
                    self.tool_call_count,
                    self.files_read_this_turn.join(", "),
                );
                self.conversation.messages.push(
                    crate::conversation::message::Message::new(
                        crate::conversation::message::Role::System,
                        warning,
                    )
                );
            }

            let system_prompt = self.build_system_prompt();
            let cancel = self.cancel_token.clone();

            // Move conversation out to avoid borrow conflicts with self in select!
            let mut conv = std::mem::take(&mut self.conversation);

            // Log LLM request to ~/.atomcode/logs/ (caller responsibility, not TurnRunner's)
            {
                let context_window = self.config
                    .providers
                    .get(&self.config.default_provider)
                    .map(|p| p.context_window)
                    .unwrap_or(16000);
                let (mut msgs, _) = conv.to_provider_messages_budgeted(&system_prompt, context_window);
                // Inflate ToolResultRef → ToolResult so logs contain actual content
                for msg in msgs.iter_mut().rev().take(20) {
                    if let crate::conversation::message::MessageContent::ToolResultRef(ref r) = msg.content {
                        let full = self.turn_runner.result_store.inflate(r);
                        msg.content = crate::conversation::message::MessageContent::ToolResult(full);
                    }
                }
                let tool_defs = self.turn_runner.tools.get_definitions();
                crate::turn::log::log_llm_request(
                    &msgs,
                    &tool_defs,
                    self.turn_runner.provider.model_name(),
                    context_window,
                    self.tool_call_count,
                );
            }

            // Run the turn in a scoped block so all borrows of self.turn_runner
            // end before we use self.conversation again.
            let (result, mut turn_rx) = {
                let (turn_tx, mut turn_rx) = mpsc::unbounded_channel::<TurnEvent>();

                // Destructure self to get split borrows — the borrow checker needs to see
                // that turn_runner and the other fields are disjoint borrows.
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
                let model_produced_text = &mut self.model_produced_text;
                let current_tool_name = &mut self.current_tool_name;
                let files_edited_this_turn = &mut self.files_edited_this_turn;
                let active_file = &mut self.active_file;
                let files_read_this_turn = &mut self.files_read_this_turn;
                let file_read_counts = &mut self.file_read_counts;
                let consecutive_reads = &mut self.consecutive_reads;
                let session_files = &mut self.session_files;

                // Run TurnRunner concurrently with command processing.
                // Diagnosis tasks: restrict to read-only tools for N turns.
                // This forces the model to read code before curl/edit.
                let read_only_tools: &[&str] = &[
                    "read_file", "grep", "glob", "list_directory",
                    "find_references", "list_symbols", "read_symbol",
                ];
                let use_filter = self.diagnosis_read_only_turns > 0
                    || (self.planning_phase && self.tool_call_count == 0);
                let tool_filter: Option<&[&str]> = if use_filter {
                    Some(read_only_tools)
                } else {
                    None
                };
                let turn_fut = runner.run_with_filter(
                    &mut conv, &system_prompt, &turn_tx, cancel, tool_filter,
                );
                tokio::pin!(turn_fut);

                let result = loop {
                    tokio::select! {
                        biased;

                        result = &mut turn_fut => break result,

                        Some(event) = turn_rx.recv() => {
                            // Inline forward_turn_event to avoid borrowing self
                            match event {
                                TurnEvent::TextDelta(text) => {
                                    *model_produced_text = true;
                                    let _ = event_tx.send(AgentEvent::TextDelta(text));
                                }
                                TurnEvent::ToolCallStarted { ref name, ref arguments } => {
                                    *current_tool_name = name.clone();
                                    *phase = AgentPhase::CallingTool(name.clone());
                                    let _ = event_tx.send(AgentEvent::PhaseChange(phase.clone()));

                                    // Track files for Working Set + read counts
                                    if matches!(name.as_str(), "read_file" | "edit_file" | "write_file" | "search_replace" | "glob" | "grep") {
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
                                                }
                                            }
                                        }
                                    }

                                    let _ = event_tx.send(AgentEvent::ToolCallStarted { name: name.clone(), arguments: arguments.clone() });
                                }
                                TurnEvent::ToolCallResult { name, output, success, duration } => {
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
                                    if let Some(pos) = output.find("Wrote ") {
                                        let rest = &output[pos + 6..];
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
                                    } else if matches!(name.as_str(), "edit_file" | "write_file") {
                                        *consecutive_reads = 0;
                                    }
                                    let _ = event_tx.send(AgentEvent::ToolCallResult {
                                        name, output, success, duration,
                                    });
                                }
                                TurnEvent::TokenUsage { prompt_tokens, completion_tokens, total_tokens: _ } => {
                                    let _ = event_tx.send(AgentEvent::TokenUsage(
                                        crate::stream::TokenUsage {
                                            prompt_tokens,
                                            completion_tokens,
                                        }
                                    ));
                                }
                                TurnEvent::ContextStats { system_tokens, hot_tokens, cold_tokens, working_set_tokens, total_messages } => {
                                    let _ = event_tx.send(AgentEvent::ContextStats {
                                        system_tokens, hot_tokens, cold_tokens, working_set_tokens, total_messages,
                                    });
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

                // turn_tx drops here (owned by this block), turn_fut also drops
                (result, turn_rx)
            };
            // All borrows of self.turn_runner are now released.

            // Restore conversation
            self.conversation = conv;

            // Drain remaining events
            while let Ok(event) = turn_rx.try_recv() {
                self.forward_turn_event(event);
            }

            // Handle result
            match result {
                TurnResult::Responded { ref text, tokens } => {
                    self.turn_tokens += tokens;
                    self.total_tokens += tokens;

                    // ATLAS subtask extraction: if model just output a plan (FeatureDev,
                    // first response with text, no tools used yet), extract subtasks
                    // and drive execution file-by-file.
                    if self.tool_call_count == 0
                        && !text.trim().is_empty()
                        && !self.subtask_driver.active
                    {
                        self.subtask_driver.extract_from_plan(text);
                        // Store plan text for adherence reminders
                        self.plan_text = Some(text.clone());
                        if self.subtask_driver.active {
                            // Inject first subtask instruction
                            if let Some(instr) = self.subtask_driver.current_instruction() {
                                self.conversation.add_user_message(&instr);
                            }
                            continue; // Don't finish — drive subtask execution
                        }
                    }

                    // Empty response from LLM (common with DeepSeek/SiliconFlow/GLM):
                    // Retry with a nudge — model may have hit a transient issue.
                    let is_empty = text.trim().is_empty() && tokens == 0;
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
                        if !self.files_edited_this_turn.is_empty() {
                            let files = self.files_edited_this_turn.join(", ");
                            self.conversation.add_user_message(&format!(
                                "Summarize what you changed: {}", files,
                            ));
                        } else {
                            self.conversation.add_user_message("Continue.");
                        }
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                    // Plan completion guard: if model tries to stop but planned tasks remain,
                    // force it to continue. Prevents premature termination (T31 scenario).
                    if self.plan_text.is_some()
                        && self.subtask_driver.subtasks.iter().any(|t| !t.done)
                        && self.retry_count < 2
                    {
                        let remaining: Vec<&str> = self.subtask_driver.subtasks.iter()
                            .filter(|t| !t.done)
                            .map(|t| t.file.as_str())
                            .collect();
                        if !remaining.is_empty() {
                            self.retry_count += 1;
                            self.conversation.add_user_message(&format!(
                                "You are NOT done. These files from your plan still need editing: {}. Continue.",
                                remaining.join(", ")
                            ));
                            continue;
                        }
                    }

                    self.finish_turn();
                    return;
                }
                TurnResult::UsedTools { tool_count, tokens, text } => {
                    self.turn_tokens += tokens;
                    self.total_tokens += tokens;
                    self.tool_call_count += tool_count;
                    // Track silent rounds: model used tools without explaining anything.
                    let had_text = text.as_ref().map(|t| !t.trim().is_empty()).unwrap_or(false);
                    if had_text {
                        self.silent_tool_rounds = 0;
                    } else {
                        self.silent_tool_rounds += 1;
                    }
                    // Post-process: truncate large outputs + externalize to disk
                    self.post_process_tool_results(tool_count);

                    // ATLAS-style auto-verify: if files were edited, auto-compile
                    // and inject result. Catches errors immediately instead of
                    // letting model pile up 10 broken edits before compiling.
                    if !self.files_edited_this_turn.is_empty() {
                        let log_sizes = self.snapshot_devserver_log_sizes();
                        self.auto_compile_verify().await;
                        self.syntax_check_edited_files().await;
                        self.check_devserver_logs(&log_sizes).await;
                        self.check_vue_partial_edit().await;
                    }

                    // Apply discipline: inject reminders, check step limits
                    self.apply_post_turn_discipline();
                    if self.check_step_limit() {
                        self.finish_turn();
                        return;
                    }
                    // Read-loop breaker: consecutive reads without ANY edit in between.
                    // Doesn't matter if files were edited earlier in the turn — what matters
                    // is the model is currently stuck in a read loop.
                    if self.consecutive_reads >= 5 {
                        self.conversation.add_user_message(
                            "You have done 5+ consecutive reads without editing. \
                             You are likely stuck. Use edit_file NOW to fix the issue, \
                             or tell the user what's wrong."
                        );
                    }
                    if self.consecutive_reads >= 8 {
                        // Hard stop — 8 consecutive reads is a death spiral
                        self.conversation.add_user_message(
                            "STOPPED: 8 consecutive reads without editing. \
                             Summarize what you found and what the user should do."
                        );
                        self.finish_turn();
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
                        let len = self.conversation.messages.len();
                        if len > 4 {
                            self.conversation.messages.truncate(len - 4);
                        }
                        let _ = self.event_tx.send(AgentEvent::TextDelta(
                            "\n[Recovering from API error — retrying with reduced context...]\n".to_string()
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
                        let _ = self.event_tx.send(AgentEvent::Error(e));
                        self.finish_turn();
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
                        let _ = self.event_tx.send(AgentEvent::Error(e));
                        self.finish_turn();
                        return;
                    }
                }
                TurnResult::Cancelled => {
                    self.finish_turn();
                    return;
                }
            }
        }
    }

    // forward_turn_event → tool_dispatch.rs
    // post_process_tool_results → tool_dispatch.rs

    fn finish_turn(&mut self) {
        // Mark the current turn as completed in the tracker.
        self.conversation.turn_tracker.complete_current();

        let duration = self.turn_start.map(|t| t.elapsed()).unwrap_or_default();
        self.turn_start = None;
        self.phase = AgentPhase::Idle;
        let _ = self.event_tx.send(AgentEvent::TurnComplete {
            duration,
            total_tokens: self.turn_tokens,
            turn_count: self.turn_count,
            tool_call_count: self.tool_call_count,
        });
        let _ = self
            .event_tx
            .send(AgentEvent::PhaseChange(AgentPhase::Idle));
        // Persist conversation history.
        self.conversation.save(&Conversation::history_path());
    }

    // resolve_args → tool_dispatch.rs
    // intercept_redundant_call → tool_dispatch.rs
    // store_tool_result → tool_dispatch.rs
    // normalize_bash_cmd → tool_dispatch.rs (standalone fn)

    // detect_running_services → services.rs
    // extract_service_urls → services.rs
    // change_dir → services.rs
}

fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.rsplitn(3, '/').collect();
    match parts.len() {
        0 | 1 => path.to_string(),
        2 => format!("{}/{}", parts[1], parts[0]),
        _ => format!(".../{}/{}", parts[1], parts[0]),
    }
}

/// Extract recently edited file names from the most recent datalog file.
/// Returns a comma-separated list of unique file names (max 5).
/// Only extracts from "Edit File" and "Write File" lines — safe, factual, small.
fn extract_recent_activity_from_datalog(working_dir: &std::path::Path) -> String {
    let log_dir = working_dir.join("datalog");
    if !log_dir.is_dir() {
        return String::new();
    }

    // Find the most recent .md file in datalog/
    let mut files: Vec<_> = match std::fs::read_dir(&log_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "md").unwrap_or(false))
            .collect(),
        Err(_) => return String::new(),
    };
    files.sort_by_key(|e| std::cmp::Reverse(e.file_name()));
    let latest = match files.first() {
        Some(f) => f.path(),
        None => return String::new(),
    };

    // Read and extract file names from "Edit File" / "Write File" lines
    let content = match std::fs::read_to_string(&latest) {
        Ok(c) => c,
        Err(_) => return String::new(),
    };

    let mut edited_files = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for line in content.lines() {
        let trimmed = line.trim();
        // Match: "- Edit File .../SomeFile.ext" or "- Write File .../SomeFile.ext"
        if (trimmed.starts_with("- Edit File") || trimmed.starts_with("- Write File"))
            && trimmed.contains("...")
        {
            // Extract the short file path after "..."
            if let Some(pos) = trimmed.rfind('/') {
                let file_name = &trimmed[pos + 1..];
                // Clean up: remove trailing content like " (-3 +5 lines)"
                let clean = file_name.split(|c: char| c == ' ' || c == '(')
                    .next()
                    .unwrap_or(file_name)
                    .trim();
                if !clean.is_empty() && seen.insert(clean.to_string()) {
                    edited_files.push(clean.to_string());
                    if edited_files.len() >= 5 { break; }
                }
            }
        }
    }

    edited_files.join(", ")
}




