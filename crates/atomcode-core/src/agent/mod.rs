//! The AgentLoop — a standalone agent that processes user messages,
//! calls LLM providers, executes tools, and communicates with the UI
//! via channels. Decoupled from any TUI concerns.

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
        self.conversation.add_user_message(&content);
        self.turn_tokens = 0;
        self.tool_call_count = 0;
        self.retry_count = 0;
        self.turn_start = Some(Instant::now());
        self.cancel_token = CancellationToken::new();

        self.phase = AgentPhase::Thinking;
        let _ = self
            .event_tx
            .send(AgentEvent::PhaseChange(AgentPhase::Thinking));

        self.call_llm().await;
    }

    /// Core agent turn: calls the LLM, processes the stream, and handles tool
    /// calls or finishes the turn. Boxed to allow mutual recursion with
    /// execute_tool / handle_tool_result / process_next_tool_call.
    fn call_llm(&mut self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            let system_prompt = self.build_system_prompt();
            let messages = self
                .conversation
                .to_provider_messages_windowed(&system_prompt, 30);
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
                                self.conversation.push_delta(&text);
                                let _ = self.event_tx.send(AgentEvent::TextDelta(text));
                            }
                            Some(Ok(StreamEvent::ToolCallStart { id, name })) => {
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
                            Some(Ok(StreamEvent::ToolCallDone(call))) => {
                                self.conversation.tool_call_buffer = None;
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
                                    self.finish_turn();
                                }
                                return;
                            }
                            Some(Ok(StreamEvent::Error(e))) => {
                                // Retry on transient network errors; fail fast on API errors.
                                let is_api_error = e.contains("API error")
                                    || e.contains("400 ")
                                    || e.contains("401 ")
                                    || e.contains("403 ")
                                    || e.contains("illegal");

                                if !is_api_error {
                                    self.retry_count += 1;
                                    let wait = (self.retry_count as u64 * 2).min(15);
                                    tokio::time::sleep(Duration::from_secs(wait)).await;
                                    // Recursive retry — safe because retry_count is bounded.
                                    self.call_llm().await;
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
                Err(e) => ToolResult {
                    call_id: call.id,
                    output: format!("Error: {}", e),
                    success: false,
                },
            };

            // Append execution duration to output.
            let dur_str = if duration.as_millis() < 1000 {
                format!(" ({}ms)", duration.as_millis())
            } else {
                format!(" ({:.1}s)", duration.as_secs_f64())
            };
            tool_result.output.push_str(&dur_str);

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

            self.conversation.add_tool_result(result);
            self.tool_call_count += 1;

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

    fn build_system_prompt(&self) -> String {
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
            .map(|g| g.display().to_string())
            .unwrap_or_else(|_| "unknown".to_string());

        let project_ctx =
            crate::project_context::build_project_context(&std::path::PathBuf::from(&wd));

        format!(
            "You are AtomCode, a terminal coding agent.\n\nWorking directory: {}\n\n{}\n\n---\nRULES (follow strictly):\n{}",
            wd, project_ctx, rules
        )
    }

    fn resolve_args(&self, call: &ToolCall) -> String {
        let wd = self
            .tool_context
            .working_dir
            .try_read()
            .map(|g| g.clone())
            .unwrap_or_default();

        if let Ok(mut args) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
            if let Some(fp) = args.get("file_path").and_then(|v| v.as_str()) {
                let path = std::path::Path::new(fp);
                if !path.is_absolute() {
                    let resolved = wd.join(path);
                    args["file_path"] =
                        serde_json::json!(resolved.to_string_lossy().to_string());
                }
                return serde_json::to_string(&args).unwrap_or(call.arguments.clone());
            }
        }
        call.arguments.clone()
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
            let _ = self
                .event_tx
                .send(AgentEvent::WorkingDirChanged(resolved));
        }
    }
}
