// crates/atomcode-tuix/src/commands.rs
#[derive(Debug, Clone, Copy)]
pub struct Command {
    pub name: &'static str,
    pub desc: &'static str,
    /// Commands that are *useless* without an argument (e.g. `/background <task>`).
    /// When the slash-menu Enter handler sees one, it auto-completes the name
    /// with a trailing space and leaves the cursor parked for the user to
    /// type the argument — instead of firing a bad invocation immediately.
    /// Commands that do something sensible with no arg (e.g. `/cd` opens the
    /// recent-dirs picker, `/help` prints help) leave this `false`.
    pub needs_args: bool,
}

pub struct CommandRegistry {
    commands: &'static [Command],
}

impl CommandRegistry {
    pub fn builtin() -> Self {
        Self {
            commands: BUILTIN_COMMANDS,
        }
    }

    pub fn all(&self) -> &'static [Command] {
        self.commands
    }

    pub fn find(&self, name: &str) -> Option<Command> {
        // Built-in command names are all ASCII, so an ASCII
        // case-insensitive match is equivalent to a Unicode-correct
        // one here. `/SESSION` resolves to the same `session` entry
        // as `/session`.
        self.commands
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
            .copied()
    }

    pub fn matching_prefix(&self, prefix: &str) -> Vec<Command> {
        let prefix_lower = prefix.to_ascii_lowercase();
        self.commands
            .iter()
            .filter(|c| c.name.starts_with(prefix_lower.as_str()))
            .copied()
            .collect()
    }

    pub fn help_text(&self) -> String {
        let max_name = self
            .commands
            .iter()
            .map(|c| c.name.len())
            .max()
            .unwrap_or(6);
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
    Command { name: "codingplan", desc: "Claim CodingPlan + set up models from the plan's model list", needs_args: false },
    Command { name: "resume",  desc: "Resume a previous session", needs_args: false },
    Command { name: "login",   desc: "Sign in with AtomGit OAuth", needs_args: false },
    Command { name: "logout",  desc: "Sign out of AtomGit", needs_args: false },
    Command { name: "whoami",  desc: "Show current logged-in user", needs_args: false },
    Command { name: "model",   desc: "Switch provider / model", needs_args: false },
    Command { name: "provider", desc: "Manage providers (add / edit / delete)", needs_args: false },
    Command { name: "status",  desc: "Show session status", needs_args: false },
    Command { name: "config",  desc: "Show config path", needs_args: false },
    Command { name: "reload",  desc: "Reload ~/.atomcode/config.toml from disk", needs_args: false },
    Command { name: "cd",      desc: "Change working directory", needs_args: false },
    Command { name: "init",    desc: "Generate .atomcode.md project instructions from the working directory", needs_args: false },
    Command { name: "background", desc: "Run a one-shot task in an isolated background context (read-only-ish tool subset)", needs_args: true },
    Command { name: "diff",    desc: "Show git diff", needs_args: false },
    Command { name: "clear",   desc: "Clear screen", needs_args: false },
    Command { name: "session", desc: "Start a new session (clears conversation)", needs_args: false },
    Command { name: "cost",    desc: "Show token cost", needs_args: false },
    Command { name: "context", desc: "Show context budget breakdown", needs_args: false },
    Command { name: "compact", desc: "Compact conversation history", needs_args: false },
    Command { name: "mcp",     desc: "Show MCP server status (subcommand: reload)", needs_args: false },
    Command { name: "undo",    desc: "Undo last change (not yet supported)", needs_args: false },
    Command { name: "worktree", desc: "Git worktree isolation (create/list/done/cleanup)", needs_args: true },
    Command { name: "upgrade", desc: "Upgrade atomcode to latest (subcommand: rollback)", needs_args: false },
    Command { name: "issue",   desc: "Report a bug / request a feature for AtomCode itself (interactive wizard)", needs_args: false },
    Command { name: "plan",    desc: "Switch to Plan mode (read-only exploration)", needs_args: false },
    Command { name: "build",   desc: "Switch to Build mode (full execution)", needs_args: false },
    Command { name: "think",   desc: "Extended thinking control (on/off/budget N)", needs_args: false },
    Command { name: "help",    desc: "Show this help", needs_args: false },
    Command { name: "quit",    desc: "Exit AtomCode", needs_args: false },
];

/// Parse `"/cmd args..."` into `(cmd, args)` when the leading `/` is a
/// command invocation. Returns `None` when the `/` is actually part of a
/// filesystem path, URL, or any other text the user wants sent to the
/// agent verbatim.
///
/// A valid command name is ASCII alphanumeric + `_`/`-`, followed by
/// whitespace or end-of-input. `/Users/me`, `/tmp`, `/https://...`,
/// `/path/with/mixed/字符` all fail the shape test and fall through to
/// agent dispatch.
pub fn parse_slash_line(s: &str) -> Option<(&str, &str)> {
    let rest = s.strip_prefix('/')?;
    let name_end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .unwrap_or(rest.len());
    if name_end == 0 {
        return None;
    }
    let name = &rest[..name_end];
    let after = &rest[name_end..];
    match after.chars().next() {
        None => Some((name, "")),
        Some(c) if c.is_whitespace() => Some((name, after.trim_start())),
        // Non-space follow-on (`/`, `.`, `:`, etc.) means the `/` was
        // a literal character in a path / URL — not a command.
        _ => None,
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
    fn parse_rejects_path_starting_with_slash() {
        // A filesystem path the user pastes must reach the agent
        // untouched, not trigger "Unknown command: /Users/...".
        assert!(parse_slash_line("/Users/me/file.txt").is_none());
        assert!(parse_slash_line("/tmp/x").is_none());
        assert!(parse_slash_line("/path/with/中文/pic.png").is_none());
    }

    #[test]
    fn parse_rejects_url_starting_with_slash() {
        assert!(parse_slash_line("/https://example.com/x").is_none());
    }

    #[test]
    fn parse_command_with_slash_argument_ok() {
        // `/cd /path` is a command with a path argument — the second
        // slash sits in args, not the command name.
        let (cmd, arg) = parse_slash_line("/cd /tmp/x").unwrap();
        assert_eq!(cmd, "cd");
        assert_eq!(arg, "/tmp/x");
    }

    #[test]
    fn parse_rejects_cjk_touching_command_name() {
        // `/session是干什么的` — the user is asking the agent "what
        // does /session do", NOT invoking /session. A CJK char
        // directly after the command name (no whitespace) means it's
        // prose, so parse_slash_line must return None and the line
        // reaches the agent verbatim.
        assert!(parse_slash_line("/session是干什么的").is_none());
        assert!(parse_slash_line("/quit退出吗").is_none());
        assert!(parse_slash_line("/model模型").is_none());
    }

    #[test]
    fn parse_accepts_command_with_cjk_arg_after_space() {
        // Whitespace separates cmd from args, so `/session 是干什么的`
        // IS an invocation (with CJK-tail arg).
        let (cmd, arg) = parse_slash_line("/session 是干什么的").unwrap();
        assert_eq!(cmd, "session");
        assert_eq!(arg, "是干什么的");
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
