use crossterm::event::{KeyCode, KeyEvent};

use atomcode_core::config::provider::ProviderConfig;
use atomcode_core::config::Config;

/// Sub-states for the provider manager wizard.
#[derive(Debug, Clone, PartialEq)]
pub enum ManagerState {
    /// Browsing the provider list.
    List,
    /// Adding a new provider, step by step.
    Adding(AddStep),
    /// Confirming deletion of a provider.
    ConfirmDelete,
    /// Editing a specific field of a provider.
    Editing(EditField),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AddStep {
    Name,
    Type,
    ApiKey,
    BaseUrl,
    Model,
    OAuthPending,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EditField {
    Type = 0,
    ApiKey = 1,
    BaseUrl = 2,
    Model = 3,
}

/// Type selector for add/edit.
pub const PROVIDER_TYPES: &[(&str, &str)] = &[
    ("atomgit_login", "AtomGit (Login)"),
    ("claude", "Claude (Anthropic)"),
    ("openai", "OpenAI / Compatible"),
    ("ollama", "Ollama (local)"),
];

/// The provider manager state.
pub struct ProviderManager {
    pub state: ManagerState,
    pub selected: usize,
    pub input_buf: String,
    pub cursor_pos: usize,
    /// For the add wizard, accumulate fields here.
    pub new_name: String,
    pub new_type: String,
    pub new_api_key: String,
    pub new_base_url: String,
    pub new_model: String,
    /// Type selector index (for add/edit type step).
    pub type_selected: usize,
    /// Edit field selector index.
    pub edit_field_selected: usize,
    /// Message to display at bottom (feedback).
    pub message: Option<String>,
    /// Provider names in display order (cached from config).
    pub provider_names: Vec<String>,
}

impl ProviderManager {
    pub fn new(config: &Config) -> Self {
        let provider_names: Vec<String> = config.providers.keys().cloned().collect();
        Self {
            state: ManagerState::List,
            selected: 0,
            input_buf: String::new(),
            cursor_pos: 0,
            new_name: String::new(),
            new_type: String::new(),
            new_api_key: String::new(),
            new_base_url: String::new(),
            new_model: String::new(),
            type_selected: 0,
            edit_field_selected: 0,
            message: None,
            provider_names,
        }
    }

    /// Set input buffer and place cursor at end.
    fn set_input(&mut self, s: String) {
        self.cursor_pos = s.len();
        self.input_buf = s;
    }

    /// Clear input buffer and reset cursor.
    fn clear_input(&mut self) {
        self.input_buf.clear();
        self.cursor_pos = 0;
    }

    /// Insert a character at cursor position.
    fn insert_at_cursor(&mut self, c: char) {
        self.input_buf.insert(self.cursor_pos, c);
        self.cursor_pos += c.len_utf8();
    }

    /// Delete the character before cursor (backspace).
    fn backspace_at_cursor(&mut self) {
        if self.cursor_pos > 0 {
            // Find the previous char boundary
            let prev = self.input_buf[..self.cursor_pos]
                .char_indices()
                .next_back()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.input_buf.remove(prev);
            self.cursor_pos = prev;
        }
    }

    /// Render the input buffer with cursor marker for UI display.
    pub fn input_display(&self) -> String {
        let (before, after) = self.input_buf.split_at(self.cursor_pos);
        format!("{}│{}", before, after)
    }

    /// Handle common text-input keys (cursor movement, backspace, delete).
    /// Returns true if the key was consumed.
    fn handle_cursor_key(&mut self, key: &KeyEvent) -> bool {
        match key.code {
            KeyCode::Left => {
                if self.cursor_pos > 0 {
                    let prev = self.input_buf[..self.cursor_pos].char_indices().next_back().map(|(i, _)| i).unwrap_or(0);
                    self.cursor_pos = prev;
                }
                true
            }
            KeyCode::Right => {
                if self.cursor_pos < self.input_buf.len() {
                    let next = self.input_buf[self.cursor_pos..].char_indices().nth(1).map(|(i, _)| self.cursor_pos + i).unwrap_or(self.input_buf.len());
                    self.cursor_pos = next;
                }
                true
            }
            KeyCode::Home => { self.cursor_pos = 0; true }
            KeyCode::End => { self.cursor_pos = self.input_buf.len(); true }
            KeyCode::Delete => {
                if self.cursor_pos < self.input_buf.len() {
                    self.input_buf.remove(self.cursor_pos);
                }
                true
            }
            _ => false,
        }
    }

    pub fn refresh_names(&mut self, config: &Config) {
        self.provider_names = config.providers.keys().cloned().collect();
        if self.selected >= self.provider_names.len() && !self.provider_names.is_empty() {
            self.selected = self.provider_names.len() - 1;
        }
    }

    pub fn selected_name(&self) -> Option<&str> {
        self.provider_names.get(self.selected).map(|s| s.as_str())
    }

    /// Handle a key event. Returns Some(action) if the manager should do something,
    /// or None if it consumed the event internally.
    pub fn handle_key(&mut self, key: KeyEvent, config: &Config) -> Option<ManagerAction> {
        match &self.state {
            ManagerState::List => self.handle_list_key(key, config),
            ManagerState::Adding(step) => {
                let step = step.clone();
                self.handle_add_key(key, &step)
            }
            ManagerState::ConfirmDelete => self.handle_confirm_delete_key(key),
            ManagerState::Editing(field) => {
                let field = field.clone();
                self.handle_edit_key(key, &field)
            }
        }
    }

    fn handle_list_key(&mut self, key: KeyEvent, config: &Config) -> Option<ManagerAction> {
        match key.code {
            KeyCode::Up => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
                self.message = None;
                None
            }
            KeyCode::Down => {
                if self.selected + 1 < self.provider_names.len() {
                    self.selected += 1;
                }
                self.message = None;
                None
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                // Start adding
                self.state = ManagerState::Adding(AddStep::Name);
                self.clear_input();
                self.new_name.clear();
                self.new_type.clear();
                self.new_api_key.clear();
                self.new_base_url.clear();
                self.new_model.clear();
                self.type_selected = 0;
                self.message = None;
                None
            }
            KeyCode::Char('e') | KeyCode::Char('E') => {
                if self.provider_names.is_empty() {
                    return None;
                }
                self.state = ManagerState::Editing(EditField::Type);
                self.edit_field_selected = 0;
                self.clear_input();
                // Pre-fill type_selected based on current provider
                if let Some(name) = self.selected_name() {
                    if let Some(p) = config.providers.get(name) {
                        self.type_selected = PROVIDER_TYPES
                            .iter()
                            .position(|(t, _)| *t == p.provider_type)
                            .unwrap_or(0);
                    }
                }
                self.message = None;
                None
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                if self.provider_names.is_empty() {
                    return None;
                }
                if let Some(name) = self.selected_name() {
                    if name == config.default_provider {
                        self.message = Some("Cannot delete the default provider. Set another as default first.".to_string());
                        return None;
                    }
                }
                self.state = ManagerState::ConfirmDelete;
                self.message = None;
                None
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                // Set as default
                if let Some(name) = self.selected_name() {
                    let name = name.to_string();
                    self.message = Some(format!("'{}' set as default provider", name));
                    return Some(ManagerAction::SetDefault(name));
                }
                None
            }
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                Some(ManagerAction::Close)
            }
            _ => None,
        }
    }

