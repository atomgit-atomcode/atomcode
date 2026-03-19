use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
use tokio::sync::mpsc;

use atomcode_core::config::Config;
use atomcode_core::config::DEFAULT_SYSTEM_PROMPT;
use atomcode_core::conversation::Conversation;
use atomcode_core::provider::LlmProvider;
use atomcode_core::stream::StreamEvent;
use atomcode_core::tool::{Tool, ToolCall, ToolCallBuffer, ToolRegistry, ToolResult, ApprovalRequirement};

use crate::command::SlashMenu;
use crate::event::AppEvent;
use crate::provider_manager::{ManagerAction, ProviderManager};

#[derive(Debug, Clone)]
pub enum AppMode {
    Normal,
    Streaming,
    WaitingApproval(ToolCall),
    ToolExecuting,
    ProviderManager,
    ModelSelector,
    Exiting,
}

impl AppMode {
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
    /// Files attached to the next message (detected from pasted paths).
    pub attached_files: Vec<crate::file_attach::AttachedFile>,
    pub slash_menu: SlashMenu,
    pub provider_mgr: Option<ProviderManager>,
    /// Model selector: list of (provider_name, model_name), selected index
    pub model_list: Vec<(String, String)>,
    pub model_selected: usize,
    pub tool_registry: ToolRegistry,
    pub tool_call_count: usize,
    pub tick_count: usize,
    /// Retry count for stream errors (auto-retry up to 3 times).
    pub retry_count: usize,
    /// Last Ctrl+C timestamp for double-press detection.
    pub last_ctrl_c: Option<Instant>,
    /// Generation counter — incremented on cancel/new message. Background tasks
    /// with a stale generation are ignored when they send events back.
    pub generation: u64,
    /// When the current turn (user message → agent loop) started.
    pub turn_start: Option<Instant>,
    /// When the last tool execution started (for per-step timing).
    pub tool_start: Option<Instant>,
    /// Duration of the last completed turn.
    pub last_turn_duration: Option<Duration>,
    pub working_dir: PathBuf,
    /// Input history — past user prompts for Up/Down navigation.
    pub input_history: Vec<String>,
    /// Current position in input history (-1 = not browsing).
    pub history_index: Option<usize>,
    /// Stashed input text when entering history browse mode.
    pub history_stash: Option<String>,
    /// Estimated token counts for the current session.
    pub total_tokens: usize,
    /// Tokens used in the current turn.
    pub turn_tokens: usize,
    /// Suggested next prompt shown as ghost text in the input box.
    pub suggestion: Option<String>,
    /// Cache of rendered lines for completed messages. Invalidated on message count change.
    pub render_cache: Vec<ratatui::text::Line<'static>>,
    pub render_cache_msg_count: usize,
    pub provider: Box<dyn LlmProvider>,
    pub config: Config,
}

impl App {
    pub fn new(provider: Box<dyn LlmProvider>, config: Config, tool_registry: ToolRegistry, working_dir: PathBuf) -> Self {
        let conversation = Conversation::load(&Conversation::history_path());
        // Build input history from past user messages
        let input_history: Vec<String> = conversation.messages.iter()
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
        Self {
            mode: AppMode::Normal,
            conversation,
            input: InputState::new(),
            scroll_offset: 0,
            at_bottom: true,
            confirm_quit: false,
            pending_editor: None,
            attached_files: Vec::new(),
            slash_menu: SlashMenu::new(),
            provider_mgr: None,
            model_list: Vec::new(),
            model_selected: 0,
            tool_registry,
            tool_call_count: 0,
            retry_count: 0,
            last_ctrl_c: None,
            generation: 0,
            tick_count: 0,
            turn_start: None,
            tool_start: None,
            last_turn_duration: None,
            working_dir,
            input_history,
            history_index: None,
            history_stash: None,
            total_tokens: 0,
            turn_tokens: 0,
            suggestion: None,
            render_cache: Vec::new(),
            render_cache_msg_count: 0,
            provider,
            config,
        }
    }

