/// Whether a command entry is a built-in or a user-defined skill.
#[derive(Debug, Clone, PartialEq)]
pub enum CommandKind {
    BuiltIn,
    Skill,
}

/// A single entry in the slash-command menu — either a built-in command or a loaded skill.
#[derive(Debug, Clone)]
pub struct CommandEntry {
    /// Full name with leading slash, e.g. "/commit"
    pub name: String,
    /// Short description shown in the menu.
    pub description: String,
    /// Optional argument hint shown in autocomplete, e.g. "[issue-number]".
    pub argument_hint: Option<String>,
    pub kind: CommandKind,
}

/// Static built-in slash commands.
/// Ordered by importance: core features first, utilities second, help/config last.
pub const BUILTIN_COMMANDS: &[(&str, &str)] = &[
    // === Core features ===
    ("/resume",   "Resume or switch session"),
    ("/session",  "Create a new session"),
    ("/provider", "Manage providers"),
    ("/model",    "Switch model/provider"),
    ("/login",    "Login with AtomGit OAuth"),
    ("/cd",       "Change working directory"),
    
    // === Utilities ===
    ("/undo",     "Undo last turn's edits"),
    ("/diff",     "Show git diff of current changes"),
    ("/cost",     "Show token usage for this session"),
    ("/copy",     "Copy last AI response"),
    ("/clear",    "Clear conversation"),
    ("/issue",    "Create issue on AtomGit"),
    
    // === Config & Help ===
    ("/config",   "Edit config file"),
    ("/status",   "Show login status and model info"),
    ("/logout",   "Logout from AtomGit"),
    ("/help",     "Show commands & shortcuts"),
    ("/quit",     "Exit (or Ctrl+C x2)"),
];

/// Build the command list with only built-in commands.
/// Skills are not shown in the menu but can still be invoked by typing their name.
pub fn build_command_list(_registry: &atomcode_core::skill::SkillRegistry) -> Vec<CommandEntry> {
    BUILTIN_COMMANDS
        .iter()
        .map(|(name, desc)| CommandEntry {
            name: name.to_string(),
            description: desc.to_string(),
            argument_hint: None,
            kind: CommandKind::BuiltIn,
        })
        .collect()
}

/// State of the slash command autocomplete menu.
#[derive(Debug, Clone)]
pub struct SlashMenu {
    /// Whether the menu is currently visible.
    pub visible: bool,
    /// Entries that match the current filter query.
    pub filtered: Vec<CommandEntry>,
    /// Currently highlighted index within `filtered`.
    pub selected: usize,
    /// Scroll offset for when items exceed visible area.
    pub scroll_offset: usize,
}

impl SlashMenu {
    pub fn new() -> Self {
        Self {
            visible: false,
            filtered: Vec::new(),
            selected: 0,
            scroll_offset: 0,
        }
    }

    /// Rebuild the filtered list from `all_commands` based on the current input text.
    pub fn update(&mut self, input: &str, all_commands: &[CommandEntry]) {
        let trimmed = input.trim();

        // Only activate on a single line starting with /
        if !trimmed.starts_with('/') || trimmed.contains('\n') {
            self.visible = false;
            self.filtered.clear();
            self.selected = 0;
            return;
        }

        let query = trimmed.to_lowercase();
        self.filtered = all_commands
            .iter()
            .filter(|cmd| cmd.name.to_lowercase().starts_with(&query))
            .cloned()
            .collect();

        self.visible = !self.filtered.is_empty();

        if self.selected >= self.filtered.len() {
            self.selected = 0;
        }
    }

    /// Move selection up.
    pub fn prev(&mut self) {
        if !self.filtered.is_empty() {
            if self.selected == 0 {
                self.selected = self.filtered.len() - 1;
            } else {
                self.selected -= 1;
            }
            self.adjust_scroll();
        }
    }

    /// Move selection down.
    pub fn next(&mut self) {
        if !self.filtered.is_empty() {
            self.selected = (self.selected + 1) % self.filtered.len();
            self.adjust_scroll();
        }
    }

    /// Ensure selected item is visible by adjusting scroll_offset.
    /// Max visible items = 12 (menu_height 14 - 2 for borders).
    fn adjust_scroll(&mut self) {
        const MAX_VISIBLE: usize = 12;
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + MAX_VISIBLE {
            self.scroll_offset = self.selected - MAX_VISIBLE + 1;
        }
    }

    /// Get the currently selected entry, if any.
    pub fn selected_command(&self) -> Option<&CommandEntry> {
        self.filtered.get(self.selected)
    }

    /// Close the menu.
    pub fn close(&mut self) {
        self.visible = false;
        self.filtered.clear();
        self.selected = 0;
        self.scroll_offset = 0;
    }
}
