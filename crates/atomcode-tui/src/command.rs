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
pub const BUILTIN_COMMANDS: &[(&str, &str)] = &[
    ("/model",    "Switch model/provider"),
    ("/provider", "Manage providers"),
    ("/cd",       "Change working directory"),
    ("/copy",     "Copy last AI response"),
    ("/clear",    "Clear conversation"),
    ("/config",   "Edit config file"),
    ("/login",    "Login with AtomGit OAuth"),
    ("/logout",   "Logout from AtomGit"),
    ("/help",     "Show commands & shortcuts"),
    ("/quit",     "Exit (or Ctrl+C x2)"),
];

/// Build the combined list of built-in commands + loaded skills.
///
/// Built-ins appear first in their original order.
/// Skills are appended in alphabetical order.
/// If a skill name collides with a built-in, the built-in wins (skill is silently omitted).
pub fn build_command_list(registry: &atomcode_core::skill::SkillRegistry) -> Vec<CommandEntry> {
    let mut entries: Vec<CommandEntry> = BUILTIN_COMMANDS
        .iter()
        .map(|(name, desc)| CommandEntry {
            name: name.to_string(),
            description: desc.to_string(),
            argument_hint: None,
            kind: CommandKind::BuiltIn,
        })
        .collect();

    let builtin_names: std::collections::HashSet<&str> =
        BUILTIN_COMMANDS.iter().map(|(name, _)| *name).collect();

    let mut skill_entries: Vec<CommandEntry> = registry
        .user_invocable()
        .filter(|s| !builtin_names.contains(format!("/{}", s.name).as_str()))
        .map(|s| CommandEntry {
            name: format!("/{}", s.name),
            description: s.description.clone(),
            argument_hint: s.argument_hint.clone(),
            kind: CommandKind::Skill,
        })
        .collect();
    skill_entries.sort_by(|a, b| a.name.cmp(&b.name));

    entries.extend(skill_entries);
    entries
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
}

impl SlashMenu {
    pub fn new() -> Self {
        Self {
            visible: false,
            filtered: Vec::new(),
            selected: 0,
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
        }
    }

    /// Move selection down.
    pub fn next(&mut self) {
        if !self.filtered.is_empty() {
            self.selected = (self.selected + 1) % self.filtered.len();
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
    }
}
