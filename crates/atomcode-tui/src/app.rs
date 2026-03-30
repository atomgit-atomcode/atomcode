use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc;

use atomcode_core::agent::{AgentCommand, AgentEvent, AgentHandle};
use atomcode_core::config::Config;
use atomcode_core::conversation::Conversation;
use atomcode_core::provider::LlmProvider;
use atomcode_core::tool::{ToolCall, ToolContext, ToolResult};

use base64::Engine as _;

use crate::command::{build_command_list, SlashMenu};
use crate::event::AppEvent;
use crate::provider_manager::{ManagerAction, ProviderManager};

/// App-level text selection state for mouse drag-to-select.
/// When mouse tracking is enabled, the terminal doesn't do native selection,
/// so we implement it ourselves: drag to select, auto-copy to clipboard on mouse-up.
#[derive(Debug, Clone)]
pub struct TextSelection {
    /// Whether a drag is currently in progress.
    pub dragging: bool,
    /// Start position (column, row) in terminal coordinates.
    pub start: (u16, u16),
    /// End position (column, row) in terminal coordinates.
    pub end: (u16, u16),
    /// Whether there is a completed (non-empty) selection to highlight.
    pub has_selection: bool,
}

impl TextSelection {
    pub fn new() -> Self {
        Self {
            dragging: false,
            start: (0, 0),
            end: (0, 0),
            has_selection: false,
        }
    }

    /// Normalize start/end so start <= end in reading order.
    pub fn normalized(&self) -> ((u16, u16), (u16, u16)) {
        if self.start.1 < self.end.1
            || (self.start.1 == self.end.1 && self.start.0 <= self.end.0)
        {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        }
    }
}

/// State for the first-run welcome/setup screen.
#[derive(Debug, Clone)]
pub struct WelcomeState {
    /// Selected option: 0 = Login with AtomGit, 1 = Configure manually, 2 = Skip
    pub selected: usize,
    /// Error message from a failed OAuth attempt.
    pub error: Option<String>,
}

impl WelcomeState {
    pub fn new() -> Self {
        Self { selected: 0, error: None }
    }
}

#[derive(Debug, Clone)]
pub enum AppMode {
    Welcome,
    Normal,
    Streaming,
    WaitingApproval(ToolCall),
    ToolExecuting,
    ProviderManager,
    ModelSelector,
    Exiting,
}

impl AppMode {
    pub fn is_welcome(&self) -> bool { matches!(self, AppMode::Welcome) }
    pub fn is_normal(&self) -> bool { matches!(self, AppMode::Normal) }
    pub fn is_streaming(&self) -> bool { matches!(self, AppMode::Streaming) }
    pub fn is_exiting(&self) -> bool { matches!(self, AppMode::Exiting) }
    pub fn is_provider_manager(&self) -> bool { matches!(self, AppMode::ProviderManager) }
    pub fn is_streaming_or_executing(&self) -> bool {
        matches!(self, AppMode::Streaming | AppMode::ToolExecuting)
    }
}

pub struct App {
    pub mode: AppMode,
    pub conversation: Conversation,
    pub input: InputState,
    pub scroll_offset: usize,
    pub at_bottom: bool,
    pub confirm_quit: bool,
    pub pending_editor: Option<String>,
    /// Flag to trigger OAuth login flow.
    pub pending_login: bool,
    /// Last key event timestamp — for paste detection when bracketed paste isn't available.
    pub last_key_time: Instant,
    /// Files attached to the next message (detected from pasted paths).
    pub attached_files: Vec<crate::file_attach::AttachedFile>,
    pub pasted_text: Option<String>,
    pub slash_menu: SlashMenu,
    pub provider_mgr: Option<ProviderManager>,
    pub welcome_state: WelcomeState,
    /// Name to use when OAuth login is triggered from ProviderManager.
    pub pending_oauth_name: Option<String>,
    /// Model selector: list of (provider_name, model_name), selected index
    pub model_list: Vec<(String, String)>,
    pub model_selected: usize,
    /// Skill registry — drives the slash menu and manual `/skill-name` invocations.
    pub skill_registry: atomcode_core::skill::SkillRegistry,
    /// Pre-built command list (built-ins + skills). Rebuilt only on skill reload.
    pub command_list: Vec<crate::command::CommandEntry>,
    pub tick_count: usize,
    /// Last Ctrl+C timestamp for double-press detection.
    pub last_ctrl_c: Option<Instant>,
    /// When the current turn (user message -> agent loop) started.
    pub turn_start: Option<Instant>,
    /// Time to first token (TTFT) — set when first TextDelta or ToolCallStarted arrives per LLM call.
    pub first_token_ms: Option<u64>,
    /// When the current LLM call started (reset on each Thinking phase).
    pub llm_call_start: Option<Instant>,
    /// Name of the last completed tool (for display: "after read_file, thinking...").
    pub last_completed_tool: String,
    /// When the last tool execution started (for per-step timing).
    pub tool_start: Option<Instant>,
    /// Duration of the last completed turn.
    pub last_turn_duration: Option<Duration>,
    pub working_dir: PathBuf,
    /// Previous working directory for `/cd -` support.
    pub previous_working_dir: Option<PathBuf>,
    /// Recent project directories (most recent first, max 5).
    pub recent_dirs: Vec<PathBuf>,
    /// Directory selector state: Some(selected_index) when picker is open.
    pub dir_selector: Option<usize>,
    /// Shared tool execution context (holds working_dir for cross-thread access).
    pub tool_context: ToolContext,
    /// Input history — past user prompts for Up/Down navigation.
    pub input_history: Vec<String>,
    /// Current position in input history (-1 = not browsing).
    pub history_index: Option<usize>,
    /// Stashed input text when entering history browse mode.
    pub history_stash: Option<String>,
    /// Cached project context (rebuilt on /cd, not every API call).
    pub project_context_cache: Option<String>,
    /// Cached step count for current turn (avoid per-frame O(n) scan).
    pub current_step_count: usize,
    /// Cached tool info string for ToolExecuting mode (avoid per-frame JSON parse).
    pub executing_tool_info: String,
    /// Estimated token counts for the current session.
    pub total_tokens: usize,
    /// Tokens used in the current turn.
    pub turn_tokens: usize,
    /// Suggested next prompt shown as ghost text in the input box.
    pub suggestion: Option<String>,
    /// Cache of rendered lines for completed messages. Invalidated on message count change.
    pub render_cache: Vec<ratatui::text::Line<'static>>,
    pub render_cache_msg_count: usize,
    /// App-level text selection (mouse drag-to-select with auto-copy).
    pub selection: TextSelection,
    /// Actual scroll offset as computed by the last render (for text extraction).
    pub last_rendered_scroll: usize,
    /// Chat panel viewport height from the last render.
    pub last_viewport_height: u16,
    pub provider: Box<dyn LlmProvider>,
    pub config: Config,
    /// CancellationToken for the current streaming/tool task.
    pub cancel_token: tokio_util::sync::CancellationToken,
    /// Channel pair for communicating with the AgentLoop.
    pub agent_handle: AgentHandle,
    /// Display name of the active model (cached so we don't need the provider ref).
    pub model_name: String,
    /// Per-turn logger: writes each turn to datalog/ as a markdown file.
    pub turn_log: crate::turn_log::TurnLog,
}

impl App {
    pub fn new(
        model_name: String,
        config: Config,
        agent_handle: AgentHandle,
        tool_context: ToolContext,
        working_dir: PathBuf,
    ) -> Self {
        // Load history ONLY for input history (up/down arrow), NOT for conversation context.
        // Each session starts fresh — prevents corrupted messages from causing API errors.
        let old_history = Conversation::load(&Conversation::history_path());
        let input_history: Vec<String> = old_history.messages.iter()
            .filter_map(|m| {
                use atomcode_core::conversation::message::{MessageContent, Role};
                if matches!(m.role, Role::User) {
                    if let MessageContent::Text(s) = &m.content {
                        if !s.starts_with('/') { return Some(s.clone()); }
                    }
                }
                None
            })
            .collect();
        // Start with a fresh conversation (like Claude Code)
        let conversation = Conversation::new();
        // Load skills for slash menu and manual invocation
        let mut skill_registry = atomcode_core::skill::SkillRegistry::new();
        skill_registry.reload(&working_dir);
        let command_list = build_command_list(&skill_registry);
        Self {
            mode: if config.providers.is_empty() { AppMode::Welcome } else { AppMode::Normal },
            conversation,
            input: InputState::new(),
            scroll_offset: 0,
            at_bottom: true,
            confirm_quit: false,
            last_key_time: Instant::now(),
            pending_editor: None,
            pending_login: false,
            attached_files: Vec::new(),
            pasted_text: None,
            slash_menu: SlashMenu::new(),
            provider_mgr: None,
            welcome_state: WelcomeState::new(),
            pending_oauth_name: None,
            model_list: Vec::new(),
            model_selected: 0,
            skill_registry,
            command_list,
            last_ctrl_c: None,
            tick_count: 0,
            project_context_cache: None,
            current_step_count: 0,
            executing_tool_info: String::new(),
            turn_start: None,
            first_token_ms: None,
            llm_call_start: None,
            last_completed_tool: String::new(),
            tool_start: None,
            last_turn_duration: None,
            turn_log: crate::turn_log::TurnLog::new(&working_dir),
            tool_context,
            previous_working_dir: None,
            recent_dirs: {
                let mut dirs = load_recent_dirs();
                // Add current dir to recent list on startup
                dirs.retain(|d| d != &working_dir);
                dirs.insert(0, working_dir.clone());
                dirs.truncate(5);
                save_recent_dirs(&dirs);
                dirs
            },
            dir_selector: None,
            working_dir,
            input_history,
            history_index: None,
            history_stash: None,
            total_tokens: 0,
            turn_tokens: 0,
            suggestion: None,
            render_cache: Vec::new(),
            render_cache_msg_count: 0,
            selection: TextSelection::new(),
            last_rendered_scroll: 0,
            last_viewport_height: 0,
            // Keep a dummy provider for rebuild_provider path (legacy). The real LLM
            // work is now handled by AgentLoop. This avoids removing all provider refs at once.
            provider: {
                use atomcode_core::provider::create_provider;
                use atomcode_core::config::provider::ProviderConfig;
                // Create a no-op placeholder; rebuild_provider will set the real one on /provider changes.
                create_provider(&ProviderConfig {
                    provider_type: "openai".to_string(),
                    api_key: Some("placeholder".to_string()),
                    model: model_name.clone(),
                    base_url: Some("http://localhost:1".to_string()),
                    system_prompt: None,
                    user_agent: None,
                    context_window: atomcode_core::config::provider::default_context_window_for("openai"),
                }).unwrap_or_else(|_| {
                    // Fallback: should never reach production path since AgentLoop handles LLM
                    panic!("Failed to create placeholder provider")
                })
            },
            config,
            cancel_token: tokio_util::sync::CancellationToken::new(),
            agent_handle,
            model_name,
        }
    }

