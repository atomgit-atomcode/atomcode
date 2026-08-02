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
    /// Hidden commands are still dispatchable but are excluded from the `/`
    /// slash-menu, Tab completion, and `/help` output. Use this for deprecated
    /// aliases that must keep working for muscle memory / scripting without
    /// cluttering the discovery surfaces.
    pub hidden: bool,
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
        // as `/session`. Aliases resolve to their canonical command first,
        // so `find("new")` returns the `session` entry (and its `needs_args`).
        let canonical = canonical_command_name(name);
        self.commands
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(canonical))
            .copied()
    }

    pub fn matching_prefix(&self, prefix: &str) -> Vec<Command> {
        let prefix_lower = prefix.to_ascii_lowercase();
        self.commands
            .iter()
            .filter(|c| !c.hidden && command_name_or_alias_has_prefix(c.name, &prefix_lower))
            .copied()
            .collect()
    }

    pub fn help_text(&self) -> String {
        use crate::i18n::{t, Msg};
        let max_name = self
            .commands
            .iter()
            .filter(|c| !c.hidden)
            .map(|c| c.name.len())
            .max()
            .unwrap_or(6);
        let mut out = t(Msg::HelpAvailableCommands).into_owned();
        for c in self.commands.iter().filter(|c| !c.hidden) {
            let desc = cmd_desc_i18n(c.name).unwrap_or_else(|| c.desc.into());
            out.push_str(&format!(
                "    /{:<width$}  {}\n",
                c.name,
                desc,
                width = max_name
            ));
        }
        out
    }
}

/// Alias → canonical built-in command name.
///
/// Unlike a hidden duplicate `Command` entry (the old `exit`/`quit` style), an
/// alias is NOT a separate registry row: it shares the canonical command's
/// single entry, is searchable by its own prefix in the slash menu, resolves to
/// the canonical name on dispatch, and renders as `canonical (alias)` — one
/// annotated row, not two. Add a pair here to give any command an alias.
const COMMAND_ALIASES: &[(&str, &str)] = &[
    // `/new` is a memorable alias for `/session` (start a fresh session).
    ("new", "session"),
];

/// Resolve an alias to its canonical command name (ASCII case-insensitive).
/// Returns the input unchanged when it is not a known alias — so it is safe to
/// call on every command name before dispatch.
pub fn canonical_command_name(name: &str) -> &str {
    COMMAND_ALIASES
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(name))
        .map(|(_, canonical)| *canonical)
        .unwrap_or(name)
}

/// The aliases that resolve to `canonical`, in table order.
fn aliases_for(canonical: &str) -> impl Iterator<Item = &'static str> + '_ {
    COMMAND_ALIASES
        .iter()
        .filter(move |(_, c)| c.eq_ignore_ascii_case(canonical))
        .map(|(alias, _)| *alias)
}

/// True when `prefix_lower` (already lowercased) is a prefix of the command
/// name OR of any of its aliases. Lets `/ne` surface the `session` command
/// while `session` stays a single entry.
fn command_name_or_alias_has_prefix(name: &str, prefix_lower: &str) -> bool {
    name.starts_with(prefix_lower) || aliases_for(name).any(|a| a.starts_with(prefix_lower))
}

/// Slash-menu display label for a command: `session (new)` when it has
/// aliases, else just the name. The renderer uses this so an alias shows as one
/// annotated row; the stored item name stays canonical for insert/dispatch.
pub fn command_display_name(canonical: &str) -> String {
    let joined = aliases_for(canonical).collect::<Vec<_>>().join(", ");
    if joined.is_empty() {
        canonical.to_string()
    } else {
        format!("{canonical} ({joined})")
    }
}

