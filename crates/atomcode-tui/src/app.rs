use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
use tokio::sync::mpsc;

use atomcode_core::config::Config;
use atomcode_core::config::DEFAULT_SYSTEM_PROMPT;
use atomcode_core::conversation::Conversation;
use atomcode_core::provider::LlmProvider;
use atomcode_core::stream::StreamEvent;

use crate::command::SlashMenu;
use crate::event::AppEvent;
use crate::provider_manager::{ManagerAction, ProviderManager};

#[derive(Debug, Clone, PartialEq)]
pub enum AppMode {
    Normal,
    Streaming,
    ProviderManager,
    Exiting,
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
    pub provider: Box<dyn LlmProvider>,
    pub config: Config,
}

impl App {
    pub fn new(provider: Box<dyn LlmProvider>, config: Config) -> Self {
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
            provider,
            config,
        }
    }

    pub fn handle_event(&mut self, event: AppEvent, event_tx: &mpsc::UnboundedSender<AppEvent>) {
        // Any key press (except Esc) cancels quit confirmation
        if self.confirm_quit {
            if let AppEvent::Key(key) = &event {
                if key.code != KeyCode::Esc {
                    self.confirm_quit = false;
                }
            }
        }

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
            }
            AppEvent::StreamError(err) => {
                self.conversation.push_delta(&format!("\n\n[Error: {}]", err));
                self.conversation.finalize_stream();
                self.mode = AppMode::Normal;
                self.at_bottom = true;
            }
            AppEvent::Resize(_, _) => {}
            AppEvent::Tick => {}
        }
    }

    fn handle_key(&mut self, key: KeyEvent, event_tx: &mpsc::UnboundedSender<AppEvent>) {
        match self.mode {
            AppMode::Normal => self.handle_key_normal(key, event_tx),
            AppMode::Streaming => self.handle_key_streaming(key),
            AppMode::ProviderManager => self.handle_key_provider_manager(key),
            AppMode::Exiting => {}
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
                self.mode = AppMode::Exiting;
            }
            (KeyModifiers::SHIFT, KeyCode::Enter) => {
                self.input.insert_newline();
            }
            (KeyModifiers::CONTROL, KeyCode::Char('j')) | (_, KeyCode::Enter) => {
                self.slash_menu.close();
                self.send_message(event_tx);
            }
            (_, KeyCode::Esc) => {
                if self.conversation.messages.is_empty() || self.confirm_quit {
                    self.mode = AppMode::Exiting;
                } else {
                    self.confirm_quit = true;
                }
            }
            (KeyModifiers::CONTROL, KeyCode::Up) => {
                self.scroll_offset = self.scroll_offset.saturating_sub(3);
                self.at_bottom = false;
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
            (_, KeyCode::Char(c)) => {
                self.input.insert_char(c);
            }
            _ => {}
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
            "/clear" => {
                self.conversation = Conversation::new();
                self.scroll_offset = 0;
                self.at_bottom = true;
                // Don't add any message — just clear everything
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

        let provider_name = &self.config.default_provider;
        let system_prompt = self.config.providers
            .get(provider_name)
            .and_then(|p| p.system_prompt.as_deref())
            .unwrap_or(DEFAULT_SYSTEM_PROMPT)
            .to_string();

        let messages = self.conversation.to_provider_messages(&system_prompt);

        let tx = event_tx.clone();
        let stream_result = self.provider.chat_stream(&messages);

        tokio::spawn(async move {
            match stream_result {
                Ok(mut stream) => {
                    while let Some(event) = stream.next().await {
                        match event {
                            Ok(StreamEvent::Delta(text)) => {
                                let _ = tx.send(AppEvent::StreamDelta(text));
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