    fn handle_add_key(&mut self, key: KeyEvent, step: &AddStep) -> Option<ManagerAction> {
        match step {
            AddStep::Name => {
                match key.code {
                    KeyCode::Esc => {
                        self.state = ManagerState::List;
                        return None;
                    }
                    KeyCode::Enter => {
                        let name = self.input_buf.trim().to_string();
                        if name.is_empty() {
                            self.message = Some("Name cannot be empty".to_string());
                            return None;
                        }
                        if self.provider_names.contains(&name) {
                            self.message = Some(format!("'{}' already exists", name));
                            return None;
                        }
                        self.new_name = name;
                        self.clear_input();
                        self.state = ManagerState::Adding(AddStep::Type);
                        self.type_selected = 0;
                        self.message = None;
                    }
                    KeyCode::Backspace => {
                        self.backspace_at_cursor();
                    }
                    KeyCode::Char(c) => {
                        // Only allow alphanumeric, dash, underscore
                        if c.is_alphanumeric() || c == '-' || c == '_' {
                            self.insert_at_cursor(c);
                        }
                    }
                    _ => { self.handle_cursor_key(&key); }
                }
                None
            }
            AddStep::Type => {
                match key.code {
                    KeyCode::Esc => {
                        self.state = ManagerState::Adding(AddStep::Name);
                        self.set_input(self.new_name.clone());
                        return None;
                    }
                    KeyCode::Up => {
                        if self.type_selected > 0 {
                            self.type_selected -= 1;
                        }
                    }
                    KeyCode::Down => {
                        if self.type_selected + 1 < PROVIDER_TYPES.len() {
                            self.type_selected += 1;
                        }
                    }
                    KeyCode::Enter => {
                        let selected_type = PROVIDER_TYPES[self.type_selected].0;
                        if selected_type == "atomgit_login" {
                            // Skip manual fields — trigger OAuth instead
                            self.state = ManagerState::Adding(AddStep::OAuthPending);
                            self.message = Some("Opening browser...".to_string());
                            return Some(ManagerAction::StartAtomGitOAuth(self.new_name.clone()));
                        }
                        self.new_type = selected_type.to_string();
                        self.clear_input();
                        self.state = ManagerState::Adding(AddStep::ApiKey);
                        self.message = None;
                    }
                    _ => {}
                }
                None
            }
            AddStep::ApiKey => {
                match key.code {
                    KeyCode::Esc => {
                        self.state = ManagerState::Adding(AddStep::Type);
                        return None;
                    }
                    KeyCode::Enter => {
                        self.new_api_key = self.input_buf.trim().to_string();
                        self.clear_input();
                        self.state = ManagerState::Adding(AddStep::BaseUrl);
                        self.message = None;
                        // Pre-fill base_url hint
                        if self.new_type == "openai" {
                            self.set_input("https://api.openai.com/v1".to_string());
                        } else if self.new_type == "ollama" {
                            self.set_input("http://localhost:11434".to_string());
                        }
                    }
                    KeyCode::Backspace => {
                        self.backspace_at_cursor();
                    }
                    KeyCode::Char(c) => {
                        self.insert_at_cursor(c);
                    }
                    _ => { self.handle_cursor_key(&key); }
                }
                None
            }
            AddStep::BaseUrl => {
                match key.code {
                    KeyCode::Esc => {
                        self.state = ManagerState::Adding(AddStep::ApiKey);
                        self.clear_input();
                        return None;
                    }
                    KeyCode::Enter => {
                        self.new_base_url = self.input_buf.trim().to_string();
                        self.clear_input();
                        self.state = ManagerState::Adding(AddStep::Model);
                        self.message = None;
                    }
                    KeyCode::Backspace => {
                        self.backspace_at_cursor();
                    }
                    KeyCode::Char(c) => {
                        self.insert_at_cursor(c);
                    }
                    _ => { self.handle_cursor_key(&key); }
                }
                None
            }
            AddStep::Model => {
                match key.code {
                    KeyCode::Esc => {
                        self.state = ManagerState::Adding(AddStep::BaseUrl);
                        self.clear_input();
                        return None;
                    }
                    KeyCode::Enter => {
                        self.new_model = self.input_buf.trim().to_string();
                        if self.new_model.is_empty() {
                            self.message = Some("Model cannot be empty".to_string());
                            return None;
                        }
                        self.clear_input();
                        self.state = ManagerState::List;
                        self.message = Some(format!("Provider '{}' added", self.new_name));

                        let provider = ProviderConfig {
                            provider_type: self.new_type.clone(),
                            api_key: if self.new_api_key.is_empty() {
                                None
                            } else {
                                Some(self.new_api_key.clone())
                            },
                            model: self.new_model.clone(),
                            base_url: if self.new_base_url.is_empty() {
                                None
                            } else {
                                Some(self.new_base_url.clone())
                            },
                            system_prompt: None,
                            user_agent: None,
                            context_window: atomcode_core::config::provider::default_context_window_for(&self.new_type),
                            max_tokens: None,
                            thinking_type: None,
                            thinking_keep: None,
            reasoning_history: None,
                            ephemeral: false,
                        };
                        return Some(ManagerAction::Add(self.new_name.clone(), provider));
                    }
                    KeyCode::Backspace => {
                        self.backspace_at_cursor();
                    }
                    KeyCode::Char(c) => {
                        self.insert_at_cursor(c);
                    }
                    _ => { self.handle_cursor_key(&key); }
                }
                None
            }
            AddStep::OAuthPending => {
                // Waiting for OAuth callback — only Esc is handled here.
                // The actual OAuth runs in lib.rs via pending_login.
                if let KeyCode::Esc = key.code {
                    self.state = ManagerState::Adding(AddStep::Type);
                    self.message = None;
                }
                None
            }
        }
    }