    /// Generate a follow-up suggestion based on conversation context.
    /// Language-agnostic: never references specific file extensions or build tools.
    fn generate_suggestion(&self) -> Option<String> {
        use atomcode_core::conversation::message::MessageContent;

        let msgs = &self.conversation.messages;
        if msgs.is_empty() {
            return None;
        }

        let last = &msgs[msgs.len() - 1];

        // Only scan last 20 messages (not entire conversation)
        let recent = if msgs.len() > 20 { &msgs[msgs.len()-20..] } else { msgs.as_slice() };

        let had_write = recent.iter().any(|m| matches!(&m.content,
            MessageContent::AssistantWithToolCalls { tool_calls, .. }
            if tool_calls.iter().any(|c| c.name == "write_file" || c.name == "edit_file")
        ));
        let had_bash = recent.iter().any(|m| matches!(&m.content,
            MessageContent::AssistantWithToolCalls { tool_calls, .. }
            if tool_calls.iter().any(|c| c.name == "bash")
        ));
        let had_error = recent.iter().rev().take(3).any(|m| matches!(&m.content,
            MessageContent::ToolResult(r) if !r.success
        ));

        // Last assistant text for context
        let last_text = last.text().unwrap_or("");
        let last_lower = last_text.to_lowercase();

        // Error happened recently -> suggest fix
        if had_error {
            return Some("Fix the error".to_string());
        }

        // Wrote/edited files -> suggest testing (no language-specific commands)
        if had_write && !had_bash {
            return Some("Run the project to test changes".to_string());
        }

        // Ran a command -> suggest follow-up
        if had_bash && !had_write {
            if last_lower.contains("error") || last_lower.contains("failed") {
                return Some("Fix the issue".to_string());
            }
            if last_lower.contains("test") && last_lower.contains("pass") {
                return Some("Commit the changes".to_string());
            }
        }

        // Wrote files + ran commands successfully -> suggest commit
        if had_write && had_bash && !had_error {
            if last_lower.contains("success") || last_lower.contains("pass") || last_lower.contains("done") {
                return Some("Commit the changes with a descriptive message".to_string());
            }
        }

        // Generic: conversation has content, suggest continue
        if msgs.len() >= 2 {
            return Some("Continue".to_string());
        }

        None
    }

    /// Try to change working directory. Returns (success, message).
    fn try_change_dir(&mut self, path: &str) -> (bool, String) {
        let new_path = if path.starts_with('/') || path.starts_with('~') {
            if path.starts_with('~') {
                dirs::home_dir()
                    .map(|h| h.join(path.strip_prefix("~/").unwrap_or(&path[1..])))
                    .unwrap_or_else(|| PathBuf::from(path))
            } else {
                PathBuf::from(path)
            }
        } else {
            self.working_dir.join(path)
        };

        // Try canonicalize first, fall back to direct path check
        let resolved = std::fs::canonicalize(&new_path)
            .unwrap_or_else(|_| new_path.clone());

        if resolved.is_dir() {
            self.working_dir = resolved.clone();
            self.turn_log.set_working_dir(&resolved);
            // Sync tool context (best-effort; won't block since we're in the main task)
            if let Ok(mut wd) = self.tool_context.working_dir.try_write() {
                *wd = resolved.clone();
            }
            self.project_context_cache = None; // Invalidate project context cache
            self.config.default_workdir = Some(resolved.to_string_lossy().to_string());
            let _ = self.config.save(&Config::default_path());
            (true, format!("Changed working directory to {}", resolved.display()))
        } else if new_path.is_dir() {
            // canonicalize failed but path exists as dir
            self.working_dir = new_path.clone();
            self.turn_log.set_working_dir(&new_path);
            if let Ok(mut wd) = self.tool_context.working_dir.try_write() {
                *wd = new_path.clone();
            }
            self.config.default_workdir = Some(new_path.to_string_lossy().to_string());
            let _ = self.config.save(&Config::default_path());
            (true, format!("Changed working directory to {}", new_path.display()))
        } else {
            (false, format!("Not a directory: {}", new_path.display()))
        }
    }

