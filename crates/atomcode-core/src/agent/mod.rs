//! The AgentLoop — a standalone agent that processes user messages,
//! calls LLM providers, executes tools, and communicates with the UI
//! via channels. Decoupled from any TUI concerns.

pub mod git_checkpoint;
pub mod knowledge;
pub mod subtask_driver;
pub mod task_classifier;

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

    /// DO NOT pre-read source files. Claude Code doesn't do this either.
    /// Pre-reading stuffs the system prompt with 50K+ tokens of irrelevant code,
    /// diluting model attention and making it WORSE at following rules.
    ///
    /// Instead: give the model a good file tree (project_context) and let it
    /// read_file what it needs. Each read is 1 step — trivial cost.
    /// A compact system prompt → model follows rules → fewer mistakes → fewer steps.
    async fn build_preread_context(&self, _content: &str) -> String {
        String::new()
    }

    /// Auto-diagnose: when user mentions error keywords, scan log files for recent errors
    /// and append them to the user message. The model starts Turn 1 with the real error.
    async fn auto_diagnose_errors(&self, content: &str) -> String {
        let lower = content.to_lowercase();
        let has_error_keyword = ["错误", "报错", "失败", "error", "500", "404", "crash",
            "异常", "exception", "内部错误", "not work", "不行", "不好使", "bug"]
            .iter().any(|k| lower.contains(k));

        if !has_error_keyword {
            return content.to_string();
        }

        let wd: PathBuf = self.turn_runner.context.working_dir.try_read()
            .map(|g| g.clone())
            .unwrap_or_default();

        // Find log files: *.log in project root and common subdirs
        let log_candidates = ["backend.log", "server.log", "app.log", "nohup.out",
            "backend/backend.log", "backend/nohup.out",
            "logs/app.log", "log/development.log"];

        let mut diagnostics = Vec::new();

        for log_name in &log_candidates {
            let log_path = wd.join(log_name);
            if !log_path.exists() { continue; }

            // Check if log is stale (mtime > 5 min ago).
            // Stale logs contain only old startup output, not the runtime error
            // the user is reporting. Still scan but tag as stale.
            let is_stale = std::fs::metadata(&log_path).ok()
                .and_then(|m| m.modified().ok())
                .map(|mtime| mtime.elapsed().unwrap_or_default().as_secs() > 300)
                .unwrap_or(false);

            if let Ok(output) = tokio::process::Command::new("grep")
                .args(&["-i", "-E", "error|exception|fail|caused by",
                    &log_path.to_string_lossy()])
                .output()
                .await
            {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if !stdout.trim().is_empty() {
                    let lines: Vec<&str> = stdout.lines().collect();
                    let start = lines.len().saturating_sub(15);
                    let recent = lines[start..].join("\n");
                    if is_stale {
                        diagnostics.push(format!(
                            "[Auto-detected from {} (STALE — last modified >5min ago, errors may be old):]\n{}",
                            log_name, recent
                        ));
                    } else {
                        diagnostics.push(format!("[Auto-detected from {}:]\n{}", log_name, recent));
                    }
                }
            }
        }

        // Fallback: if all logs are stale or empty, try to capture live output
        // from running Java/Node processes via their recent stderr.
        let all_stale_or_empty = diagnostics.is_empty()
            || diagnostics.iter().all(|d| d.contains("STALE"));
        if all_stale_or_empty {
            // Try Spring Boot default log location
            let spring_log = wd.join("backend/logs/spring.log");
            if spring_log.exists() {
                if let Ok(output) = tokio::process::Command::new("tail")
                    .args(&["-50", &spring_log.to_string_lossy()])
                    .output().await
                {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let error_lines: Vec<&str> = stdout.lines()
                        .filter(|l| {
                            let low = l.to_lowercase();
                            low.contains("error") || low.contains("exception") || low.contains("caused by")
                        })
                        .collect();
                    if !error_lines.is_empty() {
                        let start = error_lines.len().saturating_sub(15);
                        diagnostics.push(format!(
                            "[Auto-detected from logs/spring.log:]\n{}",
                            error_lines[start..].join("\n")
                        ));
                    }
                }
            }
        }

        if diagnostics.is_empty() {
            return content.to_string();
        }

        // Phase 2: Parse stack traces for file:line references, extract function code via tree-sitter.
        // This gives the model the actual broken code so it can edit directly in Turn 1.
        let diag_text = diagnostics.join("\n");
        let mut extracted_code = Vec::new();
        let mut searcher = self.turn_runner.context.semantic.lock().await;

        // Match patterns like "FileName.java:45" or "file.py:123" or "file.rs:45"
        let file_line_re = regex::Regex::new(r"(\w+\.\w+):(\d+)").unwrap_or_else(|_| regex::Regex::new(".^").unwrap());
        let mut seen_files = std::collections::HashSet::new();

        for cap in file_line_re.captures_iter(&diag_text) {
            let filename = &cap[1];
            let line_no: usize = cap[2].parse().unwrap_or(0);
            if line_no == 0 || seen_files.contains(filename) { continue; }

            // Find the actual file path in the project
            let file_path = Self::find_file_in_project(&wd, filename);
            if let Some(ref fp) = file_path {
                seen_files.insert(filename.to_string());
                // Use tree-sitter to find the enclosing function at this line
                if let Some(symbols) = searcher.list_symbols(fp) {
                    if let Some(sym) = symbols.iter().find(|s| line_no >= s.start_line && line_no <= s.end_line) {
                        // Extract the function code
                        if let Some(slice) = searcher.extract_symbol(fp, &sym.name) {
                            let mut code = format!(
                                "[Source: {} → {}() lines {}-{}]\n",
                                filename, sym.name, slice.start_line, slice.end_line
                            );
                            for (i, line) in slice.text.lines().enumerate() {
                                code.push_str(&format!("{:4}| {}\n", slice.start_line + i, line));
                            }
                            extracted_code.push(code);
                            if extracted_code.len() >= 2 { break; } // Max 2 functions
                        }
                    }
                }
            }
        }
        // If the stack trace mentions a specific object/call (e.g., "tagRepository.count"),
        // scan the entire file for ALL similar calls so the model can fix them all at once.
        // This prevents the "fix one call, miss nine others" pattern.
        {
            let obj_re = regex::Regex::new(r"(\w+Repository|\w+Service|\w+Dao)\.\w+")
                .unwrap_or_else(|_| regex::Regex::new(".^").unwrap());
            // First pass: collect object names to scan
            let mut objects_to_scan: Vec<String> = Vec::new();
            for code in &extracted_code {
                for cap in obj_re.captures_iter(code) {
                    let obj_name = cap[1].to_string();
                    if !objects_to_scan.contains(&obj_name) {
                        objects_to_scan.push(obj_name);
                    }
                }
            }
            // Second pass: scan and append results
            for obj_name in &objects_to_scan {
                for fp in &seen_files {
                    if let Some(file_path) = Self::find_file_in_project(&wd, fp) {
                        if let Some(call_list) = searcher.find_similar_calls(&file_path, &obj_name.to_lowercase()) {
                            extracted_code.push(format!(
                                "\n[All {} calls in this file — fix ALL at once:]\n{}",
                                obj_name, call_list
                            ));
                        }
                    }
                }
            }
        }

        drop(searcher);

        // Extract exception signature (e.g. "TransactionRequiredException") for recurrence detection.
        let exception_re = regex::Regex::new(r"(\w+Exception|\w+Error)")
            .unwrap_or_else(|_| regex::Regex::new(".^").unwrap());
        let current_exception = exception_re.captures_iter(&diag_text)
            .next()
            .map(|c| c[1].to_string())
            .unwrap_or_default();

        let mut result = format!("{}\n\n{}", content, diagnostics.join("\n\n"));

        // If the same exception recurs after a previous fix attempt, tell the model
        // its approach isn't working and it needs a different strategy.
        if !current_exception.is_empty() && current_exception == self.last_diagnosed_error {
            result.push_str(&format!(
                "\n\n[RECURRING ERROR: {} appeared again after your previous fix. \
                 Your last approach did not resolve it. Try a fundamentally different fix — \
                 e.g. add @Transactional at the method level instead of wrapping individual calls.]",
                current_exception
            ));
        }
        // Store for next comparison (caller updates self.last_diagnosed_error)
        // We embed it in the result with a hidden marker for the caller to extract.
        if !current_exception.is_empty() {
            result.push_str(&format!("\n<!-- diag_exception:{} -->", current_exception));
        }

        if !extracted_code.is_empty() {
            result.push_str("\n\n[Relevant source code from stack trace — you can edit directly:]\n");
            result.push_str(&extracted_code.join("\n"));
        }
        result
    }

    /// Find a file by name in the project directory (searches up to 4 levels deep).
    fn find_file_in_project(wd: &std::path::Path, filename: &str) -> Option<std::path::PathBuf> {
        use crate::tool::SKIP_DIRS;
        fn walk(dir: &std::path::Path, target: &str, depth: usize) -> Option<std::path::PathBuf> {
            if depth > 4 { return None; }
            let entries = std::fs::read_dir(dir).ok()?;
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str == target && entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    return Some(entry.path());
                }
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                    && !SKIP_DIRS.contains(&name_str.as_ref())
                {
                    if let Some(found) = walk(&entry.path(), target, depth + 1) {
                        return Some(found);
                    }
                }
            }
            None
        }
        walk(wd, filename, 0)
    }

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
                let runner = &self.turn_runner;
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
                        if self.subtask_driver.active {
                            // Inject first subtask instruction
                            if let Some(instr) = self.subtask_driver.current_instruction() {
                                self.conversation.add_user_message(&instr);
                            }
                            continue; // Don't finish — drive subtask execution
                        }
                    }

                    // Empty response from LLM (common with DeepSeek/SiliconFlow):
                    // If we edited files, ask model to summarize before ending.
                    let is_empty = text.trim().is_empty() && tokens == 0;
                    if is_empty && self.retry_count < 2 && self.tool_call_count > 0 {
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
                        self.auto_compile_verify().await;
                        self.syntax_check_edited_files().await;
                    }

                    // Apply discipline: inject reminders, check step limits
                    self.apply_post_turn_discipline();
                    if self.check_step_limit() {
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

    /// Forward a TurnEvent to the TUI as an AgentEvent.
    fn forward_turn_event(&mut self, event: TurnEvent) {
        match event {
            TurnEvent::TextDelta(text) => {
                self.model_produced_text = true;
                let _ = self.event_tx.send(AgentEvent::TextDelta(text));
            }
            TurnEvent::ToolCallStarted { ref name, ref arguments } => {
                self.current_tool_name = name.clone();
                self.phase = AgentPhase::CallingTool(name.clone());
                let _ = self.event_tx.send(AgentEvent::PhaseChange(self.phase.clone()));

                // Track bash command for failure categorization
                if name == "bash" {
                    if let Ok(args) = serde_json::from_str::<serde_json::Value>(arguments) {
                        self.last_bash_cmd = args.get("command")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                    }
                }

                // Track files for Working Set
                if matches!(name.as_str(), "read_file" | "edit_file" | "write_file" | "search_replace" | "glob" | "grep") {
                    if let Ok(args) = serde_json::from_str::<serde_json::Value>(arguments) {
                        let fp = args.get("file_path").and_then(|v| v.as_str())
                            .or_else(|| args.get("path").and_then(|v| v.as_str()));
                        if let Some(fp) = fp {
                            let short = std::path::Path::new(fp)
                                .file_name()
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_else(|| fp.to_string());
                            self.session_files.insert(short, std::path::PathBuf::from(fp));
                        }
                    }
                }

                let _ = self.event_tx.send(AgentEvent::ToolCallStarted { name: name.clone(), arguments: arguments.clone() });
            }
            TurnEvent::ToolCallResult { name, output, success, duration } => {
                // Track files for discipline
                if let Some(pos) = output.find("Edited ") {
                    // Extract full path from "Edited /path/to/file ..." or "Edited /path/to/file\n..."
                    let rest = &output[pos + 7..];
                    let full_path_end = rest.find(|c: char| c == ' ' || c == '\n' || c == '(').unwrap_or(rest.len());
                    let full_path_str = rest[..full_path_end].trim();
                    if !full_path_str.is_empty() {
                        self.active_file = Some(PathBuf::from(full_path_str));
                    }
                    if let Some(end) = rest.find(|c: char| c == '\n' || c == '.') {
                        let file = short_path(&rest[..end]);
                        if !self.files_edited_this_turn.contains(&file) {
                            self.files_edited_this_turn.push(file);
                        }
                    }
                }
                if let Some(pos) = output.find("Wrote ") {
                    let rest = &output[pos + 6..];
                    let full_path_end = rest.find(|c: char| c == ' ' || c == '\n' || c == '(').unwrap_or(rest.len());
                    let full_path_str = rest[..full_path_end].trim();
                    if !full_path_str.is_empty() {
                        self.active_file = Some(PathBuf::from(full_path_str));
                    }
                    if let Some(end) = rest.find(|c: char| c == '\n' || c == ' ') {
                        let file = short_path(&rest[..end]);
                        if !self.files_edited_this_turn.contains(&file) {
                            self.files_edited_this_turn.push(file);
                        }
                    }
                }
                if matches!(name.as_str(), "read_file" | "list_directory" | "glob" | "grep") {
                    self.consecutive_reads += 1;
                } else if matches!(name.as_str(), "edit_file" | "write_file") {
                    self.consecutive_reads = 0;
                }

                // Track scouting commands for datalog metrics (no injection).
                if name == "bash" {
                    let cmd = self.last_bash_cmd.to_lowercase();
                    if cmd.contains("curl") || cmd.contains("lsof")
                        || cmd.contains("ps aux") || cmd.contains("tail") {
                        self.scouting_count += 1;
                    }
                } else if matches!(name.as_str(), "read_file" | "edit_file" | "write_file") {
                    self.scouting_count = 0;
                }

                // Extract and persist cross-session knowledge (db credentials, ports, etc.)
                let entries = knowledge::extract_knowledge(&output);
                if !entries.is_empty() {
                    let wd = self.turn_runner.context.working_dir.try_read()
                        .map(|g| g.clone()).unwrap_or_default();
                    knowledge::save_knowledge(&wd, &entries);
                }

                let _ = self.event_tx.send(AgentEvent::ToolCallResult {
                    name, output, success, duration,
                });
            }
            TurnEvent::TokenUsage { prompt_tokens, completion_tokens, total_tokens: _ } => {
                let _ = self.event_tx.send(AgentEvent::TokenUsage(
                    crate::stream::TokenUsage {
                        prompt_tokens,
                        completion_tokens,
                    }
                ));
            }
            TurnEvent::ContextStats { system_tokens, hot_tokens, cold_tokens, working_set_tokens, total_messages } => {
                let _ = self.event_tx.send(AgentEvent::ContextStats {
                    system_tokens, hot_tokens, cold_tokens, working_set_tokens, total_messages,
                });
            }
            TurnEvent::Error(e) => {
                let _ = self.event_tx.send(AgentEvent::Error(e));
            }
        }
    }

    /// Post-process tool results added by TurnRunner: truncate large outputs
    /// and externalize to disk store. TurnRunner adds raw results; we clean them up.
    fn post_process_tool_results(&mut self, tool_count: usize) {
        let context_window = self.config
            .providers
            .get(&self.config.default_provider)
            .map(|p| p.context_window)
            .unwrap_or(16000);
        crate::turn::truncation::post_process_tool_results(
            &mut self.conversation.messages,
            tool_count,
            &self.current_tool_name,
            &self.result_store,
            context_window,
        );
    }

    /// Apply discipline after a turn with tool calls.
    /// Injects system reminders into conversation and tracks usage.
    fn apply_post_turn_discipline(&mut self) {
        // System reminders: re-inject rules + task every 4 steps.
        if self.tool_call_count > 0 && self.tool_call_count % 4 == 0 {
            let task_hint = if self.current_task.chars().count() > 100 {
                format!("{}...", self.current_task.chars().take(97).collect::<String>())
            } else {
                self.current_task.clone()
            };

            // Build file tracking status
            let read_list = if self.files_read_this_turn.is_empty() {
                "none".to_string()
            } else {
                self.files_read_this_turn.join(", ")
            };
            let edit_list = if self.files_edited_this_turn.is_empty() {
                "none yet — you should be editing!".to_string()
            } else {
                self.files_edited_this_turn.join(", ")
            };

            let urgency = if self.tool_call_count >= 15 {
                "URGENT: You MUST take action NOW. Either edit code, restart a service, or explain the issue to the user."
            } else if self.files_edited_this_turn.is_empty() && self.tool_call_count >= 10 {
                "STOP diagnosing. Take action NOW: edit code, restart service, or explain to user."
            } else if self.files_edited_this_turn.is_empty() && self.tool_call_count >= 6 {
                "Decide NOW: code bug → edit_file. Service old code → restart. Can't tell → ask user."
            } else {
                "Only read files you plan to edit."
            };

            // Find sibling files that might have the same bug pattern
            let sibling_hint = if !self.files_edited_this_turn.is_empty() {
                self.find_sibling_files_hint()
            } else {
                String::new()
            };

            let reminder = format!(
                "\n\n<system-reminder>\n\
                 TASK: \"{}\"\n\
                 STEP: {}/{}\n\
                 FILES READ: {}\n\
                 FILES EDITED: {}\n\
                 {}\n\
                 {}\
                 </system-reminder>",
                task_hint, self.tool_call_count,
                25 + self.files_edited_this_turn.len() * 5,
                read_list, edit_list, urgency, sibling_hint
            );

            // Append reminder to the last tool result in conversation
            if let Some(last_msg) = self.conversation.messages.last_mut() {
                match &mut last_msg.content {
                    crate::conversation::message::MessageContent::ToolResult(ref mut r) => {
                        r.output.push_str(&reminder);
                    }
                    _ => {}
                }
            }
        }

        // Re-read guard: inject warnings for files read too many times.
        // count >= 3: hard block warning — the model is looping on the same file.
        // count >= 2 without offset in last read: warn about full re-reads.
        let mut reread_warnings: Vec<String> = Vec::new();
        for (file, count) in &self.file_read_counts {
            if *count >= 3 {
                reread_warnings.push(format!(
                    "[BLOCKED: You have read {} {} times this turn. \
                     You already have the content. STOP re-reading and use what you have. \
                     If you need to edit, use edit_file now.]",
                    file, count
                ));
            }
        }
        if !reread_warnings.is_empty() {
            let warning = reread_warnings.join("\n");
            if let Some(last_msg) = self.conversation.messages.last_mut() {
                match &mut last_msg.content {
                    crate::conversation::message::MessageContent::ToolResult(ref mut r) => {
                        r.output.push_str(&format!("\n{}", warning));
                    }
                    _ => {}
                }
            }
        }

        // NOTE: Silent-round progress prompt disabled — add_user_message injections
        // confuse weak models and waste context. Let the model work silently.
    }

    /// Check if step limit has been reached.
    fn check_step_limit(&self) -> bool {
        let dynamic_limit = 35 + (5 * self.files_edited_this_turn.len());
        let hard_limit = dynamic_limit.min(60);
        self.tool_call_count >= hard_limit
    }

    // Legacy methods removed: call_llm, dispatch_pending_tools,
    // execute_tools_parallel, process_next_tool_call, execute_tool, handle_tool_result.
    // Their responsibilities are now split between TurnRunner and run_turn_loop.


    // -------------------------------------------------------------------------
    // Helper methods
    // -------------------------------------------------------------------------

    /// Find sibling files (same directory, same extension) of edited files
    /// and suggest the model check them for the same bug pattern.
    fn find_sibling_files_hint(&self) -> String {
        let wd: PathBuf = self.turn_runner.context.working_dir
            .try_read()
            .map(|g| g.clone())
            .unwrap_or_default();

        let mut siblings: Vec<String> = Vec::new();
        let mut seen_dirs: std::collections::HashSet<String> = std::collections::HashSet::new();

        for _edited in &self.files_edited_this_turn {
            // Reconstruct full path from short path
            // edited is like ".../views/SearchView.vue"
            // We need to find the directory and list siblings
            for msg in self.conversation.messages.iter().rev() {
                if let crate::conversation::message::MessageContent::AssistantWithToolCalls { tool_calls, .. } = &msg.content {
                    for tc in tool_calls {
                        if tc.name == "edit_file" || tc.name == "write_file" {
                            if let Ok(args) = serde_json::from_str::<serde_json::Value>(&tc.arguments) {
                                if let Some(fp) = args.get("file_path").and_then(|v| v.as_str()) {
                                    let path = std::path::Path::new(fp);
                                    if let (Some(dir), Some(ext)) = (path.parent(), path.extension()) {
                                        let dir_key = dir.to_string_lossy().to_string();
                                        if seen_dirs.contains(&dir_key) { continue; }
                                        seen_dirs.insert(dir_key);

                                        // List sibling files with same extension
                                        if let Ok(entries) = std::fs::read_dir(dir) {
                                            for entry in entries.flatten() {
                                                let name = entry.file_name().to_string_lossy().to_string();
                                                let entry_path = entry.path();
                                                if entry_path.extension() == Some(ext)
                                                    && entry_path != path
                                                    && !self.files_edited_this_turn.iter().any(|e| name.contains(e) || e.contains(&name))
                                                {
                                                    let rel = entry_path.strip_prefix(&wd)
                                                        .map(|p| p.to_string_lossy().to_string())
                                                        .unwrap_or_else(|_| name.clone());
                                                    siblings.push(rel);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if siblings.is_empty() {
            return String::new();
        }

        siblings.truncate(5);
        format!(
            "IMPORTANT: You fixed a bug in {}. These sibling files may have the SAME bug: {}. Check them before finishing.\n",
            self.files_edited_this_turn.join(", "),
            siblings.join(", ")
        )
    }

    /// Check if the model should verify its changes before finishing.
    /// Returns true if: edits were made AND no bash/build command was run AFTER the last edit.
    #[allow(dead_code)]
    fn should_verify(&self) -> bool {
        if self.files_edited_this_turn.is_empty() {
            return false; // No edits, nothing to verify
        }
        if self.tool_call_count >= 20 {
            return false; // Near step limit, don't waste steps
        }

        // Check the LAST tool call and its result.
        // If it's a SUCCESSFUL bash → already verified. No need for another.
        // If it's a FAILED bash (build error) → need to verify/fix.
        // If it's edit/write/read → hasn't verified yet.
        let mut last_tool_name = String::new();
        let mut last_result_success = true;
        for msg in self.conversation.messages.iter().rev() {
            if let (Some(success), Some(output)) = (msg.tool_result_success(), msg.tool_result_output()) {
                if last_tool_name.is_empty() {
                    last_result_success = success;
                    // Also check output for build failure keywords
                    let out = output.to_lowercase();
                    if out.contains("build failed") || out.contains("error") || out.contains("failed") {
                        last_result_success = false;
                    }
                }
            }
            if let crate::conversation::message::MessageContent::AssistantWithToolCalls { tool_calls, .. } = &msg.content {
                if let Some(last_tc) = tool_calls.last() {
                    if last_tool_name.is_empty() {
                        last_tool_name = last_tc.name.clone();
                    }
                    // If last tool was bash AND it succeeded → no verify needed
                    // If last tool was bash AND it failed → verify/fix needed
                    return last_tool_name != "bash" || !last_result_success;
                }
            }
            if matches!(msg.role, crate::conversation::message::Role::User) {
                break;
            }
        }
        false
    }

    /// Inject a verification prompt into the conversation as a user message,
    /// forcing the model to check its work before declaring success.
    #[allow(dead_code)]
    fn inject_verify_prompt(&mut self) {
        let files = self.files_edited_this_turn.join(", ");
        let verify_msg = format!(
            "[SYSTEM: You edited {}. Before finishing, verify your changes work. \
             Run a quick check: look for syntax errors, check if the dev server shows errors, \
             or re-read a key edited file to confirm it's correct. \
             If you find errors, fix them now.]",
            files
        );
        // Inject as assistant thought + will trigger another LLM call
        self.conversation.push_delta(&verify_msg);
        self.conversation.finalize_stream();
    }

    /// ATLAS-style auto-compile verification after edits.
    /// Detects the project's compile command and runs it.
    /// Injects result into conversation so model sees errors immediately.
    /// Only runs once per "batch" of edits (tracked by last_compile_at_step).
    async fn auto_compile_verify(&mut self) {
        // Only auto-compile for compiled languages (Java/Rust/Go/TS).
        // Skip if we already compiled at this step count.
        static COMPILE_COMMANDS: &[(&str, &str)] = &[
            ("pom.xml", "mvn compile -q 2>&1 | tail -20"),
            ("build.gradle", "gradle compileJava -q 2>&1 | tail -20"),
            ("Cargo.toml", "cargo check 2>&1 | tail -20"),
            ("tsconfig.json", "npx tsc --noEmit 2>&1 | tail -20"),
        ];

        let wd = self.turn_runner.context.working_dir.try_read()
            .map(|g| g.clone()).unwrap_or_default();

        // Find compile command by checking for build files (project root or subdirs)
        let mut compile_cmd: Option<String> = None;
        let mut compile_dir = wd.clone();

        for &(marker, cmd) in COMPILE_COMMANDS {
            // Check project root
            if wd.join(marker).exists() {
                compile_cmd = Some(cmd.to_string());
                compile_dir = wd.clone();
                break;
            }
            // Check all immediate subdirectories for marker files.
            // No hardcoded directory names — any subdir with a build marker gets checked.
            if let Ok(entries) = std::fs::read_dir(&wd) {
                for entry in entries.flatten() {
                    if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) { continue; }
                    let sub = entry.path();
                    if sub.join(marker).exists() {
                        compile_cmd = Some(format!("cd {} && {}", sub.display(), cmd));
                        compile_dir = sub;
                        break;
                    }
                }
            }
            if compile_cmd.is_some() { break; }
        }

        let compile_cmd = match compile_cmd {
            Some(c) => c,
            None => return, // No compiled language detected
        };

        // Auto-compile after every edit. The 5-10s compile cost is worth it —
        // catching errors immediately saves 5-10 steps of broken edit accumulation.

        // Run compile
        let output = tokio::process::Command::new("bash")
            .arg("-c")
            .arg(&compile_cmd)
            .current_dir(&compile_dir)
            .output()
            .await;

        match output {
            Ok(o) => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                let stderr = String::from_utf8_lossy(&o.stderr);
                let combined = format!("{}{}", stdout, stderr);

                if o.status.success() {
                    // Compile passed — inject short confirmation
                    self.build_fail_count = 0;
                    // Advance subtask driver on compile pass
                    if self.subtask_driver.active {
                        self.subtask_driver.advance();
                        if let Some(instr) = self.subtask_driver.current_instruction() {
                            self.conversation.add_user_message(&format!(
                                "[Auto-compile: PASSED]\n{}", instr
                            ));
                        } else {
                            self.conversation.add_user_message(
                                "[Auto-compile: PASSED. All subtasks done. Verify and summarize.]"
                            );
                        }
                    } else {
                        self.conversation.add_user_message("[Auto-compile: PASSED. Continue.]");
                    }
                } else {
                    // Compile failed — inject error with source diagnosis
                    self.build_fail_count += 1;
                    let enhanced = crate::tool::devserver::java::enhance_compile_error(
                        &combined, &compile_dir,
                    );
                    // Trim to keep context small
                    let error_lines: String = enhanced.lines()
                        .filter(|l| l.contains("[ERROR]") || l.contains(">>>") || l.contains("---") || l.contains("[AUTO"))
                        .take(20)
                        .collect::<Vec<_>>()
                        .join("\n");
                    let msg = if error_lines.is_empty() {
                        format!("[Auto-compile: FAILED]\n{}", combined.lines().take(15).collect::<Vec<_>>().join("\n"))
                    } else {
                        format!("[Auto-compile: FAILED]\n{}", error_lines)
                    };
                    self.conversation.add_user_message(&msg);
                }
            }
            Err(_) => {} // Compile command not available, skip
        }
    }

    /// Tree-sitter syntax check on recently edited files.
    /// Language-agnostic: works on any file tree-sitter can parse.
    /// Catches bracket mismatches, missing closings, duplicate declarations
    /// that build tools may miss (e.g., Vite doesn't catch Vue SFC syntax errors).
    async fn syntax_check_edited_files(&mut self) {
        let wd = self.turn_runner.context.working_dir.try_read()
            .map(|g| g.clone()).unwrap_or_default();

        let mut warnings: Vec<String> = Vec::new();
        let mut searcher = self.turn_runner.context.semantic.lock().await;

        for file in &self.files_edited_this_turn {
            // Resolve to full path
            let path = if std::path::Path::new(file).is_absolute() {
                std::path::PathBuf::from(file)
            } else {
                wd.join(file)
            };
            if let Ok(content) = std::fs::read_to_string(&path) {
                let (errors, lines) = searcher.count_syntax_errors(&content, &path);
                if errors > 0 {
                    let lines_str = lines.iter()
                        .map(|l| format!("L{}", l))
                        .collect::<Vec<_>>()
                        .join(", ");
                    warnings.push(format!(
                        "{}: {} syntax error(s) at {}",
                        file, errors, lines_str
                    ));
                }
            }
        }
        drop(searcher);

        if !warnings.is_empty() {
            let msg = format!(
                "[SYNTAX CHECK: {}. Fix these before continuing — the file structure may be broken.]",
                warnings.join("; ")
            );
            self.conversation.add_user_message(&msg);
        }
    }

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

    fn build_system_prompt(&mut self) -> String {
        // Dynamic rules: select prompt sections based on task type.
        // If user has a custom system_prompt in config, use that instead (override).
        let rules = if let Some(custom) = self.config.providers
            .get(&self.config.default_provider)
            .and_then(|p| p.system_prompt.as_deref())
        {
            custom.to_string()
        } else {
            crate::config::prompt_sections::build_rules().to_string()
        };

        let wd: PathBuf = self
            .turn_runner.context
            .working_dir
            .try_read()
            .map(|g| g.clone())
            .unwrap_or_default();

        // Use cached project context if working dir hasn't changed
        let project_ctx = match &self.project_context_cache {
            Some((cached_wd, cached_ctx)) if cached_wd == &wd => cached_ctx.clone(),
            _ => {
                let pc = crate::project_context::build_project_context(&wd);
                self.project_context_cache = Some((wd.clone(), pc.text.clone()));
                self.context_included_files = pc.included_files;
                pc.text
            }
        };

        // No file suggestions — let the model decide which files to read
        // based on the project structure and conversation context (like Claude Code).

        // Load project-level instructions (.atomcode.md or ATOMCODE.md)
        let project_instructions = [".atomcode.md", "ATOMCODE.md"]
            .iter()
            .find_map(|name| {
                let path = wd.join(name);
                std::fs::read_to_string(&path).ok()
            })
            .unwrap_or_default();

        // Inject environment metadata
        let shell = if cfg!(target_os = "windows") {
            std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".into())
        } else {
            std::env::var("SHELL").unwrap_or_else(|_| "bash".into())
        };
        let date_str = if cfg!(target_os = "windows") {
            // Windows: use PowerShell for date
            std::process::Command::new("cmd.exe")
                .args(&["/C", "echo %date%"])
                .output()
                .ok().and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| "unknown".into())
        } else {
            std::process::Command::new("date").arg("+%Y-%m-%d").output()
                .ok().and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|| "unknown".into())
        };
        let env_info = format!(
            "Platform: {} | Shell: {} | Date: {}",
            std::env::consts::OS, shell, date_str,
        );

        // Git context (branch + status summary)
        let git_info = std::process::Command::new("git")
            .args(&["status", "--short", "--branch"])
            .current_dir(&wd)
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| {
                let lines: Vec<&str> = s.lines().take(10).collect();
                lines.join("\n")
            })
            .unwrap_or_default();

        // Assemble prompt: env + project context + pre-read files (bulk) → rules LAST.
        // Models attend most to the START and END of context (primacy + recency).
        // Pre-read files go in the middle (bulk reference material).
        // Rules go LAST so the model remembers them when generating tool calls.
        let mut prompt = format!(
            "Working directory: {wd}\nALL file paths MUST start with {wd}. NEVER use paths from previous sessions.\n{env_info}\n",
            wd = wd.display(), env_info = env_info,
        );

        if !git_info.is_empty() {
            prompt.push_str(&format!("Git: {}\n", git_info));
        }

        // Recent activity: extract edited file names from the most recent datalog.
        // Only file names (not content/user messages) — safe, small, factual.
        let recent_activity = extract_recent_activity_from_datalog(&wd);
        if !recent_activity.is_empty() {
            prompt.push_str(&format!("Recent activity: {}\n", recent_activity));
        }

        // Active services detected via lsof + extracted from tool outputs.
        if !self.active_services.is_empty() {
            prompt.push_str("Running services (live):\n");
            let mut has_node = false;
            for (label, url) in &self.active_services {
                prompt.push_str(&format!("  {} — {}\n", url, label));
                if label.contains("node") {
                    has_node = true;
                }
            }
            if has_node {
                prompt.push_str("(Node dev server detected — auto-reloads on save, no build needed.)\n");
            }
        }

        prompt.push_str(&format!(
            "\n=== PROJECT STRUCTURE ===\n{project_ctx}\n"
        ));

        // Pre-read files (bulk content — middle of prompt)
        if !self.preread_context.is_empty() {
            prompt.push_str(&format!("\n\n{}", self.preread_context));
        }

        // NOTE: Active file full-content injection disabled — it consumes too much
        // context window on weak models (32K), degrading decision quality.
        // The working-set skeleton mechanism is sufficient.

        // Project instructions (if any)
        if !project_instructions.is_empty() {
            prompt.push_str(&format!(
                "\n=== PROJECT INSTRUCTIONS (.atomcode.md) ===\n{}\n",
                project_instructions
            ));
        }

        // Cross-session knowledge: db credentials, ports, startup commands, etc.
        let project_knowledge = knowledge::load_knowledge(&wd);
        if !project_knowledge.is_empty() {
            prompt.push_str(&format!("\n{}\n", project_knowledge));
        }

        // Previous session context: inject the last few completed turns' outcomes
        // so the model knows what was done before (prevents re-doing the same work).
        let prev_context = self.build_previous_session_context();
        if !prev_context.is_empty() {
            prompt.push_str(&format!(
                "\n=== PREVIOUS SESSION ===\n{}\n",
                prev_context
            ));
        }

        // Available skills (descriptions only — full content loaded lazily via use_skill tool)
        if let Ok(reg) = self.skill_registry.read() {
            let mut skill_lines: Vec<String> = reg.invocable_by_llm()
                .map(|s| format!("  - {}: {}", s.name, s.description))
                .collect();
            if !skill_lines.is_empty() {
                skill_lines.sort();
                prompt.push_str("\n=== AVAILABLE SKILLS ===\n");
                prompt.push_str("Use the `use_skill` tool to load a skill's full instructions when the task matches.\n");
                prompt.push_str(&skill_lines.join("\n"));
                prompt.push('\n');
            }
        }

        // RULES GO LAST — recency effect ensures the model remembers these
        // when it starts generating tool calls.
        prompt.push_str(&format!("\n=== RULES (follow these strictly) ===\n{rules}\n"));

        // Platform-specific rules — only injected on the target OS.
        // macOS/Linux get nothing extra; Windows gets cmd.exe syntax rules.
        let platform = crate::config::platform_rules();
        if !platform.is_empty() {
            prompt.push_str(platform);
            prompt.push('\n');
        }

        prompt
    }

    /// Build a summary of the previous session's completed turns.
    /// This gives the model context about what was already done, preventing
    /// it from re-doing work (e.g., re-fixing Java version compatibility).
    /// Only includes turns that are Completed (not the current Active turn).
    /// Capped at the last 5 turns and 1500 chars total.
    fn build_previous_session_context(&self) -> String {
        let turns = &self.conversation.turn_tracker.turns;
        if turns.is_empty() {
            return String::new();
        }

        // Only include Completed turns (not Active).
        let completed: Vec<_> = turns.iter()
            .filter(|t| t.status == crate::conversation::turn::TurnStatus::Completed)
            .collect();

        if completed.is_empty() {
            return String::new();
        }

        // Take the last 5 completed turns.
        let recent = &completed[completed.len().saturating_sub(5)..];
        let mut ctx = String::new();

        for turn in recent {
            let msgs = &self.conversation.messages[turn.start_idx..turn.end_idx()];

            // Extract user question.
            let user_q = msgs.first()
                .and_then(|m| m.text())
                .unwrap_or("(unknown)");
            let user_short = if user_q.chars().count() > 80 {
                format!("{}...", user_q.chars().take(77).collect::<String>())
            } else {
                user_q.to_string()
            };

            // Extract assistant outcome (last text message in turn).
            let mut outcome = String::new();
            for msg in msgs.iter().rev() {
                if let Some(text) = msg.text() {
                    if matches!(msg.role, crate::conversation::message::Role::Assistant) && !text.trim().is_empty() {
                        outcome = if text.chars().count() > 200 {
                            format!("{}...", text.chars().take(197).collect::<String>())
                        } else {
                            text.to_string()
                        };
                        break;
                    }
                }
            }

            if outcome.is_empty() {
                // Synthesize from tool results
                outcome = self.conversation.synthesize_turn_outcome(msgs);
            }

            if !outcome.is_empty() {
                ctx.push_str(&format!("- User: \"{}\"\n  Result: {}\n", user_short, outcome));
            }

            if ctx.len() > 1500 {
                // Truncate at a char boundary to avoid panic on multi-byte UTF-8.
                let mut end = 1500;
                while end > 0 && !ctx.is_char_boundary(end) {
                    end -= 1;
                }
                ctx.truncate(end);
                ctx.push_str("\n...(truncated)");
                break;
            }
        }

        ctx
    }


    #[allow(dead_code)]
    fn resolve_args(&self, call: &ToolCall) -> String {
        let wd: PathBuf = self
            .turn_runner.context
            .working_dir
            .try_read()
            .map(|g| g.clone())
            .unwrap_or_default();

        // Try parsing JSON directly, then repair, then specialized extractors
        let args_str = &call.arguments;
        let parsed = serde_json::from_str::<serde_json::Value>(args_str)
            .or_else(|_| serde_json::from_str::<serde_json::Value>(&crate::turn::json_repair::repair_json(args_str)))
            .or_else(|_| {
                // For edit_file: specialized parser that handles unescaped source code
                if call.name == "edit_file" {
                    if let Some(v) = crate::turn::json_repair::extract_edit_file_args(args_str) {
                        return Ok(v);
                    }
                }
                Ok::<serde_json::Value, serde_json::Error>(crate::turn::json_repair::extract_json_fields(args_str))
            });

        if let Ok(mut args) = parsed {
            // Resolve relative paths for ALL path-like fields
            for field in &["file_path", "path"] {
                if let Some(fp) = args.get(*field).and_then(|v| v.as_str()) {
                    let p = std::path::Path::new(fp);
                    if !fp.is_empty() && !p.is_absolute() && fp != "." {
                        let resolved = wd.join(p);
                        args[*field] = serde_json::json!(resolved.to_string_lossy().to_string());
                    }
                }
            }

            // Glob pattern resolution is handled inside the glob tool itself.
            // Do NOT resolve here — it breaks patterns containing `**/`.

            // read_file: first read of a file → force full read (ignore offset/limit).
            // Prevents the model from doing 4-7 partial reads of a file it hasn't seen yet.
            // Subsequent re-reads are allowed to use offset/limit.
            if call.name == "read_file" {
                if let Some(fp) = args.get("file_path").and_then(|v| v.as_str()) {
                    let short = short_path(fp);
                    let read_count = self.file_read_counts.get(&short).copied().unwrap_or(0);
                    if read_count == 0 && (args.get("offset").is_some() || args.get("limit").is_some()) {
                        // First read — remove offset/limit to get the full file.
                        if let Some(obj) = args.as_object_mut() {
                            obj.remove("offset");
                            obj.remove("limit");
                        }
                    }
                }
            }

            return serde_json::to_string(&args).unwrap_or(call.arguments.clone());
        }
        call.arguments.clone()
    }

    /// Intercept tool calls that are provably redundant — returns a short message
    /// instead of executing the tool. Only intercepts cases where the data is
    /// already available in the system prompt (descriptor files, working dir tree).
    /// Does NOT intercept duplicate reads (the model may re-read with different params).
    #[allow(dead_code)]
    fn intercept_redundant_call(&mut self, tool_name: &str, args: &str) -> Option<String> {
        // Pre-read cache hit: if a file was already pre-read, return its content
        // directly from disk (zero overhead — the file is in OS cache).
        // This is better than saying "SKIPPED" because the model needs the actual
        // content to construct edit_file calls, and it can't access the system prompt
        // content mid-conversation with limited context windows.
        // We still avoid counting this as a "real" read for budget purposes.

        // No read blocking — trust the model to read what it needs,
        // like Claude Code does. The "NO RE-READING" rule in the system
        // prompt is a guideline, not a hard block.

        // Loop detection: if the same (tool, args) appears 3+ times in recent calls, block it.
        // EXCEPTION: if there was an edit_file/write_file between repeats, the model is
        // retrying after a fix — that's legitimate, not a loop. Reset the counter on edits.
        let args_hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            args.hash(&mut h);
            h.finish()
        };
        let sig = (tool_name.to_string(), args_hash);
        self.recent_calls.push(sig.clone());
        if self.recent_calls.len() > 20 {
            self.recent_calls.remove(0);
        }

        // Count consecutive repeats of this exact call, but reset if an edit happened in between.
        let mut repeat_count = 0usize;
        let mut saw_edit = false;
        for entry in self.recent_calls.iter().rev() {
            if entry.0 == "edit_file" || entry.0 == "write_file" {
                saw_edit = true;
            }
            if *entry == sig {
                if saw_edit {
                    // An edit happened between this repeat and the previous one.
                    // This is "fix then retry", not a blind loop. Don't count earlier repeats.
                    repeat_count += 1;
                    break;
                }
                repeat_count += 1;
            }
        }

        if repeat_count >= 3 {
            return Some(format!(
                "[BLOCKED: You have called {} with the same arguments {} times without making changes. \
                 This is a loop. STOP. If the command failed, the error message tells you why — \
                 fix the underlying issue instead of retrying. \
                 If you cannot fix it, summarize what you completed and what failed for the user.]",
                tool_name, repeat_count
            ));
        }

        // ── Same-file multi-edit detection ──
        // Track consecutive edits to the same file. On the 3rd+ edit without
        // reading the file in between, block and force a re-read.
        if tool_name == "edit_file" || tool_name == "write_file" {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(args) {
                if let Some(fp) = parsed.get("file_path").and_then(|v| v.as_str()) {
                    let short = short_path(fp);
                    if self.consecutive_edits_file.as_deref() == Some(&short) {
                        self.consecutive_edits_count += 1;
                    } else {
                        self.consecutive_edits_file = Some(short.clone());
                        self.consecutive_edits_count = 1;
                    }
                    if self.consecutive_edits_count >= 8 {
                        return Some(format!(
                            "[BLOCKED: You have edited {} {} times in a row. \
                             STOP and re-read the file first to see the current state, \
                             then make ONE comprehensive edit. \
                             Multiple small edits means you're not seeing the full picture.]",
                            short, self.consecutive_edits_count
                        ));
                    }
                }
            }
        } else {
            // Any non-edit tool resets the consecutive edit counter.
            if tool_name == "read_file" {
                // Reading resets the counter — model is re-orienting.
                self.consecutive_edits_file = None;
                self.consecutive_edits_count = 0;
            }
        }

        match tool_name {
            "read_file" => {
                // Only intercept reads of descriptor files already in project context.
                let parsed: serde_json::Value = serde_json::from_str(args).ok()?;
                let file_path = parsed.get("file_path")?.as_str()?;
                let path = std::path::Path::new(file_path);
                let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
                if self.context_included_files.contains(&canonical) {
                    let filename = path.file_name()
                        .map(|f| f.to_string_lossy().to_string())
                        .unwrap_or_else(|| file_path.to_string());
                    return Some(format!(
                        "[SKIPPED: {} content is already in your system prompt. \
                         Use that information. Read the file you need to EDIT instead.]",
                        filename
                    ));
                }
                None
            }
            "list_directory" => {
                // Intercept listing the working directory — tree is already in context.
                let parsed: serde_json::Value = serde_json::from_str(args).ok()?;
                let list_path = parsed.get("path").and_then(|v| v.as_str()).unwrap_or(".");
                let wd = self.turn_runner.context.working_dir.try_read().ok()?;
                let wd_str = wd.to_string_lossy();
                if list_path == "." || list_path == wd_str.as_ref() {
                    return Some(
                        "[SKIPPED: Working directory file tree is already in your system prompt. \
                         Read the file you need to EDIT instead.]".to_string()
                    );
                }
                None
            }
            _ => None,
        }
    }

    /// Inflate ToolResultRef messages in the provider message list.
    // inflate_recent_refs — moved to TurnRunner.run() where messages are built.

    /// Add a tool result to the conversation, externalizing large outputs to disk.
    /// Results smaller than the threshold are stored inline for simplicity.
    /// Note: post_process_tool_results() now handles this for TurnRunner-added messages.
    /// This method is retained for any future direct-add paths.
    #[allow(dead_code)]
    fn store_tool_result(&mut self, result: ToolResult) {
        const EXTERNALIZE_THRESHOLD: usize = 512;
        if result.output.len() >= EXTERNALIZE_THRESHOLD {
            let result_ref = self.result_store.store(&result);
            self.conversation.add_tool_result_ref(result_ref);
        } else {
            self.conversation.add_tool_result(result);
        }
    }

    /// Detect already-running dev servers by probing common ports.
    /// Runs once at startup to populate active_services.
    /// Detect running services via `lsof` — shows actual listening ports with process names.
    /// No hardcoded ports. The process name (java/node/python) is the label.
    async fn detect_running_services(&mut self) {
        let output = tokio::process::Command::new("lsof")
            .args(&["-i", "-P", "-n", "-sTCP:LISTEN"])
            .output()
            .await;

        let stdout = match output {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
            _ => return, // lsof not available or failed — skip silently
        };

        // Parse lsof output. Each line looks like:
        // node    80162 yubangxu   23u  IPv4 0x... TCP 127.0.0.1:3004 (LISTEN)
        // java    79842 yubangxu   45u  IPv6 0x... TCP *:8080 (LISTEN)
        for line in stdout.lines().skip(1) { // skip header
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 9 {
                continue;
            }
            let process = parts[0].to_lowercase();
            // Find the TCP address:port part
            // Match any TCP address:port — localhost, 127.0.0.1, [::1], *:
            let addr_part = parts.iter()
                .find(|p| p.contains(':') && (
                    p.contains("localhost") || p.contains("127.0.0.1")
                    || p.contains("[::1]") || p.starts_with("*:")
                ))
                .copied()
                .unwrap_or("");

            if let Some(colon) = addr_part.rfind(':') {
                if let Ok(port) = addr_part[colon + 1..].parse::<u16>() {
                    if port >= 1024 {
                        let url = format!("http://localhost:{}", port);
                        let label = format!("{} ({})", process, port);
                        self.active_services.insert(label, url);
                    }
                }
            }
        }
    }

    /// Extract http://localhost:PORT URLs from tool output and store them.
    /// Uses the command to guess a label (frontend/backend/service).
    #[allow(dead_code)]
    fn extract_service_urls(
        output: &str,
        cmd: &str,
        services: &mut std::collections::HashMap<String, String>,
    ) {
        // Find all http://localhost:NNNN patterns in the output.
        let mut i = 0;
        let _bytes = output.as_bytes();
        while i < output.len() {
            if let Some(pos) = output[i..].find("http://localhost:") {
                let start = i + pos;
                let after = start + "http://localhost:".len();
                // Extract port digits.
                let port_end = output[after..].find(|c: char| !c.is_ascii_digit())
                    .map(|p| after + p)
                    .unwrap_or(output.len());
                if port_end > after {
                    let url = &output[start..port_end];
                    // Guess label from the command.
                    let cmd_lower = cmd.to_lowercase();
                    let label = if cmd_lower.contains("vite") || cmd_lower.contains("npm run dev")
                        || cmd_lower.contains("next") || cmd_lower.contains("webpack")
                        || cmd_lower.contains("frontend") || cmd_lower.contains("yarn dev") {
                        "frontend"
                    } else if cmd_lower.contains("spring") || cmd_lower.contains("mvn")
                        || cmd_lower.contains("gradle") || cmd_lower.contains("flask")
                        || cmd_lower.contains("uvicorn") || cmd_lower.contains("backend")
                        || cmd_lower.contains("cargo run") || cmd_lower.contains("go run") {
                        "backend"
                    } else {
                        "service"
                    };
                    services.insert(label.to_string(), url.to_string());
                    i = port_end;
                } else {
                    i = after;
                }
            } else {
                break;
            }
        }
    }

    /// Normalize a bash command for repeated-execution detection.
    /// Strips: env var prefixes (FOO=bar), stderr redirects (2>&1, 2>/dev/null),
    /// sleep prefixes (sleep N &&), and leading/trailing whitespace.
    /// Returns a stable key so that semantically identical commands match.
    #[allow(dead_code)]
    fn normalize_bash_cmd(cmd: &str) -> String {
        let mut s = cmd.trim().to_string();

        // Strip leading "sleep N && " or "sleep N; " — these are just wait wrappers.
        while let Some(rest) = s.strip_prefix("sleep ") {
            // Find the next "&&" or ";"
            if let Some(pos) = rest.find("&&") {
                s = rest[pos + 2..].trim().to_string();
            } else if let Some(pos) = rest.find(';') {
                s = rest[pos + 1..].trim().to_string();
            } else {
                break; // bare "sleep N" — keep as-is
            }
        }

        // Strip leading env var assignments (KEY=VALUE ...)
        let words: Vec<&str> = s.split_whitespace().collect();
        let start = words.iter().position(|w| !w.contains('=')).unwrap_or(0);
        s = words[start..].join(" ");

        // Strip stderr redirects
        s = s.replace("2>&1", "").replace("2>/dev/null", "");

        // Collapse whitespace
        s.split_whitespace().collect::<Vec<&str>>().join(" ")
    }

    fn change_dir(&mut self, path: &str) {
        let new_path = if path.starts_with('/') {
            std::path::PathBuf::from(path)
        } else if path.starts_with('~') {
            dirs::home_dir()
                .map(|h| h.join(path.strip_prefix("~/").unwrap_or(&path[1..])))
                .unwrap_or_else(|| std::path::PathBuf::from(path))
        } else {
            let wd: PathBuf = self
                .turn_runner.context
                .working_dir
                .try_read()
                .map(|g| g.clone())
                .unwrap_or_default();
            wd.join(path)
        };

        let resolved = std::fs::canonicalize(&new_path).unwrap_or(new_path);
        if resolved.is_dir() {
            if let Ok(mut wd) = self.turn_runner.context.working_dir.try_write() {
                *wd = resolved.clone();
            }
            self.project_context_cache = None; // invalidate on dir change
            // Clear conversation history — old paths from previous directory will confuse the model
            self.conversation.messages.clear();
            self.conversation.turn_tracker = crate::conversation::turn::TurnTracker::new();
            self.session_files.clear();
            // Reload skills for the new working directory (project-level skills may differ)
            if let Ok(mut reg) = self.skill_registry.write() {
                reg.reload(&resolved);
            }
            let _ = self
                .event_tx
                .send(AgentEvent::WorkingDirChanged(resolved));
        }
    }
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




