//! The AgentLoop — a standalone agent that processes user messages,
//! calls LLM providers, executes tools, and communicates with the UI
//! via channels. Decoupled from any TUI concerns.

use std::collections::HashSet;
use std::path::PathBuf;
use std::pin::Pin;
use std::future::Future;
use std::time::{Duration, Instant};

use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::conversation::Conversation;
use crate::provider::LlmProvider;
use crate::skill::SkillRegistry;
use crate::stream::StreamEvent;
use crate::tool::{
    PermissionDecision, PermissionStore, ToolCall, ToolCallBuffer, ToolContext, ToolRegistry,
    ToolResult,
};
use crate::tool::result_store::ToolResultStore;
use crate::tool::use_skill::UseSkillTool;

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
    pub tool_registry: ToolRegistry,
    pub provider: Box<dyn LlmProvider>,
    pub tool_context: ToolContext,
    pub permission_store: PermissionStore,
    pub config: Config,

    // Execution state
    pub phase: AgentPhase,
    pub turn_tokens: usize,
    pub total_tokens: usize,
    pub turn_start: Option<Instant>,

    // Per-turn counters
    tool_call_count: usize,
    retry_count: usize,

    // Pending tool calls from multi-tool LLM responses
    pending_tool_calls: Vec<ToolCall>,
    pending_approval: Option<ToolCall>,

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
    /// Recently edited file contents — injected at the END of messages in call_llm
    /// so the model has the latest file content in its highest-attention zone.
    /// Key: short file name, Value: (full_path, content).
    recent_file_cache: std::collections::HashMap<String, (String, String)>,
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

        let agent = Self {
            conversation,
            tool_registry,
            provider,
            tool_context,
            permission_store: PermissionStore::new(),
            config,
            phase: AgentPhase::Idle,
            turn_tokens: 0,
            total_tokens: 0,
            turn_start: None,
            tool_call_count: 0,
            retry_count: 0,
            pending_tool_calls: Vec::new(),
            pending_approval: None,
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
            recent_file_cache: std::collections::HashMap::new(),
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
                    self.pending_tool_calls.clear();
                    self.pending_approval = None;
                    let _ = self.event_tx.send(AgentEvent::PhaseChange(AgentPhase::Idle));
                }
                AgentCommand::ApproveTool => {
                    if let Some(call) = self.pending_approval.take() {
                        self.execute_tool(call).await;
                    }
                }
                AgentCommand::ApproveToolAlways => {
                    if let Some(call) = self.pending_approval.take() {
                        self.permission_store.grant_session(&call.name);
                        self.execute_tool(call).await;
                    }
                }
                AgentCommand::DenyTool => {
                    if let Some(call) = self.pending_approval.take() {
                        let result = ToolResult {
                            call_id: call.id.clone(),
                            output: "Denied by user".to_string(),
                            success: false,
                        };
                        self.handle_tool_result(result).await;
                    }
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
                                self.provider = new_provider;
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
                    self.config = new_config;
                    let default_name = self.config.default_provider.clone();
                    if let Some(provider_config) = self.config.providers.get(&default_name) {
                        match crate::provider::create_provider(provider_config) {
                            Ok(new_provider) => {
                                self.provider = new_provider;
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
        self.recent_file_cache.clear();
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

        self.call_llm().await;
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

        let wd = self.tool_context.working_dir.try_read()
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
        let mut searcher = self.tool_context.semantic.lock().await;

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

    /// Core agent turn: calls the LLM, processes the stream, and handles tool
    /// calls or finishes the turn. Boxed to allow mutual recursion with
    /// execute_tool / handle_tool_result / process_next_tool_call.
    fn call_llm(&mut self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            // Inject any pending user input appended during streaming.
            if let Some(input) = self.pending_input.take() {
                self.conversation.add_user_message(&format!("[Additional context from user]: {}", input));
            }
            let system_prompt = self.build_system_prompt();
            let context_window = self
                .config
                .providers
                .get(&self.config.default_provider)
                .map(|p| p.context_window)
                .unwrap_or(16000);
            let mut messages = self
                .conversation
                .to_provider_messages_budgeted(&system_prompt, context_window);

            // Inflate ToolResultRef messages: recent ones get full content from disk,
            // older ones keep their summary (already compact from budgeted windowing).
            self.inflate_recent_refs(&mut messages);

            // NOTE: recently edited file cache was removed (v2.3.0).
            // It injected full file contents (~8K tokens) OUTSIDE the context budget,
            // causing 40K+ actual input on a 32K budget. The model ignored the cache
            // and re-read files anyway. Net effect was purely negative (slower, no benefit).

            let tool_defs = self.tool_registry.get_definitions();

            // Planning phase: inject a planning instruction before the first LLM call.
            // Tools are still available so the model uses proper function calling format.
            // The model may output plan text + tool calls together — that's fine.
            if self.planning_phase {
                self.planning_phase = false;
                messages.push(crate::conversation::message::Message::new(
                    crate::conversation::message::Role::System,
                    "This is a complex task. FIRST output a brief implementation plan (under 15 lines):\n\
                     - List files to create/modify and what changes each needs\n\
                     - Note the order (dependencies)\n\
                     Then start executing the plan."
                ));
            }

            // Log the complete request to disk for debugging/analysis.
            Self::log_llm_request(
                &messages,
                &tool_defs,
                self.provider.model_name(),
                context_window,
                self.tool_call_count,
            );

            let stream_result = self.provider.chat_stream(&messages, Some(&tool_defs));

            let mut stream = match stream_result {
                Ok(s) => s,
                Err(e) => {
                    let _ = self.event_tx.send(AgentEvent::Error(e.to_string()));
                    self.finish_turn();
                    return;
                }
            };

            let cancel = self.cancel_token.clone();
            let mut tool_calls_buf: Vec<ToolCall> = Vec::new();

            loop {
                tokio::select! {
                    _ = cancel.cancelled() => {
                        self.conversation.finalize_stream();
                        self.finish_turn();
                        return;
                    }
                    cmd = self.cmd_rx.recv() => {
                        match cmd {
                            Some(AgentCommand::Cancel) | None => {
                                self.cancel_token.cancel();
                                self.conversation.finalize_stream();
                                self.finish_turn();
                                return;
                            }
                            Some(AgentCommand::Shutdown) => {
                                self.conversation.finalize_stream();
                                return;
                            }
                            _ => {
                                // Non-cancel commands (ApproveTool, DenyTool, etc.)
                                // during streaming — skip this select iteration,
                                // the command stays consumed but doesn't break the loop.
                                // These shouldn't arrive here in normal flow.
                            }
                        }
                    }
                    event = stream.next() => {
                        match event {
                            Some(Ok(StreamEvent::Delta(text))) => {
                                self.model_produced_text = true;
                                self.conversation.push_delta(&text);
                                let _ = self.event_tx.send(AgentEvent::TextDelta(text));
                                // Real-time repeat detection: if the buffer is repeating
                                // earlier content, truncate and terminate the stream immediately.
                                // All subsequent tokens would be waste — stop the model now.
                                if let Some(ref buf) = self.conversation.stream_buffer {
                                    if buf.len() > 200 {
                                        if let Some(cut) = detect_streaming_repeat(buf) {
                                            let truncated = buf[..cut].trim_end().to_string();
                                            self.conversation.stream_buffer = Some(truncated);
                                            // Kill the stream — drop it by breaking out of the loop.
                                            // Then finalize as if Done was received.
                                            self.conversation.finalize_stream();
                                            self.finish_turn();
                                            return;
                                        }
                                    }
                                }
                            }
                            Some(Ok(StreamEvent::ToolCallStart { id, name })) => {
                                // Reset: if the model generated text before tool calls (plan text),
                                // it doesn't count as a final summary. Only text AFTER all tools = summary.
                                self.model_produced_text = false;
                                self.conversation.tool_call_buffer = Some(ToolCallBuffer {
                                    id,
                                    name: name.clone(),
                                    arguments: String::new(),
                                });
                                self.phase = AgentPhase::CallingTool(name.clone());
                                let _ = self.event_tx.send(AgentEvent::PhaseChange(
                                    AgentPhase::CallingTool(name),
                                ));
                            }
                            Some(Ok(StreamEvent::ToolCallDelta(args))) => {
                                if let Some(ref mut buf) = self.conversation.tool_call_buffer {
                                    buf.arguments.push_str(&args);
                                    let partial = &buf.arguments;
                                    let arg_size = partial.len();

                                    // Extract file_path or command for display
                                    let target = if let Some(fp_start) = partial.find("\"file_path\"") {
                                        if let Some(val_start) = partial[fp_start..].find(":\"").or_else(|| partial[fp_start..].find(": \"")) {
                                            let s = fp_start + val_start;
                                            let after_colon = partial[s..].find('"').map(|p| s + p + 1);
                                            if let Some(start) = after_colon {
                                                if let Some(end) = partial[start..].find('"') {
                                                    let fp = &partial[start..start + end];
                                                    Some(std::path::Path::new(fp)
                                                        .file_name()
                                                        .map(|n| n.to_string_lossy().to_string())
                                                        .unwrap_or_else(|| fp.to_string()))
                                                } else { None }
                                            } else { None }
                                        } else { None }
                                    } else if let Some(cmd_start) = partial.find("\"command\"") {
                                        if let Some(val_start) = partial[cmd_start..].find(":\"").or_else(|| partial[cmd_start..].find(": \"")) {
                                            let s = cmd_start + val_start;
                                            let after_colon = partial[s..].find('"').map(|p| s + p + 1);
                                            if let Some(start) = after_colon {
                                                let end = partial[start..].find('"').unwrap_or(partial.len() - start).min(50);
                                                Some(partial[start..start + end].to_string())
                                            } else { None }
                                        } else { None }
                                    } else { None };

                                    // Always update label with size — shows live progress during large writes
                                    let size_str = if arg_size > 1024 {
                                        format!(" ({:.1}KB)", arg_size as f64 / 1024.0)
                                    } else if arg_size > 100 {
                                        format!(" ({}B)", arg_size)
                                    } else {
                                        String::new()
                                    };
                                    let label = if let Some(ref t) = target {
                                        format!("{}: {}{}", buf.name, t, size_str)
                                    } else if !size_str.is_empty() {
                                        format!("{}{}", buf.name, size_str)
                                    } else {
                                        buf.name.clone()
                                    };
                                    let _ = self.event_tx.send(AgentEvent::PhaseChange(
                                        AgentPhase::CallingTool(label),
                                    ));
                                }
                            }
                            Some(Ok(StreamEvent::ToolCallDone(mut call))) => {
                                self.conversation.tool_call_buffer = None;
                                // Repair malformed JSON arguments from weak models
                                if serde_json::from_str::<serde_json::Value>(&call.arguments).is_err() {
                                    call.arguments = repair_json(&call.arguments);
                                }
                                tool_calls_buf.push(call);
                            }
                            Some(Ok(StreamEvent::Usage(usage))) => {
                                self.turn_tokens += usage.completion_tokens;
                                self.total_tokens += usage.completion_tokens;
                                let _ = self.event_tx.send(AgentEvent::TokenUsage(usage));
                            }
                            Some(Ok(StreamEvent::Done)) => {
                                self.conversation.finalize_stream();

                                if !tool_calls_buf.is_empty() {
                                    self.conversation
                                        .finalize_stream_with_tool_calls(&tool_calls_buf);
                                    self.pending_tool_calls = tool_calls_buf;
                                    self.dispatch_pending_tools().await;
                                } else {
                                    // Verification: inject ONCE if edits were made but not verified.
                                    if !self.verify_injected && self.should_verify() {
                                        self.verify_injected = true;
                                        self.inject_verify_prompt();
                                        self.call_llm().await;
                                    } else {
                                        self.finish_turn();
                                    }
                                }
                                return;
                            }
                            Some(Ok(StreamEvent::Error(e))) => {
                                let is_messages_illegal = e.contains("illegal") || e.contains("messages");
                                let is_rate_limited = e.contains("429") || e.contains("rate") || e.contains("Too Many");
                                let is_auth_error = e.contains("401 ") || e.contains("403 ");
                                let is_fatal_api_error = (e.contains("400 ") || is_auth_error)
                                    && !is_rate_limited; // 429 is NOT fatal

                                if is_messages_illegal && self.retry_count == 0 {
                                    // "messages illegal" — auto-recover by trimming conversation
                                    self.retry_count += 1;
                                    let len = self.conversation.messages.len();
                                    if len > 4 {
                                        self.conversation.messages.truncate(len - 4);
                                    }
                                    let _ = self.event_tx.send(AgentEvent::TextDelta(
                                        "\n[Recovering from API error — retrying with reduced context...]\n".to_string()
                                    ));
                                    self.call_llm().await;
                                    return;
                                } else if is_rate_limited && self.retry_count < 5 {
                                    // 429 rate limit — back off and retry (up to 5 times).
                                    self.retry_count += 1;
                                    let wait = (self.retry_count as u64 * 3).min(30);
                                    let _ = self.event_tx.send(AgentEvent::TextDelta(
                                        format!("\n[Rate limited — retrying in {}s...]\n", wait)
                                    ));
                                    tokio::time::sleep(Duration::from_secs(wait)).await;
                                    self.call_llm().await;
                                    return;
                                } else if !is_fatal_api_error && !is_rate_limited {
                                    // Transient error (network, timeout) — retry up to 3 times.
                                    self.retry_count += 1;
                                    if self.retry_count <= 3 {
                                        let wait = (self.retry_count as u64 * 2).min(15);
                                        tokio::time::sleep(Duration::from_secs(wait)).await;
                                        self.call_llm().await;
                                        return;
                                    }
                                    let _ = self.event_tx.send(AgentEvent::Error(e));
                                    self.finish_turn();
                                    return;
                                } else {
                                    // Fatal: 400 Bad Request, 401/403 Auth error — no retry.
                                    let _ = self.event_tx.send(AgentEvent::Error(e));
                                    self.finish_turn();
                                    return;
                                }
                            }
                            Some(Err(e)) => {
                                let _ = self.event_tx.send(AgentEvent::Error(e.to_string()));
                                self.finish_turn();
                                return;
                            }
                            None => {
                                self.conversation.finalize_stream();
                                self.finish_turn();
                                return;
                            }
                        }
                    }
                }
            }
        })
    }

    /// Decide whether to execute pending tool calls in parallel or sequentially.
    /// Parallel execution is used when there are 2+ calls AND all are auto-approved.
    fn dispatch_pending_tools(&mut self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            if self.pending_tool_calls.len() <= 1 {
                self.process_next_tool_call().await;
                return;
            }

            // Check if ALL pending calls can be auto-approved
            let all_auto = self.pending_tool_calls.iter().all(|call| {
                self.tool_registry.get(&call.name)
                    .map(|tool| {
                        let approval = tool.approval(&call.arguments);
                        matches!(self.permission_store.check(&call.name, &approval), PermissionDecision::Allow)
                    })
                    .unwrap_or(false)
            });

            if !all_auto {
                // Some need approval — fall back to sequential
                self.process_next_tool_call().await;
                return;
            }

            self.execute_tools_parallel().await;
        })
    }

    /// Execute all pending tool calls concurrently, then process results sequentially.
    fn execute_tools_parallel(&mut self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let calls = std::mem::take(&mut self.pending_tool_calls);
            let start = Instant::now();

            // Phase 1: Pre-process — resolve args, get tool refs, send start events
            let mut prepared: Vec<(ToolCall, String, std::sync::Arc<dyn crate::tool::Tool>)> = Vec::new();
            for call in &calls {
                let name = call.name.clone();
                let args = self.resolve_args(call);

                let tool = match self.tool_registry.get_arc(&call.name) {
                    Some(t) => t,
                    None => {
                        // Unknown tool — put everything back and go sequential
                        self.pending_tool_calls = calls;
                        self.process_next_tool_call().await;
                        return;
                    }
                };

                // Send start event for each tool
                let _ = self.event_tx.send(AgentEvent::ToolCallStarted {
                    name: name.clone(),
                    arguments: args.clone(),
                });

                // Track file access state
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&args) {
                    let file = parsed.get("file_path").and_then(|v| v.as_str())
                        .map(|s| short_path(s));
                    match name.as_str() {
                        "read_file" | "list_directory" | "glob" | "grep" => {
                            self.consecutive_reads += 1;
                            if let Some(f) = file {
                                if !self.files_read_this_turn.contains(&f) {
                                    self.files_read_this_turn.push(f);
                                }
                            }
                        }
                        "edit_file" | "write_file" => {
                            self.consecutive_reads = 0;
                            if let Some(f) = file {
                                if !self.files_edited_this_turn.contains(&f) {
                                    self.files_edited_this_turn.push(f);
                                }
                            }
                        }
                        _ => {}
                    }
                }

                prepared.push((call.clone(), args, tool));
            }

            // Phase 2: Execute all tools concurrently
            let ctx = self.tool_context.clone();
            let cancel = self.cancel_token.clone();

            let handles: Vec<_> = prepared.iter().map(|(call, args, tool)| {
                let args = args.clone();
                let ctx = ctx.clone();
                let tool = std::sync::Arc::clone(tool);
                let call_id = call.id.clone();
                tokio::spawn(async move {
                    let t = Instant::now();
                    let result = tool.execute(&args, &ctx).await;
                    (call_id, result, t.elapsed())
                })
            }).collect();

            let raw_results = tokio::select! {
                _ = cancel.cancelled() => {
                    self.finish_turn();
                    return;
                }
                r = futures::future::join_all(handles) => r,
            };

            let _total_duration = start.elapsed();

            // Phase 3: Convert raw results into ToolResults and queue them.
            // They'll be processed one-by-one through handle_tool_result which
            // includes ALL post-processing: system reminders, step limits,
            // bash misuse detection, file re-read warnings, etc.
            let mut completed: Vec<(ToolResult, String)> = Vec::new();
            for (i, join_result) in raw_results.into_iter().enumerate() {
                let (call, _args, _) = &prepared[i];
                let name = call.name.clone();

                let (mut tool_result, duration) = match join_result {
                    Ok((call_id, Ok(mut r), dur)) => {
                        r.call_id = call_id;
                        (r, dur)
                    }
                    Ok((call_id, Err(e), dur)) => {
                        (ToolResult {
                            call_id,
                            output: format!("Error: {}", e),
                            success: false,
                        }, dur)
                    }
                    Err(e) => {
                        (ToolResult {
                            call_id: call.id.clone(),
                            output: format!("Task panicked: {}", e),
                            success: false,
                        }, Duration::ZERO)
                    }
                };

                // Append per-tool duration
                let dur_str = if duration.as_millis() < 1000 {
                    format!(" ({}ms)", duration.as_millis())
                } else {
                    format!(" ({:.1}s)", duration.as_secs_f64())
                };
                tool_result.output.push_str(&dur_str);

                // Send result event
                let _ = self.event_tx.send(AgentEvent::ToolCallResult {
                    name: name.clone(),
                    output: tool_result.output.clone(),
                    success: tool_result.success,
                    duration,
                });

                completed.push((tool_result, name));
            }

            // Phase 4: Feed results through handle_tool_result sequentially.
            // All but the last go directly into conversation; the last one triggers
            // handle_tool_result which continues the agent loop (calls call_llm).
            let last = completed.pop();
            for (result, tool_name) in completed {
                // Inline the essential bookkeeping (truncate + count + add).
                // We skip handle_tool_result's continuation logic since there are
                // more results to process. System reminders fire via handle_tool_result
                // on the final result.
                let mut r = result;
                self.truncate_output(&mut r, &tool_name);
                self.tool_call_count += 1;
                self.store_tool_result(r);
            }
            if let Some((final_result, _final_name)) = last {
                // The last result goes through full handle_tool_result which
                // includes system reminders, step limits, and continuation.
                self.handle_tool_result(final_result).await;
            } else {
                // No results (shouldn't happen) — continue the loop
                self.phase = AgentPhase::Thinking;
                let _ = self.event_tx.send(AgentEvent::PhaseChange(AgentPhase::Thinking));
                self.call_llm().await;
            }
        })
    }

    fn process_next_tool_call(&mut self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            if let Some(call) = self.pending_tool_calls.first().cloned() {
                if let Some(tool) = self.tool_registry.get(&call.name) {
                    let approval = tool.approval(&call.arguments);
                    match self.permission_store.check(&call.name, &approval) {
                        PermissionDecision::Allow => {
                            self.pending_tool_calls.remove(0);
                            self.execute_tool(call).await;
                        }
                        PermissionDecision::Ask(reason) => {
                            self.pending_tool_calls.remove(0); // Remove BEFORE storing as pending
                            self.pending_approval = Some(call);
                            self.phase = AgentPhase::WaitingApproval;
                            let _ = self.event_tx.send(AgentEvent::ApprovalNeeded {
                                tool_name: self
                                    .pending_approval
                                    .as_ref()
                                    .unwrap()
                                    .name
                                    .clone(),
                                reason,
                                call: self.pending_approval.as_ref().unwrap().clone(),
                            });
                        }
                        PermissionDecision::Deny => {
                            self.pending_tool_calls.remove(0);
                            let result = ToolResult {
                                call_id: call.id,
                                output: "Permission denied".to_string(),
                                success: false,
                            };
                            self.handle_tool_result(result).await;
                        }
                    }
                } else {
                    // Unknown tool — return an error result and continue.
                    self.pending_tool_calls.remove(0);
                    let result = ToolResult {
                        call_id: call.id.clone(),
                        output: format!("Unknown tool: {}", call.name),
                        success: false,
                    };
                    self.handle_tool_result(result).await;
                }
            }
        })
    }

    fn execute_tool(&mut self, call: ToolCall) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let start = Instant::now();
            let name = call.name.clone();
            let mut args = self.resolve_args(&call);

            // Auto-extend bash timeout for install/setup/build commands
            if name == "bash" {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&args) {
                    if parsed.get("timeout").is_none() {
                        if let Some(cmd) = parsed.get("command").and_then(|v| v.as_str()) {
                            let cmd_lower = cmd.to_lowercase();
                            // Tech-stack-agnostic: detect install/build BEHAVIOR, not specific tools.
                            // Any command with "install" or "setup" keyword is likely slow.
                            let is_install = cmd_lower.contains(" install")
                                || cmd_lower.contains("-setup")
                                || cmd_lower.contains("build --release");
                            // Compound restart: kill+sleep+start pattern (any stack)
                            let is_compound_restart = (cmd_lower.contains("pkill") || cmd_lower.contains("kill"))
                                && (cmd_lower.contains("sleep") || cmd_lower.contains("curl"));
                            if is_install {
                                if let Ok(mut obj) = serde_json::from_str::<serde_json::Value>(&args) {
                                    obj["timeout"] = serde_json::json!(180);
                                    if let Ok(new_args) = serde_json::to_string(&obj) {
                                        args = new_args;
                                    }
                                }
                            } else if is_compound_restart {
                                if let Ok(mut obj) = serde_json::from_str::<serde_json::Value>(&args) {
                                    obj["timeout"] = serde_json::json!(60);
                                    if let Ok(new_args) = serde_json::to_string(&obj) {
                                        args = new_args;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Track files read/edited and consecutive read count
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&args) {
                let file = parsed.get("file_path").and_then(|v| v.as_str())
                    .map(|s| short_path(s));
                match name.as_str() {
                    "read_file" | "list_directory" | "glob" | "grep" => {
                        self.consecutive_reads += 1;
                        if let Some(ref f) = file {
                            if !self.files_read_this_turn.contains(f) {
                                self.files_read_this_turn.push(f.clone());
                            }
                            // Track per-file read count
                            if name == "read_file" {
                                *self.file_read_counts.entry(f.clone()).or_insert(0) += 1;
                            }
                        }
                    }
                    "edit_file" | "write_file" => {
                        self.consecutive_reads = 0; // Reset on edit action
                        if let Some(f) = file {
                            if !self.files_edited_this_turn.contains(&f) {
                                self.files_edited_this_turn.push(f);
                            }
                        }
                    }
                    "bash" => {
                        // Don't reset consecutive_reads here — let the bash handler decide
                        // based on whether the command is file-reading or an action.
                        // The bash handler increments for grep/sed/cat and the
                        // read budget check handles the rest.
                    }
                    _ => {}
                }
            }

            // --- Intercept redundant tool calls ---
            if let Some(intercepted) = self.intercept_redundant_call(&name, &args) {
                // Count how many consecutive INTERCEPTED calls (same tool + same args hash).
                // Different edits on the same file are NOT a loop — don't count them.
                let args_hash = {
                    use std::hash::{Hash, Hasher};
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    args.hash(&mut h);
                    h.finish()
                };
                let block_count = self.recent_calls.iter()
                    .rev()
                    .take_while(|s| s.0 == name && s.1 == args_hash)
                    .count();

                let _ = self.event_tx.send(AgentEvent::ToolCallStarted {
                    name: name.clone(),
                    arguments: args.clone(),
                });

                if block_count >= 4 {
                    // FORCE END TURN — model is stuck in an unbreakable loop.
                    // Use a friendly message if work was actually completed.
                    let has_work = !self.files_edited_this_turn.is_empty();
                    let msg = if has_work {
                        "Loop in cleanup step stopped. Your changes were applied successfully."
                    } else {
                        "Agent stuck in a loop. Turn force-terminated. Please try a more specific request."
                    };

                    let _ = self.event_tx.send(AgentEvent::ToolCallResult {
                        name: name.clone(),
                        output: format!("[{}]", msg),
                        success: has_work,
                        duration: start.elapsed(),
                    });
                    if !has_work {
                        let _ = self.event_tx.send(AgentEvent::Error(msg.to_string()));
                    }
                    // Force-stop: generate a fallback summary since we can't call LLM again
                    if !self.model_produced_text {
                        let files = self.files_edited_this_turn.join(", ");
                        let summary = if files.is_empty() {
                            format!("Task stopped (repeated failed command). No files were modified.")
                        } else {
                            format!("Task stopped due to a verification error. Files modified: {}. \
                                     The final verification step failed — please check manually.", files)
                        };
                        self.conversation.push_delta(&summary);
                        let _ = self.event_tx.send(AgentEvent::TextDelta(summary));
                    }
                    self.conversation.finalize_stream();
                    self.finish_turn();
                    return;
                }

                let result = ToolResult {
                    call_id: call.id,
                    output: intercepted,
                    success: false,
                };
                let _ = self.event_tx.send(AgentEvent::ToolCallResult {
                    name: name.clone(),
                    output: result.output.clone(),
                    success: false,
                    duration: start.elapsed(),
                });
                // Intercepted calls do NOT count toward step limit —
                // they accomplished nothing and shouldn't eat the model's budget.
                // Feed back directly without incrementing tool_call_count.
                self.current_tool_name = name;
                self.store_tool_result(result);
                // Continue the agent loop without counting this as a step.
                self.call_llm().await;
                return;
            }

            let _ = self.event_tx.send(AgentEvent::ToolCallStarted {
                name: name.clone(),
                arguments: args.clone(),
            });
            let _ = self.event_tx.send(AgentEvent::PhaseChange(
                AgentPhase::CallingTool(name.clone()),
            ));

            let ctx = self.tool_context.clone();
            let cancel = self.cancel_token.clone();

            let tool = match self.tool_registry.get_arc(&call.name) {
                Some(t) => t,
                None => {
                    let result = ToolResult {
                        call_id: call.id,
                        output: format!("Unknown tool: {}", name),
                        success: false,
                    };
                    self.handle_tool_result(result).await;
                    return;
                }
            };

            let result = tokio::select! {
                _ = cancel.cancelled() => {
                    self.finish_turn();
                    return;
                },
                cmd = self.cmd_rx.recv() => {
                    match cmd {
                        Some(AgentCommand::Cancel) | None => {
                            self.cancel_token.cancel();
                            self.finish_turn();
                            return;
                        }
                        Some(AgentCommand::Shutdown) => return,
                        _ => {
                            // Non-cancel command during tool execution — ignore.
                            // Wait for the tool to finish normally.
                            tool.execute(&args, &ctx).await
                        }
                    }
                },
                r = tool.execute(&args, &ctx) => r,
            };

            let duration = start.elapsed();
            let mut tool_result = match result {
                Ok(mut r) => {
                    r.call_id = call.id;
                    r
                }
                Err(e) => {
                    let err_str = e.to_string();
                    // If it's a JSON parse error, give the model the correct format
                    let output = if err_str.contains("expected") || err_str.contains("missing field") || err_str.contains("invalid type") {
                        let example = match name.as_str() {
                            "list_directory" => r#"{"path": "src", "depth": 2}"#,
                            "read_file" => r#"{"file_path": "/absolute/path/to/file"}"#,
                            "edit_file" => r#"{"file_path": "/path", "old_string": "old", "new_string": "new"}"#,
                            "write_file" => r#"{"file_path": "/path", "content": "file content"}"#,
                            "grep" => r#"{"pattern": "search_term", "path": "src"}"#,
                            "bash" => r#"{"command": "ls -la"}"#,
                            "glob" => r#"{"pattern": "**/*.vue"}"#,
                            _ => "{}",
                        };
                        format!(
                            "Error: Invalid JSON arguments. {}\n\
                             Correct format for {}: {}",
                            err_str, name, example
                        )
                    } else {
                        format!("Error: {}", err_str)
                    };
                    ToolResult {
                        call_id: call.id,
                        output,
                        success: false,
                    }
                },
            };

            // Append execution duration to output.
            let dur_str = if duration.as_millis() < 1000 {
                format!(" ({}ms)", duration.as_millis())
            } else {
                format!(" ({:.1}s)", duration.as_secs_f64())
            };
            tool_result.output.push_str(&dur_str);

            // ── Re-read reminder for read_file ──
            // Block excessive FULL re-reads of the same file.
            // Offset/limit reads are allowed (model is narrowing focus for precise edits).
            // Only block full re-reads (no offset) on 3rd+ attempt.
            if name == "read_file" {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&args) {
                    if let Some(fp) = parsed.get("file_path").and_then(|v| v.as_str()) {
                        let has_offset = parsed.get("offset").is_some() || parsed.get("limit").is_some();
                        if !has_offset {
                            let short = short_path(fp);
                            let count = self.file_read_counts.get(&short).copied().unwrap_or(0);
                            if count >= 2 {
                                tool_result.output = format!(
                                    "[BLOCKED: You already read {} {} times. The content is in your conversation. \
                                     Make your edit NOW. If you need a specific section, use offset/limit.]",
                                    short, count
                                );
                                tool_result.success = false;
                            } else if count == 1 {
                                tool_result.output.push_str(
                                    "\n\n[WARNING: You already read this file. Next full read will be blocked. \
                                     Use offset/limit if you need a specific section, or make your edit NOW.]"
                                );
                            }
                        }
                    }
                }
            }

            // Detect and warn about bash misuse patterns.
            if name == "bash" {
                let cmd = serde_json::from_str::<serde_json::Value>(&args)
                    .ok()
                    .and_then(|v| v.get("command").and_then(|c| c.as_str()).map(String::from))
                    .unwrap_or_default();

                // ── Service URL discovery ──
                // Scan tool output for http://localhost:PORT patterns.
                // Store them so the model always knows the actual running ports.
                Self::extract_service_urls(&tool_result.output, &cmd, &mut self.active_services);
                let cmd_lower = cmd.to_lowercase();
                let cmd_start = cmd.split_whitespace().next().unwrap_or("");

                // Pattern 1: Using bash to read files
                let is_file_read_cmd = matches!(cmd_start, "grep" | "sed" | "cat" | "head" | "tail" | "awk" | "wc");

                // Block bash-as-read: cat/head/tail/sed/awk should ALWAYS use
                // dedicated tools (read_file, grep). Don't just warn — inject a
                // hard redirect so the model learns to use the right tool.
                // grep is allowed in bash since it sometimes needs piping/flags
                // that the grep tool doesn't support.
                if is_file_read_cmd && cmd_start != "grep" {
                    tool_result.output.push_str(
                        "\n\n[SYSTEM: Do NOT use bash for reading files. \
                         Use read_file to read files, grep to search contents. \
                         Bash is only for: builds, tests, git, server commands.]"
                    );
                }

                // Also catch piped patterns: "cat X | grep Y" should be "grep Y X"
                if cmd.contains("| grep") || cmd.contains("|grep") {
                    tool_result.output.push_str(
                        "\n\n[SYSTEM: Use the grep tool instead of piped bash commands. \
                         grep tool supports pattern matching and is more efficient.]"
                    );
                }

                // ── Sleep loop detection ──
                // Detect "sleep N && check" polling patterns. After 2 occurrences,
                // hard-block further sleeps.
                if cmd_lower.starts_with("sleep ") || cmd_lower.contains("&& sleep ") || cmd_lower.contains("; sleep ") {
                    self.sleep_count += 1;
                    if self.sleep_count >= 3 {
                        tool_result.output.push_str(
                            "\n\n[SYSTEM: STOP. You have used sleep-and-check 3+ times this turn. \
                             This is a polling anti-pattern. Instead: \
                             1) For brew/npm install: run the command ONCE, it will complete when done. \
                             2) For server startup: use nohup, then wait a reasonable time (one sleep 10-15), then curl to verify. \
                             3) If a command hasn't finished, it may be downloading dependencies — that's normal. \
                             Do NOT keep sleeping and checking. Take a different action or tell the user to wait.]"
                        );
                    } else if self.sleep_count >= 2 {
                        tool_result.output.push_str(
                            "\n\n[SYSTEM: Warning: you've used sleep-and-check twice. \
                             Avoid polling loops. If the previous command is still running, wait once more then move on.]"
                        );
                    }
                }

                // ── Repeated command detection (tech-stack agnostic) ──
                // Normalize the command (strip env vars, redirects, timestamps) and
                // check if it was already executed this turn. Warns on 2nd, blocks on 3rd+.
                let cmd_key = Self::normalize_bash_cmd(&cmd);
                if !cmd_key.is_empty() {
                    let count = self.executed_cmds.entry(cmd_key.clone()).or_insert(0);
                    *count += 1;
                    if *count >= 3 {
                        tool_result.output.push_str(&format!(
                            "\n\n[SYSTEM: STOP. You have run this same command {} times this turn: '{}'. \
                             Repeating the same command will not produce a different result. \
                             Read the error output, diagnose the root cause, and try a DIFFERENT approach.]",
                            count, cmd_key
                        ));
                    } else if *count == 2 {
                        tool_result.output.push_str(&format!(
                            "\n\n[SYSTEM: Warning: you already ran '{}' earlier this turn. \
                             If it failed before, re-running the same command is unlikely to help. \
                             Analyze the previous error and try a different approach.]",
                            cmd_key
                        ));
                    }
                }

                // ── Over-verification detection ──
                // Detect consecutive "check-only" commands: --version, list, status,
                // which, ls, ps. These don't change state — if the previous action
                // succeeded, further verification is wasted.
                let is_verify_cmd = cmd_lower.contains("--version")
                    || cmd_lower.contains("version")
                    || cmd_lower.contains(" list")
                    || cmd_lower.contains(" status")
                    || cmd_start == "which"
                    || cmd_start == "ls"
                    || cmd_start == "ps";
                if is_verify_cmd && !is_file_read_cmd {
                    self.consecutive_verify_count += 1;
                    if self.consecutive_verify_count >= 3 {
                        tool_result.output.push_str(
                            "\n\n[SYSTEM: You have run 3+ consecutive verification commands \
                             (version/list/status/which/ls). One verification is enough. \
                             If the previous action succeeded, move on to the next step or \
                             respond to the user with the result.]"
                        );
                    }
                } else if !is_verify_cmd {
                    self.consecutive_verify_count = 0;
                }

                if is_file_read_cmd {
                    self.consecutive_reads += 1;
                    // Diagnosis timeout: if 3+ consecutive read/grep/bash-read without ANY edit,
                    // inject a prompt forcing the model to act instead of spinning.
                    if self.consecutive_reads >= 3 && self.files_edited_this_turn.is_empty() {
                        tool_result.output.push_str(
                            "\n\n[SYSTEM: You have spent 3+ turns reading/searching without making any edit. \
                             STOP investigating and make your best fix NOW based on what you already know. \
                             If you're unsure, fix the most likely cause. You can always iterate after.]"
                        );
                    }
                    if self.consecutive_reads >= 3 && !self.files_edited_this_turn.is_empty() {
                        // Model is lost — auto-attach the last edited file's content
                        let last_edited = self.files_edited_this_turn.last().cloned();
                        if let Some(ref short_name) = last_edited {
                            // Find the full path from recent tool calls
                            let full_path = self.conversation.messages.iter().rev()
                                .filter_map(|m| {
                                    if let crate::conversation::message::MessageContent::AssistantWithToolCalls { tool_calls, .. } = &m.content {
                                        for tc in tool_calls {
                                            if tc.name == "edit_file" || tc.name == "write_file" {
                                                if let Ok(a) = serde_json::from_str::<serde_json::Value>(&tc.arguments) {
                                                    if let Some(fp) = a.get("file_path").and_then(|v| v.as_str()) {
                                                        return Some(fp.to_string());
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    None
                                })
                                .next();

                            if let Some(fp) = full_path {
                                if let Ok(content) = std::fs::read_to_string(&fp) {
                                    let lines: Vec<&str> = content.lines().collect();
                                    let show = lines.len().min(200);
                                    let preview: String = lines[..show].iter().enumerate()
                                        .map(|(i, l)| format!("{:>4}| {}", i + 1, l))
                                        .collect::<Vec<_>>()
                                        .join("\n");
                                    tool_result.output.push_str(&format!(
                                        "\n\n[SYSTEM: You keep using bash to navigate {}. Here is the current file content. \
                                         Use this to make your next edit_file call directly:]\n{}",
                                        short_name, preview
                                    ));
                                }
                            }
                        }
                    } else {
                        tool_result.output.push_str(
                            "\n\n[SYSTEM: Use read_file or grep tool instead of bash for reading files.]"
                        );
                    }
                }

                // Non-read bash commands (build, install, restart) reset the read counter
                if !is_file_read_cmd {
                    self.consecutive_reads = 0;
                }

                // Pattern 2: Scouting commands
                let is_scouting = matches!(cmd_start, "ps" | "lsof" | "netstat" | "curl" | "wget")
                    || (cmd_start == "tail" && cmd.contains("log"))
                    || cmd_start == "kill" || cmd_start == "pkill";
                let task_is_runtime = {
                    let t = self.current_task.to_lowercase();
                    t.contains("启动") || t.contains("运行") || t.contains("访问")
                        || t.contains("不能用") || t.contains("不工作") || t.contains("不好使")
                        || t.contains("失败") || t.contains("报错") || t.contains("拒绝")
                        || t.contains("加载") || t.contains("显示") || t.contains("骨架")
                        || t.contains("loading") || t.contains("blank") || t.contains("empty")
                        || t.contains("crash") || t.contains("broken") || t.contains("not work")
                        || t.contains("start") || t.contains("run") || t.contains("deploy")
                        || t.contains("搞定") || t.contains("修一下") || t.contains("fix")
                };
                if is_scouting {
                    self.scouting_count += 1;
                }
                if is_scouting && self.tool_call_count <= 3 && !task_is_runtime {
                    tool_result.output.push_str(
                        "\n\n[SYSTEM: You are scouting (checking processes/ports/APIs) instead of fixing the code. \
                         STOP scouting. Read the relevant source file and edit it directly.]"
                    );
                }
                // Scouting budget: even for runtime tasks, cap scouting at 6 commands
                if is_scouting && self.scouting_count >= 6 && self.files_edited_this_turn.is_empty() {
                    tool_result.output.push_str(
                        "\n\n[SYSTEM: SCOUTING BUDGET EXCEEDED. You have run 6+ diagnostic commands \
                         without editing any code. STOP running curl/lsof/ps/kill. \
                         Read the source code NOW and fix the issue. \
                         If the backend API works (returned data), the problem is in the FRONTEND.]"
                    );
                }

                // Pattern 2b: API/server confirmed working — detect curl returning 200 or valid data
                if (cmd_start == "curl" || cmd_start == "wget") && tool_result.success {
                    // Check for HTTP status code responses (e.g., curl -w "%{http_code}" → "200")
                    let is_http_200 = {
                        let out = &tool_result.output;
                        let first_line = out.trim().split('\n').next().unwrap_or("").trim();
                        first_line == "200" || first_line.starts_with("200 ") || out.contains("200 OK")
                    };
                    if is_http_200 {
                        self.api_confirmed_working = true;
                        tool_result.output.push_str(
                            "\n\n[SYSTEM: Server returned HTTP 200 — it is running. \
                             You can now respond to the user. No need for additional checks (tail, logs, etc).]"
                        );
                    }
                    // Use actual JSON parsing to confirm valid API response.
                    let trimmed = tool_result.output.trim().split("\n\n[").next().unwrap_or("").trim().to_string();
                    let is_valid_json = serde_json::from_str::<serde_json::Value>(&trimmed).is_ok();
                    let looks_like_error = tool_result.output.contains("Connection refused")
                        || tool_result.output.contains("Could not resolve")
                        || tool_result.output.contains("timed out");
                    if is_valid_json && !looks_like_error {
                        self.api_confirmed_working = true;
                        // If user's task mentions UI/display/loading issues, redirect to frontend
                        let task_l = self.current_task.to_lowercase();
                        let task_is_frontend = task_l.contains("加载") || task_l.contains("骨架")
                            || task_l.contains("显示") || task_l.contains("loading")
                            || task_l.contains("blank") || task_l.contains("页面")
                            || task_l.contains("前端") || task_l.contains("frontend");
                        if task_is_frontend {
                            tool_result.output.push_str(
                                "\n\n[SYSTEM: The backend API returned valid data — it is WORKING. \
                                 The user reported a FRONTEND display issue (loading/blank/skeleton). \
                                 STOP diagnosing the backend. Read the FRONTEND source code \
                                 (Vue/React components, API client, route handlers) and fix the display logic.]"
                            );
                        }
                    }
                }

                // Pattern 3: Build/compile failure tracking — escalating intervention
                let is_build = cmd_lower.contains("build") || cmd_lower.contains("compile")
                    || cmd_lower.contains("check") || cmd_lower.contains("tsc")
                    || (cmd_lower.contains("run") && (cmd_lower.contains("build") || cmd_lower.contains("check")));
                let is_restart = cmd_lower.contains("kill") || cmd_lower.contains("restart")
                    || cmd_lower.contains("run dev") || cmd_lower.contains("run serve");

                if is_build {
                    if !tool_result.success {
                        self.build_fail_count += 1;
                        if self.build_fail_count >= 2 {
                            tool_result.output.push_str(
                                "\n\n[SYSTEM: BUILD FAILED AGAIN. You have failed the build multiple times. \
                                 DO NOT run build again until you have:\n\
                                 1. Read the COMPLETE error output above\n\
                                 2. Identified ALL errors (not just the first one)\n\
                                 3. Fixed ALL of them in one pass\n\
                                 The fix-one-build-fix-one-build pattern wastes steps. Fix everything, THEN build once.]"
                            );
                        }
                    } else {
                        self.build_fail_count = 0; // Reset on success
                        // Build passed — but for loading/spinner issues, build passing
                        // does NOT mean the problem is fixed. Remind to check runtime.
                        let task_l = self.current_task.to_lowercase();
                        let is_runtime_issue = task_l.contains("转") || task_l.contains("加载")
                            || task_l.contains("空白") || task_l.contains("不显示")
                            || task_l.contains("loading") || task_l.contains("spinner")
                            || task_l.contains("blank") || task_l.contains("empty");
                        if is_runtime_issue {
                            tool_result.output.push_str(
                                "\n\n[SYSTEM: Build passed, but the user reported a RUNTIME issue (loading/blank/spinner). \
                                 Build passing only means no compile errors — the page may still not work. \
                                 You MUST trace the data flow: \
                                 1. Is the API call URL correct? Does the frontend call the right endpoint? \
                                 2. Does the response format match what the frontend expects? (field names, nesting) \
                                 3. Is there error handling that silently swallows failures? \
                                 4. Use curl to see the actual API response, then compare with the frontend's type definition. \
                                 Do NOT claim 'fixed' just because build passed.]"
                            );
                        }
                    }
                }

                if !tool_result.success && !is_file_read_cmd && !is_scouting && (is_restart || is_build) {
                    let recent_bash_fails = self.conversation.messages.iter().rev()
                        .take(self.tool_call_count * 2 + 2)
                        .filter(|m| {
                            if let (Some(false), Some(out)) = (m.tool_result_success(), m.tool_result_output()) {
                                out.contains("Error") || out.contains("error") || out.contains("failed")
                            } else { false }
                        })
                        .count();

                    if recent_bash_fails >= 2 {
                        tool_result.output.push_str(
                            "\n\n[SYSTEM: STOP the restart loop. You have had multiple failures. \
                             Read the FULL error log (tail -50, not tail -10), identify ALL issues, \
                             fix ALL of them in one pass, then restart ONCE. \
                             Do not fix-one-restart-fix-one-restart.]"
                        );
                    }
                }

            }

            // Clean tool results — no injected noise. Behavioral guidance
            // lives in the system prompt only, matching Claude Code's architecture.

            // After successful edit/write, reset read count for that file so the model
            // can re-read the updated version. Without this, the BLOCK on re-reads
            // prevents the model from seeing its own changes, causing cascading
            // edit failures when old_string no longer matches.
            if (name == "edit_file" || name == "write_file") && tool_result.success {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&args) {
                    if let Some(fp) = parsed.get("file_path").and_then(|v| v.as_str()) {
                        let short = short_path(fp);
                        self.file_read_counts.remove(&short);
                        // Cache file content for injection into next LLM call.
                        // This puts the latest file in the model's highest-attention zone.
                        if let Ok(content) = std::fs::read_to_string(fp) {
                            let lines = content.lines().count();
                            if lines <= 600 {
                                // Only cache manageable files (≤600 lines ≈ 2400 tokens)
                                self.recent_file_cache.insert(short, (fp.to_string(), content));
                            }
                        }
                    }
                }
            }

            // No sibling hints or syntax check reminders injected here.
            // System prompt handles verification guidance.

            let _ = self.event_tx.send(AgentEvent::ToolCallResult {
                name: name.clone(),
                output: tool_result.output.clone(),
                success: tool_result.success,
                duration,
            });

            self.current_tool_name = name;
            self.handle_tool_result(tool_result).await;
        })
    }

    fn handle_tool_result(
        &mut self,
        mut result: ToolResult,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            // Smart per-tool truncation.
            let tool_name = self.current_tool_name.clone();
            self.truncate_output(&mut result, &tool_name);

            self.tool_call_count += 1;

            // System reminders: re-inject rules + task every 4 steps.
            // This is the #1 technique Claude Code uses to keep weak models on track.
            if self.tool_call_count > 0 && self.tool_call_count.is_multiple_of(4) {
                let task_hint = if self.current_task.chars().count() > 100 {
                    format!("{}...", self.current_task.chars().take(97).collect::<String>())
                } else {
                    self.current_task.clone()
                };

                // Check if we already have successful edits — if so, maybe we're done
                let _has_edits = self.conversation.messages.iter().rev()
                    .take(self.tool_call_count * 2 + 2)
                    .any(|m| {
                        if let (Some(true), Some(out)) = (m.tool_result_success(), m.tool_result_output()) {
                            out.contains("Edited ") || out.contains("Wrote ")
                        } else {
                            false
                        }
                    });

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

                let _unedited: Vec<&String> = self.files_read_this_turn.iter()
                    .filter(|f| !self.files_edited_this_turn.contains(f))
                    .collect();

                // Detect "backend works but model keeps restarting" pattern
                let api_confirmed_ok = self.conversation.messages.iter().rev()
                    .take(self.tool_call_count * 2 + 2)
                    .any(|m| {
                        if let (Some(true), Some(out)) = (m.tool_result_success(), m.tool_result_output()) {
                            out.contains("200 OK") || out.contains("\"success\":true") || out.contains("success: True")
                        } else { false }
                    });
                let many_bash_restarts = self.conversation.messages.iter().rev()
                    .take(self.tool_call_count * 2 + 2)
                    .filter(|m| {
                        if let crate::conversation::message::MessageContent::AssistantWithToolCalls { tool_calls, .. } = &m.content {
                            tool_calls.iter().any(|tc| tc.name == "bash" && (tc.arguments.contains("kill") || tc.arguments.contains("pkill") || tc.arguments.contains("restart")))
                        } else { false }
                    })
                    .count() >= 2;

                let urgency = if api_confirmed_ok && many_bash_restarts && self.tool_call_count >= 6 {
                    "STOP: The backend API is working (returned 200 OK). The problem is likely in the FRONTEND code. \
                     Read the frontend file and check: imports, API call methods, response handling."
                } else if self.tool_call_count >= 15 {
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

                result.output.push_str(&format!(
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
                ));
            }

            // Dynamic step limit: base 25, +5 for each unique file edited.
            // A task that edits 5 files gets 50 steps. A task that reads 25 files
            // without editing anything gets stopped at 25.
            let dynamic_limit = 35 + (self.files_edited_this_turn.len() * 5);
            let hard_limit = dynamic_limit.min(60); // absolute max 50

            if self.tool_call_count >= hard_limit {
                result.output.push_str(&format!(
                    "\n\n[SYSTEM: Step limit ({}) reached. Turn terminated.]",
                    hard_limit
                ));
                self.store_tool_result(result.clone());

                // Check if the last action failed — don't blindly say "Done"
                let last_failed = !result.success;
                let has_edits = !self.files_edited_this_turn.is_empty();

                if last_failed && has_edits {
                    let warning = format!(
                        "Stopped at step limit ({}). Files modified: {}. \
                         However, the last action failed — the changes may not be fully working. \
                         Please check manually.",
                        hard_limit, self.files_edited_this_turn.join(", ")
                    );
                    // Only emit TextDelta — TurnComplete handler will add to conversation via finalize_stream
                    self.conversation.push_delta(&warning);
                    let _ = self.event_tx.send(AgentEvent::TextDelta(warning));
                } else if !has_edits {
                    let warning = format!(
                        "Stopped at step limit ({}) without completing any edits. \
                         Please try a more specific request.",
                        hard_limit
                    );
                    self.conversation.push_delta(&warning);
                    let _ = self.event_tx.send(AgentEvent::TextDelta(warning));
                }
                self.conversation.finalize_stream();
                self.finish_turn();
                return;
            }

            self.store_tool_result(result);

            // Process remaining pending tool calls, or continue the agent loop.
            if !self.pending_tool_calls.is_empty() {
                self.dispatch_pending_tools().await;
            } else {
                self.phase = AgentPhase::Thinking;
                let _ = self
                    .event_tx
                    .send(AgentEvent::PhaseChange(AgentPhase::Thinking));
                self.call_llm().await;
            }
        })
    }

    // -------------------------------------------------------------------------
    // Helper methods
    // -------------------------------------------------------------------------

    /// Find sibling files (same directory, same extension) of edited files
    /// and suggest the model check them for the same bug pattern.
    fn find_sibling_files_hint(&self) -> String {
        let wd = self.tool_context.working_dir
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

        let wd = self
            .tool_context
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
            "Working directory: {wd}\n{env_info}\n",
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

    fn resolve_args(&self, call: &ToolCall) -> String {
        let wd = self
            .tool_context
            .working_dir
            .try_read()
            .map(|g| g.clone())
            .unwrap_or_default();

        // Try parsing JSON directly, then repair, then specialized extractors
        let args_str = &call.arguments;
        let parsed = serde_json::from_str::<serde_json::Value>(args_str)
            .or_else(|_| serde_json::from_str::<serde_json::Value>(&repair_json(args_str)))
            .or_else(|_| {
                // For edit_file: specialized parser that handles unescaped source code
                if call.name == "edit_file" {
                    if let Some(v) = extract_edit_file_args(args_str) {
                        return Ok(v);
                    }
                }
                Ok::<serde_json::Value, serde_json::Error>(extract_json_fields(args_str))
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
                    if self.consecutive_edits_count >= 4 {
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
                let wd = self.tool_context.working_dir.try_read().ok()?;
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
    /// Recent messages (last N) get their full content loaded from disk;
    /// older refs keep their summary (they're in the cold zone anyway).
    fn inflate_recent_refs(&self, messages: &mut Vec<crate::conversation::message::Message>) {
        // Inflate the last 20 tool-result messages (roughly the hot zone).
        let mut inflated = 0usize;
        const MAX_INFLATE: usize = 20;
        for msg in messages.iter_mut().rev() {
            if inflated >= MAX_INFLATE {
                break;
            }
            if let crate::conversation::message::MessageContent::ToolResultRef(ref r) = msg.content {
                let full = self.result_store.inflate(r);
                msg.content = crate::conversation::message::MessageContent::ToolResult(full);
                inflated += 1;
            }
        }
    }

    /// Add a tool result to the conversation, externalizing large outputs to disk.
    /// Results smaller than the threshold are stored inline for simplicity.
    fn store_tool_result(&mut self, result: ToolResult) {
        const EXTERNALIZE_THRESHOLD: usize = 512;
        if result.output.len() >= EXTERNALIZE_THRESHOLD {
            let result_ref = self.result_store.store(&result);
            self.conversation.add_tool_result_ref(result_ref);
        } else {
            self.conversation.add_tool_result(result);
        }
    }

    /// Smart truncation: applies per-tool strategies to keep the most useful
    /// parts of tool output while staying within token budget.
    fn truncate_output(&self, result: &mut ToolResult, tool_name: &str) {
        match tool_name {
            "bash" => self.truncate_bash(result),
            "read_file" => self.truncate_read_file(result),
            "web_fetch" => self.truncate_generic(result, 150, 20, 40),
            _ => self.truncate_generic(result, 200, 30, 50),
        }
        // Hard char limit as a safety net.
        if result.output.len() > 12000 {
            result.output = result.output.chars().take(12000).collect::<String>()
                + "\n[output truncated at 12000 chars]";
        }
    }

    /// Bash: preserve error lines, strip verbose build noise.
    /// Errors are the highest-value signal — keep all lines containing "error",
    /// "Error", "FAILED", "STDERR", "panic", plus surrounding context.
    fn truncate_bash(&self, result: &mut ToolResult) {
        let lines: Vec<&str> = result.output.lines().collect();
        if lines.len() <= 80 {
            return; // Short enough — keep everything.
        }

        // Phase 1: Identify error/important lines.
        // Generic error patterns — no language-specific strings.
        let error_patterns = ["error", "Error", "ERROR", "FAILED", "STDERR:",
            "panic", "Panic", "PANIC", "not found", "No such file",
            "Permission denied", "cannot find", "undefined", "unresolved"];
        let mut important: Vec<bool> = vec![false; lines.len()];

        for (i, line) in lines.iter().enumerate() {
            if error_patterns.iter().any(|p| line.contains(p)) {
                // Mark this line and 2 lines of context above/below.
                let start = i.saturating_sub(2);
                let end = (i + 3).min(lines.len());
                for j in start..end {
                    important[j] = true;
                }
            }
        }

        // Phase 2: Always keep head (first 10 lines) and tail (last 20 lines).
        const HEAD: usize = 10;
        const TAIL: usize = 20;
        for i in 0..HEAD.min(lines.len()) {
            important[i] = true;
        }
        for i in lines.len().saturating_sub(TAIL)..lines.len() {
            important[i] = true;
        }

        // Phase 3: Assemble, collapsing unimportant runs into "[N lines skipped]".
        let mut output = String::with_capacity(result.output.len() / 2);
        let mut skipping = false;
        let mut skip_count = 0usize;

        for (i, line) in lines.iter().enumerate() {
            if important[i] {
                if skipping {
                    output.push_str(&format!("\n[... {} lines skipped ...]\n", skip_count));
                    skipping = false;
                    skip_count = 0;
                }
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(line);
            } else {
                skipping = true;
                skip_count += 1;
            }
        }
        if skipping {
            output.push_str(&format!("\n[... {} lines skipped ...]", skip_count));
        }

        result.output = output;
    }

    /// read_file: if output is very long, extract an outline — keep top-level
    /// declarations (lines at indent level 0-1) plus head/tail for orientation.
    /// Tech-stack agnostic: uses indentation depth as a universal proxy for
    /// "important structural line" — works across all languages.
    ///
    /// Threshold is 2000 lines. Truncating forces multi-read/multi-edit cycles
    /// that waste far more tokens than keeping the full file.
    /// At 32K context window, 2000 lines ≈ 8000 tokens = 25% of budget.
    /// Files over 2000 lines are extremely rare in practice.
    fn truncate_read_file(&self, result: &mut ToolResult) {
        let lines: Vec<&str> = result.output.lines().collect();
        if lines.len() <= 2000 {
            return;
        }

        // Always keep first 30 and last 20 lines (file header/imports + end).
        const HEAD: usize = 30;
        const TAIL: usize = 20;

        let mut important: Vec<bool> = vec![false; lines.len()];

        // Head and tail.
        for i in 0..HEAD.min(lines.len()) {
            important[i] = true;
        }
        for i in lines.len().saturating_sub(TAIL)..lines.len() {
            important[i] = true;
        }

        // Top-level lines in the middle: detect by indentation depth.
        // read_file output has line-number prefix: "  123| content"
        // Extract content after "| " and check its indent level.
        for (i, line) in lines.iter().enumerate() {
            // Extract the actual code content after the line-number prefix.
            let content = if let Some(pos) = line.find("| ") {
                &line[pos + 2..]
            } else {
                line
            };

            // Skip empty/whitespace-only lines.
            if content.trim().is_empty() {
                continue;
            }

            // Count leading whitespace (spaces or tabs).
            let indent = content.len() - content.trim_start().len();
            // Indent 0-1 = top-level declaration (function, class, struct, etc.)
            // across virtually all languages.
            if indent <= 1 && content.trim().len() > 2 {
                important[i] = true;
                // Include the line below (often opening brace, docstring, or type annotation).
                if i + 1 < lines.len() {
                    important[i + 1] = true;
                }
            }
        }

        // Assemble with skip markers.
        let mut output = String::with_capacity(result.output.len() / 2);
        let mut skipping = false;
        let mut skip_count = 0usize;

        for (i, line) in lines.iter().enumerate() {
            if important[i] {
                if skipping {
                    output.push_str(&format!("\n[... {} lines skipped ...]\n", skip_count));
                    skipping = false;
                    skip_count = 0;
                }
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(line);
            } else {
                skipping = true;
                skip_count += 1;
            }
        }
        if skipping {
            output.push_str(&format!("\n[... {} lines skipped ...]", skip_count));
        }

        result.output = output;
    }

    /// Log the complete LLM request (messages + tools + metadata) to
    /// `~/.atomcode/logs/YYYY-MM-DD_HH-MM-SS_NNN.json`.
    /// This is fire-and-forget — logging failures are silently ignored.
    fn log_llm_request(
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

    /// Generic truncation: head + tail, skipping middle.
    fn truncate_generic(&self, result: &mut ToolResult, max_lines: usize, head: usize, tail: usize) {
        let lines: Vec<&str> = result.output.lines().collect();
        if lines.len() > max_lines {
            let head_part: String = lines[..head].join("\n");
            let tail_part: String = lines[lines.len() - tail..].join("\n");
            result.output = format!(
                "{}\n\n[... {} lines omitted ...]\n\n{}",
                head_part,
                lines.len() - head - tail,
                tail_part
            );
        }
    }

    fn change_dir(&mut self, path: &str) {
        let new_path = if path.starts_with('/') {
            std::path::PathBuf::from(path)
        } else if path.starts_with('~') {
            dirs::home_dir()
                .map(|h| h.join(path.strip_prefix("~/").unwrap_or(&path[1..])))
                .unwrap_or_else(|| std::path::PathBuf::from(path))
        } else {
            let wd = self
                .tool_context
                .working_dir
                .try_read()
                .map(|g| g.clone())
                .unwrap_or_default();
            wd.join(path)
        };

        let resolved = std::fs::canonicalize(&new_path).unwrap_or(new_path);
        if resolved.is_dir() {
            if let Ok(mut wd) = self.tool_context.working_dir.try_write() {
                *wd = resolved.clone();
            }
            self.project_context_cache = None; // invalidate on dir change
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

/// Last-resort: extract ALL key-value pairs from malformed JSON by string matching.
/// Tool-agnostic — no hardcoded field lists. Finds any `"key": "value"` or `key: value` pattern.
fn extract_json_fields(s: &str) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    let chars: Vec<char> = s.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Find a key: either "key" or bare_key followed by :
        let key = if chars[i] == '"' {
            // Quoted key
            let start = i + 1;
            i = start;
            while i < len && chars[i] != '"' { i += 1; }
            if i >= len { break; }
            let k: String = chars[start..i].iter().collect();
            i += 1; // skip closing "
            k
        } else if chars[i].is_alphabetic() || chars[i] == '_' {
            // Bare key
            let start = i;
            while i < len && (chars[i].is_alphanumeric() || chars[i] == '_') { i += 1; }
            chars[start..i].iter().collect()
        } else {
            i += 1;
            continue;
        };

        // Skip whitespace, expect :
        while i < len && chars[i].is_whitespace() { i += 1; }
        if i >= len || chars[i] != ':' { continue; }
        i += 1; // skip :
        while i < len && chars[i].is_whitespace() { i += 1; }
        if i >= len { break; }

        // Read value
        if chars[i] == '"' {
            // String value — extract and unescape JSON escape sequences
            let start = i + 1;
            i = start;
            while i < len && chars[i] != '"' {
                if chars[i] == '\\' { i += 1; }
                i += 1;
            }
            let raw: String = chars[start..i.min(len)].iter().collect();
            // Unescape JSON sequences: \n → newline, \t → tab, \" → quote, \\ → backslash
            let val = raw.replace("\\n", "\n")
                         .replace("\\t", "\t")
                         .replace("\\\"", "\"")
                         .replace("\\\\", "\\");
            map.insert(key, serde_json::json!(val));
            if i < len { i += 1; }
        } else if chars[i] == 't' || chars[i] == 'f' {
            // Boolean
            let start = i;
            while i < len && chars[i].is_alphabetic() { i += 1; }
            let word: String = chars[start..i].iter().collect();
            match word.as_str() {
                "true" => { map.insert(key, serde_json::json!(true)); }
                "false" => { map.insert(key, serde_json::json!(false)); }
                _ => { map.insert(key, serde_json::json!(word)); }
            }
        } else if chars[i].is_ascii_digit() || chars[i] == '-' {
            // Number
            let start = i;
            while i < len && (chars[i].is_ascii_digit() || chars[i] == '.' || chars[i] == '-') { i += 1; }
            let num_str: String = chars[start..i].iter().collect();
            if let Ok(n) = num_str.parse::<i64>() {
                map.insert(key, serde_json::json!(n));
            } else if let Ok(f) = num_str.parse::<f64>() {
                map.insert(key, serde_json::json!(f));
            }
        } else {
            // Unquoted string value — read until , } ]
            let start = i;
            while i < len && !matches!(chars[i], ',' | '}' | ']' | '\n') { i += 1; }
            let val: String = chars[start..i].iter().collect::<String>().trim().to_string();
            if !val.is_empty() {
                map.insert(key, serde_json::json!(val));
            }
        }
    }

    serde_json::Value::Object(map)
}

/// Specialized parser for edit_file arguments when JSON parsing fails.
/// Models often generate old_string/new_string with unescaped quotes/newlines.
/// This parser uses the known field order to extract content by position.
fn extract_edit_file_args(raw: &str) -> Option<serde_json::Value> {
    let fp_marker = raw.find("\"file_path\"")?;
    let old_marker = raw.find("\"old_string\"")?;
    let new_marker = raw.find("\"new_string\"")?;
    if old_marker <= fp_marker || new_marker <= old_marker { return None; }

    // Extract file_path (simple quoted string before old_string)
    let fp_region = &raw[fp_marker + 11..old_marker];
    let fp_colon = fp_region.find(':')?;
    let fp_val = fp_region[fp_colon + 1..].trim().trim_matches(|c| c == '"' || c == ',').trim();
    if fp_val.is_empty() { return None; }
    let file_path = fp_val.to_string();

    // Extract old_string: everything between "old_string": " and ", "new_string"
    let old_colon = raw[old_marker..].find(':')?;
    let old_start = old_marker + old_colon + 1;
    let old_raw = &raw[old_start..new_marker];
    let old_string = unescape_field_value(old_raw);

    // Extract new_string: everything after "new_string": " to the end
    let new_colon = raw[new_marker..].find(':')?;
    let new_start = new_marker + new_colon + 1;
    let new_raw = &raw[new_start..];
    let new_string = unescape_field_value_end(new_raw);

    if old_string.is_empty() && new_string.is_empty() { return None; }

    let replace_all = raw.contains("\"replace_all\"")
        && raw.rfind("true").map_or(false, |t| {
            raw.rfind("\"replace_all\"").map_or(false, |r| t > r)
        });

    Some(serde_json::json!({
        "file_path": file_path,
        "old_string": old_string,
        "new_string": new_string,
        "replace_all": replace_all,
    }))
}

fn unescape_field_value(raw: &str) -> String {
    let t = raw.trim().trim_end_matches(',').trim();
    let inner = if t.starts_with('"') { &t[1..] } else { t };
    let inner = inner.trim_end_matches('"');
    inner.replace("\\n", "\n").replace("\\t", "\t").replace("\\\"", "\"").replace("\\\\", "\\")
}

fn unescape_field_value_end(raw: &str) -> String {
    let t = raw.trim();
    let inner = if t.starts_with('"') { &t[1..] } else { t };
    // Remove trailing "} or ", "replace_all": ... }
    let end = inner.rfind("\", \"replace_all\"")
        .or_else(|| inner.rfind("\"}"))
        .or_else(|| inner.rfind("\"\n}"))
        .unwrap_or(inner.len());
    let content = &inner[..end];
    content.replace("\\n", "\n").replace("\\t", "\t").replace("\\\"", "\"").replace("\\\\", "\\")
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

/// Attempt to repair common JSON issues from LLM output:
/// - Trailing commas before } or ]
/// - Single quotes instead of double quotes (outside of string values)
/// - Missing closing braces
/// - Unescaped newlines in strings
fn repair_json(s: &str) -> String {
    let mut result = s.to_string();

    // Fix invalid JSON backslash escapes: \. \( \) \| \w \d \s \+ \* etc.
    // JSON only allows: \\ \" \/ \n \r \t \b \f \uXXXX
    // Models often write regex like @app\.(get|post) which has \. — invalid in JSON.
    // Fix by doubling the backslash: \. → \\. so JSON parses it as literal backslash + dot.
    let valid_escapes = ['\\', '"', '/', 'n', 'r', 't', 'b', 'f', 'u'];
    let chars: Vec<char> = result.chars().collect();
    let mut fixed = String::with_capacity(result.len() + 20);
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() {
            let next = chars[i + 1];
            if valid_escapes.contains(&next) {
                // Valid JSON escape — keep as-is
                fixed.push('\\');
                fixed.push(next);
                i += 2;
            } else {
                // Invalid JSON escape (like \. \( \| \w \d \s \+ \*)
                // Double the backslash so JSON parser sees \\ followed by the char
                fixed.push('\\');
                fixed.push('\\');
                fixed.push(next);
                i += 2;
            }
        } else {
            fixed.push(chars[i]);
            i += 1;
        }
    }
    result = fixed;

    // Remove leading/trailing whitespace and any markdown code fences
    result = result.trim().to_string();
    if result.starts_with("```json") {
        result = result.strip_prefix("```json").unwrap_or(&result).to_string();
    }
    if result.starts_with("```") {
        result = result.strip_prefix("```").unwrap_or(&result).to_string();
    }
    if result.ends_with("```") {
        result = result.strip_suffix("```").unwrap_or(&result).to_string();
    }
    result = result.trim().to_string();

    // Replace single quotes with double quotes for keys/values
    // Be careful not to break strings containing apostrophes
    // Simple heuristic: replace ' at JSON structural positions
    if !result.contains('"') && result.contains('\'') {
        result = result.replace('\'', "\"");
    }

    // Fix missing commas between key-value pairs: }" " → }", "
    // Pattern: value followed by whitespace then another key
    // e.g., {"path": "src" "depth": 2} → {"path": "src", "depth": 2}
    let mut chars: Vec<char> = result.chars().collect();
    let mut insertions = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        // Look for pattern: " <whitespace> " where the second " starts a key
        if chars[i] == '"' {
            let j = i + 1;
            // Skip whitespace
            let mut k = j;
            while k < chars.len() && chars[k].is_whitespace() { k += 1; }
            // If next non-whitespace is " and it looks like a key (followed by :), insert comma
            if k < chars.len() && chars[k] == '"' && k > j {
                // Check if this looks like key: find the closing " then :
                let mut q = k + 1;
                while q < chars.len() && chars[q] != '"' { q += 1; }
                if q + 1 < chars.len() {
                    let mut r = q + 1;
                    while r < chars.len() && chars[r].is_whitespace() { r += 1; }
                    if r < chars.len() && chars[r] == ':' {
                        // This is a missing comma: insert after position i
                        insertions.push(j);
                    }
                }
            }
        }
        i += 1;
    }
    // Insert commas in reverse order to preserve indices
    for pos in insertions.into_iter().rev() {
        chars.insert(pos, ',');
    }
    result = chars.into_iter().collect();

    // Fix unquoted keys: {path: "src"} → {"path": "src"}
    // Simple approach: find patterns like {key: or ,key: and add quotes
    let mut fixed = String::with_capacity(result.len() + 20);
    let rchars: Vec<char> = result.chars().collect();
    let mut ri = 0;
    while ri < rchars.len() {
        if rchars[ri] == '{' || rchars[ri] == ',' {
            fixed.push(rchars[ri]);
            ri += 1;
            // Skip whitespace
            while ri < rchars.len() && rchars[ri].is_whitespace() {
                fixed.push(rchars[ri]);
                ri += 1;
            }
            // Check if next is an unquoted key (alphanumeric/underscore followed by :)
            if ri < rchars.len() && rchars[ri].is_alphanumeric() {
                let key_start = ri;
                while ri < rchars.len() && (rchars[ri].is_alphanumeric() || rchars[ri] == '_') {
                    ri += 1;
                }
                // Skip whitespace after key
                let mut ki = ri;
                while ki < rchars.len() && rchars[ki].is_whitespace() { ki += 1; }
                if ki < rchars.len() && rchars[ki] == ':' {
                    // Unquoted key — add quotes
                    fixed.push('"');
                    for c in &rchars[key_start..ri] { fixed.push(*c); }
                    fixed.push('"');
                } else {
                    // Not a key, just copy
                    for c in &rchars[key_start..ri] { fixed.push(*c); }
                }
            }
        } else {
            fixed.push(rchars[ri]);
            ri += 1;
        }
    }
    result = fixed;

    // Remove trailing commas before } or ]
    loop {
        let before = result.clone();
        result = result.replace(",}", "}").replace(",]", "]");
        if result == before { break; }
    }

    // If it doesn't start with { or [, wrap it
    if !result.starts_with('{') && !result.starts_with('[') {
        result = format!("{{{}}}", result);
    }

    // Count braces and add missing closing ones
    let open_braces = result.chars().filter(|c| *c == '{').count();
    let close_braces = result.chars().filter(|c| *c == '}').count();
    for _ in 0..(open_braces.saturating_sub(close_braces)) {
        result.push('}');
    }

    result
}

/// Detect if a streaming text buffer is repeating earlier content.
/// Returns Some(byte_position) where the repeat starts, None if no repeat detected.
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