    /// Change working directory from /cd command.
    /// Clears conversation and rebuilds context — like starting fresh in a new project.
    pub fn change_working_dir(&mut self, path: &str) {
        let old_dir = self.working_dir.display().to_string();
        let (ok, _) = self.try_change_dir(path);
        if ok {
            // Save previous dir for `/cd -`
            self.previous_working_dir = Some(PathBuf::from(&old_dir));
            // Track in recent dirs (max 5, deduplicated)
            let new_dir = self.working_dir.clone();
            self.recent_dirs.retain(|d| d != &new_dir);
            self.recent_dirs.insert(0, new_dir);
            self.recent_dirs.truncate(5);
            save_recent_dirs(&self.recent_dirs);
            // Reload skills for the new project directory
            self.skill_registry.reload(&self.working_dir);
            self.command_list = build_command_list(&self.skill_registry);
            // Clear conversation — new project, fresh context
            self.conversation = atomcode_core::conversation::Conversation::new();
            self.render_cache.clear();
            self.render_cache_msg_count = 0;
            self.scroll_offset = 0;
            self.at_bottom = true;
            // Show new project info
            let new_dir = self.working_dir.display().to_string();
            let tree = crate::project_context::build_project_context(&self.working_dir);
            self.project_context_cache = Some(tree.text.clone());
            let summary = format!(
                "Switched to `{}`\n\n```\n{}\n```",
                new_dir,
                tree.text.lines().take(20).collect::<Vec<_>>().join("\n"),
            );
            self.conversation.push_delta(&summary);
            self.conversation.finalize_stream();
        } else {
            // Directory doesn't exist — create it, git init, and add CLAUDE.md
            let new_path = if path.starts_with('/') || path.starts_with('~') {
                if path.starts_with('~') {
                    dirs::home_dir()
                        .map(|h| h.join(path.strip_prefix("~/").unwrap_or(&path[1..])))
                        .unwrap_or_else(|| PathBuf::from(path))
                } else {
                    PathBuf::from(path)
                }
            } else {
                self.working_dir.join(path)
            };

            if let Err(e) = std::fs::create_dir_all(&new_path) {
                self.conversation.push_delta(&format!("Failed to create directory: {}", e));
                self.conversation.finalize_stream();
                return;
            }

            // git init
            let _ = std::process::Command::new("git")
                .args(&["init"])
                .current_dir(&new_path)
                .output();

            // Create CLAUDE.md with project name
            let project_name = new_path.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "project".to_string());
            let claude_md = format!("# {}\n\nNew project.\n", project_name);
            let _ = std::fs::write(new_path.join("CLAUDE.md"), &claude_md);

            // Now cd into it
            let created_path = new_path.to_string_lossy().to_string();
            let (ok, _) = self.try_change_dir(&created_path);
            if ok {
                self.previous_working_dir = Some(PathBuf::from(&old_dir));
                // Track in recent dirs (same as existing-dir branch)
                let new_dir = self.working_dir.clone();
                self.recent_dirs.retain(|d| d != &new_dir);
                self.recent_dirs.insert(0, new_dir);
                self.recent_dirs.truncate(5);
                save_recent_dirs(&self.recent_dirs);
                self.conversation = atomcode_core::conversation::Conversation::new();
                self.render_cache.clear();
                self.render_cache_msg_count = 0;
                self.scroll_offset = 0;
                self.at_bottom = true;
                let summary = format!(
                    "Created and switched to `{}`\n\nInitialized git repo and CLAUDE.md.",
                    self.working_dir.display()
                );
                self.conversation.push_delta(&summary);
                self.conversation.finalize_stream();
                self.project_context_cache = None;
            }
        }
    }

    /// If the AI ended the turn without giving a summary (just tool calls, no final text),
    /// auto-generate a brief summary from the tool results.
    /// Rebuild the LLM provider from current config (after provider/model change).
    /// Also updates model_name for status bar display.
    pub fn rebuild_provider(&mut self) {
        use atomcode_core::provider::create_provider;
        if let Ok(provider_config) = self.config.active_provider(None) {
            self.model_name = provider_config.model.clone();
            if let Ok(new_provider) = create_provider(&provider_config.clone()) {
                self.provider = new_provider;
            }
        }
    }

    /// Process an event coming from the AgentLoop. Updates local conversation mirror and UI state.
    pub fn handle_agent_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TextDelta(text) => {
                // Track TTFT — first text delta in this LLM call
                if self.first_token_ms.is_none() {
                    if let Some(start) = self.llm_call_start {
                        self.first_token_ms = Some(start.elapsed().as_millis() as u64);
                    }
                }
                self.conversation.push_delta(&text);
            }
            AgentEvent::ToolCallStarted { name, arguments } => {
                // Track TTFT — tool call is also a "first token" from the LLM
                if self.first_token_ms.is_none() {
                    if let Some(start) = self.llm_call_start {
                        self.first_token_ms = Some(start.elapsed().as_millis() as u64);
                    }
                }
                // If model produced text before tool calls (looks like a premature summary),
                // append a visual separator so the user knows more work is coming.
                if self.conversation.stream_buffer.as_ref().map_or(false, |b| b.len() > 50) {
                    self.conversation.push_delta("\n\n---\n*[continuing...]*\n");
                }
                self.turn_log.log_tool_call(&name, &arguments);
                let call = ToolCall {
                    id: format!("call_{}", self.current_step_count),
                    name: name.clone(),
                    arguments: arguments.clone(),
                };
                self.executing_tool_info = format_tool_info(&call);
                self.conversation.finalize_stream_with_tool_call(call);
                self.render_cache_msg_count = 0;
                self.mode = AppMode::ToolExecuting;
                self.tool_start = Some(Instant::now());
                self.at_bottom = true;
            }
            AgentEvent::ToolCallResult { name, output, success, duration: _ } => {
                self.last_completed_tool = name;
                self.turn_log.log_tool_result(&output, success);
                // Add result to conversation mirror so it renders in the UI
                self.conversation.add_tool_result(ToolResult {
                    call_id: format!("call_{}", self.current_step_count),
                    output,
                    success,
                });
                self.render_cache_msg_count = 0;
                self.at_bottom = true;
            }
            AgentEvent::ApprovalNeeded { tool_name: _, reason: _, call } => {
                self.mode = AppMode::WaitingApproval(call);
            }
            AgentEvent::PhaseChange(phase) => {
                use atomcode_core::agent::AgentPhase;
                match phase {
                    AgentPhase::Idle => {
                        // TurnComplete handles mode reset; ignore standalone Idle transitions.
                    }
                    AgentPhase::Thinking => {
                        self.mode = AppMode::Streaming;
                        // Count LLM round-trips (not individual tool calls) — matches Claude Code's step counting.
                        self.current_step_count += 1;
                        self.turn_log.log_llm_call();
                        // Reset TTFT for each LLM call (not just the first in a turn)
                        self.first_token_ms = None;
                        self.llm_call_start = Some(Instant::now());
                    }
                    AgentPhase::CallingTool(name) => {
                        self.mode = AppMode::Streaming;
                        // Show which tool the LLM is preparing (streaming args)
                        let display_name = name.split('_')
                            .map(|w| {
                                let mut c = w.chars();
                                match c.next() {
                                    None => String::new(),
                                    Some(ch) => ch.to_uppercase().to_string() + c.as_str(),
                                }
                            })
                            .collect::<Vec<_>>()
                            .join(" ");
                        self.executing_tool_info = format!("Preparing {}...", display_name);
                    }
                    AgentPhase::WaitingApproval => {
                        // Handled by ApprovalNeeded event which carries the ToolCall.
                    }
                }
            }
            AgentEvent::TurnComplete { duration, total_tokens: _ } => {
                // Finalize stream FIRST so auto-summary TextDelta becomes a message
                self.conversation.finalize_stream();
                // Then log the final assistant text
                if let Some(last) = self.conversation.messages.last() {
                    if matches!(last.role, atomcode_core::conversation::message::Role::Assistant) {
                        if let Some(text) = last.text() {
                            self.turn_log.log_text(text);
                        }
                    }
                }
                self.turn_log.end_turn(self.turn_tokens);
                self.mode = AppMode::Normal;
                self.last_turn_duration = Some(duration);
                self.turn_start = None;
                self.render_cache_msg_count = 0; // Invalidate cache
                self.suggestion = self.generate_suggestion();
                self.at_bottom = true;
            }
            AgentEvent::Error(e) => {
                self.turn_log.log_error(&e);
                self.turn_log.end_turn(self.turn_tokens);
                self.conversation.push_delta(&format!("\n\n[Error: {}]", e));
                self.conversation.finalize_stream();
                self.mode = AppMode::Normal;
                self.last_turn_duration = self.turn_start.map(|t| t.elapsed());
                self.turn_start = None;
                self.at_bottom = true;
            }
            AgentEvent::TokenUsage(usage) => {
                self.turn_tokens += usage.completion_tokens;
                self.total_tokens += usage.completion_tokens;
            }
            AgentEvent::WorkingDirChanged(new_dir) => {
                // Only /cd (user command) triggers this — LLM tools cannot change working dir.
                self.previous_working_dir = Some(self.working_dir.clone());
                self.working_dir = new_dir.clone();
                self.turn_log.set_working_dir(&new_dir);
                self.project_context_cache = None;
                // Sync recent dirs + config so /cd list and next startup remember this dir
                self.recent_dirs.retain(|d| d != &new_dir);
                self.recent_dirs.insert(0, new_dir.clone());
                self.recent_dirs.truncate(5);
                save_recent_dirs(&self.recent_dirs);
                self.config.default_workdir = Some(new_dir.to_string_lossy().to_string());
                let _ = self.config.save(&Config::default_path());
            }
            AgentEvent::ContextStats { system_tokens, hot_tokens, cold_tokens, working_set_tokens, total_messages } => {
                self.turn_log.log_context_stats(system_tokens, hot_tokens, cold_tokens, working_set_tokens, total_messages);
            }
        }
    }

    pub fn handle_event(&mut self, event: AppEvent, event_tx: &mpsc::UnboundedSender<AppEvent>) {
        match event {
            AppEvent::Key(key) => {
                self.selection.has_selection = false;

                // Paste detection for terminals without bracketed paste (macOS/Linux only).
                // Windows terminals don't support bracketed paste reliably, and the
                // rapid-key detection causes input lag and IME issues. On Windows,
                // paste is handled via Ctrl+V → clipboard read instead.
                let now = Instant::now();
                let interval_ms = now.duration_since(self.last_key_time).as_millis();
                self.last_key_time = now;

                // Provider manager / model selector have their own input —
                // skip fast-paste detection and Ctrl+V so keys reach their handler.
                let is_overlay_mode = matches!(self.mode, AppMode::ProviderManager | AppMode::ModelSelector);

                if !is_overlay_mode && !cfg!(target_os = "windows") && interval_ms < 10
                    && !matches!(self.mode, AppMode::WaitingApproval(_) | AppMode::Exiting)
                {
                    match key.code {
                        KeyCode::Enter => {
                            self.input.insert_newline();
                            return;
                        }
                        KeyCode::Char(c) => {
                            self.input.insert_char(c);
                            self.suggestion = None;
                            return;
                        }
                        _ => {}
                    }
                }

                // Ctrl+V: clipboard paste
                if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('v') {
                    if let Some(text) = read_clipboard() {
                        if is_overlay_mode {
                            // Forward paste to provider manager's input buffer
                            if let Some(ref mut mgr) = self.provider_mgr {
                                mgr.input_buf.push_str(&text);
                            }
                        } else if text.lines().count() > 3 || text.len() > 200 {
                            self.pasted_text = Some(text);
                        } else {
                            self.input.insert_text(&text);
                        }
                        self.suggestion = None;
                    }
                    return;
                }

                self.handle_key(key, event_tx);
            }
            AppEvent::Paste(text) => {
                if matches!(self.mode, AppMode::ProviderManager) {
                    // Forward paste to provider manager's input buffer
                    if let Some(ref mut mgr) = self.provider_mgr {
                        // Normalize line endings
                        mgr.input_buf.push_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
                    }
                } else if matches!(self.mode, AppMode::Normal | AppMode::Streaming | AppMode::ToolExecuting) {
                    // Normalize line endings: \r\n -> \n, \r -> \n
                    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
                    if normalized.lines().count() > 3 || normalized.len() > 200 {
                        self.pasted_text = Some(normalized);
                    } else {
                        self.input.insert_text(&normalized);
                    }
                    self.suggestion = None;
                }
            }
            AppEvent::Resize(_, _) => {}
            AppEvent::Tick => {
                self.tick_count = self.tick_count.wrapping_add(1);
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent, event_tx: &mpsc::UnboundedSender<AppEvent>) {
        // nothing here — key timing handled in handle_key_normal

        // Ctrl+Shift+C: copy selection to clipboard (like Ctrl+C in Claude Code with selection)
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.modifiers.contains(KeyModifiers::SHIFT)
            && key.code == KeyCode::Char('C')
        {
            if self.selection.has_selection {
                self.copy_selection_to_clipboard();
            }
            return;
        }

        // Global Ctrl+C handling — like Claude Code:
        // 1st press: cancel current operation
        // 2nd press (within 1s): exit program
        if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('c') {
            let now = Instant::now();
            let double_press = self.last_ctrl_c
                .map(|t| now.duration_since(t).as_millis() < 1000)
                .unwrap_or(false);
            self.last_ctrl_c = Some(now);

            if double_press {
                // Double Ctrl+C: exit
                self.mode = AppMode::Exiting;
                return;
            }

            // Single Ctrl+C: cancel current operation
            match &self.mode {
                AppMode::Streaming | AppMode::ToolExecuting => {
                    // Signal the AgentLoop to cancel.
                    let _ = self.agent_handle.cmd_tx.send(AgentCommand::Cancel);
                    // Also cancel any legacy local tasks still using the old token.
                    self.cancel_token.cancel();
                    self.conversation.stream_buffer = None;
                    self.conversation.tool_call_buffer = None;
                    self.conversation.finalize_stream();
                    self.mode = AppMode::Normal;
                    self.at_bottom = true;
                    self.last_turn_duration = self.turn_start.map(|t| t.elapsed());
                    self.turn_start = None;
                }
                AppMode::WaitingApproval(_) => {
                    // Deny the pending tool and cancel the agent turn.
                    let _ = self.agent_handle.cmd_tx.send(AgentCommand::DenyTool);
                    let _ = self.agent_handle.cmd_tx.send(AgentCommand::Cancel);
                    self.mode = AppMode::Normal;
                    self.at_bottom = true;
                }
                AppMode::Normal => {
                    if !self.input.is_empty() {
                        self.input.clear();
                        self.slash_menu.close();
                    }
                    // Show hint about double-press to exit
                }
                AppMode::ProviderManager => {
                    self.provider_mgr = None;
                    self.mode = AppMode::Normal;
                }
                _ => {}
            }
            return;
        }

        // Reset double-press timer on any other key
        self.last_ctrl_c = None;

        // Directory selector (overlay, works in Normal mode)
        if let Some(selected) = self.dir_selector {
            match key.code {
                KeyCode::Up => {
                    if selected > 0 {
                        self.dir_selector = Some(selected - 1);
                    }
                }
                KeyCode::Down => {
                    if selected + 1 < self.recent_dirs.len() {
                        self.dir_selector = Some(selected + 1);
                    }
                }
                KeyCode::Enter => {
                    if let Some(dir) = self.recent_dirs.get(selected).cloned() {
                        let dir_str = dir.to_string_lossy().to_string();
                        self.dir_selector = None;
                        self.change_working_dir(&dir_str);
                        let _ = self.agent_handle.cmd_tx.send(AgentCommand::ChangeDir(dir_str));
                    }
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.dir_selector = None;
                }
                _ => {}
            }
            return;
        }

        match &self.mode {
            AppMode::Welcome => self.handle_key_welcome(key),
            AppMode::Normal => self.handle_key_normal(key, event_tx),
            AppMode::Streaming | AppMode::ToolExecuting => {
                if key.code == KeyCode::Esc {
                    if self.slash_menu.visible {
                        // Slash menu is open — close it instead of cancelling the agent
                        self.slash_menu.close();
                    } else if !self.input.is_empty() {
                        // Clear input instead of cancelling
                        self.input.clear();
                    } else {
                        // Esc cancels the operation
                        let _ = self.agent_handle.cmd_tx.send(AgentCommand::Cancel);
                        self.cancel_token.cancel();
                        self.conversation.stream_buffer = None;
                        self.conversation.tool_call_buffer = None;
                        self.conversation.finalize_stream();
                        self.mode = AppMode::Normal;
                        self.at_bottom = true;
                        self.last_turn_duration = self.turn_start.map(|t| t.elapsed());
                        self.turn_start = None;
                    }
                } else if (key.code == KeyCode::Enter && key.modifiers == KeyModifiers::NONE)
                    || (key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('j')) {
                    // During streaming: if input has content, append it as additional context.
                    let content = self.input.content();
                    if !content.trim().is_empty() {
                        // Show the appended input in the chat
                        self.conversation.push_delta(&format!("\n\n[User added: {}]\n", content.trim()));
                        // Send to agent loop
                        let _ = self.agent_handle.cmd_tx.send(
                            atomcode_core::agent::AgentCommand::AppendInput(content)
                        );
                        self.input.clear();
                    }
                } else {
                    // All other keys work normally: scroll, type, Ctrl+A/E, etc.
                    self.handle_key_normal(key, event_tx);
                    // Don't show slash menu during streaming — it overlays the output
                    // and Esc would need to close menu vs cancel agent (confusing UX)
                    self.slash_menu.close();
                }
            }
            AppMode::WaitingApproval(_) => {
                if !self.handle_scroll_keys(key) {
                    self.handle_key_approval(key, event_tx);
                }
            }
            AppMode::ModelSelector => self.handle_key_model_selector(key),
            AppMode::ProviderManager => self.handle_key_provider_manager(key),
            AppMode::Exiting => {}
        }
    }

    fn handle_key_model_selector(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => {
                if self.model_selected > 0 {
                    self.model_selected -= 1;
                }
            }
            KeyCode::Down => {
                if self.model_selected + 1 < self.model_list.len() {
                    self.model_selected += 1;
                }
            }
            KeyCode::Enter => {
                // Switch to selected provider
                if let Some((name, _)) = self.model_list.get(self.model_selected).cloned() {
                    self.config.default_provider = name.clone();
                    let _ = self.config.save(&Config::default_path());
                    self.rebuild_provider();
                    // Tell AgentLoop to switch to the new provider
                    let _ = self.agent_handle.cmd_tx.send(AgentCommand::SwitchProvider(name));
                }
                self.mode = AppMode::Normal;
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = AppMode::Normal;
            }
            _ => {}
        }
    }

    fn handle_key_approval(&mut self, key: KeyEvent, _event_tx: &mpsc::UnboundedSender<AppEvent>) {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                let _ = self.agent_handle.cmd_tx.send(AgentCommand::ApproveTool);
                self.mode = AppMode::ToolExecuting;
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                let _ = self.agent_handle.cmd_tx.send(AgentCommand::ApproveToolAlways);
                self.mode = AppMode::ToolExecuting;
            }
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                let _ = self.agent_handle.cmd_tx.send(AgentCommand::DenyTool);
                self.mode = AppMode::Normal;
            }
            _ => {}
        }
    }

    fn handle_key_provider_manager(&mut self, key: KeyEvent) {
        let action = if let Some(ref mut mgr) = self.provider_mgr {
            mgr.handle_key(key, &self.config)
        } else {
            return;
        };

        if let Some(action) = action {
            match action {
                ManagerAction::Close => {
                    let config_path = Config::default_path();
                    let _ = self.config.save(&config_path);
                    // Rebuild provider from current config
                    self.rebuild_provider();
                    self.provider_mgr = None;
                    self.mode = AppMode::Normal;
                }
                ManagerAction::SetDefault(name) => {
                    self.config.default_provider = name;
                    let config_path = Config::default_path();
                    let _ = self.config.save(&config_path);
                    self.rebuild_provider();
                }
                ManagerAction::Delete(name) => {
                    self.config.providers.remove(&name);
                    if let Some(ref mut mgr) = self.provider_mgr {
                        mgr.refresh_names(&self.config);
                    }
                    let config_path = Config::default_path();
                    let _ = self.config.save(&config_path);
                }
                ManagerAction::Add(name, provider_config) => {
                    self.config.providers.insert(name, provider_config);
                    if let Some(ref mut mgr) = self.provider_mgr {
                        mgr.refresh_names(&self.config);
                    }
                    let config_path = Config::default_path();
                    let _ = self.config.save(&config_path);
                }
                ManagerAction::StartAtomGitOAuth(name) => {
                    // Store the desired provider name, then trigger the existing OAuth flow.
                    self.pending_oauth_name = Some(name);
                    self.pending_login = true;
                    // Close provider manager — OAuth runs in lib.rs main loop
                    self.provider_mgr = None;
                    self.mode = AppMode::Normal;
                }
                ManagerAction::UpdateField(name, field, value) => {
                    if let Some(p) = self.config.providers.get_mut(&name) {
                        match field.as_str() {
                            "type" => p.provider_type = value.clone(),
                            "api_key" => {
                                p.api_key = if value.is_empty() { None } else { Some(value.clone()) }
                            }
                            "base_url" => {
                                p.base_url = if value.is_empty() { None } else { Some(value.clone()) }
                            }
                            "model" => p.model = value.clone(),
                            _ => {}
                        }
                    }
                    // Rebuild provider if the active provider was modified
                    if name == self.config.default_provider {
                        self.rebuild_provider();
                    }
                    let config_path = Config::default_path();
                    let _ = self.config.save(&config_path);
                }
            }
        }
    }

    fn handle_key_welcome(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => {
                if self.welcome_state.selected > 0 {
                    self.welcome_state.selected -= 1;
                }
            }
            KeyCode::Down => {
                if self.welcome_state.selected < 2 {
                    self.welcome_state.selected += 1;
                }
            }
            KeyCode::Char('1') => {
                self.welcome_state.selected = 0;
                self.trigger_welcome_action();
            }
            KeyCode::Char('2') => {
                self.welcome_state.selected = 1;
                self.trigger_welcome_action();
            }
            KeyCode::Char('3') | KeyCode::Esc => {
                self.welcome_state.selected = 2;
                self.trigger_welcome_action();
            }
            KeyCode::Enter => {
                self.trigger_welcome_action();
            }
            _ => {}
        }
    }

    fn trigger_welcome_action(&mut self) {
        match self.welcome_state.selected {
            0 => {
                // Login with AtomGit
                self.pending_login = true;
                self.mode = AppMode::Normal;
            }
            1 => {
                // Configure manually → open ProviderManager
                self.provider_mgr = Some(ProviderManager::new(&self.config));
                self.mode = AppMode::ProviderManager;
            }
            _ => {
                // Skip
                self.mode = AppMode::Normal;
            }
        }
    }

    fn handle_key_normal(&mut self, key: KeyEvent, event_tx: &mpsc::UnboundedSender<AppEvent>) {
        // When slash menu is visible, intercept navigation keys
        if self.slash_menu.visible {
            match key.code {
                KeyCode::Up => {
                    self.slash_menu.prev();
                    return;
                }
                KeyCode::Down => {
                    self.slash_menu.next();
                    return;
                }
                KeyCode::Tab => {
                    // Tab accepts the selected command into the input
                    if let Some(cmd) = self.slash_menu.selected_command() {
                        self.input.clear();
                        for c in cmd.name.chars() {
                            self.input.insert_char(c);
                        }
                        self.slash_menu.close();
                    }
                    return;
                }
                KeyCode::Enter => {
                    // Enter accepts and immediately executes
                    if let Some(cmd) = self.slash_menu.selected_command() {
                        self.input.clear();
                        for c in cmd.name.chars() {
                            self.input.insert_char(c);
                        }
                        self.slash_menu.close();
                        self.send_message(event_tx);
                    }
                    return;
                }
                KeyCode::Esc => {
                    self.slash_menu.close();
                    return;
                }
                _ => {
                    // Fall through to normal input handling, menu will update after
                }
            }
        }

        match (key.modifiers, key.code) {
            // Enter handling:
            // - Plain Enter (no modifiers) = send message
            // - Any modifier + Enter (Shift/Ctrl/Alt) = newline
            // - Rapid Enter (<50ms since last key) = newline (paste fallback)
            (KeyModifiers::NONE, KeyCode::Enter) => {
                self.slash_menu.close();
                self.send_message(event_tx);
            }
            (_, KeyCode::Enter) => {
                // Any modifier + Enter = newline
                self.input.insert_newline();
            }
            (KeyModifiers::CONTROL, KeyCode::Char('j')) => {
                // Ctrl+J also sends (alternative send shortcut)
                self.slash_menu.close();
                self.send_message(event_tx);
            }
            (_, KeyCode::Esc) => {
                if self.pasted_text.is_some() {
                    self.pasted_text = None;
                } else if !self.input.is_empty() {
                    self.input.clear();
                    self.slash_menu.close();
                }
            }
            // Ctrl+L: clear conversation (like Claude Code)
            (KeyModifiers::CONTROL, KeyCode::Char('l')) => {
                self.conversation = atomcode_core::conversation::Conversation::new();
                self.render_cache.clear();
                self.render_cache_msg_count = 0;
                self.scroll_offset = 0;
                self.at_bottom = true;
                self.current_step_count = 0;
                self.turn_tokens = 0;
                self.total_tokens = 0;
                self.suggestion = None;
            }
            // Emacs-style line editing (like Claude Code)
            (KeyModifiers::CONTROL, KeyCode::Char('a')) => {
                self.input.move_home();
            }
            (KeyModifiers::CONTROL, KeyCode::Char('e')) => {
                self.input.move_end();
            }
            (KeyModifiers::CONTROL, KeyCode::Char('u')) => {
                self.input.clear_line();
            }
            (KeyModifiers::CONTROL, KeyCode::Char('k')) => {
                self.input.kill_to_end();
            }
            (KeyModifiers::CONTROL, KeyCode::Char('w')) => {
                self.input.delete_word_backward();
            }
            (_, KeyCode::Home) => {
                self.input.move_home();
            }
            (_, KeyCode::End) => {
                self.input.move_end();
            }
            (_, KeyCode::Delete) => {
                self.input.delete_forward();
            }
            // Scroll keys: Ctrl+Up/Down (3 lines), PageUp/PageDown (20 lines)
            // Also handle plain Up/Down for scroll when input is empty AND no history available.
            // If there's history, Up should enter history browse mode, not scroll.
            (_, KeyCode::Up) if self.input.is_empty() && self.input_history.is_empty() => {
                // Scroll up when input is empty (terminal scroll wheel sends Up/Down in alternate mode)
                if self.at_bottom {
                    let total = self.render_cache.len();
                    self.scroll_offset = total.saturating_sub(3);
                    self.at_bottom = false;
                } else {
                    self.scroll_offset = self.scroll_offset.saturating_sub(3);
                }
            }
            (_, KeyCode::Down) if self.input.is_empty() && self.input_history.is_empty() => {
                // Scroll down when input is empty (terminal scroll wheel sends Up/Down in alternate mode)
                self.scroll_offset += 3;
                let total = self.render_cache.len();
                if self.scroll_offset >= total {
                    self.at_bottom = true;
                }
            }
            _ if self.handle_scroll_keys(key) => {}
            (_, KeyCode::Backspace) => {
                self.input.backspace();
            }
            (_, KeyCode::Up) => {
                // Multi-line: move cursor up within input
                if self.input.cursor_row > 0 {
                    self.input.cursor_row -= 1;
                    self.input.cursor_col = snap_to_char_boundary(
                        &self.input.lines[self.input.cursor_row],
                        self.input.cursor_col,
                    );
                } else if !self.input_history.is_empty() {
                    // Single line, at top: browse history
                    if self.history_index.is_none() {
                        // Stash current input
                        self.history_stash = Some(self.input.content());
                        self.history_index = Some(self.input_history.len().saturating_sub(1));
                    } else if let Some(idx) = self.history_index {
                        if idx > 0 {
                            self.history_index = Some(idx - 1);
                        }
                    }
                    if let Some(idx) = self.history_index {
                        if let Some(hist) = self.input_history.get(idx).cloned() {
                            self.suggestion = None;
                            self.pasted_text = None;
                            self.load_history_entry(&hist);
                        }
                    }
                }
            }
            (_, KeyCode::Down) => {
                if self.input.cursor_row + 1 < self.input.lines.len() {
                    self.input.cursor_row += 1;
                    self.input.cursor_col = snap_to_char_boundary(
                        &self.input.lines[self.input.cursor_row],
                        self.input.cursor_col,
                    );
                } else if let Some(idx) = self.history_index {
                    if idx + 1 < self.input_history.len() {
                        self.history_index = Some(idx + 1);
                        let hist = self.input_history[idx + 1].clone();
                        self.suggestion = None;
                        self.pasted_text = None;
                        self.load_history_entry(&hist);
                    } else {
                        self.history_index = None;
                        self.pasted_text = None;
                        self.input.clear();
                        if let Some(stash) = self.history_stash.take() {
                            for c in stash.chars() { self.input.insert_char(c); }
                        }
                    }
                }
            }
            (_, KeyCode::Left) => {
                if self.input.cursor_col > 0 {
                    // Move to previous char boundary
                    let line = &self.input.lines[self.input.cursor_row];
                    self.input.cursor_col = line[..self.input.cursor_col]
                        .char_indices()
                        .last()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                }
            }
            (_, KeyCode::Right) => {
                let line = &self.input.lines[self.input.cursor_row];
                if self.input.cursor_col < line.len() {
                    // Move to next char boundary
                    self.input.cursor_col = line[self.input.cursor_col..]
                        .char_indices()
                        .nth(1)
                        .map(|(i, _)| self.input.cursor_col + i)
                        .unwrap_or(line.len());
                }
            }
            (_, KeyCode::Tab) => {
                // Accept suggestion into input
                if let Some(ref suggestion) = self.suggestion.take() {
                    self.input.clear();
                    for c in suggestion.chars() {
                        self.input.insert_char(c);
                    }
                }
                return; // Don't clear suggestion below
            }
            (_, KeyCode::Char(c)) => {
                self.input.insert_char(c);
            }
            _ => {}
        }

        // Clear suggestion on any input change (except Tab which was handled above)
        if !self.input.is_empty() {
            self.suggestion = None;
        }

        // After any input change, update the slash menu (uses pre-built list, no alloc)
        self.slash_menu.update(&self.input.content(), &self.command_list);

        // Detect file path — auto-attach if input is a valid file path
        let content = self.input.content();
        let trimmed = content.trim();
        // Skip slash commands (except /file which is explicitly for file attachment)
        if trimmed.is_empty() || trimmed.starts_with('/') && !trimmed.starts_with("/file") {
            // Not a file path
        } else if let Some(file) = crate::file_attach::detect_file_path(trimmed, &self.working_dir) {
            // Only attach if not already attached
            if !self.attached_files.iter().any(|f| f.path == file.path) {
                self.attached_files.push(file);
                self.input.clear(); // Clear the path from input
            }
        }
    }

    /// Handle scroll keys (Ctrl+Up/Down, PageUp/PageDown). Returns true if the key was a scroll key.
    fn handle_scroll_keys(&mut self, key: KeyEvent) -> bool {
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Up) => {
                if self.at_bottom {
                    let total = self.render_cache.len();
                    self.scroll_offset = total.saturating_sub(3);
                    self.at_bottom = false;
                } else {
                    self.scroll_offset = self.scroll_offset.saturating_sub(3);
                }
                true
            }
            (KeyModifiers::CONTROL, KeyCode::Down) => {
                self.scroll_offset += 3;
                let total = self.render_cache.len();
                if self.scroll_offset >= total {
                    self.at_bottom = true;
                }
                true
            }
            (_, KeyCode::PageUp) => {
                if self.at_bottom {
                    let total = self.render_cache.len();
                    self.scroll_offset = total.saturating_sub(20);
                    self.at_bottom = false;
                } else {
                    self.scroll_offset = self.scroll_offset.saturating_sub(20);
                }
                true
            }
            (_, KeyCode::PageDown) => {
                self.scroll_offset += 20;
                let total = self.render_cache.len();
                if self.scroll_offset >= total {
                    self.at_bottom = true;
                }
                true
            }
            _ => false,
        }
    }

    /// Extract text from the rendered content between the selection coordinates,
    /// then copy it to the system clipboard.
    /// On macOS/Windows/Linux local: use native clipboard commands (no permission prompt).
    /// On SSH: use OSC 52 escape sequence (requires terminal support).
    fn copy_selection_to_clipboard(&self) {
        let text = self.extract_selection_text();
        if text.is_empty() {
            return;
        }

        // Check if we're in an SSH session
        let is_ssh = std::env::var("SSH_CONNECTION").is_ok()
            || std::env::var("SSH_TTY").is_ok()
            || std::env::var("SSH_CLIENT").is_ok();

        if is_ssh {
            // OSC 52 clipboard (works across SSH and on terminals that support it)
            let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
            let osc = format!("\x1b]52;c;{}\x07", encoded);
            let _ = std::io::Write::write_all(&mut std::io::stdout(), osc.as_bytes());
            let _ = std::io::Write::flush(&mut std::io::stdout());
        } else {
            // Local: use native clipboard commands (no permission prompt on iTerm2)
            let _ = copy_to_clipboard(&text);
        }
    }

    /// Extract the selected text from render cache + dynamic content.
    /// Maps terminal coordinates to the rendered lines using the current scroll offset.
    fn extract_selection_text(&self) -> String {
        let ((start_col, start_row), (end_col, end_row)) = self.selection.normalized();

        // The layout is: row 0 = status bar (1 line), then chat panel, then input box.
        // Chat panel starts at row 1. We need to map terminal rows to render_cache indices.
        let chat_start_row: u16 = 1;

        // Convert terminal rows to line indices in the full content
        let start_line = (start_row.saturating_sub(chat_start_row)) as usize + self.effective_scroll();
        let end_line = (end_row.saturating_sub(chat_start_row)) as usize + self.effective_scroll();

        let mut result = String::new();
        for i in start_line..=end_line {
            let line_text = if i < self.render_cache.len() {
                // Get text from render cache line
                self.render_cache[i]
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            } else {
                continue;
            };

            let chars: Vec<char> = line_text.chars().collect();

            if i == start_line && i == end_line {
                // Single line selection
                let s = start_col as usize;
                let e = end_col as usize;
                let slice: String = chars[s.min(chars.len())..=e.min(chars.len().saturating_sub(1))]
                    .iter()
                    .collect();
                result.push_str(&slice);
            } else if i == start_line {
                let s = start_col as usize;
                let slice: String = chars[s.min(chars.len())..].iter().collect();
                result.push_str(&slice);
                result.push('\n');
            } else if i == end_line {
                let e = end_col as usize;
                let slice: String = chars[..=e.min(chars.len().saturating_sub(1))].iter().collect();
                result.push_str(&slice);
            } else {
                result.push_str(&line_text);
                result.push('\n');
            }
        }
        result
    }

    /// Get the effective scroll offset (uses the value computed during the last render).
    fn effective_scroll(&self) -> usize {
        self.last_rendered_scroll
    }

    /// Handle typing in the input box — works in any mode (Normal, Streaming, etc.)
    #[allow(dead_code)]
    fn handle_key_input(&mut self, key: KeyEvent) {
        match (key.modifiers, key.code) {
            (KeyModifiers::SHIFT, KeyCode::Enter) => {
                self.input.insert_newline();
            }
            (_, KeyCode::Backspace) => {
                self.input.backspace();
            }
            (_, KeyCode::Left) => {
                if self.input.cursor_col > 0 {
                    let line = &self.input.lines[self.input.cursor_row];
                    self.input.cursor_col = line[..self.input.cursor_col]
                        .char_indices().last().map(|(i, _)| i).unwrap_or(0);
                }
            }
            (_, KeyCode::Right) => {
                let line = &self.input.lines[self.input.cursor_row];
                if self.input.cursor_col < line.len() {
                    self.input.cursor_col = line[self.input.cursor_col..]
                        .char_indices().nth(1)
                        .map(|(i, _)| self.input.cursor_col + i)
                        .unwrap_or(line.len());
                }
            }
            (_, KeyCode::Char(c)) => {
                self.input.insert_char(c);
            }
            _ => {}
        }
    }

    /// Handle slash commands. Returns true if the input was a command.
    fn handle_slash_command(&mut self) -> bool {
        let content = self.input.content();
        let trimmed = content.trim();

        if !trimmed.starts_with('/') {
            return false;
        }

        let cmd = trimmed.to_string();
        self.input.clear();
        self.slash_menu.close();
        self.conversation.add_user_message(&cmd);

        match cmd.as_str() {
            "/quit" => {
                self.mode = AppMode::Exiting;
            }
            "/provider" => {
                self.provider_mgr = Some(ProviderManager::new(&self.config));
                self.mode = AppMode::ProviderManager;
                // Remove the /provider message from conversation since we're entering a mode
                self.conversation.messages.pop();
            }
            "/config" => {
                let config_path = atomcode_core::config::Config::default_path();
                if !config_path.exists() {
                    self.conversation.push_delta(&format!(
                        "Config file not found at `{}`.\nRun AtomCode without an existing config to create one via the setup wizard.",
                        config_path.display()
                    ));
                    self.conversation.finalize_stream();
                } else {
                    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());
                    self.conversation.push_delta(&format!(
                        "Opening config in `{}`...\n\n`{}`",
                        editor,
                        config_path.display()
                    ));
                    self.conversation.finalize_stream();
                    self.pending_editor = Some(config_path.to_string_lossy().to_string());
                }
            }
            "/model" => {
                // Enter model selector mode
                self.model_list = self.config.providers.iter()
                    .map(|(name, p)| (name.clone(), p.model.clone()))
                    .collect();
                self.model_list.sort_by(|a, b| a.0.cmp(&b.0));
                // Pre-select current provider
                self.model_selected = self.model_list.iter()
                    .position(|(name, _)| name == &self.config.default_provider)
                    .unwrap_or(0);
                self.conversation.messages.pop(); // Remove the /model user message
                self.mode = AppMode::ModelSelector;
            }
            "/copy" => {
                // Find the last assistant text and copy to clipboard
                let last_text = self.conversation.messages.iter().rev()
                    .find_map(|m| {
                        use atomcode_core::conversation::message::{MessageContent, Role};
                        match &m.content {
                            MessageContent::Text(s) if matches!(m.role, Role::Assistant) => Some(s.clone()),
                            MessageContent::AssistantWithToolCalls { text: Some(t), .. } => Some(t.clone()),
                            _ => None,
                        }
                    });
                if let Some(text) = last_text {
                    match copy_to_clipboard(&text) {
                        Ok(_) => {
                            self.conversation.push_delta("Copied to clipboard");
                            self.conversation.finalize_stream();
                        }
                        Err(e) => {
                            self.conversation.push_delta(&format!("Failed to copy: {}", e));
                            self.conversation.finalize_stream();
                        }
                    }
                } else {
                    self.conversation.push_delta("No AI response to copy");
                    self.conversation.finalize_stream();
                }
            }
            _ if cmd.starts_with("/cd ") || cmd == "/cd" => {
                let arg = cmd.strip_prefix("/cd").unwrap().trim();
                if arg.is_empty() {
                    if self.recent_dirs.is_empty() {
                        self.conversation.push_delta(&format!(
                            "Working directory: `{}`\n\nNo recent projects. Use `/cd <path>` to switch.",
                            self.working_dir.display()
                        ));
                        self.conversation.finalize_stream();
                    } else {
                        // Open directory picker
                        self.dir_selector = Some(0);
                    }
                } else if arg == "-" {
                    // /cd - : go back to previous directory
                    if let Some(prev) = self.previous_working_dir.clone() {
                        let prev_str = prev.to_string_lossy().to_string();
                        self.change_working_dir(&prev_str);
                        let _ = self.agent_handle.cmd_tx.send(AgentCommand::ChangeDir(prev_str));
                    } else {
                        self.conversation.push_delta("No previous directory");
                        self.conversation.finalize_stream();
                    }
                } else {
                    self.change_working_dir(arg);
                    let _ = self.agent_handle.cmd_tx.send(AgentCommand::ChangeDir(arg.to_string()));
                }
            }
            "/clear" => {
                // Clear agent's conversation context (but keep UI messages)
                let _ = self.agent_handle.cmd_tx.send(AgentCommand::ClearConversation);
                // Clear history file
                tokio::spawn(async move {
                    let path = Conversation::history_path();
                    let temp_path = path.with_extension("json.tmp");
                    if tokio::fs::write(&temp_path, "[]").await.is_ok() {
                        let _ = tokio::fs::rename(&temp_path, &path).await;
                    }
                });
                // Add a placeholder message to indicate conversation was cleared
                self.conversation.push_delta("(no content)");
                self.conversation.finalize_stream();
                return true;
            }
            "/login" => {
                self.pending_login = true;
                self.conversation.push_delta("Opening browser for AtomGit login...");
                self.conversation.finalize_stream();
            }
            "/logout" => {
                let mut logged_out = false;
                let mut messages = Vec::new();
                
                // 1. Remove auth.toml (access_token and user info)
                let auth_path = atomcode_core::config::Config::config_dir().join("auth.toml");
                    
                if auth_path.exists() {
                    match std::fs::remove_file(&auth_path) {
                        Ok(_) => {
                            messages.push(format!("Auth file removed: `{}`", auth_path.display()));
                            logged_out = true;
                        }
                        Err(e) => {
                            messages.push(format!("Failed to remove auth file: {}", e));
                        }
                    }
                }
                
                // 2. Remove AtomGit provider from config.toml
                let config_path = Config::default_path();
                if config_path.exists() {
                    if let Ok(mut config) = Config::load(&config_path) {
                        // Case-insensitive search for AtomGit provider
                        let atomgit_key = config.providers.keys()
                            .find(|k| k.to_lowercase() == "atomgit")
                            .cloned();
                            
                        if let Some(key) = atomgit_key {
                            config.providers.remove(&key);
                            messages.push("AtomGit provider removed from config.".to_string());
                            
                            // If default provider was AtomGit, switch to another
                            if config.default_provider.to_lowercase() == "atomgit" {
                                if let Some(new_default) = config.providers.keys().next().cloned() {
                                    config.default_provider = new_default.clone();
                                    messages.push(format!("Switched default provider to: {}", new_default));
                                }
                            }
                            
                            let _ = config.save(&config_path);
                            
                            // Update app config and rebuild provider
                            self.config = config;
                            self.rebuild_provider();
                            logged_out = true;
                        }
                    }
                }
                
                // 3. Show result message
                if logged_out {
                    self.conversation.push_delta(&format!(
                        "**Logged out from AtomGit.**\n\n{}",
                        messages.join("\n\n")
                    ));
                } else {
                    self.conversation.push_delta("Not logged in with AtomGit.");
                }
                self.conversation.finalize_stream();
            }
            "/status" => {
                let mut status = String::new();
                
                // Check login status
                let auth_path = atomcode_core::config::Config::config_dir().join("auth.toml");
                    
                if auth_path.exists() {
                    if let Ok(content) = std::fs::read_to_string(&auth_path) {
                        // Parse user info from auth.toml
                        let mut username: Option<String> = None;
                        let mut user_id: Option<String> = None;
                        
                        for line in content.lines() {
                            if line.starts_with("username") {
                                username = line.split('=').nth(1)
                                    .map(|s| s.trim().trim_matches('"').to_string());
                            }
                            if line.starts_with("id") {
                                user_id = line.split('=').nth(1)
                                    .map(|s| s.trim().trim_matches('"').to_string());
                            }
                        }
                        
                        status.push_str("**Login Status**\n\n");
                        status.push_str("- Status: **Logged in**\n");
                        if let Some(u) = username {
                            status.push_str(&format!("- Username: `{}`\n", u));
                        }
                        if let Some(id) = user_id {
                            status.push_str(&format!("- User ID: `{}`\n", id));
                        }
                        status.push_str("\n");
                    }
                } else {
                    status.push_str("**Login Status**\n\n");
                    status.push_str("- Status: Not logged in\n\n");
                }
                
                // Show model info
                let provider_name = &self.config.default_provider;
                let model_name = self.provider.model_name().to_string();
                    
                status.push_str("**Model Info**\n\n");
                status.push_str(&format!("- Provider: `{}`\n", provider_name));
                status.push_str(&format!("- Model: `{}`\n", model_name));
                
                if let Some(provider_config) = self.config.providers.get(provider_name) {
                    if let Some(base_url) = &provider_config.base_url {
                        status.push_str(&format!("- Base URL: `{}`\n", base_url));
                    }
                }
                
                self.conversation.push_delta(&status);
                self.conversation.finalize_stream();
            }
            "/help" => {
                let mut help = String::from("**Available commands:**\n\n");
                for (name, desc) in crate::command::BUILTIN_COMMANDS {
                    help.push_str(&format!("  `{}` — {}\n", name, desc));
                }
                // List loaded skills
                let mut skills: Vec<_> = self.skill_registry.all().collect();
                if !skills.is_empty() {
                    skills.sort_by(|a, b| a.name.cmp(&b.name));
                    help.push_str("\n**Skills** (from `~/.claude/skills/`, `.claude/skills/`, `commands/` dirs):\n\n");
                    for s in skills {
                        let desc = if s.description.is_empty() { "skill".to_string() } else { s.description.clone() };
                        help.push_str(&format!("  `/{0}` — {1}\n", s.name, desc));
                    }
                }
                help.push_str("\n**Shortcuts:**\n\n");
                help.push_str("  `Enter` — Send message\n");
                help.push_str("  `Esc` — Clear input\n");
                help.push_str("  `Ctrl+Up/Down` — Scroll chat\n");
                help.push_str("  `Tab` — Accept suggestion\n");
                help.push_str("  `/quit` — Exit\n");
                help.push_str("\n**Copy text:**\n\n");
                help.push_str("  `/copy` — Copy last AI response to clipboard\n");
                help.push_str("  `Shift+Drag` — Native text selection (temporary disable mouse)\n");
                help.push_str("  `Ctrl+Shift+C` — Copy current selection\n");
                self.conversation.push_delta(&help);
                self.conversation.finalize_stream();
            }
            _ => {
                // Check if it's a skill invocation: /skill-name [args]
                let without_slash = cmd.strip_prefix('/').unwrap_or(&cmd);
                let (skill_name, skill_args) = without_slash
                    .split_once(' ')
                    .map(|(n, a)| (n, a))
                    .unwrap_or((without_slash, ""));

                if let Some(skill) = self.skill_registry.get(skill_name) {
                    let expanded = skill.expand(skill_args, "");
                    if expanded.trim().is_empty() {
                        self.conversation.push_delta(&format!(
                            "Skill `{}` has an empty template.",
                            skill_name
                        ));
                        self.conversation.finalize_stream();
                    } else {
                        // Replace the raw /skill-name message with the expanded prompt
                        self.conversation.messages.pop();
                        self.conversation.add_user_message(&expanded);
                        self.mode = AppMode::Streaming;
                        self.at_bottom = true;
                        self.current_step_count = 0;
                        self.turn_start = Some(Instant::now());
                        self.first_token_ms = None;
                        self.llm_call_start = Some(Instant::now());
                        self.last_completed_tool = String::new();
                        self.last_turn_duration = None;
                        let _ = self.agent_handle.cmd_tx.send(AgentCommand::SendMessage(expanded));
                    }
                } else {
                    // Not a known command or skill - treat as regular message
                    // Remove the user message we added earlier and return false
                    self.conversation.messages.pop();
                    return false;
                }
            }
        }

        true
    }

    /// Load a history entry into input. Long entries shown as pasted_text reference.
    fn load_history_entry(&mut self, entry: &str) {
        self.input.clear();
        let line_count = entry.lines().count();
        if line_count > 3 || entry.len() > 200 {
            // Long entry — show as pasted reference
            self.pasted_text = Some(entry.to_string());
        } else {
            // Short entry — load inline using insert_text to preserve newlines
            self.input.insert_text(entry);
        }
    }

    fn send_message(&mut self, _event_tx: &mpsc::UnboundedSender<AppEvent>) {
        let typed = self.input.content();
        let content = if let Some(pasted) = self.pasted_text.take() {
            if typed.trim().is_empty() { pasted }
            else { format!("{}\n\n{}", typed, pasted) }
        } else {
            typed
        };
        if content.trim().is_empty() {
            return;
        }

        // Check for slash commands first
        if self.handle_slash_command() {
            return;
        }

        // Add full content to history (typed + pasted)
        self.input_history.push(content.clone());
        if self.input_history.len() > 100 {
            self.input_history.drain(..self.input_history.len() - 100);
        }
        self.history_index = None;
        self.history_stash = None;

        self.turn_tokens = 0;

        // Build message with attached files
        let full_content = if self.attached_files.is_empty() {
            content.clone()
        } else {
            let mut parts = vec![content.clone()];
            for file in &self.attached_files {
                let path = std::path::Path::new(&file.path);
                match crate::file_attach::extract_file(path, &self.working_dir) {
                    Ok(fc) => {
                        parts.push(format!(
                            "\n[Attached: {} ({})]\n{}",
                            fc.filename, fc.file_type, fc.content
                        ));
                    }
                    Err(e) => {
                        parts.push(format!("\n[Failed to read {}: {}]", file.filename, e));
                    }
                }
            }
            self.attached_files.clear();
            parts.join("\n")
        };

        // Log this turn
        self.turn_log.begin_turn(&full_content);

        // Add user message to our local mirror for immediate display.
        self.conversation.add_user_message(&full_content);
        self.input.clear();
        self.mode = AppMode::Streaming;
        self.at_bottom = true;
        self.current_step_count = 0;
        self.turn_start = Some(Instant::now());
        self.first_token_ms = None;
        self.llm_call_start = Some(Instant::now());
        self.last_completed_tool = String::new();
        self.last_turn_duration = None;

        // Delegate to the AgentLoop via channel.
        let _ = self.agent_handle.cmd_tx.send(AgentCommand::SendMessage(full_content));
    }

}

