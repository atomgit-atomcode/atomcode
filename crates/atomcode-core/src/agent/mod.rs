//! The AgentLoop — a standalone agent that processes user messages,
//! calls LLM providers, executes tools, and communicates with the UI
//! via channels. Decoupled from any TUI concerns.

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

    /// Pending user input appended during streaming. Injected before next LLM call.
    pending_input: Option<String>,
    /// Session-level file tracker: all files read/edited across the entire session.
    /// Used to build the "working set" — tree-sitter skeletons injected before each LLM call.
    /// This replaces the old recent_file_cache with a smarter, budget-aware approach.
    session_files: std::collections::HashMap<String, PathBuf>,
    /// Whether planning phase is active (first LLM call without tools to force a plan).
    planning_phase: bool,

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
            pending_input: None,
            planning_phase: false,
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
                AgentCommand::Shutdown => break,
            }
        }
    }

    // -------------------------------------------------------------------------
    // Core agent logic
    // -------------------------------------------------------------------------

    async fn handle_send_message(&mut self, content: String) {
        self.current_task = content.clone();
        self.preread_context = self.build_preread_context(&content).await;

        // Auto-diagnose: if user mentions error keywords, scan logs and attach findings.
        // This gives the model the real error from Turn 1, instead of spending 3-5 turns grepping.
        let enriched = self.auto_diagnose_errors(&content).await;
        self.conversation.add_user_message(&enriched);
        self.turn_tokens = 0;
        self.tool_call_count = 0;
        self.retry_count = 0;
        self.recent_calls.clear();
        self.files_read_this_turn.clear();
        self.files_edited_this_turn.clear();
        self.consecutive_reads = 0;
        self.verify_injected = false;
        self.model_produced_text = false;
        self.build_fail_count = 0;
        self.file_read_counts.clear();
        self.scouting_count = 0;
        self.api_confirmed_working = false;
        self.consecutive_edits_file = None;
        self.consecutive_edits_count = 0;
        self.sleep_count = 0;
        self.consecutive_verify_count = 0;
        self.executed_cmds.clear();
        // Clear session_files on each new user message.
        // Working Set only tracks files from the CURRENT task.
        // Previous files are remembered via cold zone summaries.
        self.session_files.clear();
        self.turn_start = Some(Instant::now());
        self.cancel_token = CancellationToken::new();

        // Detect if this task needs a planning phase.
        // Feature tasks (create/implement/refactor) benefit from planning first.
        // Simple tasks (fix bug, change style, start server) should act directly.
        self.planning_phase = Self::needs_planning(&content);

        self.phase = AgentPhase::Thinking;
        let _ = self
            .event_tx
            .send(AgentEvent::PhaseChange(AgentPhase::Thinking));

        self.run_turn_loop().await;
    }

    /// Detect if a task is complex enough to benefit from a planning phase.
    /// Returns true for feature implementation, refactoring, multi-file tasks.
    fn needs_planning(content: &str) -> bool {
        let s = content.to_lowercase();
        let len = content.chars().count();

        // Short messages are follow-ups or simple tasks — no plan needed.
        if len < 15 {
            return false;
        }

        // Follow-up messages — no plan needed.
        let follow_up_patterns = ["继续", "没有变化", "还是不行", "不行", "报错",
            "失败", "改对了", "好的", "ok", "对", "错", "不对",
            "启动", "start", "install", "安装", "部署"];
        if follow_up_patterns.iter().any(|p| s.contains(p)) {
            return false;
        }

        // Feature/creation patterns — plan needed.
        let feature_patterns = ["实现", "功能", "创建", "新增", "添加",
            "重构", "refactor", "implement", "feature", "build",
            "设计", "开发", "做一个", "做个", "帮我做"];
        feature_patterns.iter().any(|p| s.contains(p))
    }

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
        let log_candidates = ["backend.log", "server.log", "app.log",
            "backend/backend.log", "logs/app.log", "log/development.log"];

        let mut diagnostics = Vec::new();

        for log_name in &log_candidates {
            let log_path = wd.join(log_name);
            if !log_path.exists() { continue; }

            // grep for recent errors/exceptions
            if let Ok(output) = tokio::process::Command::new("grep")
                .args(&["-i", "-E", "error|exception|fail|caused by",
                    &log_path.to_string_lossy()])
                .output()
                .await
            {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if !stdout.trim().is_empty() {
                    // Take last 15 lines of errors
                    let lines: Vec<&str> = stdout.lines().collect();
                    let start = lines.len().saturating_sub(15);
                    let recent = lines[start..].join("\n");
                    diagnostics.push(format!("[Auto-detected from {}:]\n{}", log_name, recent));
                }
            }
        }

        // Also check if any service on common ports is responding with errors
        if diagnostics.is_empty() {
            // Try npm/vite dev server log
            for log_name in &["frontend/.vite/log", "nohup.out"] {
                let log_path = wd.join(log_name);
                if log_path.exists() {
                    if let Ok(content_str) = tokio::fs::read_to_string(&log_path).await {
                        let lines: Vec<&str> = content_str.lines().collect();
                        let start = lines.len().saturating_sub(10);
                        let tail = lines[start..].join("\n");
                        if tail.to_lowercase().contains("error") {
                            diagnostics.push(format!("[Auto-detected from {}:]\n{}", log_name, tail));
                        }
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
        drop(searcher);

        let mut result = format!("{}\n\n{}", content, diagnostics.join("\n\n"));
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
                Self::log_llm_request(
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
                let consecutive_reads = &mut self.consecutive_reads;
                let session_files = &mut self.session_files;

                // Run TurnRunner concurrently with command processing
                let turn_fut = runner.run(&mut conv, &system_prompt, &turn_tx, cancel);
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

                                    // Track files for Working Set
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
                                                session_files.insert(short, std::path::PathBuf::from(fp));
                                            }
                                        }
                                    }

                                    let _ = event_tx.send(AgentEvent::ToolCallStarted { name: name.clone(), arguments: arguments.clone() });
                                }
                                TurnEvent::ToolCallResult { name, output, success, duration } => {
                                    // Track files for discipline
                                    if let Some(pos) = output.find("Edited ") {
                                        let rest = &output[pos + 7..];
                                        if let Some(end) = rest.find(|c: char| c == '\n' || c == '.') {
                                            let file = short_path(&rest[..end]);
                                            if !files_edited_this_turn.contains(&file) {
                                                files_edited_this_turn.push(file);
                                            }
                                        }
                                    }
                                    if let Some(pos) = output.find("Wrote ") {
                                        let rest = &output[pos + 6..];
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
                TurnResult::Responded { text: _, tokens } => {
                    self.turn_tokens += tokens;
                    self.total_tokens += tokens;
                    self.finish_turn();
                    return;
                }
                TurnResult::UsedTools { tool_count, tokens, .. } => {
                    self.turn_tokens += tokens;
                    self.total_tokens += tokens;
                    self.tool_call_count += tool_count;
                    // Post-process: truncate large outputs + externalize to disk
                    self.post_process_tool_results(tool_count);
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
                    // Extract short filename from edit confirmation
                    let rest = &output[pos + 7..];
                    if let Some(end) = rest.find(|c: char| c == '\n' || c == '.') {
                        let file = short_path(&rest[..end]);
                        if !self.files_edited_this_turn.contains(&file) {
                            self.files_edited_this_turn.push(file);
                        }
                    }
                }
                if let Some(pos) = output.find("Wrote ") {
                    let rest = &output[pos + 6..];
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
            TurnEvent::Error(e) => {
                let _ = self.event_tx.send(AgentEvent::Error(e));
            }
        }
    }

    /// Post-process tool results added by TurnRunner: truncate large outputs
    /// and externalize to disk store. TurnRunner adds raw results; we clean them up.
    fn post_process_tool_results(&mut self, tool_count: usize) {
        crate::turn::truncation::post_process_tool_results(
            &mut self.conversation.messages,
            tool_count,
            &self.current_tool_name,
            &self.result_store,
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

    fn finish_turn(&mut self) {
        // Mark the current turn as completed in the tracker.
        self.conversation.turn_tracker.complete_current();

        let duration = self.turn_start.map(|t| t.elapsed()).unwrap_or_default();
        self.turn_start = None;
        self.phase = AgentPhase::Idle;
        let _ = self.event_tx.send(AgentEvent::TurnComplete {
            duration,
            total_tokens: self.turn_tokens,
        });
        let _ = self
            .event_tx
            .send(AgentEvent::PhaseChange(AgentPhase::Idle));
        // Persist conversation history.
        self.conversation.save(&Conversation::history_path());
    }

    fn build_system_prompt(&mut self) -> String {
        let rules = self
            .config
            .providers
            .get(&self.config.default_provider)
            .and_then(|p| p.system_prompt.as_deref())
            .unwrap_or(crate::config::DEFAULT_SYSTEM_PROMPT)
            .to_string();

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

        // Project instructions (if any)
        if !project_instructions.is_empty() {
            prompt.push_str(&format!(
                "\n=== PROJECT INSTRUCTIONS (.atomcode.md) ===\n{}\n",
                project_instructions
            ));
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

    /// Analyze the user's task message and the project file tree to suggest
    /// which files are most likely relevant. This reduces the number of exploratory
    /// reads the model needs to do.
    #[allow(dead_code)]
    fn suggest_files_for_task(&self, task: &str, working_dir: &std::path::Path) -> String {
        let mut suggestions = Vec::new();

        // Walk the file tree (2 levels) and find files whose names match keywords in the task
        let task_lower = task.to_lowercase();

        // Collect all files in the project (up to 2 levels deep)
        let files = collect_project_files(working_dir, 0, 3);

        for file_path in &files {
            let filename = file_path.file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_default()
                .to_lowercase();

            let rel_path = file_path.strip_prefix(working_dir)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| file_path.to_string_lossy().to_string());

            // Skip noise
            if filename.starts_with('.') || filename.ends_with(".log")
                || filename.ends_with(".lock") || filename == "node_modules"
            {
                continue;
            }

            let mut score = 0;

            // Check if filename appears in the task
            let name_no_ext = filename.split('.').next().unwrap_or(&filename);
            if task_lower.contains(name_no_ext) && name_no_ext.len() > 2 {
                score += 10;
            }

            // Check path components
            for component in rel_path.split('/') {
                let comp_lower = component.to_lowercase();
                if comp_lower.len() > 2 && task_lower.contains(&comp_lower) {
                    score += 5;
                }
            }

            // Keyword heuristics (tech-stack agnostic)
            let keyword_map: &[(&[&str], &[&str])] = &[
                (&["接口", "api", "endpoint", "请求", "request", "搜索", "search"],
                 &["api", "route", "handler", "controller", "main", "app", "server", "search"]),
                (&["样式", "style", "css", "布局", "layout", "ui", "界面", "design"],
                 &["css", "style", "layout", "theme", "tailwind"]),
                (&["配置", "config", "设置", "setting"],
                 &["config", "setting", "env"]),
                (&["路由", "router", "route", "导航", "nav"],
                 &["router", "route", "nav"]),
                (&["数据库", "database", "db", "model", "schema"],
                 &["model", "schema", "migration", "db", "database"]),
            ];

            for (task_keywords, file_keywords) in keyword_map {
                let task_match = task_keywords.iter().any(|kw| task_lower.contains(kw));
                let file_match = file_keywords.iter().any(|kw| filename.contains(kw));
                if task_match && file_match {
                    score += 8;
                }
            }

            if score > 0 {
                suggestions.push((score, rel_path));
            }
        }

        suggestions.sort_by(|a, b| b.0.cmp(&a.0));
        suggestions.truncate(5);

        if suggestions.is_empty() {
            return String::new();
        }

        suggestions.iter()
            .map(|(_, path)| format!("- {}", path))
            .collect::<Vec<_>>()
            .join("\n")
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

    /// Log the complete LLM request (messages + tools + metadata) to
    /// `~/.atomcode/logs/YYYY-MM-DD_HH-MM-SS_NNN.json`.
    /// This is fire-and-forget — logging failures are silently ignored.
    pub(crate) fn log_llm_request(
    messages: &[crate::conversation::message::Message],
    tool_defs: &[crate::tool::ToolDef],
    model: &str,
    context_window: usize,
    step: usize,
) {
        use std::io::Write;

        let log_dir = crate::config::Config::config_dir().join("logs");
        let _ = std::fs::create_dir_all(&log_dir);

        // Build timestamp filename.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let secs = now.as_secs();
        let millis = now.subsec_millis();
        // Format as readable timestamp (UTC).
        let ts = {
            // Simple manual formatting to avoid adding chrono dependency.
            let s = secs % 60;
            let m = (secs / 60) % 60;
            let h = (secs / 3600) % 24;
            let days = secs / 86400;
            // Days since epoch → approximate date (good enough for filenames).
            let (y, mo, d) = epoch_days_to_ymd(days);
            format!("{:04}-{:02}-{:02}_{:02}-{:02}-{:02}_{:03}", y, mo, d, h, m, s, millis)
        };
        let path = log_dir.join(format!("{}.json", ts));

        // Serialize messages.
        let msgs_json = serde_json::to_value(messages).unwrap_or(serde_json::json!([]));

        // Serialize tool defs (not Serialize-derived, so do it manually).
        let tools_json: Vec<serde_json::Value> = tool_defs.iter().map(|td| {
            serde_json::json!({
                "name": td.name,
                "description": td.description,
                "parameters": td.parameters,
            })
        }).collect();

        // Token estimate.
        let total_tokens: usize = messages.iter().map(|m| m.estimate_tokens()).sum();

        let log = serde_json::json!({
            "timestamp": ts,
            "model": model,
            "context_window": context_window,
            "step": step,
            "message_count": messages.len(),
            "estimated_tokens": total_tokens,
            "tool_count": tool_defs.len(),
            "messages": msgs_json,
            "tools": tools_json,
        });

        // Write atomically via temp file.
        let tmp = path.with_extension("json.tmp");
        if let Ok(mut f) = std::fs::File::create(&tmp) {
            // Use pretty print for readability.
            let _ = f.write_all(serde_json::to_string_pretty(&log).unwrap_or_default().as_bytes());
            let _ = std::fs::rename(&tmp, &path);
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

/// Extract file_path from tool call arguments JSON.
#[allow(dead_code)]
fn extract_file_from_args(args: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()?
        .get("file_path")?
        .as_str()
        .map(|s| s.to_string())
}

/// Extract command from bash tool call arguments.
#[allow(dead_code)]
fn extract_cmd_from_args(args: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()?
        .get("command")?
        .as_str()
        .map(|s| s.to_string())
}

/// Shorten a file path for display: keep last 2 components.
/// Convert days since Unix epoch to (year, month, day). Simple civil calendar math.
fn epoch_days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.rsplitn(3, '/').collect();
    match parts.len() {
        0 | 1 => path.to_string(),
        2 => format!("{}/{}", parts[1], parts[0]),
        _ => format!(".../{}/{}", parts[1], parts[0]),
    }
}

use crate::tool::SKIP_DIRS;

/// Collect all file paths in a directory tree up to max_depth.
#[allow(dead_code)]
fn collect_project_files(
    dir: &std::path::Path,
    depth: usize,
    max_depth: usize,
) -> Vec<std::path::PathBuf> {
    let mut result = Vec::new();
    if depth > max_depth { return result; }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return result,
    };

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if SKIP_DIRS.contains(&name.as_str()) { continue; }

        let path = entry.path();
        if path.is_dir() {
            result.extend(collect_project_files(&path, depth + 1, max_depth));
        } else {
            result.push(path);
        }
    }
    result
}

/// Detect if a streaming text buffer is repeating earlier content.
/// Returns Some(byte_position) where the repeat starts, None if no repeat detected.
#[allow(dead_code)]
fn detect_streaming_repeat(buf: &str) -> Option<usize> {
    let lines: Vec<&str> = buf.lines().collect();
    if lines.len() < 6 { return None; }

    let half = lines.len() / 2;

    // Check ANY distinctive line (>= 15 chars) that appears in both halves.
    // Previous version only checked markdown headings — too narrow.
    for i in 0..half {
        let line = lines[i].trim();
        if line.len() < 15 { continue; }

        for j in half..lines.len() {
            if lines[j].trim() == line {
                // Verify: at least 2 of the next 4 lines also match
                let match_count = lines[i..].iter().zip(lines[j..].iter())
                    .take(4)
                    .filter(|(a, b)| a.trim() == b.trim())
                    .count();
                if match_count >= 2 {
                    let byte_pos: usize = lines[..j].iter()
                        .map(|l| l.len() + 1)
                        .sum();
                    return Some(byte_pos.min(buf.len()));
                }
            }
        }
    }
    None
}



#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::message::{Message, Role};
    use crate::tool::ToolDef;

    #[test]
    fn test_log_llm_request_creates_json_file() {
        let messages = vec![
            Message::new(Role::System, "You are helpful."),
            Message::new(Role::User, "Hello"),
        ];
        let tool_defs = vec![ToolDef {
            name: "bash",
            description: "Run a command".to_string(),
            parameters: serde_json::json!({"type": "object"}),
        }];

        // Record files before
        let log_dir = crate::config::Config::config_dir().join("logs");
        let before: std::collections::HashSet<_> = std::fs::read_dir(&log_dir)
            .ok()
            .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
            .unwrap_or_default();

        // Call
        AgentLoop::log_llm_request(&messages, &tool_defs, "test-model", 16000, 3);

        // Find new file
        let after: std::collections::HashSet<_> = std::fs::read_dir(&log_dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        let new_files: Vec<_> = after.difference(&before).collect();
        assert_eq!(new_files.len(), 1, "Expected exactly 1 new log file");

        let log_path = new_files[0];
        assert!(log_path.extension().unwrap() == "json");

        // Verify JSON content
        let content = std::fs::read_to_string(log_path).unwrap();
        let json: serde_json::Value = serde_json::from_str(&content).unwrap();

        assert_eq!(json["model"], "test-model");
        assert_eq!(json["context_window"], 16000);
        assert_eq!(json["step"], 3);
        assert_eq!(json["message_count"], 2);
        assert_eq!(json["tool_count"], 1);
        assert!(json["messages"].is_array());
        assert!(json["tools"].is_array());
        assert_eq!(json["tools"][0]["name"], "bash");

        // Cleanup
        let _ = std::fs::remove_file(log_path);
    }
}
