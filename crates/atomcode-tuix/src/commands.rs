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
    Command { name: "remember", desc: "Save a fact to memory (/remember --global for global)", needs_args: true },
    Command { name: "forget", desc: "Remove matching memories", needs_args: true },
    Command { name: "memory", desc: "Show all saved memories", needs_args: false },
    Command { name: "mcp",     desc: "Show MCP server status (subcommand: reload)", needs_args: false },
    Command { name: "undo",    desc: "Undo last change (not yet supported)", needs_args: false },
    Command { name: "worktree", desc: "Git worktree isolation (create/list/done/cleanup)", needs_args: true },
    Command { name: "upgrade", desc: "Upgrade atomcode to latest (subcommand: rollback)", needs_args: false },
    Command { name: "issue",   desc: "Report a bug / request a feature for AtomCode itself (interactive wizard)", needs_args: false },
    Command { name: "plan",    desc: "Switch to Plan mode (read-only exploration)", needs_args: false },
    Command { name: "build",   desc: "Switch to Build mode (full execution)", needs_args: false },
    Command { name: "think",   desc: "Extended thinking control (on/off/budget N)", needs_args: false },
    Command { name: "goal",    desc: "Set a completion goal (autonomous loop until met)", needs_args: false },
    Command { name: "help",    desc: "Show this help", needs_args: false },
    Command { name: "quit",    desc: "Exit AtomCode", needs_args: false },
    // Gateway entry that opens a second-level palette listing all
    // user-invocable skills. needs_args=true so Enter rewrites the
    // buffer to `/skills ` and lets the sub-mode menu render the
    // skill list. Selecting a skill commits as `/skills <name>` →
    // dispatched by the `skills` arm in execute_slash_command.
    Command { name: "skills",  desc: "Browse loaded skills", needs_args: true },
    Command { name: "plugin",  desc: "Plugin marketplace (subcommands: marketplace, install, uninstall, list)", needs_args: true },
];

/// A completion candidate for slash-command Tab completion, merging built-in
/// and user-defined custom commands.
#[derive(Debug, Clone)]
pub struct CompletionCandidate {
    pub name: String,
    pub description: String,
    pub is_custom: bool,
}

/// Merge built-in and custom command completions for a given prefix.
/// Results are sorted with built-ins first, then custom commands, each
/// group sorted by name. Custom commands whose names collide with a
/// built-in are suppressed.
pub fn complete_commands(
    prefix: &str,
    custom_names: &[(String, String)],
) -> Vec<CompletionCandidate> {
    let prefix = prefix.strip_prefix('/').unwrap_or(prefix);
    let mut candidates = Vec::new();
    for cmd in BUILTIN_COMMANDS {
        if cmd.name.starts_with(prefix) {
            candidates.push(CompletionCandidate {
                name: cmd.name.to_string(),
                description: cmd.desc.to_string(),
                is_custom: false,
            });
        }
    }
    for (name, desc) in custom_names {
        if name.starts_with(prefix) && !candidates.iter().any(|c| c.name == *name) {
            candidates.push(CompletionCandidate {
                name: name.clone(),
                description: desc.clone(),
                is_custom: true,
            });
        }
    }
    candidates.sort_by_key(|c| (c.is_custom, c.name.clone()));
    candidates
}

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
    // Allow `:` in command names so namespaced skills like
    // `/skills:brainstorming` (loose skill, atomcode prefix) and
    // `/superpowers:writing-plans` (Claude Code plugin convention)
    // parse as a single command name. Paths like `/Users/me/...` are
    // still rejected by the non-whitespace follow-on check below.
    let name_end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == ':'))
        .unwrap_or(rest.len());
    if name_end == 0 {
        return None;
    }
    let name = &rest[..name_end];
    let after = &rest[name_end..];
    match after.chars().next() {
        None => Some((name, "")),
        Some(c) if c.is_whitespace() => Some((name, after.trim_start())),
        // Non-space follow-on (`/`, `.`, etc.) means the `/` was
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
    fn parse_accepts_colon_namespaced_command() {
        // Skills load under a `skills:` namespace; plugins (future) use
        // their manifest name. The parser must keep the colon segment as
        // part of the command name, not split on it.
        let (cmd, arg) = parse_slash_line("/skills:brainstorming").unwrap();
        assert_eq!(cmd, "skills:brainstorming");
        assert_eq!(arg, "");

        let (cmd, arg) = parse_slash_line("/skills:brainstorming why is X").unwrap();
        assert_eq!(cmd, "skills:brainstorming");
        assert_eq!(arg, "why is X");

        let (cmd, _) = parse_slash_line("/superpowers:writing-plans").unwrap();
        assert_eq!(cmd, "superpowers:writing-plans");
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

    #[test]
    fn complete_builtin_commands() {
        let candidates = complete_commands("mo", &[]);
        assert!(
            candidates.iter().any(|c| c.name == "model"),
            "\"mo\" should match built-in \"model\""
        );
        assert!(
            candidates.iter().all(|c| !c.is_custom),
            "built-in-only query should have no custom candidates"
        );
    }

    #[test]
    fn complete_custom_commands() {
        let custom = vec![("review".to_string(), "Code review".to_string())];
        let candidates = complete_commands("rev", &custom);
        assert!(
            candidates.iter().any(|c| c.name == "review" && c.is_custom),
            "\"rev\" should match custom \"review\""
        );
    }

    #[test]
    fn builtin_takes_precedence() {
        // Custom "help" should NOT appear because built-in "help" exists.
        let custom = vec![("help".to_string(), "Custom help".to_string())];
        let candidates = complete_commands("help", &custom);
        let help_count = candidates.iter().filter(|c| c.name == "help").count();
        assert_eq!(
            help_count, 1,
            "custom \"help\" must not duplicate built-in \"help\""
        );
        assert!(
            !candidates.iter().any(|c| c.name == "help" && c.is_custom),
            "the surviving \"help\" must be the built-in, not custom"
        );
    }

    #[test]
    fn empty_prefix_returns_all() {
        let custom = vec![
            ("review".to_string(), "Code review".to_string()),
            ("deploy".to_string(), "Deploy app".to_string()),
        ];
        let candidates = complete_commands("", &custom);
        // At least all 25 built-in commands + 2 custom
        assert!(
            candidates.len() >= 20,
            "empty prefix should return at least 20 results, got {}",
            candidates.len()
        );
        // Custom commands should be present
        assert!(candidates.iter().any(|c| c.name == "review"));
        assert!(candidates.iter().any(|c| c.name == "deploy"));
    }

    #[test]
    fn complete_commands_strips_leading_slash() {
        // Calling with "/mo" should behave identically to "mo".
        let with_slash = complete_commands("/mo", &[]);
        let without_slash = complete_commands("mo", &[]);
        assert_eq!(with_slash.len(), without_slash.len());
        for (a, b) in with_slash.iter().zip(without_slash.iter()) {
            assert_eq!(a.name, b.name);
        }
    }
}