/// Format tool info for display (called once, cached in App).
fn format_tool_info(call: &ToolCall) -> String {
    let name: String = call.name.split('_')
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().to_string() + c.as_str(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");

    if let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
        if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
            let short: String = cmd.chars().take(50).collect();
            return format!("{}: {}", name, short);
        }
        if let Some(fp) = args.get("file_path").and_then(|v| v.as_str()) {
            let fname = std::path::Path::new(fp)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| fp.to_string());
            return format!("{}: {}", name, fname);
        }
        if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
            return format!("{}: {}", name, path);
        }
    }
    name
}

fn snap_to_char_boundary(s: &str, pos: usize) -> usize {
    let pos = pos.min(s.len());
    if s.is_char_boundary(pos) {
        pos
    } else {
        s[..pos]
            .char_indices()
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0)
    }
}

fn copy_to_clipboard(text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    if cfg!(target_os = "windows") {
        // Windows: use clip.exe
        let mut child = Command::new("clip.exe")
            .stdin(Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
        }
        child.wait()?;
    } else {
        let cmd = if cfg!(target_os = "macos") { "pbcopy" } else { "xclip" };
        let mut child = Command::new(cmd)
            .stdin(Stdio::piped())
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes())?;
        }
        child.wait()?;
    }
    Ok(())
}

