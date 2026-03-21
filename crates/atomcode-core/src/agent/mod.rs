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
use crate::stream::StreamEvent;
use crate::tool::{
    PermissionDecision, PermissionStore, ToolCall, ToolCallBuffer, ToolContext, ToolRegistry,
    ToolResult,
};

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
    /// Change working directory.
    ChangeDir(String),
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
    /// Pre-read file contents injected as system context (not synthetic tool calls).
    preread_context: String,

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
        tool_registry: ToolRegistry,
        tool_context: ToolContext,
        conversation: Conversation,
    ) -> (Self, AgentHandle) {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();

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
            preread_context: String::new(),
            cmd_rx,
            event_tx,
        };

        let handle = AgentHandle { cmd_tx, event_rx };

        (agent, handle)
    }

    /// Run the agent loop. This is the main entry point — call from a tokio task.
    /// The loop processes commands from the UI and emits events back.
    pub async fn run(mut self) {
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
                AgentCommand::ChangeDir(path) => {
                    self.change_dir(&path);
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
        // Pre-read: analyze user's message, find relevant files, and store
        // their contents. They'll be injected as system context (not synthetic tool calls)
        // so the model doesn't learn to "read first, edit later".
        self.preread_context = self.build_preread_context(&content).await;
        self.conversation.add_user_message(&content);
        self.turn_tokens = 0;
        self.tool_call_count = 0;
        self.retry_count = 0;
        self.recent_calls.clear();
        self.files_read_this_turn.clear();
        self.files_edited_this_turn.clear();
        self.consecutive_reads = 0;
        self.verify_injected = false;
        self.model_produced_text = false;
        self.turn_start = Some(Instant::now());
        self.cancel_token = CancellationToken::new();

        self.phase = AgentPhase::Thinking;
        let _ = self
            .event_tx
            .send(AgentEvent::PhaseChange(AgentPhase::Thinking));

        self.call_llm().await;
    }

    /// Analyze the user's message, find relevant files, pre-read them,
    /// and return their contents as a context string to inject into the system prompt.
    /// NOT injected as synthetic tool calls (which teaches the model to read more).
    async fn build_preread_context(&self, content: &str) -> String {
        let wd = self.tool_context.working_dir
            .try_read()
            .map(|g| g.clone())
            .unwrap_or_default();

        let files = collect_project_files(&wd, 0, 3);
        let task_lower = content.to_lowercase();

        let mut scored: Vec<(i32, std::path::PathBuf)> = Vec::new();
        for file_path in &files {
            let filename = file_path.file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_default()
                .to_lowercase();

            if filename.starts_with('.') || filename.ends_with(".log") || filename.ends_with(".lock") {
                continue;
            }

            let mut score = 0i32;
            let name_no_ext = filename.split('.').next().unwrap_or(&filename);

            if name_no_ext.len() > 2 && task_lower.contains(name_no_ext) {
                score += 10;
            }

            let keyword_map: &[(&[&str], &[&str])] = &[
                (&["样式", "style", "css", "圆角", "rounded", "美化", "丑", "布局", "ui", "界面", "美观", "tailwind"],
                 &["css", "style", "app", "layout", "tailwind"]),
                (&["接口", "api", "搜索", "search", "请求"],
                 &["api", "route", "main", "server", "search", "index"]),
                (&["页面", "page", "组件", "component", "视图", "view"],
                 &["vue", "jsx", "tsx", "component"]),
                (&["修复", "fix", "bug", "error", "错误", "报错", "改乱"],
                 &["app", "main", "index", "config"]),
                (&["启动", "start", "运行", "run", "报错", "crash", "崩溃"],
                 &["start", "package", "vite", "config", "main", "app"]),
            ];

            for (task_kws, file_kws) in keyword_map {
                let task_match = task_kws.iter().any(|kw| task_lower.contains(kw));
                let file_match = file_kws.iter().any(|kw| filename.contains(kw));
                if task_match && file_match {
                    score += 8;
                }
            }

            // Boost primary files
            if filename == "app.vue" || filename == "main.css" {
                score += 3;
            }

            if score > 0 {
                scored.push((score, file_path.clone()));
            }
        }

        scored.sort_by(|a, b| b.0.cmp(&a.0));

        if scored.is_empty() {
            return String::new();
        }

        // CRITICAL: When a file matches, expand to include ALL sibling files
        // in the same directory with the same extension. This is why Claude Code
        // fixes all views in one turn — it sees ALL of them, not just the matched one.
        let mut expanded: Vec<std::path::PathBuf> = Vec::new();
        let mut seen_dirs: std::collections::HashSet<String> = std::collections::HashSet::new();

        for (_, path) in &scored {
            expanded.push(path.clone());

            // Expand: include all siblings with same extension
            if let (Some(dir), Some(ext)) = (path.parent(), path.extension()) {
                let dir_key = format!("{}:{}", dir.display(), ext.to_string_lossy());
                if !seen_dirs.contains(&dir_key) {
                    seen_dirs.insert(dir_key);
                    if let Ok(entries) = std::fs::read_dir(dir) {
                        for entry in entries.flatten() {
                            let ep = entry.path();
                            if ep.extension() == Some(ext) && ep != *path && ep.is_file() {
                                if !expanded.contains(&ep) {
                                    expanded.push(ep);
                                }
                            }
                        }
                    }
                }
            }
        }

        // Include files in directories named "api", "lib", "utils", "services" —
        // these typically contain shared interfaces the model needs when rewriting other files.
        // Tech-stack agnostic: detects by directory name, not file extension.
        let interface_dirs = ["api", "lib", "utils", "services", "helpers", "hooks", "stores"];
        for file_path in &files {
            let rel = file_path.strip_prefix(&wd)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            let in_interface_dir = rel.split('/').any(|part| interface_dirs.contains(&part));
            if in_interface_dir && !expanded.contains(file_path) {
                expanded.push(file_path.clone());
            }
        }

        // Build context string with file contents
        let mut ctx = String::from("=== FILES ALREADY LOADED (do NOT re-read these) ===\n");
        let mut total_lines = 0usize;
        const MAX_LINES: usize = 3000; // Match model's context capacity — input tokens cheap, round-trips expensive

        for (idx, path) in expanded.iter().enumerate() {
            if total_lines >= MAX_LINES { break; }

            let file_content = match tokio::fs::read_to_string(path).await {
                Ok(c) => c,
                Err(_) => continue,
            };

            let lines: Vec<&str> = file_content.lines().collect();
            // First 2 files (highest scored) get full content up to 500 lines each.
            // Remaining files share what's left of the budget.
            let per_file_max = if idx < 2 { 500 } else { 200 };
            let take = lines.len().min(per_file_max).min(MAX_LINES - total_lines);
            let rel_path = path.strip_prefix(&wd)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| path.to_string_lossy().to_string());

            ctx.push_str(&format!("\n[{}] ({} lines{})\n",
                rel_path, lines.len(),
                if take >= lines.len() { " - COMPLETE" } else { "" }
            ));
            for (i, line) in lines.iter().take(take).enumerate() {
                ctx.push_str(&format!("{:>4}| {}\n", i + 1, line));
            }
            if take < lines.len() {
                ctx.push_str(&format!("[... {} more lines, use grep to find specific sections]\n", lines.len() - take));
            }

            total_lines += take;
        }

        ctx.push_str("\nYou have these files. Proceed directly to edit_file or write_file. Do NOT call read_file for files shown above.\n");
        ctx
    }

    /// Core agent turn: calls the LLM, processes the stream, and handles tool
    /// calls or finishes the turn. Boxed to allow mutual recursion with
    /// execute_tool / handle_tool_result / process_next_tool_call.
    fn call_llm(&mut self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let system_prompt = self.build_system_prompt();
            let context_window = self
                .config
                .providers
                .get(&self.config.default_provider)
                .map(|p| p.context_window)
                .unwrap_or(16000);
            let messages = self
                .conversation
                .to_provider_messages_budgeted(&system_prompt, context_window);
            let tool_defs = self.tool_registry.get_definitions();

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
                    event = stream.next() => {
                        match event {
                            Some(Ok(StreamEvent::Delta(text))) => {
                                self.model_produced_text = true;
                                self.conversation.push_delta(&text);
                                let _ = self.event_tx.send(AgentEvent::TextDelta(text));
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
                                self.phase = AgentPhase::CallingTool(name);
                            }
                            Some(Ok(StreamEvent::ToolCallDelta(args))) => {
                                if let Some(ref mut buf) = self.conversation.tool_call_buffer {
                                    buf.arguments.push_str(&args);
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
                                    self.process_next_tool_call().await;
                                } else {
                                    // Verification: inject ONCE if edits were made but not verified.
                                    if !self.verify_injected && self.should_verify() {
                                        self.verify_injected = true;
                                        self.inject_verify_prompt();
                                        self.call_llm().await;
                                    } else {
                                        self.maybe_emit_auto_summary();
                                        self.finish_turn();
                                    }
                                }
                                return;
                            }
                            Some(Ok(StreamEvent::Error(e))) => {
                                let is_messages_illegal = e.contains("illegal") || e.contains("messages");
                                let is_auth_error = e.contains("401 ") || e.contains("403 ");
                                let is_api_error = e.contains("API error") || e.contains("400 ") || is_auth_error;

                                if is_messages_illegal && self.retry_count == 0 {
                                    // "messages illegal" — auto-recover by trimming conversation
                                    // and removing potentially corrupted tool call pairs.
                                    self.retry_count += 1;
                                    let len = self.conversation.messages.len();
                                    if len > 4 {
                                        // Remove the last 4 messages (2 tool call/result pairs)
                                        // which likely contain the problematic content
                                        self.conversation.messages.truncate(len - 4);
                                    }
                                    let _ = self.event_tx.send(AgentEvent::TextDelta(
                                        "\n[Recovering from API error — retrying with reduced context...]\n".to_string()
                                    ));
                                    self.call_llm().await;
                                    return;
                                } else if !is_api_error {
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
            let args = self.resolve_args(&call);

            // Track files read/edited and consecutive read count
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
                // Count how many consecutive blocks we've had
                let block_count = self.recent_calls.iter()
                    .rev()
                    .take_while(|s| s.0 == name)
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
                    self.maybe_emit_auto_summary();
                    self.finish_turn();
                    return;
                }

                let result = ToolResult {
                    call_id: call.id,
                    output: intercepted,
                    success: false, // Mark as failure so model treats it seriously
                };
                let _ = self.event_tx.send(AgentEvent::ToolCallResult {
                    name,
                    output: result.output.clone(),
                    success: false,
                    duration: start.elapsed(),
                });
                self.handle_tool_result(result).await;
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
                _ = cancel.cancelled() => return,
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

            // Detect and warn about bash misuse patterns.
            if name == "bash" {
                let cmd = serde_json::from_str::<serde_json::Value>(&args)
                    .ok()
                    .and_then(|v| v.get("command").and_then(|c| c.as_str()).map(String::from))
                    .unwrap_or_default();
                let cmd_start = cmd.split_whitespace().next().unwrap_or("");

                // Pattern 1: Using bash to read files
                let is_file_read_cmd = matches!(cmd_start, "grep" | "sed" | "cat" | "head" | "tail" | "awk" | "wc");
                if is_file_read_cmd {
                    self.consecutive_reads += 1;
                    if self.consecutive_reads >= 3 && !self.files_edited_this_turn.is_empty() {
                        // Model is lost — auto-attach the last edited file's content
                        let last_edited = self.files_edited_this_turn.last().cloned();
                        if let Some(ref short_name) = last_edited {
                            // Find the full path from recent tool calls
                            let full_path = self.conversation.messages.iter().rev()
                                .filter_map(|m| {
                                    if let crate::conversation::message::MessageContent::AssistantWithToolCalls { tool_calls, .. } = &m.content {
                                        for tc in tool_calls {
                                            if (tc.name == "edit_file" || tc.name == "write_file") {
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

                // Pattern 2: Scouting commands — but ONLY warn if user didn't ask about runtime issues
                let is_scouting = matches!(cmd_start, "ps" | "lsof" | "netstat" | "curl" | "wget")
                    || (cmd_start == "tail" && cmd.contains("log"))
                    || (cmd_start == "kill");
                let task_is_runtime = {
                    let t = self.current_task.to_lowercase();
                    t.contains("启动") || t.contains("运行") || t.contains("访问")
                        || t.contains("start") || t.contains("run") || t.contains("deploy")
                        || t.contains("报错") || t.contains("crash") || t.contains("拒绝")
                };
                if is_scouting && self.tool_call_count <= 3 && !task_is_runtime {
                    tool_result.output.push_str(
                        "\n\n[SYSTEM: You are scouting (checking processes/ports/APIs) instead of fixing the code. \
                         STOP scouting. Read the relevant source file and edit it directly.]"
                    );
                }
            }

            // Read budget enforcement: after 3 consecutive reads without an edit, hard redirect.
            if self.consecutive_reads >= 3 && tool_result.success {
                tool_result.output.push_str(
                    "\n\n[SYSTEM: READ BUDGET EXCEEDED. You have read 3+ files without making any edits. \
                     Your next action MUST be edit_file or write_file. \
                     Do NOT call read_file, grep, glob, or list_directory. \
                     If you don't know what to edit, STOP and ask the user.]"
                );
            }
            // Post-read nudge: after every successful read, remind to edit
            else if self.consecutive_reads >= 1 && tool_result.success
                && matches!(name.as_str(), "read_file" | "grep" | "glob")
            {
                tool_result.output.push_str(
                    "\n\n[If you have enough context, call edit_file or write_file NOW. Do not read more files unless necessary.]"
                );
            }

            // Immediate sibling hint: after edit_file succeeds, find similar files
            // that might have the same bug. Don't wait for 4-step system-reminder.
            if (name == "edit_file" || name == "write_file") && tool_result.success {
                if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&args) {
                    if let Some(fp) = parsed.get("file_path").and_then(|v| v.as_str()) {
                        let edited_path = std::path::Path::new(fp);
                        if let (Some(dir), Some(ext)) = (edited_path.parent(), edited_path.extension()) {
                            let mut siblings: Vec<String> = Vec::new();
                            if let Ok(entries) = std::fs::read_dir(dir) {
                                for entry in entries.flatten() {
                                    let ep = entry.path();
                                    if ep.extension() == Some(ext)
                                        && ep != edited_path
                                        && ep.is_file()
                                    {
                                        let name_str = entry.file_name().to_string_lossy().to_string();
                                        siblings.push(name_str);
                                    }
                                }
                            }
                            if !siblings.is_empty() {
                                siblings.truncate(5);
                                tool_result.output.push_str(&format!(
                                    "\n\n[IMPORTANT: You edited {}. \
                                     (1) Run a syntax check NOW before doing anything else. \
                                     (2) These sibling files may need the same change: {}.]",
                                    edited_path.file_name().unwrap_or_default().to_string_lossy(),
                                    siblings.join(", ")
                                ));
                            } else {
                                // No siblings, but still remind to verify
                                tool_result.output.push_str(&format!(
                                    "\n\n[Run a syntax check on {} NOW to catch errors early.]",
                                    edited_path.file_name().unwrap_or_default().to_string_lossy(),
                                ));
                            }
                        }
                    }
                }
            }

            let _ = self.event_tx.send(AgentEvent::ToolCallResult {
                name,
                output: tool_result.output.clone(),
                success: tool_result.success,
                duration,
            });

            self.handle_tool_result(tool_result).await;
        })
    }

    fn handle_tool_result(
        &mut self,
        mut result: ToolResult,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            // Sync working_dir changes propagated by CdTool.
            if let Ok(wd) = self.tool_context.working_dir.try_read() {
                let _ = self.event_tx.send(AgentEvent::WorkingDirChanged(wd.clone()));
            }

            // Smart truncation: keep head + tail so errors (usually at the end) survive.
            self.truncate_output(&mut result);

            self.tool_call_count += 1;

            // System reminders: re-inject rules + task every 4 steps.
            // This is the #1 technique Claude Code uses to keep weak models on track.
            if self.tool_call_count > 0 && self.tool_call_count % 4 == 0 {
                let task_hint = if self.current_task.chars().count() > 100 {
                    format!("{}...", self.current_task.chars().take(97).collect::<String>())
                } else {
                    self.current_task.clone()
                };

                // Check if we already have successful edits — if so, maybe we're done
                let has_edits = self.conversation.messages.iter().rev()
                    .take(self.tool_call_count * 2 + 2)
                    .any(|m| {
                        if let crate::conversation::message::MessageContent::ToolResult(r) = &m.content {
                            r.success && (r.output.contains("Edited ") || r.output.contains("Wrote "))
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

                let unedited: Vec<&String> = self.files_read_this_turn.iter()
                    .filter(|f| !self.files_edited_this_turn.contains(f))
                    .collect();

                let urgency = if self.tool_call_count >= 15 {
                    "URGENT: You MUST take action NOW. Either edit code, restart a service, or explain the issue to the user."
                } else if self.files_edited_this_turn.is_empty() && self.tool_call_count >= 10 {
                    "You have made ZERO edits or fixes after 10+ steps of diagnostics. STOP diagnosing. \
                     Take action NOW: edit code with edit_file, OR restart a service if code was changed but service uses old code, \
                     OR tell the user what you found."
                } else if self.files_edited_this_turn.is_empty() && self.tool_call_count >= 6 {
                    "You have read many files but made no changes. Decide NOW: \
                     Is this a code bug? → edit_file. \
                     Is the service running old code? → restart it. \
                     Can't figure it out? → tell the user what you found."
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
            let dynamic_limit = 25 + (self.files_edited_this_turn.len() * 5);
            let hard_limit = dynamic_limit.min(50); // absolute max 50

            if self.tool_call_count >= hard_limit {
                result.output.push_str(&format!(
                    "\n\n[SYSTEM: Step limit ({}) reached. Turn terminated.]",
                    hard_limit
                ));
                self.conversation.add_tool_result(result);
                self.maybe_emit_auto_summary();
                self.finish_turn();
                return;
            }

            self.conversation.add_tool_result(result);

            // Process remaining pending tool calls, or continue the agent loop.
            if !self.pending_tool_calls.is_empty() {
                self.process_next_tool_call().await;
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

    /// If the turn ended without the model producing a text summary (common with DeepSeek),
    /// auto-generate a brief summary from the tool calls in this turn and emit it as text.
    fn maybe_emit_auto_summary(&mut self) {
        // If the model produced any text output this turn, don't auto-summarize.
        // This is the definitive check — avoids duplicate "done" messages.
        if self.model_produced_text {
            return;
        }

        // Only generate summary if we actually executed tools this turn
        if self.tool_call_count == 0 {
            return;
        }

        // Collect tool operations from this turn only.
        // Limit scan to tool_call_count * 2 + 2 messages from the end (each tool = AssistantWithToolCalls + ToolResult).
        let scan_limit = self.tool_call_count * 2 + 2;
        let mut edits: Vec<String> = Vec::new();
        let mut other_ops: Vec<String> = Vec::new();
        let mut scanned = 0;

        for msg in self.conversation.messages.iter().rev() {
            scanned += 1;
            if scanned > scan_limit { break; }

            match &msg.content {
                crate::conversation::message::MessageContent::AssistantWithToolCalls { tool_calls, .. } => {
                    for tc in tool_calls {
                        let file = extract_file_from_args(&tc.arguments);
                        match tc.name.as_str() {
                            "edit_file" => {
                                if let Some(f) = file {
                                    let short = short_path(&f);
                                    if !edits.contains(&short) {
                                        edits.push(short);
                                    }
                                }
                            }
                            "write_file" => {
                                if let Some(f) = file {
                                    let short = short_path(&f);
                                    if !edits.contains(&short) {
                                        edits.push(short);
                                    }
                                }
                            }
                            "bash" => {
                                if let Some(cmd) = extract_cmd_from_args(&tc.arguments) {
                                    let short = if cmd.len() > 40 {
                                        format!("{}...", cmd.chars().take(37).collect::<String>())
                                    } else {
                                        cmd
                                    };
                                    other_ops.push(format!("ran `{}`", short));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                crate::conversation::message::MessageContent::Text(_) => {
                    // Hit a User message = turn boundary, stop scanning
                    if matches!(msg.role, crate::conversation::message::Role::User) {
                        break;
                    }
                    // Hit an Assistant text after collecting edits = end of this turn's work
                    if !edits.is_empty() || !other_ops.is_empty() {
                        break;
                    }
                }
                _ => {}
            }
        }

        if edits.is_empty() && other_ops.is_empty() {
            return;
        }

        let mut summary = String::from("Done. ");
        if !edits.is_empty() {
            summary.push_str(&format!("Modified: {}", edits.join(", ")));
        }
        if !other_ops.is_empty() {
            if !edits.is_empty() {
                summary.push_str("; ");
            }
            summary.push_str(&other_ops.join("; "));
        }
        summary.push('.');

        // Emit as text delta so it shows up in the chat
        let _ = self.event_tx.send(AgentEvent::TextDelta(summary.clone()));
        // Add to conversation so it persists
        self.conversation.messages.push(
            crate::conversation::message::Message::new(
                crate::conversation::message::Role::Assistant,
                summary,
            )
        );
    }

    /// Find sibling files (same directory, same extension) of edited files
    /// and suggest the model check them for the same bug pattern.
    fn find_sibling_files_hint(&self) -> String {
        let wd = self.tool_context.working_dir
            .try_read()
            .map(|g| g.clone())
            .unwrap_or_default();

        let mut siblings: Vec<String> = Vec::new();
        let mut seen_dirs: std::collections::HashSet<String> = std::collections::HashSet::new();

        for edited in &self.files_edited_this_turn {
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

        // Simple check: find the LAST tool call in this turn.
        // If it's bash → already verified (ran build/test). No need for another verify.
        // If it's edit/write/read → hasn't verified yet.
        for msg in self.conversation.messages.iter().rev() {
            if let crate::conversation::message::MessageContent::AssistantWithToolCalls { tool_calls, .. } = &msg.content {
                if let Some(last_tc) = tool_calls.last() {
                    return last_tc.name != "bash";
                }
            }
            // Stop at user message (turn boundary)
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

        // Analyze user's current task to suggest relevant files
        let file_hints = if !self.current_task.is_empty() {
            self.suggest_files_for_task(&self.current_task.clone(), &wd)
        } else {
            String::new()
        };

        // Load project-level instructions (.atomcode.md or ATOMCODE.md)
        let project_instructions = [".atomcode.md", "ATOMCODE.md"]
            .iter()
            .find_map(|name| {
                let path = wd.join(name);
                std::fs::read_to_string(&path).ok()
            })
            .unwrap_or_default();

        // Inject environment metadata
        let env_info = format!(
            "Platform: {} | Shell: {} | Date: {}",
            std::env::consts::OS,
            std::env::var("SHELL").unwrap_or_else(|_| "unknown".into()),
            {
                std::process::Command::new("date").arg("+%Y-%m-%d").output()
                    .ok().and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|| "unknown".into())
            }
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

        // Assemble prompt: rules → project instructions → env → project context
        let mut prompt = format!(
            "Working directory: {wd}\n{env_info}\n",
            wd = wd.display(), env_info = env_info,
        );

        if !git_info.is_empty() {
            prompt.push_str(&format!("Git: {}\n", git_info));
        }

        prompt.push_str(&format!("\n{rules}\n"));

        if !project_instructions.is_empty() {
            prompt.push_str(&format!(
                "\n=== PROJECT INSTRUCTIONS (.atomcode.md) ===\n{}\n",
                project_instructions
            ));
        }

        prompt.push_str(&format!(
            "\n=== PROJECT CONTEXT (already loaded — do NOT re-read these files) ===\n{project_ctx}"
        ));

        // Inject pre-read file contents (replaces file suggestions with actual content)
        if !self.preread_context.is_empty() {
            prompt.push_str(&format!("\n\n{}", self.preread_context));
        } else if !file_hints.is_empty() {
            prompt.push_str(&format!("\n\n=== SUGGESTED FILES (start here) ===\n{}", file_hints));
        }

        prompt
    }

    /// Analyze the user's task message and the project file tree to suggest
    /// which files are most likely relevant. This reduces the number of exploratory
    /// reads the model needs to do.
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

            // For glob: if pattern contains a relative path prefix, resolve it
            if call.name == "glob" {
                if let Some(pattern) = args.get("pattern").and_then(|v| v.as_str()) {
                    // If pattern starts with a relative directory (e.g., "frontend/src/**/*.vue"),
                    // resolve the directory part against working dir
                    if !pattern.starts_with('/') && pattern.contains('/') {
                        let resolved = wd.join(pattern);
                        args["pattern"] = serde_json::json!(resolved.to_string_lossy().to_string());
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
        // Post-edit guard: if edits have been made and we're past step 8,
        // block new read_file calls for files we haven't read yet.
        // This prevents the "read more files after finishing" pattern.
        if tool_name == "read_file"
            && !self.files_edited_this_turn.is_empty()
            && self.tool_call_count >= 8
        {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(args) {
                if let Some(fp) = parsed.get("file_path").and_then(|v| v.as_str()) {
                    let short = short_path(fp);
                    if !self.files_read_this_turn.contains(&short) {
                        return Some(format!(
                            "[BLOCKED: You have already made edits. Do not read new files ({}). \
                             If the task is complete, stop and summarize. \
                             If you need to edit this file, explain why first.]",
                            short
                        ));
                    }
                }
            }
        }

        // Loop detection: if the same (tool, args) appears 3+ times in recent calls, block it.
        let args_hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            args.hash(&mut h);
            h.finish()
        };
        let sig = (tool_name.to_string(), args_hash);
        self.recent_calls.push(sig.clone());
        if self.recent_calls.len() > 10 {
            self.recent_calls.remove(0);
        }
        let repeat_count = self.recent_calls.iter().filter(|s| **s == sig).count();
        if repeat_count >= 3 {
            return Some(format!(
                "[BLOCKED: You have called {} with the same arguments {} times. \
                 This is a loop. STOP repeating this call and move on to the next step \
                 or tell the user you are stuck.]",
                tool_name, repeat_count
            ));
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

    fn truncate_output(&self, result: &mut ToolResult) {
        const MAX_LINES: usize = 200;
        const HEAD_LINES: usize = 30;
        const TAIL_LINES: usize = 50;

        let lines: Vec<&str> = result.output.lines().collect();
        if lines.len() > MAX_LINES {
            let head: String = lines[..HEAD_LINES].join("\n");
            let tail: String = lines[lines.len() - TAIL_LINES..].join("\n");
            result.output = format!(
                "{}\n\n[... {} lines omitted ...]\n\n{}",
                head,
                lines.len() - HEAD_LINES - TAIL_LINES,
                tail
            );
        }
        if result.output.len() > 10000 {
            result.output = result.output.chars().take(10000).collect::<String>()
                + "\n[output truncated at 10000 chars]";
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
            let _ = self
                .event_tx
                .send(AgentEvent::WorkingDirChanged(resolved));
        }
    }
}

/// Extract file_path from tool call arguments JSON.
fn extract_file_from_args(args: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()?
        .get("file_path")?
        .as_str()
        .map(|s| s.to_string())
}

/// Extract command from bash tool call arguments.
fn extract_cmd_from_args(args: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()?
        .get("command")?
        .as_str()
        .map(|s| s.to_string())
}

/// Shorten a file path for display: keep last 2 components.
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
        if (rchars[ri] == '{' || rchars[ri] == ',') {
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
