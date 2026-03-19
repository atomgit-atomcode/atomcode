/// Slash command definition.
#[derive(Debug, Clone)]
pub struct SlashCommand {
    pub name: &'static str,
    pub description: &'static str,
}

/// All available slash commands.
pub const COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "/provider",
        description: "Manage providers (add, edit, delete, set default)",
    },
    SlashCommand {
        name: "/config",
        description: "Open configuration file in editor",
    },
    SlashCommand {
        name: "/model",
        description: "Show current model info",
    },
    SlashCommand {
        name: "/clear",
        description: "Clear conversation history",
    },
    SlashCommand {
        name: "/help",
        description: "Show available commands and shortcuts",
    },
    SlashCommand {
        name: "/copy",
        description: "Copy last AI response to clipboard",
    },
    SlashCommand {
        name: "/cd",
        description: "Change working directory",
    },
    SlashCommand {
        name: "/quit",
        description: "Exit AtomCode",
    },
];

/// State of the slash command autocomplete menu.
#[derive(Debug, Clone)]
pub struct SlashMenu {
    /// Whether the menu is currently visible.
    pub visible: bool,
    /// Indices into COMMANDS that match the current input.
    pub filtered: Vec<usize>,
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

    /// Update filtered list based on current input text.
    /// Returns true if menu should be visible.
    pub fn update(&mut self, input: &str) {
        let trimmed = input.trim();

        // Only activate on first line starting with /
        if !trimmed.starts_with('/') || trimmed.contains('\n') {
            self.visible = false;
            self.filtered.clear();
            self.selected = 0;
            return;
        }

        let query = trimmed.to_lowercase();
        self.filtered = COMMANDS
            .iter()
            .enumerate()
            .filter(|(_, cmd)| cmd.name.starts_with(&query))
            .map(|(i, _)| i)
            .collect();

        self.visible = !self.filtered.is_empty();

        // Clamp selection
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

    /// Get the currently selected command, if any.
    pub fn selected_command(&self) -> Option<&'static SlashCommand> {
        self.filtered
            .get(self.selected)
            .map(|&i| &COMMANDS[i])
    }

    /// Close the menu.
    pub fn close(&mut self) {
        self.visible = false;
        self.filtered.clear();
        self.selected = 0;
    }
}