/// Read text from system clipboard.
fn read_clipboard() -> Option<String> {
    use std::process::Command;

    let output = if cfg!(target_os = "macos") {
        Command::new("pbpaste").output().ok()
    } else if cfg!(target_os = "windows") {
        Command::new("powershell")
            .args(&["-Command", "Get-Clipboard"])
            .output().ok()
    } else {
        Command::new("xclip")
            .args(&["-selection", "clipboard", "-o"])
            .output().ok()
            .or_else(|| Command::new("xsel").args(&["--clipboard", "--output"]).output().ok())
    };

    output
        .filter(|o| o.status.success())
        .map(|o| {
            let text = String::from_utf8_lossy(&o.stdout).to_string();
            // Normalize line endings: \r\n -> \n, \r -> \n
            text.replace("\r\n", "\n").replace('\r', "\n")
        })
        .filter(|s| !s.is_empty())
}

/// Load recent project directories from ~/.atomcode/recent_dirs.txt
fn load_recent_dirs() -> Vec<PathBuf> {
    let path = atomcode_core::config::Config::config_dir().join("recent_dirs.txt");
    std::fs::read_to_string(&path)
        .ok()
        .map(|s| {
            s.lines()
                .filter(|l| !l.trim().is_empty())
                .map(PathBuf::from)
                .filter(|p| p.is_dir())
                .take(5)
                .collect()
        })
        .unwrap_or_default()
}

