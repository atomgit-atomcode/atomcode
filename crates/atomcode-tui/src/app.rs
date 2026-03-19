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
    pub slash_menu: SlashMenu,
    pub provider_mgr: Option<ProviderManager>,
    pub tool_registry: ToolRegistry,
    pub tool_call_count: usize,
    pub tick_count: usize,
    /// When the current turn (user message → agent loop) started.
    pub turn_start: Option<Instant>,
    /// When the last tool execution started (for per-step timing).
    pub tool_start: Option<Instant>,
    /// Duration of the last completed turn.
    pub last_turn_duration: Option<Duration>,
    pub working_dir: PathBuf,
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
        Self {
            mode: AppMode::Normal,
            conversation: Conversation::new(),
            input: InputState::new(),
            scroll_offset: 0,
            at_bottom: true,
            confirm_quit: false,
            pending_editor: None,
            slash_menu: SlashMenu::new(),
            provider_mgr: None,
            tool_registry,
            tool_call_count: 0,
            tick_count: 0,
            turn_start: None,
            tool_start: None,
            last_turn_duration: None,
            working_dir,
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

        match std::fs::canonicalize(&new_path) {
            Ok(resolved) if resolved.is_dir() => {
                self.working_dir = resolved.clone();
                // Persist to config
                self.config.default_workdir = Some(resolved.to_string_lossy().to_string());
                let _ = self.config.save(&Config::default_path());
                (true, format!("Changed working directory to {}", resolved.display()))
            }
            Ok(_) => (false, format!("Not a directory: {}", new_path.display())),
            Err(e) => (false, format!("Cannot access {}: {}", new_path.display(), e)),
        }
    }

    /// Change working directory from /cd command (adds conversation message).
    pub fn change_working_dir(&mut self, path: &str) {
        let (_, msg) = self.try_change_dir(path);
        self.conversation.push_delta(&msg);
        self.conversation.finalize_stream();
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
        match event {
            AppEvent::Key(key) => self.handle_key(key, event_tx),
            AppEvent::StreamDelta(text) => {
                self.conversation.push_delta(&text);
                // Keep at_bottom true during streaming for auto-scroll
                // (unless user manually scrolled up)
            }
            AppEvent::StreamDone => {
                self.conversation.finalize_stream();
                self.mode = AppMode::Normal;
                self.at_bottom = true;
                self.last_turn_duration = self.turn_start.map(|t| t.elapsed());
                self.turn_start = None;
                self.suggestion = self.generate_suggestion();
            }
            AppEvent::StreamError(err) => {
                self.conversation.push_delta(&format!("\n\n[Error: {}]", err));
                self.conversation.finalize_stream();
                self.mode = AppMode::Normal;
                self.last_turn_duration = self.turn_start.map(|t| t.elapsed());
                self.turn_start = None;
                self.at_bottom = true;
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
                    // Transition from at_bottom to manual scroll: estimate current position
                    let total = crate::ui::chat_panel::total_lines(&self.conversation);
                    self.scroll_offset = total.saturating_sub(n as usize);
                    self.at_bottom = false;
                } else {
                    self.scroll_offset = self.scroll_offset.saturating_sub(n as usize);
                }
            }
            AppEvent::ScrollDown(n) => {
                self.scroll_offset += n as usize;
                // Don't set at_bottom here; render will auto-clamp
            }
            AppEvent::Resize(_, _) => {}
            AppEvent::Tick => {
                self.tick_count = self.tick_count.wrapping_add(1);
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent, event_tx: &mpsc::UnboundedSender<AppEvent>) {
        match &self.mode {
            AppMode::Normal => self.handle_key_normal(key, event_tx),
            AppMode::Streaming | AppMode::ToolExecuting => self.handle_key_streaming(key),
            AppMode::WaitingApproval(_) => self.handle_key_approval(key, event_tx),
            AppMode::ProviderManager => self.handle_key_provider_manager(key),
            AppMode::Exiting => {}
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
                    // Save config before closing
                    let config_path = Config::default_path();
                    let _ = self.config.save(&config_path);
                    self.provider_mgr = None;
                    self.mode = AppMode::Normal;
                }
                ManagerAction::SetDefault(name) => {
                    self.config.default_provider = name;
                    let config_path = Config::default_path();
                    let _ = self.config.save(&config_path);
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
                            "type" => p.provider_type = value,
                            "api_key" => {
                                p.api_key = if value.is_empty() { None } else { Some(value) }
                            }
                            "base_url" => {
                                p.base_url = if value.is_empty() { None } else { Some(value) }
                            }
                            "model" => p.model = value,
                            _ => {}
                        }
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
            (KeyModifiers::CONTROL, KeyCode::Char('c')) => {
                // Same as Esc: clear input, don't exit program
                if !self.input.is_empty() {
                    self.input.clear();
                    self.slash_menu.close();
                }
            }
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
                    let total = crate::ui::chat_panel::total_lines(&self.conversation);
                    self.scroll_offset = total.saturating_sub(3);
                    self.at_bottom = false;
                } else {
                    self.scroll_offset = self.scroll_offset.saturating_sub(3);
                }
            }
            (KeyModifiers::CONTROL, KeyCode::Down) => {
                self.scroll_offset += 3;
            }
            (_, KeyCode::Backspace) => {
                self.input.backspace();
            }
            (_, KeyCode::Up) => {
                if self.input.cursor_row > 0 {
                    self.input.cursor_row -= 1;
                    self.input.cursor_col = self.input.cursor_col
                        .min(self.input.lines[self.input.cursor_row].len());
                }
            }
            (_, KeyCode::Down) => {
                if self.input.cursor_row + 1 < self.input.lines.len() {
                    self.input.cursor_row += 1;
                    self.input.cursor_col = self.input.cursor_col
                        .min(self.input.lines[self.input.cursor_row].len());
                }
            }
            (_, KeyCode::Left) => {
                if self.input.cursor_col > 0 {
                    self.input.cursor_col -= 1;
                }
            }
            (_, KeyCode::Right) => {
                if self.input.cursor_col < self.input.lines[self.input.cursor_row].len() {
                    self.input.cursor_col += 1;
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
    }

    fn handle_key_streaming(&mut self, key: KeyEvent) {
        match (key.modifiers, key.code) {
            (KeyModifiers::CONTROL, KeyCode::Char('c')) | (_, KeyCode::Esc) => {
                self.conversation.finalize_stream();
                self.mode = AppMode::Normal;
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
                let model = self.provider.model_name().to_string();
                let provider = &self.config.default_provider;
                self.conversation.push_delta(&format!(
                    "**Current model:** `{}`\n**Provider:** `{}`",
                    model, provider
                ));
                self.conversation.finalize_stream();
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
                self.scroll_offset = 0;
                self.at_bottom = true;
                self.render_cache.clear();
                self.render_cache_msg_count = 0;
                return true;
            }
            "/help" => {
                let mut help = String::from("**Available commands:**\n\n");
                for cmd in crate::command::COMMANDS {
                    help.push_str(&format!("  `{}` — {}\n", cmd.name, cmd.description));
                }
                help.push_str("\n**Shortcuts:**\n\n");
                help.push_str("  `ctrl+j` — Send message\n");
                help.push_str("  `ctrl+c` — Quit\n");
                help.push_str("  `Esc` — Quit (with confirmation)\n");
                help.push_str("  `ctrl+Up/Down` — Scroll chat\n");
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

        self.conversation.add_user_message(&content);
        self.input.clear();
        self.mode = AppMode::Streaming;
        self.at_bottom = true;
        self.tool_call_count = 0;
        self.turn_start = Some(Instant::now());
        self.last_turn_duration = None;

        let system_prompt = self.system_prompt();
        let messages = self.conversation.to_provider_messages(&system_prompt);
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

        // Detect bash `cd` commands: check the last tool call to see if it was bash with cd
        if result.success {
            if let Some(last_call) = self.get_last_tool_call() {
                if last_call.name == "bash" {
                    if let Ok(args) = serde_json::from_str::<serde_json::Value>(&last_call.arguments) {
                        if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
                            // Extract cd target from commands like "cd /path" or "cd /path && ..."
                            let trimmed = cmd.trim();
                            if trimmed.starts_with("cd ") {
                                let cd_arg = trimmed[3..].split(&['&', ';', '|'][..]).next().unwrap_or("").trim();
                                if !cd_arg.is_empty() {
                                    let _ = self.try_change_dir(cd_arg);
                                }
                            }
                        }
                    }
                }
            }
        }

        self.conversation.add_tool_result(result);
        self.tool_call_count += 1;

        if self.tool_call_count >= 25 {
            self.conversation.push_delta("\n\n[Tool call limit reached (25). Send another message to continue.]");
            self.conversation.finalize_stream();
            self.mode = AppMode::Normal;
            self.tool_call_count = 0;
            return;
        }

        self.mode = AppMode::Streaming;
        self.at_bottom = true;
        self.continue_agent_loop(event_tx);
    }

    fn continue_agent_loop(&mut self, event_tx: &mpsc::UnboundedSender<AppEvent>) {
        let system_prompt = self.system_prompt();
        let messages = self.conversation.to_provider_messages(&system_prompt);
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