const BUILTIN_COMMANDS: &[Command] = &[
    Command { name: "login",   desc: "Sign in with AtomGit OAuth and claim CodingPlan models", needs_args: false, hidden: false },
    // needs_args=true so selecting it only completes to `/webui ` (does NOT
    // launch) — lets the user append a subcommand (stop / lan / --host <addr>)
    // before Enter. A bare `/webui ` + Enter still launches on 127.0.0.1.
    Command { name: "webui",   desc: "Launch the browser webui (subcommands: stop, lan, --host <addr>)", needs_args: true, hidden: false },
    Command { name: "sync",    desc: "Attach to live webui session (/sync off to detach)", needs_args: false, hidden: false },
    Command { name: "app", desc: "Expose this session to the mobile App via relay (QR pairing; /app stop to detach)", needs_args: true, hidden: false },
    Command { name: "setup",      desc: "First run: install recommender skill + run it. Extra text forwarded as a steering hint", needs_args: true, hidden: false },
    Command { name: "resume",  desc: "Resume a previous session", needs_args: false, hidden: false },
    Command { name: "rename",  desc: "Rename current session", needs_args: true, hidden: false },
    Command { name: "logout",  desc: "Sign out of AtomGit", needs_args: false, hidden: false },
    Command { name: "whoami",  desc: "Show current logged-in user", needs_args: false, hidden: false },
    Command { name: "model",   desc: "Switch provider / model", needs_args: false, hidden: false },
    Command { name: "provider", desc: "Manage providers (add / edit / delete)", needs_args: false, hidden: false },
    Command { name: "proxy",   desc: "Switch outbound proxy mode", needs_args: false, hidden: false },
    Command { name: "status",  desc: "Show session status", needs_args: false, hidden: false },
    Command { name: "config",  desc: "Show config path", needs_args: false, hidden: false },
    Command { name: "reload",  desc: "Reload ~/.atomcode/config.toml from disk", needs_args: false, hidden: false },
    Command { name: "cd",      desc: "Change working directory and start a new session", needs_args: false, hidden: false },
    Command { name: "init",    desc: "Analyze the project and generate AGENTS.md", needs_args: false, hidden: false },
    Command { name: "bg",      desc: "Background sessions: /bg, /bg list, /bg <N>, /bg drop <N>", needs_args: false, hidden: false },
    Command { name: "background", desc: "Compatibility alias: start a one-shot task in a /bg slot", needs_args: true, hidden: false },
    Command { name: "diff",    desc: "Show git diff", needs_args: false, hidden: false },
    Command { name: "clear",   desc: "Start a new conversation (clears context + screen)", needs_args: false, hidden: false },
    Command { name: "session", desc: "Start a new session (clears conversation)", needs_args: false, hidden: false },
    Command { name: "usage",   desc: "Show CodingPlan usage (tabs: current / overview / models)", needs_args: false, hidden: false },
    // `/cost` reports THIS SESSION's token cost from local accounting × the model
    // price table — works for ANY model, including self-integrated ones the
    // gateway-only `/usage` modal can't see.
    Command { name: "cost",    desc: "Show this session's token cost (any model)", needs_args: false, hidden: false },
    Command { name: "context", desc: "Show context budget breakdown", needs_args: false, hidden: false },
    Command { name: "compact", desc: "Compact conversation history", needs_args: false, hidden: false },
    Command { name: "remember", desc: "Save a fact to memory (/remember --global for global)", needs_args: true, hidden: false },
    Command { name: "forget", desc: "Remove matching memories", needs_args: true, hidden: false },
    Command { name: "memory", desc: "Show all saved memories", needs_args: false, hidden: false },
    Command { name: "mcp",     desc: "Show MCP server status (subcommands: reload, tools, login, logout, trust, untrust)", needs_args: false, hidden: false },
    Command { name: "undo",    desc: "Undo a turn (memory rollback): /undo or /undo N", needs_args: true, hidden: false },
    // Opens the checkpoint picker — the same modal the double-Esc gesture opens —
    // so the feature is discoverable without knowing the keybind. needs_args=false:
    // selection happens in the modal, not on the command line.
    Command { name: "rewind",  desc: "Restore the conversation to an earlier checkpoint", needs_args: false, hidden: false },
    Command { name: "worktree", desc: "Git worktree isolation (create/list/done/cleanup)", needs_args: true, hidden: false },
    Command { name: "upgrade", desc: "Upgrade atomcode to latest (subcommand: rollback)", needs_args: false, hidden: false },
    Command { name: "plan",    desc: "Switch to Plan mode (read-only exploration)", needs_args: false, hidden: false },
    Command { name: "build",   desc: "Switch to Build mode (full execution)", needs_args: false, hidden: false },
    Command { name: "auto",    desc: "Switch to Auto mode (auto-approve all tools)", needs_args: false, hidden: false },
    Command { name: "review",  desc: "Code review the current changes (/review · /review staged · /review <base>)", needs_args: false, hidden: false },
    Command { name: "think",   desc: "Extended thinking control (on/off/budget N)", needs_args: false, hidden: false },
    // Gateway entry: opens a second-level palette (high / max / off).
    // needs_args=true so Enter rewrites the buffer to `/effort ` and the
    // sub-mode menu renders the three choices. Selecting one commits as
    // `/effort <choice>` → dispatched by the `effort` arm.
    Command { name: "effort",  desc: "DeepSeek reasoning effort control (high / max / off)", needs_args: true, hidden: false },
    // needs_args=true so selecting `/goal` from the palette only completes to
    // `/goal ` and waits for the user to type the goal — it must NOT execute
    // immediately (a bare `/goal` would just print status). Setting a goal
    // requires the condition text; `/goal status` / `/goal clear` still work by
    // typing the sub-command + Enter.
    Command { name: "goal",    desc: "Set a completion goal (autonomous loop until met)", needs_args: true, hidden: false },
    Command { name: "loop",    desc: "Repeat a prompt/command on an interval, or let the model self-pace", needs_args: true, hidden: false },
    Command { name: "help",    desc: "Show this help", needs_args: false, hidden: false },
    Command { name: "guide",   desc: "Ask atomcode-guide how to use", needs_args: true, hidden: false },
    Command { name: "keys",    desc: "Show keyboard shortcuts", needs_args: false, hidden: false },
    Command { name: "language", desc: "Switch display and commit language", needs_args: false, hidden: false },
    Command { name: "welcome", desc: "Re-run the onboarding wizard", needs_args: false, hidden: false },
    Command { name: "quit",    desc: "Exit AtomCode", needs_args: false, hidden: false },
    Command { name: "exit",    desc: "Exit AtomCode", needs_args: false, hidden: false },
    // Gateway entry that opens a second-level palette listing all
    // user-invocable skills. needs_args=true so Enter rewrites the
    // buffer to `/skills ` and lets the sub-mode menu render the
    // skill list. Selecting a skill commits as `/skills <name>` →
    // dispatched by the `skills` arm in execute_slash_command.
    Command { name: "skills",  desc: "Browse loaded skills", needs_args: true, hidden: false },
    // needs_args=false so selecting `/plugin` opens the manager modal on the
    // first Enter (like /model, /provider, /session). Subcommands
    // (`/plugin install x@mp`, `uninstall`, `marketplace`, `list`) still work
    // by typing the full line — needs_args only changes the menu-Enter behavior.
    Command { name: "plugin",  desc: "Plugin marketplace (subcommands: marketplace, install, uninstall, list)", needs_args: false, hidden: false },
    // Windows fallback for Ctrl+V: Windows Terminal / conhost
    // intercept Ctrl+V as their own `paste` action (which forwards
    // only `CF_UNICODETEXT`) before the keystroke reaches atomcode,
    // so an image-only clipboard never triggers the in-app handler.
    // `/paste` calls the same `try_paste_clipboard_image` →
    // `attach_image_to_input` pipeline directly so the user has a
    // terminal-agnostic way to attach an image. Works on every OS.
    Command { name: "paste",   desc: "Attach an image from the clipboard (Windows fallback for Ctrl+V)", needs_args: false, hidden: false },
    Command { name: "copy",    desc: "Copy a code block from the last reply to the clipboard (/copy, /copy N, /copy all, /copy msg)", needs_args: false, hidden: false },
    Command { name: "save",    desc: "Save the current conversation to a markdown file (/save, /save [filename])", needs_args: false, hidden: false },
    Command { name: "view",    desc: "View file content in an overlay modal", needs_args: true, hidden: false },
    Command { name: "todo",    desc: "Show the todo list; /todo add <task> appends one, /todo clear wipes it", needs_args: false, hidden: false },
    Command { name: "schedule", desc: "List scheduled tasks and next run times", needs_args: false, hidden: false },
    Command { name: "desktop", desc: "Open the AtomCode desktop app (or show the download link)", needs_args: false, hidden: false },
];