    /// Generate a follow-up suggestion based on conversation context.
    fn generate_suggestion(&self) -> Option<String> {
        use atomcode_core::conversation::message::MessageContent;

        let msgs = &self.conversation.messages;
        if msgs.is_empty() {
            return None;
        }

        // Analyze the last few messages to detect patterns
        let last = &msgs[msgs.len() - 1];

        // Check if any tool calls were made in this conversation
        let had_write = msgs.iter().any(|m| matches!(&m.content,
            MessageContent::AssistantWithToolCalls { tool_calls, .. }
            if tool_calls.iter().any(|c| c.name == "write_file" || c.name == "edit_file")
        ));
        let had_bash = msgs.iter().any(|m| matches!(&m.content,
            MessageContent::AssistantWithToolCalls { tool_calls, .. }
            if tool_calls.iter().any(|c| c.name == "bash")
        ));
        let had_error = msgs.iter().rev().take(3).any(|m| matches!(&m.content,
            MessageContent::ToolResult(r) if !r.success
        ));

        // Last assistant text for context
        let last_text = last.text().unwrap_or("");
        let last_lower = last_text.to_lowercase();

        // Error happened recently → suggest fix
        if had_error {
            return Some("Fix the error".to_string());
        }

        // Wrote/edited files → suggest testing or running
        if had_write && !had_bash {
            // Detect language from file extensions in tool calls
            for m in msgs.iter().rev().take(5) {
                if let MessageContent::AssistantWithToolCalls { tool_calls, .. } = &m.content {
                    for tc in tool_calls {
                        if let Ok(args) = serde_json::from_str::<serde_json::Value>(&tc.arguments) {
                            if let Some(fp) = args.get("file_path").and_then(|v| v.as_str()) {
                                if fp.ends_with(".rs") {
                                    return Some("Run cargo build to check for errors".to_string());
                                } else if fp.ends_with(".py") {
                                    return Some("Run the script to test it".to_string());
                                } else if fp.ends_with(".js") || fp.ends_with(".ts") {
                                    return Some("Run npm test".to_string());
                                } else if fp.ends_with(".go") {
                                    return Some("Run go build to check for errors".to_string());
                                }
                            }
                        }
                    }
                }
            }
            return Some("Run the code to test it".to_string());
        }

        // Ran a command → suggest follow-up
        if had_bash && !had_write {
            if last_lower.contains("error") || last_lower.contains("failed") {
                return Some("Fix the issue".to_string());
            }
            if last_lower.contains("test") && last_lower.contains("pass") {
                return Some("Commit the changes".to_string());
            }
        }

        // Wrote files + ran commands successfully → suggest commit
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

    /// Get the last tool call from conversation (the one that produced the current result).
    fn get_last_tool_call(&self) -> Option<ToolCall> {
        use atomcode_core::conversation::message::MessageContent;
        for msg in self.conversation.messages.iter().rev() {
            if let MessageContent::AssistantWithToolCalls { tool_calls, .. } = &msg.content {
                return tool_calls.last().cloned();
            }
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
            self.config.default_workdir = Some(resolved.to_string_lossy().to_string());
            let _ = self.config.save(&Config::default_path());
            (true, format!("Changed working directory to {}", resolved.display()))
        } else if new_path.is_dir() {
            // canonicalize failed but path exists as dir
            self.working_dir = new_path.clone();
            self.config.default_workdir = Some(new_path.to_string_lossy().to_string());
            let _ = self.config.save(&Config::default_path());
            (true, format!("Changed working directory to {}", new_path.display()))
        } else {
            (false, format!("Not a directory: {}", new_path.display()))
        }
    }

    /// Change working directory from /cd command (adds conversation message).
    pub fn change_working_dir(&mut self, path: &str) {
        let (_, msg) = self.try_change_dir(path);
        self.conversation.push_delta(&msg);
        self.conversation.finalize_stream();
    }

    /// If the AI ended the turn without giving a summary (just tool calls, no final text),
    /// auto-generate a brief summary from the tool results.
    fn maybe_add_auto_summary(&mut self) {
        use atomcode_core::conversation::message::{MessageContent, Role};

        let msgs = &self.conversation.messages;
        if msgs.is_empty() { return; }

        // Check if the turn had tool calls
        let had_tools = msgs.iter().rev().take(20).any(|m|
            matches!(&m.content, MessageContent::AssistantWithToolCalls { .. } | MessageContent::ToolResult(_))
        );
        if !had_tools { return; }

        // Check if the last message is a text assistant response (= AI gave summary)
        let last = &msgs[msgs.len() - 1];
        if matches!(&last.content, MessageContent::Text(s) if matches!(last.role, Role::Assistant) && s.len() > 10) {
            return; // AI already gave a summary
        }

        // Build auto-summary from recent tool results
        let mut actions: Vec<String> = Vec::new();
        let mut had_error = false;
        for msg in msgs.iter().rev().take(20) {
            match &msg.content {
                MessageContent::ToolResult(r) => {
                    if !r.success { had_error = true; }
                    let summary: String = r.output.lines().next().unwrap_or("").chars().take(60).collect();
                    actions.push(summary);
                }
                MessageContent::Text(_) if matches!(msg.role, Role::User) => break, // stop at user message
                _ => {}
            }
        }
        actions.reverse();

        if actions.is_empty() { return; }

        let status = if had_error { "[DONE with errors]" } else { "[DONE]" };
        let summary = format!("{} {} actions completed.", status, actions.len());
        self.conversation.push_delta(&summary);
        self.conversation.finalize_stream();
    }

    /// Rebuild the LLM provider from current config (after provider/model change).
    fn rebuild_provider(&mut self) {
        use atomcode_core::provider::create_provider;
        if let Ok(provider_config) = self.config.active_provider(None) {
            if let Ok(new_provider) = create_provider(&provider_config.clone()) {
                self.provider = new_provider;
            }
        }
    }

    /// Build system prompt with working directory context.
    fn system_prompt(&self) -> String {
        let base = self.config.providers
            .get(&self.config.default_provider)
            .and_then(|p| p.system_prompt.as_deref())
            .unwrap_or(DEFAULT_SYSTEM_PROMPT);
        format!("{}\n\nWorking directory: {}", base, self.working_dir.display())
    }

    pub fn handle_event(&mut self, event: AppEvent, event_tx: &mpsc::UnboundedSender<AppEvent>) {
        // Drop stale stream/tool events after cancellation
        if self.mode.is_normal() {
            match &event {
                AppEvent::StreamDelta(_)
                | AppEvent::StreamToolCallStart { .. }
                | AppEvent::StreamToolCallDelta(_)
                | AppEvent::StreamToolCallDone(_)
                | AppEvent::StreamUsage(_)
                | AppEvent::StreamDone
                | AppEvent::StreamError(_)
                | AppEvent::ToolFinished(_) => return, // Stale event from cancelled task
                _ => {}
            }
        }

        match event {
            AppEvent::Key(key) => self.handle_key(key, event_tx),
            AppEvent::StreamDelta(text) => {
                self.conversation.push_delta(&text);
            }
            AppEvent::StreamUsage(usage) => {
                // Only count completion tokens (prompt is repeated context, not new work)
                self.turn_tokens += usage.completion_tokens;
                self.total_tokens += usage.completion_tokens;
            }
            AppEvent::StreamDone => {
                self.conversation.finalize_stream();

                // Auto-generate summary if the turn had tool calls but AI ended without text
                self.maybe_add_auto_summary();

                self.mode = AppMode::Normal;
                self.at_bottom = true;
                self.retry_count = 0;
                self.last_turn_duration = self.turn_start.map(|t| t.elapsed());
                self.turn_start = None;
                self.suggestion = self.generate_suggestion();
                // Persist history (async, non-blocking)
                let msgs = self.conversation.messages.clone();
                tokio::spawn(async move {
                    let path = Conversation::history_path();
                    if let Some(parent) = path.parent() {
                        let _ = tokio::fs::create_dir_all(parent).await;
                    }
                    if let Ok(data) = serde_json::to_string(&msgs) {
                        let _ = tokio::fs::write(path, data).await;
                    }
                });
            }
            AppEvent::StreamError(err) => {
                // Only retry on network/timeout errors, NOT on API errors (400, 401, etc.)
                let is_api_error = err.contains("API error")
                    || err.contains("400")
                    || err.contains("401")
                    || err.contains("403")
                    || err.contains("404")
                    || err.contains("422")
                    || err.contains("429")
                    || err.contains("illegal");

                if !is_api_error && self.retry_count < 2 {
                    self.retry_count += 1;
                    self.conversation.stream_buffer = None;
                    self.mode = AppMode::Streaming;
                    self.at_bottom = true;
                    self.continue_agent_loop(event_tx);
                } else {
                    self.conversation.push_delta(&format!("\n\n[Error: {}]", err));
                    self.conversation.finalize_stream();
                    self.mode = AppMode::Normal;
                    self.last_turn_duration = self.turn_start.map(|t| t.elapsed());
                    self.turn_start = None;
                    self.at_bottom = true;
                    self.retry_count = 0;
                }
            }
            AppEvent::StreamToolCallStart { id, name } => {
                self.conversation.tool_call_buffer = Some(ToolCallBuffer {
                    id, name, arguments: String::new(),
                });
            }
            AppEvent::StreamToolCallDelta(args) => {
                if let Some(ref mut buf) = self.conversation.tool_call_buffer {
                    buf.arguments.push_str(&args);
                }
            }
            AppEvent::StreamToolCallDone(call) => {
                self.conversation.tool_call_buffer = None;
                self.handle_tool_call(call, event_tx);
            }
            AppEvent::ToolFinished(mut result) => {
                // Append step duration to output
                if let Some(start) = self.tool_start.take() {
                    let dur = start.elapsed();
                    let dur_str = if dur.as_millis() < 1000 {
                        format!(" ({}ms)", dur.as_millis())
                    } else {
                        format!(" ({:.1}s)", dur.as_secs_f64())
                    };
                    result.output.push_str(&dur_str);
                }
                self.handle_tool_result(result, event_tx);
            }
            AppEvent::ScrollUp(n) => {
                if self.at_bottom {
                    // Use cached line count instead of recomputing render_markdown for all messages
                    let total = self.render_cache.len();
                    self.scroll_offset = total.saturating_sub(n as usize);
                    self.at_bottom = false;
                } else {
                    self.scroll_offset = self.scroll_offset.saturating_sub(n as usize);
                }
            }
            AppEvent::ScrollDown(n) => {
                self.scroll_offset += n as usize;
                // Re-engage at_bottom when scrolled past the end
                let total = self.render_cache.len();
                if self.scroll_offset >= total {
                    self.at_bottom = true;
                }
            }
            AppEvent::Resize(_, _) => {}
            AppEvent::Tick => {
                self.tick_count = self.tick_count.wrapping_add(1);
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent, event_tx: &mpsc::UnboundedSender<AppEvent>) {
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
                    self.conversation.stream_buffer = None;
                    self.conversation.tool_call_buffer = None;
                    self.conversation.finalize_stream();
                    self.mode = AppMode::Normal;
                    self.at_bottom = true;
                    self.last_turn_duration = self.turn_start.map(|t| t.elapsed());
                    self.turn_start = None;
                }
                AppMode::WaitingApproval(_) => {
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

        match &self.mode {
            AppMode::Normal => self.handle_key_normal(key, event_tx),
            AppMode::Streaming | AppMode::ToolExecuting => {
                // Esc cancels the operation
                if key.code == KeyCode::Esc {
                    self.conversation.stream_buffer = None;
                    self.conversation.tool_call_buffer = None;
                    self.conversation.finalize_stream();
                    self.mode = AppMode::Normal;
                    self.at_bottom = true;
                    self.last_turn_duration = self.turn_start.map(|t| t.elapsed());
                    self.turn_start = None;
                } else {
                    // All other keys go to input — user can type while AI is working
                    self.handle_key_input(key);
                }
            }
            AppMode::WaitingApproval(_) => self.handle_key_approval(key, event_tx),
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
                if let Some((name, _)) = self.model_list.get(self.model_selected) {
                    self.config.default_provider = name.clone();
                    let _ = self.config.save(&Config::default_path());
                    self.rebuild_provider();
                }
                self.mode = AppMode::Normal;
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = AppMode::Normal;
            }
            _ => {}
        }
    }

    fn handle_key_approval(&mut self, key: KeyEvent, event_tx: &mpsc::UnboundedSender<AppEvent>) {
        let call = match &self.mode {
            AppMode::WaitingApproval(c) => c.clone(),
            _ => return,
        };
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                self.mode = AppMode::ToolExecuting;
                self.execute_tool(call, event_tx);
            }
            KeyCode::Char('n') | KeyCode::Esc => {
                let result = ToolResult {
                    call_id: call.id.clone(),
                    output: "Denied by user".to_string(),
                    success: false,
                };
                self.handle_tool_result(result, event_tx);
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
            (KeyModifiers::SHIFT, KeyCode::Enter) => {
                self.input.insert_newline();
            }
            (KeyModifiers::CONTROL, KeyCode::Char('j')) | (_, KeyCode::Enter) => {
                self.slash_menu.close();
                self.send_message(event_tx);
            }
            (_, KeyCode::Esc) => {
                // Esc: clear input if not empty, otherwise do nothing
                // (use /quit to exit the program)
                if !self.input.is_empty() {
                    self.input.clear();
                    self.slash_menu.close();
                }
            }
            (KeyModifiers::CONTROL, KeyCode::Up) => {
                if self.at_bottom {
                    // Use cached line count instead of recomputing render_markdown for all messages
                    let total = self.render_cache.len();
                    self.scroll_offset = total.saturating_sub(3);
                    self.at_bottom = false;
                } else {
                    self.scroll_offset = self.scroll_offset.saturating_sub(3);
                }
            }
            (KeyModifiers::CONTROL, KeyCode::Down) => {
                self.scroll_offset += 3;
                let total = self.render_cache.len();
                if self.scroll_offset >= total {
                    self.at_bottom = true;
                }
            }
            (_, KeyCode::PageUp) => {
                if self.at_bottom {
                    let total = self.render_cache.len();
                    self.scroll_offset = total.saturating_sub(20);
                    self.at_bottom = false;
                } else {
                    self.scroll_offset = self.scroll_offset.saturating_sub(20);
                }
            }
            (_, KeyCode::PageDown) => {
                self.scroll_offset += 20;
                let total = self.render_cache.len();
                if self.scroll_offset >= total {
                    self.at_bottom = true;
                }
            }
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
                        if let Some(hist) = self.input_history.get(idx) {
                            self.input.clear();
                            for c in hist.chars() { self.input.insert_char(c); }
                        }
                    }
                }
            }
            (_, KeyCode::Down) => {
                // Multi-line: move cursor down within input
                if self.input.cursor_row + 1 < self.input.lines.len() {
                    self.input.cursor_row += 1;
                    self.input.cursor_col = snap_to_char_boundary(
                        &self.input.lines[self.input.cursor_row],
                        self.input.cursor_col,
                    );
                } else if let Some(idx) = self.history_index {
                    // Browsing history: go forward
                    if idx + 1 < self.input_history.len() {
                        self.history_index = Some(idx + 1);
                        let hist = self.input_history[idx + 1].clone();
                        self.input.clear();
                        for c in hist.chars() { self.input.insert_char(c); }
                    } else {
                        // Past the end: restore stashed input
                        self.history_index = None;
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

        // After any input change, update the slash menu
        self.slash_menu.update(&self.input.content());

        // Detect file path — auto-attach if input is a valid file path
        let content = self.input.content();
        let trimmed = content.trim();
        if !trimmed.is_empty() && !trimmed.starts_with('/') || trimmed.starts_with("/file") {
            // Skip — not a path or it's a slash command
        } else if let Some(file) = crate::file_attach::detect_file_path(trimmed, &self.working_dir) {
            // Only attach if not already attached
            if !self.attached_files.iter().any(|f| f.path == file.path) {
                self.attached_files.push(file);
                self.input.clear(); // Clear the path from input
            }
        }
    }

    /// Handle typing in the input box — works in any mode (Normal, Streaming, etc.)
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
                    // Show current directory
                    self.conversation.push_delta(&format!(
                        "Working directory: `{}`",
                        self.working_dir.display()
                    ));
                    self.conversation.finalize_stream();
                } else {
                    self.change_working_dir(arg);
                }
            }
            "/clear" => {
                self.conversation = Conversation::new();
                tokio::spawn(async move {
                    let _ = tokio::fs::write(Conversation::history_path(), "[]").await;
                });
                self.scroll_offset = 0;
                self.at_bottom = true;
                self.render_cache.clear();
                self.render_cache_msg_count = 0;
                self.total_tokens = 0;
                self.turn_tokens = 0;
                return true;
            }
            "/help" => {
                let mut help = String::from("**Available commands:**\n\n");
                for cmd in crate::command::COMMANDS {
                    help.push_str(&format!("  `{}` — {}\n", cmd.name, cmd.description));
                }
                help.push_str("\n**Shortcuts:**\n\n");
                help.push_str("  `Enter` — Send message\n");
                help.push_str("  `Esc` — Clear input\n");
                help.push_str("  `Ctrl+Up/Down` — Scroll chat\n");
                help.push_str("  `Tab` — Accept suggestion\n");
                help.push_str("  `/quit` — Exit\n");
                help.push_str("\n**Copy text:**\n\n");
                help.push_str("  `/copy` — Copy last AI response to clipboard\n");
                help.push_str("  `Option+drag` — Native text selection (iTerm2/Alacritty)\n");
                help.push_str("  `Fn+drag` — Native text selection (macOS Terminal)\n");
                self.conversation.push_delta(&help);
                self.conversation.finalize_stream();
            }
            _ => {
                let available: Vec<String> = crate::command::COMMANDS
                    .iter()
                    .map(|c| format!("  `{}` — {}", c.name, c.description))
                    .collect();
                self.conversation.push_delta(&format!(
                    "Unknown command: `{}`\n\nAvailable commands:\n{}",
                    cmd,
                    available.join("\n")
                ));
                self.conversation.finalize_stream();
            }
        }

        true
    }

    fn send_message(&mut self, event_tx: &mpsc::UnboundedSender<AppEvent>) {
        let content = self.input.content();
        if content.trim().is_empty() {
            return;
        }

        // Check for slash commands first
        if self.handle_slash_command() {
            return;
        }

        // Add to input history
        self.input_history.push(content.clone());
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

        self.conversation.add_user_message(&full_content);
        self.input.clear();
        self.mode = AppMode::Streaming;
        self.at_bottom = true;
        self.tool_call_count = 0;
        self.retry_count = 0;
        self.turn_start = Some(Instant::now());
        self.last_turn_duration = None;

        let system_prompt = self.system_prompt();
        let messages = self.conversation.to_provider_messages_windowed(&system_prompt, 30);
        let tool_defs = self.tool_registry.get_definitions();

        let tx = event_tx.clone();
        let stream_result = self.provider.chat_stream(&messages, Some(&tool_defs));

        tokio::spawn(async move {
            match stream_result {
                Ok(mut stream) => {
                    while let Some(event) = stream.next().await {
                        match event {
                            Ok(StreamEvent::Delta(text)) => {
                                let _ = tx.send(AppEvent::StreamDelta(text));
                            }
                            Ok(StreamEvent::ToolCallStart { id, name }) => {
                                let _ = tx.send(AppEvent::StreamToolCallStart { id, name });
                            }
                            Ok(StreamEvent::ToolCallDelta(args)) => {
                                let _ = tx.send(AppEvent::StreamToolCallDelta(args));
                            }
                            Ok(StreamEvent::ToolCallDone(call)) => {
                                let _ = tx.send(AppEvent::StreamToolCallDone(call));
                            }
                            Ok(StreamEvent::Usage(usage)) => {
                                let _ = tx.send(AppEvent::StreamUsage(usage));
                            }
                            Ok(StreamEvent::Done) => {
                                let _ = tx.send(AppEvent::StreamDone);
                                break;
                            }
                            Ok(StreamEvent::Error(e)) => {
                                let _ = tx.send(AppEvent::StreamError(e));
                                break;
                            }
                            Err(e) => {
                                let _ = tx.send(AppEvent::StreamError(e.to_string()));
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::StreamError(e.to_string()));
                }
            }
        });
    }

    fn handle_tool_call(&mut self, call: ToolCall, event_tx: &mpsc::UnboundedSender<AppEvent>) {
        self.conversation.finalize_stream_with_tool_call(call.clone());

        if let Some(tool) = self.tool_registry.get(&call.name) {
            match tool.approval(&call.arguments) {
                ApprovalRequirement::AutoApprove => {
                    self.mode = AppMode::ToolExecuting;
                    self.execute_tool(call, event_tx);
                }
                ApprovalRequirement::RequireApproval(_) => {
                    self.mode = AppMode::WaitingApproval(call);
                }
            }
        } else {
            let result = ToolResult {
                call_id: call.id.clone(),
                output: format!("Unknown tool: {}", call.name),
                success: false,
            };
            self.handle_tool_result(result, event_tx);
        }
    }

    fn execute_tool(&mut self, call: ToolCall, event_tx: &mpsc::UnboundedSender<AppEvent>) {
        self.tool_start = Some(Instant::now());
        let tool_name = call.name.clone();
        let call_id = call.id.clone();
        let args = self.resolve_tool_args(&call);
        let working_dir = self.working_dir.clone();
        let tx = event_tx.clone();

        tokio::spawn(async move {
            let result = match tool_name.as_str() {
                "read_file" => atomcode_core::tool::read::ReadFileTool.execute(&args).await,
                "write_file" => atomcode_core::tool::write::WriteFileTool.execute(&args).await,
                "edit_file" => atomcode_core::tool::edit::EditFileTool.execute(&args).await,
                "bash" => atomcode_core::tool::bash::BashTool::new(working_dir).execute(&args).await,
                "change_dir" => atomcode_core::tool::cd::CdTool.execute(&args).await,
                _ => Err(anyhow::anyhow!("Unknown tool: {}", tool_name)),
            };
            let tool_result = match result {
                Ok(mut r) => { r.call_id = call_id; r }
                Err(e) => ToolResult {
                    call_id,
                    output: format!("Error: {}", e),
                    success: false,
                },
            };
            let _ = tx.send(AppEvent::ToolFinished(tool_result));
        });
    }

    /// Resolve relative file_path in tool arguments to absolute paths based on working_dir.
    fn resolve_tool_args(&self, call: &ToolCall) -> String {
        if let Ok(mut args) = serde_json::from_str::<serde_json::Value>(&call.arguments) {
            if let Some(fp) = args.get("file_path").and_then(|v| v.as_str()) {
                let path = std::path::Path::new(fp);
                if !path.is_absolute() {
                    let resolved = self.working_dir.join(path);
                    args["file_path"] = serde_json::json!(resolved.to_string_lossy().to_string());
                }
                return serde_json::to_string(&args).unwrap_or(call.arguments.clone());
            }
        }
        call.arguments.clone()
    }

    fn handle_tool_result(&mut self, mut result: ToolResult, event_tx: &mpsc::UnboundedSender<AppEvent>) {
        // Intercept change_dir tool requests
        if result.output.starts_with("CD_REQUEST:") {
            let path = result.output.strip_prefix("CD_REQUEST:").unwrap().to_string();
            let (ok, msg) = self.try_change_dir(&path);
            result.output = msg;
            result.success = ok;
        }



        // Truncate large tool outputs to reduce API payload size
        // (LLM doesn't need 2000 lines of file content in the next request)
        const MAX_OUTPUT_CHARS: usize = 8000;
        if result.output.len() > MAX_OUTPUT_CHARS {
            let truncated: String = result.output.chars().take(MAX_OUTPUT_CHARS).collect();
            let total_lines = result.output.lines().count();
            result.output = format!("{}...\n\n[truncated, showing first ~{} chars of {} lines total]",
                truncated, MAX_OUTPUT_CHARS, total_lines);
        }

        self.conversation.add_tool_result(result);
        self.tool_call_count += 1;

        // Auto-continue: reset counter every 25 calls but keep going
        if self.tool_call_count >= 25 {
            self.tool_call_count = 0;
        }

        self.mode = AppMode::Streaming;
        self.at_bottom = true;
        self.continue_agent_loop(event_tx);
    }

    fn continue_agent_loop(&mut self, event_tx: &mpsc::UnboundedSender<AppEvent>) {
        let system_prompt = self.system_prompt();
        // Smaller window for tool continuations — recent context is enough
        let messages = self.conversation.to_provider_messages_windowed(&system_prompt, 20);
        let tool_defs = self.tool_registry.get_definitions();

        let tx = event_tx.clone();
        let stream_result = self.provider.chat_stream(&messages, Some(&tool_defs));

        tokio::spawn(async move {
            match stream_result {
                Ok(mut stream) => {
                    while let Some(event) = stream.next().await {
                        match event {
                            Ok(StreamEvent::Delta(text)) => {
                                let _ = tx.send(AppEvent::StreamDelta(text));
                            }
                            Ok(StreamEvent::ToolCallStart { id, name }) => {
                                let _ = tx.send(AppEvent::StreamToolCallStart { id, name });
                            }
                            Ok(StreamEvent::ToolCallDelta(args)) => {
                                let _ = tx.send(AppEvent::StreamToolCallDelta(args));
                            }
                            Ok(StreamEvent::ToolCallDone(call)) => {
                                let _ = tx.send(AppEvent::StreamToolCallDone(call));
                            }
                            Ok(StreamEvent::Usage(usage)) => {
                                let _ = tx.send(AppEvent::StreamUsage(usage));
                            }
                            Ok(StreamEvent::Done) => {
                                let _ = tx.send(AppEvent::StreamDone);
                                break;
                            }
                            Ok(StreamEvent::Error(e)) => {
                                let _ = tx.send(AppEvent::StreamError(e));
                                break;
                            }
                            Err(e) => {
                                let _ = tx.send(AppEvent::StreamError(e.to_string()));
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(AppEvent::StreamError(e.to_string()));
                }
            }
        });
    }
}

/// Copy text to system clipboard (macOS: pbcopy, Linux: xclip/xsel).
/// Snap a byte position to the nearest valid char boundary at or before `pos`.
fn format_file_size(bytes: u64) -> String {
    if bytes < 1024 { format!("{} B", bytes) }
    else if bytes < 1024 * 1024 { format!("{:.1} KB", bytes as f64 / 1024.0) }
    else { format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0)) }
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

    let cmd = if cfg!(target_os = "macos") {
        "pbcopy"
    } else {
        "xclip"
    };

    let mut child = Command::new(cmd)
        .stdin(Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    child.wait()?;
    Ok(())
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