/// Save recent project directories to ~/.atomcode/recent_dirs.txt
fn save_recent_dirs(dirs: &[PathBuf]) {
    let path = atomcode_core::config::Config::config_dir().join("recent_dirs.txt");
    let content: String = dirs.iter()
        .map(|d| d.to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let _ = std::fs::write(&path, content);
}

#[derive(Debug, Clone)]
pub struct InputState {
    pub lines: Vec<String>,
    pub cursor_row: usize,
    pub cursor_col: usize,
}

impl InputState {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_row: 0,
            cursor_col: 0,
        }
    }

    pub fn insert_char(&mut self, c: char) {
        let line = &mut self.lines[self.cursor_row];
        line.insert(self.cursor_col, c);
        self.cursor_col += c.len_utf8();
    }

    pub fn insert_newline(&mut self) {
        let current_line = &self.lines[self.cursor_row];
        let rest = current_line[self.cursor_col..].to_string();
        self.lines[self.cursor_row].truncate(self.cursor_col);
        self.cursor_row += 1;
        self.lines.insert(self.cursor_row, rest);
        self.cursor_col = 0;
    }

    pub fn backspace(&mut self) {
        if self.cursor_col > 0 {
            let line = &mut self.lines[self.cursor_row];
            let prev = line[..self.cursor_col]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            line.remove(prev);
            self.cursor_col = prev;
        } else if self.cursor_row > 0 {
            let current = self.lines.remove(self.cursor_row);
            self.cursor_row -= 1;
            self.cursor_col = self.lines[self.cursor_row].len();
            self.lines[self.cursor_row].push_str(&current);
        }
    }

    /// Forward delete (Delete key)
    pub fn delete_forward(&mut self) {
        let line = &self.lines[self.cursor_row];
        if self.cursor_col < line.len() {
            let next = line[self.cursor_col..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor_col + i)
                .unwrap_or(line.len());
            self.lines[self.cursor_row].drain(self.cursor_col..next);
        } else if self.cursor_row + 1 < self.lines.len() {
            // Join with next line
            let next_line = self.lines.remove(self.cursor_row + 1);
            self.lines[self.cursor_row].push_str(&next_line);
        }
    }

    /// Delete word backward (Ctrl+W)
    pub fn delete_word_backward(&mut self) {
        if self.cursor_col == 0 { return; }
        let line = &self.lines[self.cursor_row];
        let before = &line[..self.cursor_col];
        // Skip trailing whitespace, then skip word chars
        let trimmed = before.trim_end();
        let word_start = trimmed.rfind(|c: char| c.is_whitespace() || c == '/' || c == '.')
            .map(|i| i + 1)
            .unwrap_or(0);
        self.lines[self.cursor_row].drain(word_start..self.cursor_col);
        self.cursor_col = word_start;
    }

    /// Clear from cursor to end of line (Ctrl+K)
    pub fn kill_to_end(&mut self) {
        self.lines[self.cursor_row].truncate(self.cursor_col);
    }

    /// Clear entire line (Ctrl+U)
    pub fn clear_line(&mut self) {
        self.lines[self.cursor_row].clear();
        self.cursor_col = 0;
    }

    /// Move cursor to start of line (Home / Ctrl+A)
    pub fn move_home(&mut self) {
        self.cursor_col = 0;
    }

    /// Move cursor to end of line (End / Ctrl+E)
    pub fn move_end(&mut self) {
        self.cursor_col = self.lines[self.cursor_row].len();
    }

    /// Insert a block of text (paste). Handles multi-line correctly.
    pub fn insert_text(&mut self, text: &str) {
        for (i, chunk) in text.split('\n').enumerate() {
            if i > 0 {
                self.insert_newline();
            }
            for c in chunk.chars() {
                if c != '\r' { // Skip \r from Windows line endings
                    self.insert_char(c);
                }
            }
        }
    }

    pub fn content(&self) -> String {
        self.lines.join("\n")
    }

    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_input_state_new() {
        let input = InputState::new();
        assert_eq!(input.lines, vec![String::new()]);
        assert_eq!(input.cursor_row, 0);
        assert_eq!(input.cursor_col, 0);
    }

    #[test]
    fn test_input_insert_char() {
        let mut input = InputState::new();
        input.insert_char('h');
        input.insert_char('i');
        assert_eq!(input.lines[0], "hi");
        assert_eq!(input.cursor_col, 2);
    }

    #[test]
    fn test_input_newline() {
        let mut input = InputState::new();
        input.insert_char('a');
        input.insert_newline();
        input.insert_char('b');
        assert_eq!(input.lines, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(input.cursor_row, 1);
        assert_eq!(input.cursor_col, 1);
    }

    #[test]
    fn test_input_backspace() {
        let mut input = InputState::new();
        input.insert_char('a');
        input.insert_char('b');
        input.backspace();
        assert_eq!(input.lines[0], "a");
        assert_eq!(input.cursor_col, 1);
    }

    #[test]
    fn test_input_backspace_joins_lines() {
        let mut input = InputState::new();
        input.insert_char('a');
        input.insert_newline();
        input.insert_char('b');
        input.cursor_col = 0;
        input.backspace();
        assert_eq!(input.lines, vec!["ab".to_string()]);
        assert_eq!(input.cursor_row, 0);
        assert_eq!(input.cursor_col, 1);
    }

    #[test]
    fn test_input_content() {
        let mut input = InputState::new();
        input.insert_char('a');
        input.insert_newline();
        input.insert_char('b');
        assert_eq!(input.content(), "a\nb");
    }

    #[test]
    fn test_input_clear() {
        let mut input = InputState::new();
        input.insert_char('x');
        input.clear();
        assert_eq!(input.lines, vec![String::new()]);
        assert_eq!(input.cursor_row, 0);
        assert_eq!(input.cursor_col, 0);
    }
}