/// Look up the i18n translation for a built-in command description.
/// Returns `None` for unknown command names (callers fall back to
/// the static `desc` field).
pub fn cmd_desc_i18n(name: &str) -> Option<std::borrow::Cow<'static, str>> {
    use crate::i18n::{t, Msg};
    let msg = match name {
        "webui" => Msg::CmdDescWebui,
        "setup" => Msg::CmdDescSetup,
        "resume" => Msg::CmdDescResume,
        "rename" => Msg::CmdDescRename,
        "login" => Msg::CmdDescLogin,
        "logout" => Msg::CmdDescLogout,
        "whoami" => Msg::CmdDescWhoami,
        "model" => Msg::CmdDescModel,
        "provider" => Msg::CmdDescProvider,
        "status" => Msg::CmdDescStatus,
        "config" => Msg::CmdDescConfig,
        "reload" => Msg::CmdDescReload,
        "cd" => Msg::CmdDescCd,
        "init" => Msg::CmdDescInit,
        "bg" => Msg::CmdDescBg,
        "background" => Msg::CmdDescBackground,
        "diff" => Msg::CmdDescDiff,
        "clear" => Msg::CmdDescClear,
        "session" => Msg::CmdDescSession,
        "cost" => Msg::CmdDescCost,
        "usage" => Msg::CmdDescUsage,
        "context" => Msg::CmdDescContext,
        "compact" => Msg::CmdDescCompact,
        "remember" => Msg::CmdDescRemember,
        "forget" => Msg::CmdDescForget,
        "memory" => Msg::CmdDescMemory,
        "mcp" => Msg::CmdDescMcp,
        "undo" => Msg::CmdDescUndo,
        "rewind" => Msg::CmdDescRewind,
        "worktree" => Msg::CmdDescWorktree,
        "upgrade" => Msg::CmdDescUpgrade,
        "plan" => Msg::CmdDescPlan,
        "build" => Msg::CmdDescBuild,
        "auto" => Msg::CmdDescAuto,
        "think" => Msg::CmdDescThink,
        "effort" => Msg::CmdDescEffort,
        "help" => Msg::CmdDescHelp,
        "guide" => Msg::CmdDescGuide,
        "keys" => Msg::CmdDescKeys,
        "language" => Msg::CmdDescLanguage,
        "welcome" => Msg::CmdWelcomeDescription,
        "quit" => Msg::CmdDescQuit,
        "exit" => Msg::CmdDescQuit,
        "skills" => Msg::CmdDescSkills,
        "plugin" => Msg::CmdDescPlugin,
        "paste" => Msg::CmdDescPaste,
        "copy" => Msg::CmdDescCopy,
        "save" => Msg::CmdDescSave,
        "view" => Msg::CmdDescView,
        "app" => Msg::CmdDescApp,
        "sync" => Msg::CmdDescSync,
        "review" => Msg::CmdDescReview,
        "goal" => Msg::CmdDescGoal,
        "proxy" => Msg::CmdDescProxy,
        "todo" => Msg::CmdDescTodo,
        "loop" => Msg::CmdDescLoop,
        "schedule" => Msg::CmdDescSchedule,
        "desktop" => Msg::CmdDescDesktop,
        _ => return None,
    };
    Some(t(msg))
}

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
        if !cmd.hidden && cmd.name.starts_with(prefix) {
            candidates.push(CompletionCandidate {
                name: cmd.name.to_string(),
                description: cmd_desc_i18n(cmd.name)
                    .map(|cow| cow.into_owned())
                    .unwrap_or_else(|| cmd.desc.to_string()),
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

/// Detect a `!cmd` bash-mode line. Returns the trimmed command when the
/// line begins (strictly at column 0) with `!` and has a non-empty body.
/// `!` alone, whitespace-only, or a non-leading `!` returns None.
pub fn parse_bash_command(s: &str) -> Option<&str> {
    let rest = s.strip_prefix('!')?;
    let cmd = rest.trim();
    if cmd.is_empty() {
        None
    } else {
        Some(cmd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewind_is_a_registered_command_with_i18n_desc() {
        assert!(
            BUILTIN_COMMANDS.iter().any(|c| c.name == "rewind" && !c.needs_args),
            "/rewind must be a registered, no-arg builtin command"
        );
        assert!(
            cmd_desc_i18n("rewind").is_some(),
            "/rewind must map to an i18n description"
        );
    }

    #[test]
    fn bash_prefix_extracts_command() {
        assert_eq!(parse_bash_command("!ls"), Some("ls"));
        assert_eq!(parse_bash_command("!  echo hi"), Some("echo hi"));
        assert_eq!(parse_bash_command("!git status"), Some("git status"));
    }

    #[test]
    fn bare_bang_is_none() {
        assert_eq!(parse_bash_command("!"), None);
        assert_eq!(parse_bash_command("!   "), None);
    }

    #[test]
    fn leading_space_not_bash() {
        assert_eq!(parse_bash_command(" !ls"), None);
        assert_eq!(parse_bash_command("echo !x"), None);
    }

    #[test]
    fn registry_lookup_by_name() {
        let reg = CommandRegistry::builtin();
        assert!(reg.find("quit").is_some());
        assert!(reg.find("nonexistent").is_none());
    }

    #[test]
    fn canonical_name_resolves_aliases_case_insensitively() {
        assert_eq!(canonical_command_name("new"), "session");
        assert_eq!(canonical_command_name("NEW"), "session");
        // Non-alias names pass through unchanged.
        assert_eq!(canonical_command_name("session"), "session");
        assert_eq!(canonical_command_name("model"), "model");
        assert_eq!(canonical_command_name("nonexistent"), "nonexistent");
    }

    #[test]
    fn display_name_annotates_aliased_commands_only() {
        assert_eq!(command_display_name("session"), "session (new)");
        // Commands without aliases render as their bare name.
        assert_eq!(command_display_name("model"), "model");
        assert_eq!(command_display_name("resume"), "resume");
    }

    #[test]
    fn find_resolves_alias_to_canonical_command() {
        let reg = CommandRegistry::builtin();
        let via_alias = reg.find("new").expect("/new must resolve to /session");
        assert_eq!(via_alias.name, "session");
        // The alias inherits the canonical command's args behavior.
        assert_eq!(
            via_alias.needs_args,
            reg.find("session").unwrap().needs_args
        );
    }

    #[test]
    fn matching_prefix_finds_command_by_alias_prefix_as_single_row() {
        let reg = CommandRegistry::builtin();
        // Typing the alias prefix surfaces the canonical command...
        for prefix in ["n", "ne", "new"] {
            let hits = reg.matching_prefix(prefix);
            let sessions: Vec<_> = hits.iter().filter(|c| c.name == "session").collect();
            assert_eq!(
                sessions.len(),
                1,
                "prefix {prefix:?} must surface `session` exactly once (no duplicate row)"
            );
        }
        // ...and typing the canonical prefix still works.
        assert!(reg
            .matching_prefix("sess")
            .iter()
            .any(|c| c.name == "session"));
        // A prefix matching neither name nor alias yields nothing for session.
        assert!(!reg
            .matching_prefix("zzz")
            .iter()
            .any(|c| c.name == "session"));
    }

    #[test]
    fn builtin_contains_bg_command() {
        let registry = CommandRegistry::builtin();
        let cmd = registry.find("bg").unwrap();
        assert_eq!(cmd.name, "bg");
        assert!(!cmd.needs_args);
    }

    #[test]
    fn tab_completion_finds_prefix_matches() {
        let reg = CommandRegistry::builtin();
        let matches = reg.matching_prefix("h");
        assert!(matches.iter().any(|c| c.name == "help"));
    }

    #[test]
    fn goal_needs_args_so_selection_waits_for_input() {
        // Selecting `/goal` from the palette must only complete to `/goal ` and
        // wait for the user to type the goal — not execute (which would just
        // print status). The needs_args flag drives that menu behaviour.
        let reg = CommandRegistry::builtin();
        let goal = reg.find("goal").expect("/goal must be a built-in command");
        assert!(
            goal.needs_args,
            "/goal selection must wait for the goal text"
        );
    }

    #[test]
    fn keys_command_is_registered_with_i18n_description_in_both_locales() {
        let _locale = crate::i18n::test_lock();
        // `/keys` should appear in the built-in completion list and
        // resolve a non-empty description in every shipped locale —
        // if a translator misses one, the slash menu shows the bare
        // English fallback (CmdDescKeys default) and we want that to
        // be a test failure, not a UI regression.
        use crate::i18n::{Locale, Msg};
        let reg = CommandRegistry::builtin();
        let keys_cmd = reg
            .matching_prefix("keys")
            .into_iter()
            .find(|c| c.name == "keys")
            .expect("/keys must be a built-in command");
        assert!(!keys_cmd.needs_args);

        // i18n round-trip per locale: both the slash-menu description
        // and the KeybindingsHelp body must produce non-empty text
        // and carry the canonical keystroke labels. Snapshot the
        // current locale up front and restore at the end so we don't
        // poison parallel tests / future tests by leaving a side
        // effect behind. `set_locale` is process-global.
        let prev = crate::i18n::current_locale();
        for locale in [Locale::En, Locale::ZhCn] {
            crate::i18n::set_locale(locale);
            let desc = cmd_desc_i18n("keys").expect("CmdDescKeys translation");
            assert!(
                !desc.trim().is_empty(),
                "CmdDescKeys ({locale:?}) must not be empty"
            );
            let body = crate::i18n::t(Msg::KeybindingsHelp);
            assert!(
                body.contains("Ctrl+C"),
                "KeybindingsHelp ({locale:?}) must list Ctrl+C — got:\n{body}"
            );
            assert!(
                body.contains("Enter"),
                "KeybindingsHelp ({locale:?}) must list Enter — got:\n{body}"
            );
        }
        crate::i18n::set_locale(prev);
    }

    #[test]
    fn tab_completion_empty_for_unknown() {
        let reg = CommandRegistry::builtin();
        let matches = reg.matching_prefix("zzzzz");
        assert!(matches.is_empty());
    }

    #[test]
    fn every_builtin_command_has_an_i18n_description_in_both_locales() {
        let _locale = crate::i18n::test_lock();
        // A built-in without a cmd_desc_i18n arm silently falls back to the
        // English static `desc` even under zh_CN — the /app regression, which
        // also affected /sync, /review, /goal. Guard the WHOLE table so a
        // newly-added command can't ship without a translation in any locale.
        use crate::i18n::{current_locale, set_locale, Locale};
        let prev = current_locale();
        for locale in [Locale::En, Locale::ZhCn] {
            set_locale(locale);
            for c in CommandRegistry::builtin().all() {
                let desc = cmd_desc_i18n(c.name);
                assert!(
                    desc.as_ref().map(|d| !d.trim().is_empty()).unwrap_or(false),
                    "command /{} has no i18n description ({:?})",
                    c.name,
                    locale
                );
            }
        }
        set_locale(prev);
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
            if c.hidden {
                // Hidden commands are intentionally excluded from /help output.
                assert!(
                    !help.contains(&format!("/{} ", c.name)),
                    "hidden command /{} must not appear in help",
                    c.name
                );
            } else {
                assert!(help.contains(c.name), "help missing {}", c.name);
            }
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
        // Use a name with NO built-in collision ("review" is now a built-in, which would
        // shadow a same-named custom command — see `builtin_takes_precedence`).
        let custom = vec![("deploy".to_string(), "Deploy app".to_string())];
        let candidates = complete_commands("dep", &custom);
        assert!(
            candidates.iter().any(|c| c.name == "deploy" && c.is_custom),
            "\"dep\" should match custom \"deploy\""
        );
    }

    #[test]
    fn review_is_a_builtin_command() {
        let candidates = complete_commands("rev", &[]);
        assert!(
            candidates
                .iter()
                .any(|c| c.name == "review" && !c.is_custom),
            "/review must be a built-in command"
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