    fn handle_confirm_delete_key(&mut self, key: KeyEvent) -> Option<ManagerAction> {
        match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(name) = self.selected_name() {
                    let name = name.to_string();
                    self.message = Some(format!("Provider '{}' deleted", name));
                    self.state = ManagerState::List;
                    return Some(ManagerAction::Delete(name));
                }
                self.state = ManagerState::List;
                None
            }
            _ => {
                self.state = ManagerState::List;
                self.message = Some("Delete cancelled".to_string());
                None
            }
        }
    }

    fn handle_edit_key(&mut self, key: KeyEvent, field: &EditField) -> Option<ManagerAction> {
        // Edit mode: show fields, Up/Down to pick field, Enter to edit, Esc to go back
        match field {
            EditField::Type => {
                // Selecting which field to edit
                match key.code {
                    KeyCode::Esc => {
                        self.state = ManagerState::List;
                        return None;
                    }
                    KeyCode::Up => {
                        if self.edit_field_selected > 0 {
                            self.edit_field_selected -= 1;
                        }
                    }
                    KeyCode::Down => {
                        if self.edit_field_selected < 3 {
                            self.edit_field_selected += 1;
                        }
                    }
                    KeyCode::Enter => {
                        // Start editing the selected field
                        self.clear_input();
                        let edit_field = match self.edit_field_selected {
                            0 => EditField::Type,
                            1 => EditField::ApiKey,
                            2 => EditField::BaseUrl,
                            3 => EditField::Model,
                            _ => return None,
                        };
                        if edit_field == EditField::Type {
                            // Type uses selector, not text input — cycle through
                            self.type_selected = (self.type_selected + 1) % PROVIDER_TYPES.len();
                            if let Some(name) = self.selected_name() {
                                let name = name.to_string();
                                let new_type = PROVIDER_TYPES[self.type_selected].0.to_string();
                                return Some(ManagerAction::UpdateField(
                                    name,
                                    "type".to_string(),
                                    new_type,
                                ));
                            }
                        } else {
                            self.state = ManagerState::Editing(edit_field);
                        }
                    }
                    _ => {}
                }
                None
            }
            EditField::ApiKey | EditField::BaseUrl | EditField::Model => {
                // Text input for the field
                match key.code {
                    KeyCode::Esc => {
                        self.state = ManagerState::Editing(EditField::Type);
                        self.clear_input();
                        return None;
                    }
                    KeyCode::Enter => {
                        let value = self.input_buf.trim().to_string();
                        if let Some(name) = self.selected_name() {
                            let name = name.to_string();
                            let field_name = match field {
                                EditField::ApiKey => "api_key",
                                EditField::BaseUrl => "base_url",
                                EditField::Model => "model",
                                _ => return None,
                            };
                            self.state = ManagerState::Editing(EditField::Type);
                            self.clear_input();
                            self.message = Some(format!("Updated {} for '{}'", field_name, name));
                            return Some(ManagerAction::UpdateField(
                                name,
                                field_name.to_string(),
                                value,
                            ));
                        }
                        self.state = ManagerState::Editing(EditField::Type);
                        None
                    }
                    KeyCode::Backspace => {
                        self.backspace_at_cursor();
                        None
                    }
                    KeyCode::Char(c) => {
                        self.insert_at_cursor(c);
                        None
                    }
                    _ => { self.handle_cursor_key(&key); None }
                }
            }
        }
    }
}

/// Actions that the provider manager requests from the App.
pub enum ManagerAction {
    Close,
    SetDefault(String),
    Delete(String),
    Add(String, ProviderConfig),
    UpdateField(String, String, String),
    /// Trigger AtomGit OAuth login; saves provider with the given name on success.
    StartAtomGitOAuth(String),
}
