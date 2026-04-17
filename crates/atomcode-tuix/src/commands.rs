// crates/atomcode-tuix/src/commands.rs
#[derive(Debug, Clone, Copy)]
pub struct Command {
    pub name: &'static str,
    pub desc: &'static str,
}

pub struct CommandRegistry {
    commands: &'static [Command],
}

impl CommandRegistry {
    pub fn builtin() -> Self {
        Self { commands: BUILTIN_COMMANDS }
    }

    pub fn all(&self) -> &'static [Command] {
        self.commands
    }

    pub fn find(&self, name: &str) -> Option<Command> {
        self.commands.iter().find(|c| c.name == name).copied()
    }

    pub fn matching_prefix(&self, prefix: &str) -> Vec<Command> {
        self.commands
            .iter()
            .filter(|c| c.name.starts_with(prefix))
            .copied()
            .collect()
    }

    pub fn help_text(&self) -> String {
        let max_name = self.commands.iter().map(|c| c.name.len()).max().unwrap_or(6);
        let mut out = String::from("  Available commands:\n");
        for c in self.commands {
            out.push_str(&format!(
                "    /{:<width$}  {}\n",
                c.name,
                c.desc,
                width = max_name
            ));
        }
        out
    }
}

const BUILTIN_COMMANDS: &[Command] = &[
    Command { name: "login",   desc: "Sign in with AtomGit OAuth" },
    Command { name: "logout",  desc: "Sign out of AtomGit" },
    Command { name: "whoami",  desc: "Show current logged-in user" },
    Command { name: "model",   desc: "Switch provider / model" },
    Command { name: "status",  desc: "Show session status" },
    Command { name: "config",  desc: "Show config path" },
    Command { name: "cd",      desc: "Change working directory" },
    Command { name: "diff",    desc: "Show git diff" },
    Command { name: "clear",   desc: "Clear screen" },
    Command { name: "cost",    desc: "Show token cost" },
    Command { name: "undo",    desc: "Undo last change (not yet supported)" },
    Command { name: "help",    desc: "Show this help" },
    Command { name: "quit",    desc: "Exit AtomCode" },
];

/// Parse "/cmd args..." into (cmd, args). Returns None if not a slash line.
pub fn parse_slash_line(s: &str) -> Option<(&str, &str)> {
    let s = s.strip_prefix('/')?;
    match s.find(char::is_whitespace) {
        Some(i) => Some((&s[..i], s[i..].trim_start())),
        None => Some((s, "")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_lookup_by_name() {
        let reg = CommandRegistry::builtin();
        assert!(reg.find("quit").is_some());
        assert!(reg.find("nonexistent").is_none());
    }

    #[test]
    fn tab_completion_finds_prefix_matches() {
        let reg = CommandRegistry::builtin();
        let matches = reg.matching_prefix("h");
        assert!(matches.iter().any(|c| c.name == "help"));
    }

    #[test]
    fn tab_completion_empty_for_unknown() {
        let reg = CommandRegistry::builtin();
        let matches = reg.matching_prefix("zzzzz");
        assert!(matches.is_empty());
    }

    #[test]
    fn parse_extracts_command_and_args() {
        let (cmd, arg) = parse_slash_line("/cd ~/projects").unwrap();
        assert_eq!(cmd, "cd");
        assert_eq!(arg, "~/projects");
    }

    #[test]
    fn parse_no_args() {
        let (cmd, arg) = parse_slash_line("/quit").unwrap();
        assert_eq!(cmd, "quit");
        assert_eq!(arg, "");
    }

    #[test]
    fn parse_non_slash_returns_none() {
        assert!(parse_slash_line("hello").is_none());
    }

    #[test]
    fn help_text_lists_all_commands() {
        let reg = CommandRegistry::builtin();
        let help = reg.help_text();
        for c in reg.all() {
            assert!(help.contains(c.name), "help missing {}", c.name);
        }
    }
}
