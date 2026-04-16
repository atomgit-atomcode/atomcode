use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use tokio::sync::mpsc;

use atomcode_core::agent::{AgentCommand, AgentEvent, AgentHandle};
use atomcode_core::config::Config;
use atomcode_core::conversation::Conversation;
use atomcode_core::provider::LlmProvider;
use atomcode_core::session::{Session, SessionManager, SessionMeta};
use atomcode_core::tool::{ToolCall, ToolContext, ToolResult};

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

/// State for issue creation input form.
#[derive(Debug, Clone)]
pub struct IssueInputState {
    pub title: String,
    pub description: String,
    pub title_cursor: usize,
    pub desc_cursor: usize,
    pub cursor_field: IssueField,
    pub submitting: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum IssueField {
    Title,
    Description,
}

impl IssueInputState {
    pub fn new() -> Self {
        Self {
            title: String::new(),
            description: String::new(),
            title_cursor: 0,
            desc_cursor: 0,
            cursor_field: IssueField::Title,
            submitting: false,
            error: None,
        }
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
    /// Session selector mode - shows session list inline for /resume
    SessionSelector,
    /// Issue input form - create issue on AtomGit
    IssueInput(IssueInputState),
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
    /// Flag to trigger SSO login flow.
    pub pending_wecom_login: bool,
    /// Last key event timestamp — for paste detection when bracketed paste isn't available.
    pub last_key_time: Instant,
    /// Count of consecutive typable keys arriving <10ms apart. Only after
    /// `RAPID_PASTE_THRESHOLD` in a row do we switch to buffering mode — so a
    /// human-scale burst of 1-2 fast keystrokes still shows instantly in the
    /// input box instead of being swallowed until a 50ms pause / tick.
    pub rapid_streak: usize,
    /// Buffer of rapid-fire typed keys — coalesces Windows terminal paste bursts
    /// into one operation. Only populated once `rapid_streak >= RAPID_PASTE_THRESHOLD`.
    /// Flushed on slow key or Tick. Big enough bursts are promoted to
    /// `pasted_text` as a reference.
    pub rapid_buf: String,
    /// User inputs queued via AppendInput during the current streaming turn.
    /// Shown in the chat area as dim "queued" previews so the user gets immediate
    /// feedback that Enter was accepted, even though the text won't reach the LLM
    /// until the next turn starts. Cleared on TurnComplete / TurnCancelled — by
    /// then the agent has injected them as real user messages.
    pub pending_appends: Vec<String>,
    /// Files attached to the next message (detected from pasted paths).
    pub attached_files: Vec<crate::file_attach::AttachedFile>,
    /// Blocks of pasted text staged for the next message. One entry per paste
    /// event (Ctrl+V, bracketed paste, rapid-key burst). Multiple pastes are
    /// preserved — `send_message` joins them with `\n\n` before dispatch.
    pub pasted_blocks: Vec<String>,
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
    /// Cached step count for current turn (LLM round-trips, Claude Code-compatible).
    pub current_step_count: usize,
    /// Total individual tool calls in the current turn.
    pub current_tool_call_count: usize,
    /// Cached tool info string for ToolExecuting mode (avoid per-frame JSON parse).
    pub executing_tool_info: String,
    /// Tool name the LLM is currently streaming (name known, args still arriving).
    /// Cleared when ToolCallStarted fires (args fully assembled) or turn ends.
    /// Used by the spinner label so users see "⠋ Preparing write_file…" instead of
    /// an opaque "Generating…" during multi-second tool-call arg streams.
    pub streaming_tool_name: Option<String>,
    /// All tool names currently streaming (LLM emitting args).
    /// Each entry = (tool_name, file_path_hint). Rendered as individual in-flight rows.
    pub streaming_tools: Vec<(String, String)>,
    /// File path hint extracted from partial tool call args (e.g. "src/main.rs").
    pub streaming_tool_hint: String,
    /// Estimated token counts for the current session.
    pub total_tokens: usize,
    /// Tokens used in the current turn.
    pub turn_tokens: usize,
    /// Context budget: tokens used in the last LLM call.
    pub ctx_used_tokens: usize,
    /// Context budget: total context window size.
    pub context_window: usize,
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
    /// Total rendered lines from last render (for scroll handling).
    pub last_total_lines: usize,
    pub provider: Box<dyn LlmProvider>,
    pub config: Config,
    /// CancellationToken for the current streaming/tool task.
    pub cancel_token: tokio_util::sync::CancellationToken,
    /// Channel pair for communicating with the AgentLoop.
    pub agent_handle: AgentHandle,
    /// Display name of the active model (cached so we don't need the provider ref).
    pub model_name: String,
    /// Last git checkpoint SHA for /undo.
    pub last_checkpoint: Option<String>,
    /// Session manager for persistence.
    pub session_manager: SessionManager,
    /// Current session (wraps conversation for persistence).
    pub current_session: Session,
    /// Session selector state: list of session metas + selected index.
    /// selected = 0 means search box is focused, 1+ means session item is selected.
    pub session_selector: Option<(Vec<SessionMeta>, usize)>,
    /// Search filter for session selector.
    pub session_selector_query: String,
    /// Last sent user input - restored to input box if turn is cancelled.
    pub last_sent_input: Option<String>,
    /// Force a full terminal redraw on the next frame. Set after bash tool
    /// completion to overwrite any artifacts left by child processes that
    /// wrote directly to the terminal (e.g. git push hook output via /dev/tty).
    pub needs_full_redraw: bool,
}

impl App {
   pub fn new(
       model_name: String,
       config: Config,
       agent_handle: AgentHandle,
       tool_context: ToolContext,
       working_dir: PathBuf,
       session_to_continue: Option<Session>,
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

       // Initialize session manager and load or create default session
       let session_manager = SessionManager::new(&working_dir);
       let current_session = session_to_continue
           .unwrap_or_else(|| Session::default_session(working_dir.clone()));

       // Create conversation from session messages
       let mut conversation = Conversation::new();
       conversation.messages = current_session.messages.clone();

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
           rapid_streak: 0,
           rapid_buf: String::new(),
           pending_appends: Vec::new(),
           pending_editor: None,
           pending_login: false,
           pending_wecom_login: false,
           attached_files: Vec::new(),
           pasted_blocks: Vec::new(),
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
           current_tool_call_count: 0,
           executing_tool_info: String::new(),
           streaming_tool_name: None,
           streaming_tools: Vec::new(),
           streaming_tool_hint: String::new(),
           turn_start: None,
           first_token_ms: None,
           llm_call_start: None,
           last_completed_tool: String::new(),
           tool_start: None,
           last_turn_duration: None,
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
           ctx_used_tokens: 0,
           context_window: config.providers.get(&config.default_provider)
               .map(|p| p.context_window).unwrap_or(128000),
           suggestion: None,
           render_cache: Vec::new(),
           render_cache_msg_count: 0,
           selection: TextSelection::new(),
           last_rendered_scroll: 0,
           last_viewport_height: 0,
           last_total_lines: 0,
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
                   max_tokens: None,
                   ephemeral: false,
               }).unwrap_or_else(|_| {
                   // Fallback: should never reach production path since AgentLoop handles LLM
                   panic!("Failed to create placeholder provider")
               })
           },
               config,
               cancel_token: tokio_util::sync::CancellationToken::new(),
               agent_handle,
               model_name,
               last_checkpoint: None,
                session_manager,
                current_session,
                session_selector: None,
                             session_selector_query: String::new(),
                             last_sent_input: None,
                             needs_full_redraw: false,
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
            if tool_calls.iter().any(|c| c.name == "create_file" || c.name == "edit_file")
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
            // Update session manager and load latest session for this project
            self.session_manager = SessionManager::new(&self.working_dir);
            self.current_session = self.session_manager.latest()
                .ok().flatten()
                .unwrap_or_else(|| Session::default_session(self.working_dir.clone()));
            // Restore conversation from loaded session
            self.conversation = atomcode_core::conversation::Conversation::new();
            self.conversation.messages = self.current_session.messages.clone();
        } else {
            // Directory doesn't exist — create it, git init, and add ATOMCODE.md
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

            // Create ATOMCODE.md with project name (seed file for project instructions;
            // atomcode's prompt builder reads `.atomcode.md` or `ATOMCODE.md`).
            let project_name = new_path.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "project".to_string());
            let atomcode_md = format!("# {}\n\nNew project.\n", project_name);
            let _ = std::fs::write(new_path.join("ATOMCODE.md"), &atomcode_md);

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
                    "Created and switched to `{}`\n\nInitialized git repo and ATOMCODE.md.",
                    self.working_dir.display()
                );
                self.conversation.push_delta(&summary);
                self.conversation.finalize_stream();
                self.project_context_cache = None;
                // Update session manager for new project directory
                self.session_manager = SessionManager::new(&self.working_dir);
                // Create a fresh default session for the new project (don't save empty session)
                self.current_session = Session::default_session(self.working_dir.clone());
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

    /// Sync in-memory config (including ephemeral providers) to AgentLoop.
    /// Call this after any config change that should take effect immediately.
    pub(crate) fn sync_config_to_agent(&self) {
        let _ = self.agent_handle.cmd_tx.send(
            AgentCommand::ReloadConfig(self.config.clone())
        );
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
            AgentEvent::ToolCallStreaming { name, hint } => {
                // Tool name known, args still streaming. Show it in the spinner label
                // so the user isn't staring at "Generating…" for the whole args window.
                // Track TTFT here too — a tool call is a first-token signal just like text.
                if self.first_token_ms.is_none() {
                    if let Some(start) = self.llm_call_start {
                        self.first_token_ms = Some(start.elapsed().as_millis() as u64);
                    }
                }
                self.streaming_tool_name = Some(name.clone());
                if !hint.is_empty() {
                    // Hint update for the current tool — update in-place
                    if let Some(entry) = self.streaming_tools.last_mut() {
                        entry.1 = hint.clone();
                    }
                    self.streaming_tool_hint = hint;
                } else {
                    // New tool name arriving — only keep this one visible.
                    // Earlier tools already transitioned to ToolCallStarted
                    // and are rendered by the normal tool call display above.
                    self.streaming_tools.clear();
                    self.streaming_tools.push((name, String::new()));
                }
            }
            AgentEvent::ToolCallStarted { id, name, arguments } => {
                // Args fully assembled — streaming phase done for this tool.
                // Remove from streaming list (first match only).
                if let Some(pos) = self.streaming_tools.iter().position(|(n, _)| n == &name) {
                    self.streaming_tools.remove(pos);
                }
                if self.streaming_tools.is_empty() {
                    self.streaming_tool_name = None;
                    self.streaming_tool_hint.clear();
                }
                self.current_tool_call_count += 1;
                // Track TTFT — tool call is also a "first token" from the LLM
                if self.first_token_ms.is_none() {
                    if let Some(start) = self.llm_call_start {
                        self.first_token_ms = Some(start.elapsed().as_millis() as u64);
                    }
                }
                // Use the provider's real call id so ToolCallResult can pair with this
                // specific tool call — regardless of timing (PhaseChange(Thinking) bumping
                // step_count between start and result, or parallel tool calls sharing a step).
                let call = ToolCall {
                    id: id.clone(),
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
            AgentEvent::ToolCallResult { call_id, name, output, success, duration: _ } => {
                // Format a short result summary for spinner display
                let icon = if success { "\u{2713}" } else { "\u{2717}" };
                let first_line = output.lines().next().unwrap_or("").chars().take(40).collect::<String>();
                self.last_completed_tool = format!("{} {} {}", icon, name, first_line);
                // Bash tools may leave terminal artifacts from child processes
                // writing directly to /dev/tty (e.g. git push hook output).
                // Force a full redraw to overwrite any such artifacts.
                if name == "bash" {
                    self.needs_full_redraw = true;
                }
                // Use the same call_id the agent recorded on ToolCallStarted so chat_panel's
                // "in-flight tool call" detection (call.id ∈ completed_call_ids) matches.
                self.conversation.add_tool_result(ToolResult {
                    call_id,
                    output,
                    success,
                });
                // Clear the tool-info label so the spinner doesn't keep flashing
                // "Edit File: xxx" while the LLM is streaming its follow-up text.
                // Without this the label stays stale until the next tool call overwrites
                // it — users see "edit done but spinner still says edit" (2026-04-13 bug).
                self.executing_tool_info.clear();
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
            AgentEvent::TurnComplete { duration, total_tokens: _, turn_count: _, tool_call_count: _, stop_reason: _ } => {
                // Clear any lingering streaming tool state — turn is over.
                self.streaming_tool_name = None;
                self.streaming_tools.clear();
                self.streaming_tool_hint.clear();
                self.executing_tool_info.clear();
                // Orphan tool_call guard: if the turn ended but some tool_calls in
                // conversation don't have matching ToolResults, the in-flight renderer
                // will keep animating their spinner icon forever (2026-04-13 bug:
                // "edit 完成后 spinner 还在转"). This can happen when agent-core merges
                // parallel tool_calls (merge_edit_calls) — emits N ToolCallStarted but
                // only 1 ToolCallResult for the merged call. Synthesize placeholders so
                // the UI knows those calls are done.
                {
                    use atomcode_core::conversation::message::MessageContent;
                    let mut existing_results: std::collections::HashSet<String> = std::collections::HashSet::new();
                    for msg in &self.conversation.messages {
                        match &msg.content {
                            MessageContent::ToolResult(r) => { existing_results.insert(r.call_id.clone()); }
                            MessageContent::ToolResultRef(r) => { existing_results.insert(r.call_id.clone()); }
                            _ => {}
                        }
                    }
                    let mut orphans: Vec<String> = Vec::new();
                    for msg in &self.conversation.messages {
                        if let MessageContent::AssistantWithToolCalls { tool_calls, .. } = &msg.content {
                            for call in tool_calls {
                                if !existing_results.contains(&call.id) {
                                    orphans.push(call.id.clone());
                                }
                            }
                        }
                    }
                    for orphan_id in orphans {
                        self.conversation.add_tool_result(ToolResult {
                            call_id: orphan_id,
                            output: "[merged into an adjacent tool call]".to_string(),
                            success: true,
                        });
                    }
                }
                // Finalize stream FIRST so auto-summary TextDelta becomes a message
                self.conversation.finalize_stream();
                // Final assistant text logging is handled by atomcode-core's DatalogWriter
                // (see TurnResult::Responded in agent/mod.rs). TUI no longer writes datalog.
                self.mode = AppMode::Normal;
                self.last_turn_duration = Some(duration);
                self.turn_start = None;
                self.render_cache_msg_count = 0; // Invalidate cache
                // Drop queued append previews — the next turn (if any) will pick
                // these up from the agent's pending_input and render them as real
                // user messages, so the previews would duplicate otherwise.
                self.pending_appends.clear();
                self.suggestion = self.generate_suggestion();
                self.at_bottom = true;
                // Auto-save session after each turn
                self.current_session.messages = self.conversation.messages.clone();
                self.current_session.touch();
                // Auto-name session from first user message if still default
                if self.current_session.name == "default" || self.current_session.name.starts_with("session-") {
                    if let Some(first_user_msg) = self.conversation.messages.iter().find(|m| {
                        matches!(m.role, atomcode_core::conversation::message::Role::User)
                    }) {
                        if let Some(text) = first_user_msg.text() {
                            // Generate name from first user message (truncate to 40 chars)
                            let name: String = text
                                .lines()
                                .next()
                                .unwrap_or("")
                                .chars()
                                .take(40)
                                .collect();
                            if !name.is_empty() {
                                self.current_session.name = name;
                            }
                        }
                    }
                }
                // Only save if session has messages (don't save empty default sessions)
                if !self.current_session.messages.is_empty() {
                    let _ = self.session_manager.save(&self.current_session);
                }
            }
            AgentEvent::TurnCancelled { messages } => {
                // User cancelled - sync the cleaned conversation from agent
                self.conversation.messages = messages;
                self.conversation.stream_buffer = None; // Clear any partial stream
                self.mode = AppMode::Normal;
                self.turn_start = None;
                self.render_cache.clear();
                self.render_cache_msg_count = 0;
                self.at_bottom = true;
                // Queued previews are dropped on cancel — the agent side also
                // discards its pending_input when the turn cancels.
                self.pending_appends.clear();
                // Sync to session
                self.current_session.messages = self.conversation.messages.clone();
                // If cancelled turn leaves session empty, delete the saved session file
                // (session was saved during TurnComplete, need to clean up)
                if self.current_session.messages.is_empty() {
                    let _ = self.session_manager.delete(&self.current_session.id);
                }
                // Restore input box with the cancelled message for easy editing
                if let Some(input) = self.last_sent_input.take() {
                    self.input.insert_text(&input);
                }
            }
            AgentEvent::Error(e) => {
                self.streaming_tool_name = None;
                self.streaming_tools.clear();
                self.executing_tool_info.clear();
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
                if usage.cached_tokens > 0 {
                }
            }
            AgentEvent::WorkingDirChanged(new_dir) => {
                // Only /cd (user command) triggers this — LLM tools cannot change working dir.
                self.previous_working_dir = Some(self.working_dir.clone());
                self.working_dir = new_dir.clone();
                self.project_context_cache = None;
                // Sync recent dirs + config so /cd list and next startup remember this dir
                self.recent_dirs.retain(|d| d != &new_dir);
                self.recent_dirs.insert(0, new_dir.clone());
                self.recent_dirs.truncate(5);
                save_recent_dirs(&self.recent_dirs);
                self.config.default_workdir = Some(new_dir.to_string_lossy().to_string());
                let _ = self.config.save(&Config::default_path());
            }
            AgentEvent::ContextStats { system_tokens, sent_tokens, dropped_tokens: _, working_set_tokens: _, total_messages: _ } => {
                self.ctx_used_tokens = system_tokens + sent_tokens;
            }
            AgentEvent::SubAgentProgress { file, status } => {
                // Claude Code style parallel task display
                let (_icon, line) = if file.is_empty() {
                    // Header message
                    ("".to_string(), format!("\n  {}", status))
                } else if status.starts_with("done") {
                    ("\u{2713}".to_string(), format!("\n  \u{2713} {} \u{2014} {}", file, status))
                } else if status.starts_with("failed") || status.starts_with("timeout") {
                    ("\u{2717}".to_string(), format!("\n  \u{2717} {} \u{2014} {}", file, status))
                } else {
                    ("\u{25b8}".to_string(), format!("\n  \u{25b8} {} \u{2014} {}", file, status))
                };
                self.conversation.push_delta(&line);
            }
        }
    }

    pub fn handle_event(&mut self, event: AppEvent, event_tx: &mpsc::UnboundedSender<AppEvent>) {
        match event {
            AppEvent::Key(key) => {
                self.selection.has_selection = false;

                // Paste detection for terminals without bracketed paste. On Windows
                // we keep bracketed paste DISABLED (it mis-parsed Enter), so a paste
                // arrives as a burst of individual Key events. Without coalescing,
                // each char triggers String::insert (O(n)) + a full render → O(n²)
                // freeze on long pastes, and no paste reference is shown.
                //
                // Strategy: count consecutive typable keys arriving <10ms apart. Only
                // after `RAPID_PASTE_THRESHOLD` in a row do we switch to buffering
                // mode — the first 1-2 fast keystrokes still go into the input box
                // immediately so human typing bursts feel instant. Once in buffer
                // mode, further rapid keys accumulate in `rapid_buf`; a slow key or
                // a Tick flushes it: large bursts (>200 chars or >3 newlines) become
                // `pasted_text` (shown as `[pasted N chars]`), small bursts append to
                // the input via a single `insert_text` call.
                const RAPID_PASTE_THRESHOLD: usize = 3;

                let now = Instant::now();
                let interval_ms = now.duration_since(self.last_key_time).as_millis();
                self.last_key_time = now;

                // Provider manager / model selector / issue input have their own input —
                // skip fast-paste detection and Ctrl+V so keys reach their handler.
                let is_overlay_mode = matches!(self.mode, AppMode::ProviderManager | AppMode::ModelSelector)
                    || matches!(self.mode, AppMode::IssueInput(_));

                let is_typable_nomods = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
                let is_typable = is_typable_nomods && matches!(key.code, KeyCode::Char(_) | KeyCode::Enter);
                let paste_eligible = !is_overlay_mode && is_typable
                    && !matches!(self.mode, AppMode::WaitingApproval(_) | AppMode::Exiting);

                if paste_eligible && interval_ms < 10 {
                    self.rapid_streak += 1;
                } else {
                    // Streak broken — flush any accumulated burst so it lands in
                    // the right place before this key is handled, then reset.
                    if !self.rapid_buf.is_empty() {
                        self.flush_rapid_buf();
                    }
                    self.rapid_streak = 0;
                }

                // Once the streak crosses the threshold, divert the current key
                // into the paste buffer instead of the input box.
                if paste_eligible && self.rapid_streak >= RAPID_PASTE_THRESHOLD {
                    match key.code {
                        KeyCode::Enter => self.rapid_buf.push('\n'),
                        KeyCode::Char(c) => self.rapid_buf.push(c),
                        _ => unreachable!(),
                    }
                    self.suggestion = None;
                    return;
                }

                // Ctrl+V: clipboard paste
                if key.modifiers == KeyModifiers::CONTROL && key.code == KeyCode::Char('v') {
                    if let Some(text) = read_clipboard() {
                        if is_overlay_mode {
                            // Forward paste to provider manager's input buffer
                            if let Some(ref mut mgr) = self.provider_mgr {
                                mgr.input_buf.push_str(&text);
                            }
                        } else {
                            self.stage_paste(&text);
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
                    self.stage_paste(&text);
                    self.suggestion = None;
                }
            }
            AppEvent::Mouse(mouse) => {
                self.handle_mouse(mouse);
            }
            AppEvent::Resize(_, _) => {}
            AppEvent::Tick => {
                self.tick_count = self.tick_count.wrapping_add(1);
                // Flush a settled rapid-paste burst — if the user pasted and then
                // stopped typing, we need to materialize the buffer without waiting
                // for a follow-up keystroke.
                if !self.rapid_buf.is_empty()
                    && Instant::now().duration_since(self.last_key_time)
                        > std::time::Duration::from_millis(50)
                {
                    self.flush_rapid_buf();
                }
            }
            AppEvent::IssueCreated { success, message } => {
                if success {
                    // Use ASCII checkmark since emoji gets stripped by markdown renderer
                    self.conversation.push_delta(&format!(
                        "**[OK] Issue created successfully!**\n\n[View Issue]({})",
                        message
                    ));
                } else {
                    self.conversation.push_delta(&format!(
                        "**[ERROR] Failed to create issue**\n\nError: {}",
                        message
                    ));
                }
                self.conversation.finalize_stream();
                // Ensure we switch to Normal mode after handling the event
                self.mode = AppMode::Normal;
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
            AppMode::SessionSelector => self.handle_key_session_selector(key),
            AppMode::IssueInput(ref state) => self.handle_key_issue_input(key, state.clone(), event_tx),
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
                    // During streaming: queue user input for AFTER the current turn ends.
                    // Do NOT inject into assistant stream — that mixes roles and causes
                    // the model to treat user input as its own reasoning (e.g., auto-selecting
                    // options without waiting for confirmation).
                    let content = self.input.content();
                    if !content.trim().is_empty() {
                        let _ = self.agent_handle.cmd_tx.send(
                            atomcode_core::agent::AgentCommand::AppendInput(content.clone())
                        );
                        // Mirror the queue locally so the chat panel can show a
                        // "queued" preview — without this, Enter during streaming
                        // gives zero visual feedback and looks broken.
                        self.pending_appends.push(content);
                        self.input.clear();
                        self.render_cache_msg_count = 0;
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
                    self.sync_config_to_agent();
                }
                self.mode = AppMode::Normal;
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = AppMode::Normal;
            }
            _ => {}
        }
    }
    fn handle_key_session_selector(&mut self, key: KeyEvent) {
        if let Some((ref sessions, ref mut selected)) = self.session_selector {
            // Filter sessions by query for navigation
            let filtered: Vec<usize> = sessions.iter().enumerate()
                .filter(|(_, s)| self.session_selector_query.is_empty() 
                    || s.name.to_lowercase().contains(&self.session_selector_query.to_lowercase()))
                .map(|(i, _)| i)
                .collect();
            
            // Total items = search row (index 0) + filtered sessions (index 1+)
            let total_items = 1 + filtered.len();
            
            match key.code {
                KeyCode::Up => {
                    if *selected > 0 {
                        *selected -= 1;
                    } else {
                        // Wrap to last item
                        *selected = total_items.saturating_sub(1);
                    }
                }
                KeyCode::Down => {
                    if *selected + 1 < total_items {
                        *selected += 1;
                    } else {
                        // Wrap to search row
                        *selected = 0;
                    }
                }
                KeyCode::Enter => {
                    // Only load session if we're on a session item (selected >= 1)
                    if *selected >= 1 {
                        let filtered_idx = *selected - 1;
                        if filtered_idx < filtered.len() {
                            let session_idx = filtered[filtered_idx];
                            let session_id = sessions[session_idx].id.clone();
                            self.session_selector = None;
                            self.session_selector_query.clear();
                            if let Ok(session) = self.session_manager.load(&session_id) {
                                self.current_session = session;
                                self.conversation.messages = self.current_session.messages.clone();
                                self.render_cache.clear();
                                self.render_cache_msg_count = 0;
                                self.scroll_offset = 0;
                                self.at_bottom = true;
                                // Calculate total lines for scroll to work immediately
                                self.last_total_lines = crate::ui::chat_panel::total_lines(&self.conversation);
                                let _ = self.agent_handle.cmd_tx.send(AgentCommand::SetMessages(self.current_session.messages.clone()));
                            }
                            self.mode = AppMode::Normal;
                        }
                    }
                    // If on search row (selected == 0), Enter does nothing
                }
                KeyCode::Esc => {
                    self.session_selector = None;
                    self.session_selector_query.clear();
                    self.mode = AppMode::Normal;
                }
                KeyCode::Backspace => {
                    // Always allow backspace in search query, regardless of selection
                    self.session_selector_query.pop();
                    // Reset selection to first matching session (index 1)
                    if !filtered.is_empty() {
                        *selected = 1; // First session after search row
                    }
                }
                KeyCode::Char(c) => {
                    // Typing always goes to search query
                    self.session_selector_query.push(c);
                    // Auto-select first matching session (index 1 = first session)
                    if !filtered.is_empty() {
                        *selected = 1;
                    } else {
                        *selected = 0; // Stay on search row if no matches
                    }
                }
                _ => {}
            }
        }
    }

    fn handle_key_issue_input(&mut self, key: KeyEvent, state: IssueInputState, event_tx: &mpsc::UnboundedSender<AppEvent>) {
        match key.code {
            KeyCode::Tab => {
                // Toggle between title and description fields
                let cursor_field = state.cursor_field;
                let desc_len = state.description.chars().count();
                let title_len = state.title.chars().count();
                let mut new_state = state;
                match cursor_field {
                    IssueField::Title => {
                        new_state.cursor_field = IssueField::Description;
                        // Position cursor at end of description
                        new_state.desc_cursor = desc_len;
                    }
                    IssueField::Description => {
                        new_state.cursor_field = IssueField::Title;
                        // Position cursor at end of title
                        new_state.title_cursor = title_len;
                    }
                }
                self.mode = AppMode::IssueInput(new_state);
            }
            KeyCode::Enter => {
                // Submit if in description field, or move to description if in title
                match state.cursor_field {
                    IssueField::Title => {
                        let desc_cursor = state.description.chars().count();
                        let mut new_state = state;
                        new_state.cursor_field = IssueField::Description;
                        new_state.desc_cursor = desc_cursor;
                        self.mode = AppMode::IssueInput(new_state);
                    }
                    IssueField::Description => {
                        // Submit the issue
                        if state.title.trim().is_empty() {
                            let mut new_state = state;
                            new_state.error = Some("Title is required".to_string());
                            self.mode = AppMode::IssueInput(new_state);
                        } else if state.description.trim().is_empty() {
                            let mut new_state = state;
                            new_state.error = Some("Description is required".to_string());
                            self.mode = AppMode::IssueInput(new_state);
                        } else if state.submitting {
                            // Already submitting, ignore
                        } else {
                            let mut new_state = state.clone();
                            new_state.submitting = true;
                            new_state.error = None;
                            self.mode = AppMode::IssueInput(new_state);
                            
                            // Get access token
                            let auth_path = atomcode_core::config::Config::config_dir().join("auth.toml");
                            if let Ok(content) = std::fs::read_to_string(&auth_path) {
                                let access_token = content.lines()
                                    .find(|line| line.starts_with("access_token"))
                                    .and_then(|line| line.split('=').nth(1))
                                    .map(|s| s.trim().trim_matches('"').to_string());
                                
                                if let Some(token) = access_token {
                                    let title = state.title.clone();
                                    let description = state.description.clone();
                                    let tx = event_tx.clone();
                                    
                                    // Spawn async task to submit issue
                                    let _ = std::thread::spawn(move || {
                                        let rt = tokio::runtime::Runtime::new().unwrap();
                                        let result = rt.block_on(async {
                                            submit_issue_to_gitcode(&token, &title, &description).await
                                        });
                                        
                                        // Send result back to main event loop
                                        match result {
                                            Ok(issue_url) => {
                                                let _ = tx.send(AppEvent::IssueCreated {
                                                    success: true,
                                                    message: issue_url,
                                                });
                                            }
                                            Err(err) => {
                                                let _ = tx.send(AppEvent::IssueCreated {
                                                    success: false,
                                                    message: err,
                                                });
                                            }
                                        }
                                    });
                                    // Keep IssueInput mode with submitting=true until event arrives
                                    // Mode will switch to Normal in IssueCreated handler
                                } else {
                                    let mut new_state = state;
                                    new_state.submitting = false;
                                    new_state.error = Some("Failed to read access token".to_string());
                                    self.mode = AppMode::IssueInput(new_state);
                                }
                            }
                        }
                    }
                }
            }
            KeyCode::Esc => {
                self.mode = AppMode::Normal;
            }
            KeyCode::Up => {
                // Move from Description to Title
                if state.cursor_field == IssueField::Description {
                    let title_len = state.title.chars().count();
                    let mut new_state = state;
                    new_state.cursor_field = IssueField::Title;
                    // Position cursor at end of title
                    new_state.title_cursor = title_len;
                    self.mode = AppMode::IssueInput(new_state);
                }
            }
            KeyCode::Down => {
                // Move from Title to Description
                if state.cursor_field == IssueField::Title {
                    let desc_len = state.description.chars().count();
                    let mut new_state = state;
                    new_state.cursor_field = IssueField::Description;
                    // Position cursor at end of description
                    new_state.desc_cursor = desc_len;
                    self.mode = AppMode::IssueInput(new_state);
                }
            }
            KeyCode::Left => {
                let field = state.cursor_field;
                let title_cursor = state.title_cursor;
                let desc_cursor = state.desc_cursor;
                let mut new_state = state;
                match field {
                    IssueField::Title => {
                        new_state.title_cursor = title_cursor.saturating_sub(1);
                    }
                    IssueField::Description => {
                        new_state.desc_cursor = desc_cursor.saturating_sub(1);
                    }
                }
                self.mode = AppMode::IssueInput(new_state);
            }
            KeyCode::Right => {
                let field = state.cursor_field;
                let title_cursor = state.title_cursor;
                let desc_cursor = state.desc_cursor;
                let title_len = state.title.chars().count();
                let desc_len = state.description.chars().count();
                let mut new_state = state;
                match field {
                    IssueField::Title => {
                        let max = title_len;
                        new_state.title_cursor = title_cursor.min(max);
                        if new_state.title_cursor < max {
                            new_state.title_cursor += 1;
                        }
                    }
                    IssueField::Description => {
                        let max = desc_len;
                        new_state.desc_cursor = desc_cursor.min(max);
                        if new_state.desc_cursor < max {
                            new_state.desc_cursor += 1;
                        }
                    }
                }
                self.mode = AppMode::IssueInput(new_state);
            }
            KeyCode::Backspace => {
                let field = state.cursor_field;
                let title_cursor = state.title_cursor;
                let desc_cursor = state.desc_cursor;
                let title = state.title.clone();
                let description = state.description.clone();
                let mut new_state = state;
                match field {
                    IssueField::Title => {
                        if title_cursor > 0 {
                            // Delete char before cursor
                            let pos = title_cursor;
                            let chars: Vec<char> = title.chars().collect();
                            new_state.title = chars[..pos-1].iter().chain(chars[pos..].iter()).collect();
                            new_state.title_cursor = pos - 1;
                        }
                    }
                    IssueField::Description => {
                        if desc_cursor > 0 {
                            let pos = desc_cursor;
                            let chars: Vec<char> = description.chars().collect();
                            new_state.description = chars[..pos-1].iter().chain(chars[pos..].iter()).collect();
                            new_state.desc_cursor = pos - 1;
                        }
                    }
                }
                self.mode = AppMode::IssueInput(new_state);
            }
            KeyCode::Char(c) => {
                let field = state.cursor_field;
                let title_cursor = state.title_cursor;
                let desc_cursor = state.desc_cursor;
                let title = state.title.clone();
                let description = state.description.clone();
                let mut new_state = state;
                match field {
                    IssueField::Title => {
                        let pos = title_cursor;
                        let chars: Vec<char> = title.chars().collect();
                        new_state.title = chars[..pos].iter().cloned().chain(std::iter::once(c)).chain(chars[pos..].iter().cloned()).collect();
                        new_state.title_cursor = pos + 1;
                    }
                    IssueField::Description => {
                        let pos = desc_cursor;
                        let chars: Vec<char> = description.chars().collect();
                        new_state.description = chars[..pos].iter().cloned().chain(std::iter::once(c)).chain(chars[pos..].iter().cloned()).collect();
                        new_state.desc_cursor = pos + 1;
                    }
                }
                self.mode = AppMode::IssueInput(new_state);
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
                    self.rebuild_provider();
                    self.sync_config_to_agent();
                    self.provider_mgr = None;
                    self.mode = AppMode::Normal;
                }
                ManagerAction::SetDefault(name) => {
                    self.config.default_provider = name;
                    let config_path = Config::default_path();
                    let _ = self.config.save(&config_path);
                    self.rebuild_provider();
                    self.sync_config_to_agent();
                }
                ManagerAction::Delete(name) => {
                    self.config.providers.remove(&name);
                    if let Some(ref mut mgr) = self.provider_mgr {
                        mgr.refresh_names(&self.config);
                    }
                    let config_path = Config::default_path();
                    let _ = self.config.save(&config_path);
                    self.sync_config_to_agent();
                }
                ManagerAction::Add(name, provider_config) => {
                    self.config.providers.insert(name, provider_config);
                    if let Some(ref mut mgr) = self.provider_mgr {
                        mgr.refresh_names(&self.config);
                    }
                    let config_path = Config::default_path();
                    let _ = self.config.save(&config_path);
                    self.sync_config_to_agent();
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
                    self.sync_config_to_agent();
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
                if !self.pasted_blocks.is_empty() {
                    // Esc peels off one paste block at a time so users can undo
                    // an accidental extra paste without losing the rest.
                    self.pasted_blocks.pop();
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
                self.current_tool_call_count = 0;
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
            _ if self.handle_scroll_keys(key) => {}
            // Plain Up/Down are ALWAYS for history navigation, not multi-line cursor movement.
            // This matches Claude Code's behavior.
            (_, KeyCode::Up) => {
                // Always navigate history (when not empty)
                if !self.input_history.is_empty() {
                    if self.history_index.is_none() {
                        // Stash current input
                        self.history_stash = Some(self.input.content());
                        self.history_index = Some(self.input_history.len().saturating_sub(1));
                    } else if let Some(idx) = self.history_index {
                        if idx > 0 {
                            self.history_index = Some(idx - 1);
                        } else {
                            // Wrap around: oldest -> newest
                            self.history_index = Some(self.input_history.len().saturating_sub(1));
                        }
                    }
                    if let Some(idx) = self.history_index {
                        if let Some(hist) = self.input_history.get(idx).cloned() {
                            self.suggestion = None;
                            self.pasted_blocks.clear();
                            self.load_history_entry(&hist);
                        }
                    }
                }
            }
            (_, KeyCode::Down) => {
                // Always navigate history
                if let Some(idx) = self.history_index {
                    if idx + 1 < self.input_history.len() {
                        self.history_index = Some(idx + 1);
                        let hist = self.input_history[idx + 1].clone();
                        self.suggestion = None;
                        self.pasted_blocks.clear();
                        self.load_history_entry(&hist);
                    } else {
                        // Exit history mode
                        self.history_index = None;
                        self.pasted_blocks.clear();
                        self.input.clear();
                        if let Some(stash) = self.history_stash.take() {
                            for c in stash.chars() { self.input.insert_char(c); }
                        }
                    }
                } else if !self.input_history.is_empty() {
                    // Enter history from newest
                    self.history_stash = Some(self.input.content());
                    self.history_index = Some(0);
                    if let Some(hist) = self.input_history.first().cloned() {
                        self.suggestion = None;
                        self.pasted_blocks.clear();
                        self.load_history_entry(&hist);
                    }
                }
            }
            (_, KeyCode::Backspace) => {
                self.input.backspace();
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
                    let total = self.last_total_lines;
                    self.scroll_offset = total.saturating_sub(3);
                    self.at_bottom = false;
                } else {
                    self.scroll_offset = self.scroll_offset.saturating_sub(3);
                }
                true
            }
            (KeyModifiers::CONTROL, KeyCode::Down) => {
                let vh = self.last_viewport_height as usize;
                let max_scroll = self.last_total_lines.saturating_sub(vh);
                self.scroll_offset = (self.scroll_offset + 3).min(max_scroll);
                if self.scroll_offset >= max_scroll {
                    self.at_bottom = true;
                }
                true
            }
            (_, KeyCode::PageUp) => {
                if self.at_bottom {
                    let total = self.last_total_lines;
                    self.scroll_offset = total.saturating_sub(20);
                    self.at_bottom = false;
                } else {
                    self.scroll_offset = self.scroll_offset.saturating_sub(20);
                }
                true
            }
            (_, KeyCode::PageDown) => {
                let vh = self.last_viewport_height as usize;
                let max_scroll = self.last_total_lines.saturating_sub(vh);
                self.scroll_offset = (self.scroll_offset + 20).min(max_scroll);
                if self.scroll_offset >= max_scroll {
                    self.at_bottom = true;
                }
                true
            }
            _ => false,
        }
    }

    /// Handle mouse events: scroll wheel only.
    /// Mouse selection is now handled natively by the terminal (mouse capture is disabled during drag).
    fn handle_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            // ── Scroll wheel ──
            MouseEventKind::ScrollUp => {
                if self.at_bottom {
                    let total = self.last_total_lines;
                    self.scroll_offset = total.saturating_sub(3);
                    self.at_bottom = false;
                } else {
                    self.scroll_offset = self.scroll_offset.saturating_sub(3);
                }
            }
            MouseEventKind::ScrollDown => {
                let vh = self.last_viewport_height as usize;
                let max_scroll = self.last_total_lines.saturating_sub(vh);
                self.scroll_offset = (self.scroll_offset + 3).min(max_scroll);
                if self.scroll_offset >= max_scroll {
                    self.at_bottom = true;
                }
            }
            _ => {}
        }
    }

    /// Extract text from the rendered content between the selection coordinates,
    /// then copy it to the system clipboard.
    fn copy_selection_to_clipboard(&self) {
        let text = self.extract_selection_text();
        if text.is_empty() {
            return;
        }
        let _ = copy_to_clipboard(&text);
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
            "/undo" => {
                if let Some(ref checkpoint) = self.last_checkpoint.clone() {
                    let (ok, msg) = atomcode_core::agent::git_checkpoint::restore_checkpoint(
                        &self.working_dir, checkpoint,
                    );
                    self.conversation.push_delta(&msg);
                    self.conversation.finalize_stream();
                    if ok { self.last_checkpoint = None; }
                } else {
                    self.conversation.push_delta("No checkpoint available. /undo works after the agent has made edits.");
                    self.conversation.finalize_stream();
                }
                return true;
            }
            "/diff" => {
                let output = std::process::Command::new("git")
                    .args(["diff"])
                    .current_dir(&self.working_dir)
                    .output();
                match output {
                    Ok(o) => {
                        let diff = String::from_utf8_lossy(&o.stdout);
                        if diff.trim().is_empty() {
                            self.conversation.push_delta("No uncommitted changes.");
                        } else {
                            self.conversation.push_delta(&format!("```diff\n{}\n```", diff.trim()));
                        }
                    }
                    Err(e) => {
                        self.conversation.push_delta(&format!("git diff failed: {}", e));
                    }
                }
                self.conversation.finalize_stream();
                return true;
            }
            "/cost" => {
                let ctx_str = if self.context_window > 0 && self.ctx_used_tokens > 0 {
                    format!("Context: {}K / {}K", self.ctx_used_tokens / 1000, self.context_window / 1000)
                } else {
                    "Context: (not yet measured)".to_string()
                };
                self.conversation.push_delta(&format!(
                    "**Session token usage**\n- Total output tokens: {}\n- Current turn tokens: {}\n- {}",
                    self.total_tokens, self.turn_tokens, ctx_str,
                ));
                self.conversation.finalize_stream();
                return true;
            }
            "/clear" => {
                // Delete the old session file if it exists and is not the default session
                if self.current_session.name != "default" {
                    let _old_id = self.current_session.id.clone();
                }
                // Reset session to a fresh default session (don't save empty session)
                self.current_session = Session::default_session(self.working_dir.clone());
                let _ = self.session_manager.save(&self.current_session);
                // Clear agent's conversation context
                let _ = self.agent_handle.cmd_tx.send(AgentCommand::ClearConversation);
                // Clear UI conversation
                self.conversation.messages.clear();
                self.render_cache.clear();
                self.render_cache_msg_count = 0;
                // Clear the turn log (deletes the current log file)
                // Add a placeholder message to indicate conversation was cleared
                self.conversation.push_delta("(conversation cleared)");
                self.conversation.finalize_stream();
                return true;
            }
            "/resume" => {
                // Open session selector inline
                match self.session_manager.list() {
                    Ok(all_sessions) => {
                        // Filter out sessions with no messages (cleared sessions)
                        let sessions: Vec<_> = all_sessions.into_iter()
                            .filter(|s| s.message_count > 0)
                            .collect();
                        if sessions.is_empty() {
                            self.conversation.push_delta("No previous sessions found. Start a conversation first.");
                            self.conversation.finalize_stream();
                        } else {
                            // selected = 0 is search row, 1+ is session items
                            // Default to first session (index 1) so user can press Enter immediately
                            let default_selected = if sessions.is_empty() { 0 } else { 1 };
                            self.session_selector = Some((sessions, default_selected));
                            self.session_selector_query.clear();
                            self.mode = AppMode::SessionSelector;
                            self.conversation.messages.pop(); // Remove the /resume user message
                        }
                    }
                    Err(e) => {
                        self.conversation.push_delta(&format!("Failed to list sessions: {}", e));
                        self.conversation.finalize_stream();
                    }
                }
            }
            "/session" => {
                // Create a new session (fresh conversation)
                self.current_session = Session::new(self.working_dir.clone());
                self.conversation = Conversation::new();
                self.render_cache.clear();
                self.render_cache_msg_count = 0;
                self.scroll_offset = 0;
                self.at_bottom = true;
                // Sync with agent (clear conversation)
                let _ = self.agent_handle.cmd_tx.send(AgentCommand::ClearConversation);
                // Don't save empty session - will be saved when first user message is sent
                // Remove the /session user message
                self.conversation.messages.pop();
            }
            "/login" => {
                self.pending_login = true;
                self.conversation.push_delta("Opening browser for AtomGit login...");
                self.conversation.finalize_stream();
            }
            "/login-with-sso" => {
                self.pending_wecom_login = true;
                self.conversation.push_delta("Opening browser for SSO login...");
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
                
                // 2. Remove AtomGit provider from in-memory config
                let atomgit_key = self.config.providers.keys()
                    .find(|k| k.to_lowercase() == "atomgit")
                    .cloned();

                if let Some(key) = atomgit_key {
                    self.config.providers.remove(&key);
                    messages.push("AtomGit provider removed.".to_string());

                    // If default provider was AtomGit, switch to another
                    if self.config.default_provider.to_lowercase() == "atomgit" {
                        if let Some(new_default) = self.config.providers.keys().next().cloned() {
                            self.config.default_provider = new_default.clone();
                            messages.push(format!("Switched default provider to: {}", new_default));
                        }
                    }

                    let _ = self.config.save(&Config::default_path());
                    self.rebuild_provider();
                    self.sync_config_to_agent();
                    logged_out = true;
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
            "/issue" => {
                // Check login status first
                let auth_path = atomcode_core::config::Config::config_dir().join("auth.toml");
                let mut logged_in = false;
                
                if auth_path.exists() {
                    if let Ok(content) = std::fs::read_to_string(&auth_path) {
                        // Check if access_token exists
                        if content.lines().any(|line| line.starts_with("access_token")) {
                            logged_in = true;
                        }
                    }
                }
                
                if logged_in {
                    // Switch to issue input mode
                    self.mode = AppMode::IssueInput(IssueInputState::new());
                    self.conversation.messages.pop(); // Remove the /issue user message
                } else {
                    self.conversation.push_delta("**Not logged in.**\n\nPlease use `/login` to authenticate with AtomGit first.");
                    self.conversation.finalize_stream();
                }
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
                help.push_str("  `Drag` — Native text selection and copy\n");
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
                        self.current_tool_call_count = 0;
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

    /// Load a history entry into input. Long entries shown as a paste reference.
    fn load_history_entry(&mut self, entry: &str) {
        self.input.clear();
        self.pasted_blocks.clear();
        let line_count = entry.lines().count();
        if line_count > 3 || entry.len() > 200 {
            // Long entry — show as pasted reference
            self.pasted_blocks.push(entry.to_string());
        } else {
            // Short entry — load inline using insert_text to preserve newlines
            self.input.insert_text(entry);
        }
    }

    /// Flush a rapid-key burst (Windows paste without bracketed paste).
    fn flush_rapid_buf(&mut self) {
        if self.rapid_buf.is_empty() {
            return;
        }
        let buf = std::mem::take(&mut self.rapid_buf);
        self.stage_paste(&buf);
        self.suggestion = None;
    }

    /// Single entry point for every paste pathway (Ctrl+V, bracketed paste,
    /// rapid-key burst). Runs file-path extraction first so drag-and-drop of
    /// multiple files from Explorer becomes N attachment tags instead of one
    /// opaque "Pasted text" block. Whatever isn't a valid path falls back to
    /// the usual rule: long remainder → `pasted_blocks`, short → inline input.
    fn stage_paste(&mut self, text: &str) {
        // Windows Explorer Ctrl+C on files populates CF_HDROP on the clipboard.
        // Windows Terminal's Ctrl+V intercepts and only injects CF_UNICODETEXT
        // (at most the last selected filename), so the `text` we receive here
        // is an incomplete representation. Peek CF_HDROP directly — if it has
        // files, that's the authoritative list.
        #[cfg(target_os = "windows")]
        {
            if let Ok(files) = clipboard_win::get_clipboard::<Vec<String>, _>(
                clipboard_win::formats::FileList,
            ) {
                if !files.is_empty() {
                    // Guard against false positives. CF_HDROP sticks around
                    // across unrelated operations — if the user copied files
                    // earlier and then just typed fast enough to trigger
                    // rapid_buf, we'd pull in stale files from the clipboard.
                    // Only consume CF_HDROP if the pasted text actually
                    // contains one of the filenames (or full paths) from it,
                    // which is the signature of a real file-paste event.
                    let related = files.iter().any(|p| {
                        if text.contains(p) {
                            return true;
                        }
                        std::path::Path::new(p)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .map(|fname| !fname.is_empty() && text.contains(fname))
                            .unwrap_or(false)
                    });

                    if related {
                        let as_text = files.join("\n");
                        let (attachments, _remainder) = crate::file_attach::extract_file_paths(
                            &as_text,
                            &self.working_dir,
                        );
                        let mut any_added = false;
                        for file in attachments {
                            if !self.attached_files.iter().any(|f| f.path == file.path) {
                                self.attached_files.push(file);
                                any_added = true;
                            }
                        }
                        if any_added {
                            return;
                        }
                    }
                }
            }
        }

        let normalized = if text.contains('\r') {
            text.replace("\r\n", "\n").replace('\r', "\n")
        } else {
            text.to_string()
        };

        let (files, remainder) = crate::file_attach::extract_file_paths(
            &normalized,
            &self.working_dir,
        );

        for file in files {
            if !self.attached_files.iter().any(|f| f.path == file.path) {
                self.attached_files.push(file);
            }
        }

        let remainder = remainder.trim();
        if remainder.is_empty() {
            return;
        }
        if remainder.lines().count() > 3 || remainder.len() > 200 {
            self.pasted_blocks.push(remainder.to_string());
        } else {
            self.input.insert_text(remainder);
        }
    }

    fn send_message(&mut self, _event_tx: &mpsc::UnboundedSender<AppEvent>) {
        let typed = self.input.content();
        let pasted = if self.pasted_blocks.is_empty() {
            None
        } else {
            // Join all staged paste blocks with a blank line between them so the
            // model sees them as distinct sections. Drains the Vec (`mem::take`).
            Some(std::mem::take(&mut self.pasted_blocks).join("\n\n"))
        };
        let content = if let Some(pasted) = pasted {
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

        // Log this turn with env info

        // Save user's original input (not full_content with attachments) for restore on cancel
        self.last_sent_input = Some(content.clone());

        // Add user message to our local mirror for immediate display.
        self.conversation.add_user_message(&full_content);
        self.input.clear();
        self.at_bottom = true;
        self.current_step_count = 0;
        self.current_tool_call_count = 0;
        self.turn_start = Some(Instant::now());
        self.first_token_ms = None;
        self.llm_call_start = Some(Instant::now());
        self.last_completed_tool = String::new();
        self.last_turn_duration = None;

        // Git checkpoint before agent edits
        self.last_checkpoint = atomcode_core::agent::git_checkpoint::create_checkpoint(&self.working_dir);

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
    #[cfg(target_os = "windows")]
    {
        // Try CF_HDROP first — Explorer's Ctrl+C on files puts the file list
        // there. CF_UNICODETEXT only gets the last selected filename as a
        // fallback, so without this check pasting N copied files would yield
        // just one attachment.
        //
        // IMPORTANT: must use `get_clipboard()` helper, NOT `Getter::read_clipboard`
        // directly — the latter requires the caller to have already opened the
        // clipboard via `Clipboard::new()`. `get_clipboard` handles open/close.
        match clipboard_win::get_clipboard::<Vec<String>, _>(clipboard_win::formats::FileList) {
            Ok(files) if !files.is_empty() => {
                // Join with '\n' so the downstream extractor treats it as
                // newline-separated paths (and doesn't tokenize on spaces
                // inside e.g. `C:\My Docs\file.txt`).
                return Some(files.join("\n"));
            }
            _ => {}
        }

        // Normal text path — native Win32 API avoids ~1s PowerShell cold start.
        return clipboard_win::get_clipboard_string()
            .ok()
            .map(|t| t.replace("\r\n", "\n").replace('\r', "\n"))
            .filter(|s| !s.is_empty());
    }

    #[cfg(not(target_os = "windows"))]
    {
        use std::process::Command;

        let output = if cfg!(target_os = "macos") {
            Command::new("pbpaste").output().ok()
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
                text.replace("\r\n", "\n").replace('\r', "\n")
            })
            .filter(|s| !s.is_empty())
    }
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
            // Strip CR from CRLF line endings, then insert the whole chunk in one
            // O(n) String::insert_str — avoids the O(n²) char-by-char path that
            // previously froze the UI on long pastes.
            let clean: String = if chunk.contains('\r') {
                chunk.chars().filter(|&c| c != '\r').collect()
            } else {
                chunk.to_string()
            };
            if clean.is_empty() {
                continue;
            }
            let line = &mut self.lines[self.cursor_row];
            line.insert_str(self.cursor_col, &clean);
            self.cursor_col += clean.len();
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

/// Submit an issue to AtomGit API
async fn submit_issue_to_gitcode(access_token: &str, title: &str, body: &str) -> Result<String, String> {
    use reqwest::Client;
    
    let client = Client::new();

    // AtomGit API endpoint for creating issues
    // Format: https://api.atomgit.com/api/v5/repos/:owner/issues
    let url = "https://api.atomgit.com/api/v5/repos/bangxu/issues";
    
    let response = client
        .post(url)
        .json(&serde_json::json!({
            "access_token": access_token,
            "repo": "atomcode",
            "title": title,
            "body": body,
        }))
        .send()
        .await
        .map_err(|e| format!("Request failed: {}", e))?;
    
    if response.status().is_success() {
        let json: serde_json::Value = response.json().await
            .map_err(|e| format!("Failed to parse response: {}", e))?;
        let issue_url = json["html_url"].as_str()
            .ok_or("No html_url in response")?
            .to_string();
        Ok(issue_url)
    } else {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        Err(format!("API error {}: {}", status, text))
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
