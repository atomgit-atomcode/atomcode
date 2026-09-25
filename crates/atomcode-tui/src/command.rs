//! Slash commands: contributed by rows, discovered from the registry.
//!
//! A command is not a TUI feature. `/model` belongs to whoever owns the model
//! row, `/compact` to whoever owns compaction — the UI only knows how to
//! *discover* them, *show* them and *dispatch* them. That split is the same
//! three-role convention the seams use, applied to the command surface, and it
//! is why adding a capability adds its command without touching this file.

use crate::i18n::{t, Msg};
use std::borrow::Cow;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use atomcode_plexus::Context;

use crate::keymap::Action;

/// What a command looks like in the menu.
///
/// Owned or borrowed: the screen's own commands are written into the build,
/// the agent's arrive in its description.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub name: Cow<'static, str>,
    pub about: Cow<'static, str>,
    /// What it takes, e.g. `<id>`. Drawn at the **end** of its row, after the
    /// gloss (`crate::menu::Item::hint`), rather than after the name: an argument
    /// list has no length anyone controls, and in the name column a long one
    /// moved every gloss in the table out of column.
    pub takes: Option<Cow<'static, str>>,
    /// Other names that reach this same command — muscle memory that must keep
    /// working (`/exit` for `/quit`, `/new` for `/session`). An alias is not a
    /// second row: it shares this one entry, is searchable by its own prefix in
    /// the slash menu, resolves to `name` on dispatch, and renders as
    /// `name (alias)`. Empty for the agent's own commands, which have none.
    pub aliases: &'static [&'static str],
    /// A closed set of values this command takes, offered inline in the slash
    /// menu in place of a modal. When the command is fully named the menu
    /// expands it into one row per option, and picking a row dispatches
    /// `{name} {value}`. Empty for a command that takes free text or nothing.
    pub options: Vec<CommandOption>,
    /// The free-text argument is required and has no useful bare form, so taking
    /// the row completes the name onto the line (`/rename `) for the argument to
    /// be typed rather than dispatching it — which would only answer "needs a
    /// name". Distinct from [`takes`](Self::takes) being set: `/model` and
    /// `/resume` take an argument too, but running them bare opens a picker, so
    /// they still dispatch. Only for commands with no closed [`options`] and no
    /// bare form worth reaching.
    pub require_arg: bool,
}

/// One value a command offers to pick inline in the slash menu.
///
/// A command with a closed argument set — `/effort`'s levels — lists them here
/// rather than answering with a modal. The menu expands the command into a row
/// per option once it is fully named; a pick is dispatched as `{name} {value}`,
/// so the menu and a typed `/effort high` reach one implementation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOption {
    /// The argument this row stands for, dispatched after the command name.
    pub value: Cow<'static, str>,
    /// The dim gloss shown after it, in the column a command's `about` is in.
    pub about: Cow<'static, str>,
}

impl CommandOption {
    pub fn new(value: impl Into<Cow<'static, str>>, about: impl Into<Cow<'static, str>>) -> Self {
        Self {
            value: value.into(),
            about: about.into(),
        }
    }

    /// The row this value takes once its command has been named out — one row per
    /// value, which is how the menu offers a closed set instead of a modal.
    ///
    /// `active` is the value in force, marked with `mark`: a person reading the
    /// menu is asking which one is on, and the answer belongs on the row rather
    /// than in a sentence after it. The value is `{command} {value}` because that
    /// is what a pick dispatches.
    pub fn menu_row(&self, command: &str, active: bool, mark: &str) -> crate::menu::Item {
        let shown = format!("{command} {}", self.value);
        let label = if active {
            format!("{shown} {mark}")
        } else {
            shown.clone()
        };
        crate::menu::Item::new(shown, label).about(self.about.clone())
    }
}

impl Command {
    pub const fn new(name: &'static str, about: &'static str) -> Self {
        Self {
            name: Cow::Borrowed(name),
            about: Cow::Borrowed(about),
            takes: None,
            aliases: &[],
            options: Vec::new(),
            require_arg: false,
        }
    }
    pub const fn taking(name: &'static str, takes: &'static str, about: &'static str) -> Self {
        Self {
            name: Cow::Borrowed(name),
            about: Cow::Borrowed(about),
            takes: Some(Cow::Borrowed(takes)),
            aliases: &[],
            options: Vec::new(),
            require_arg: false,
        }
    }
    /// The same, described by the language table.
    ///
    /// [`new`](Self::new) and [`taking`](Self::taking) stay `const` because a
    /// catalogue that says the same thing in every language can be a `const`
    /// array. One that reads its words from the table cannot: what it says
    /// depends on the language in force when it is asked for, and `/language`
    /// changes that mid-session. So a described catalogue is a function, and
    /// this is what its entries are built with.
    pub fn said(name: &'static str, about: Cow<'static, str>) -> Self {
        Self {
            name: Cow::Borrowed(name),
            about,
            takes: None,
            aliases: &[],
            options: Vec::new(),
            require_arg: false,
        }
    }
    /// [`said`](Self::said) for a command that takes something.
    pub fn said_taking(
        name: &'static str,
        takes: Cow<'static, str>,
        about: Cow<'static, str>,
    ) -> Self {
        Self {
            name: Cow::Borrowed(name),
            about,
            takes: Some(takes),
            aliases: &[],
            options: Vec::new(),
            require_arg: false,
        }
    }

    /// The same command, reachable by these extra names.
    pub const fn with_aliases(mut self, aliases: &'static [&'static str]) -> Self {
        self.aliases = aliases;
        self
    }

    /// The same command, but taking its row completes it onto the line for the
    /// argument to be typed rather than dispatching it bare. See
    /// [`require_arg`](Self::require_arg).
    pub fn requiring(mut self) -> Self {
        self.require_arg = true;
        self
    }

    /// The same command, offering a closed set of values inline in the slash
    /// menu rather than a modal. See [`CommandOption`].
    pub fn selecting(mut self, options: Vec<CommandOption>) -> Self {
        self.options = options;
        self
    }

    /// True when `typed_lower` (already lowercased by the caller) is a prefix of
    /// this command's name or of any of its aliases — what the slash menu filters
    /// on. Case-insensitive, like [`answers_to`](Self::answers_to): an agent
    /// command named `MySkill` still surfaces for `/my`.
    pub fn matches_prefix(&self, typed_lower: &str) -> bool {
        let has_prefix = |s: &str| s.to_lowercase().starts_with(typed_lower);
        has_prefix(&self.name) || self.aliases.iter().any(|a| has_prefix(a))
    }

    /// True when `typed` is this command's name or one of its aliases (exact,
    /// ASCII case-insensitive) — what dispatch resolves on.
    pub fn answers_to(&self, typed: &str) -> bool {
        self.name.eq_ignore_ascii_case(typed)
            || self.aliases.iter().any(|a| a.eq_ignore_ascii_case(typed))
    }

    /// The slash-menu label: `name (alias1, alias2)` when it has aliases, else
    /// just the name. The inserted/dispatched value stays the canonical `name`.
    pub fn display_name(&self) -> String {
        if self.aliases.is_empty() {
            self.name.to_string()
        } else {
            format!("{} ({})", self.name, self.aliases.join(", "))
        }
    }

    /// The row this command takes in the menu: `/name (alias)`, what it does, and
    /// what it takes at the tail.
    ///
    /// The shape in one function rather than written where the menu is filled,
    /// because more than one caller has to agree about it: the screen draws the
    /// rows, and a judgement that the menu reads as a table asks this same
    /// function for the rows a person would be looking at. A second copy of the
    /// shape in a test would answer for a menu nobody sees.
    pub fn menu_row(&self) -> crate::menu::Item {
        crate::menu::Item::new(self.name.clone(), self.display_name())
            .about(self.about.clone())
            .hint(self.takes.clone().unwrap_or_default())
    }
}

/// What running one produced.
#[derive(Clone)]
pub enum Outcome {
    /// Say this on screen.
    Said(String),
    /// Do this instead — so a command and a key can share one implementation.
    Do(Action),
    /// Nothing to report.
    Quiet,
    /// It could not run, and this is why. Never a panic, never a silent no-op.
    Refused(String),
    /// Put a modal on screen. What it picks is dispatched as a command in turn,
    /// so a modal and a typed command reach the same implementation.
    Open(Arc<dyn crate::overlay::Overlay>),
}

impl std::fmt::Debug for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Said(t) => write!(f, "Said({t:?})"),
            Outcome::Do(a) => write!(f, "Do({a:?})"),
            Outcome::Quiet => write!(f, "Quiet"),
            Outcome::Refused(t) => write!(f, "Refused({t:?})"),
            Outcome::Open(o) => write!(f, "Open({})", o.id()),
        }
    }
}

impl PartialEq for Outcome {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Outcome::Said(a), Outcome::Said(b)) => a == b,
            (Outcome::Do(a), Outcome::Do(b)) => a == b,
            (Outcome::Quiet, Outcome::Quiet) => true,
            (Outcome::Refused(a), Outcome::Refused(b)) => a == b,
            (Outcome::Open(a), Outcome::Open(b)) => a.id() == b.id(),
            _ => false,
        }
    }
}

/// A row's contribution to the command surface.
#[async_trait]
pub trait CommandSet: Send + Sync {
    fn id(&self) -> &'static str;
    fn commands(&self) -> Vec<Command>;
    /// Dispatchable but not listed. For the targets a modal's pick dispatches
    /// to: they are not something anyone types, and putting them in the menu
    /// would be noise.
    fn hidden(&self) -> Vec<Command> {
        Vec::new()
    }
    /// Names this set takes over from whoever already has them.
    ///
    /// The one way a name may be claimed twice, and it has to be said out loud.
    /// Without it a downstream build that wants its own `/copy` has to drop the
    /// whole set the shipped one lives in and lose the other twenty commands
    /// with it; with it, it mounts one row that names `copy` and nothing else
    /// moves. Declared rather than settled by mount order, because the whole
    /// reason [`Commands::add`] refuses a clash is that last-write-wins leaves
    /// nothing saying which one you got.
    ///
    /// A name nobody has yet is not an error: a set may ship with the override
    /// declared and be mounted alongside a build that never had that command.
    fn overrides(&self) -> Vec<&'static str> {
        Vec::new()
    }
    /// Run one. `args` is everything after the name, untrimmed of meaning.
    async fn run(&self, name: &str, args: &str, ctx: &Context) -> Outcome;
}

/// One command a person ran, as the registry resolved it.
pub struct CommandRun<'a> {
    /// What to call it: an alias resolved to its canonical name, the slash
    /// gone, case folded. For a name nothing answers to, what was typed.
    pub name: &'a str,
    /// Whether any set owned the name. `false` means nothing ran.
    pub found: bool,
}

/// Told about every command a person ran.
///
/// A port rather than a dependency. Whether a command is counted, logged or
/// ignored is the launcher's decision, and this layer may know neither the
/// host nor what it counts with (`gates/layers.sh`, "UI 不认 Host,也不认
/// Product"). The registry states the fact; a launcher decides what it is for.
pub trait CommandObserver: Send + Sync {
    fn ran(&self, run: &CommandRun<'_>);
}

/// Every command mounted, with conflicts refused at mount time.
#[derive(Default)]
pub struct Commands {
    sets: RwLock<Vec<Arc<dyn CommandSet>>>,
    /// Filled by the launcher, if it has a use for it. Unset: dispatch is
    /// exactly as it was.
    observer: RwLock<Option<Arc<dyn CommandObserver>>>,
}

impl Commands {
    pub fn new() -> Self {
        Self::default()
    }

    /// Watch what a person runs.
    ///
    /// One observer, not a list: two of them would be two answers to "what
    /// does this front end report", and the launcher is the one place that
    /// knows. A second call replaces the first.
    pub fn observe(&self, observer: Arc<dyn CommandObserver>) {
        *self.observer.write().expect("commands poisoned") = Some(observer);
    }

    /// Two rows claiming one name is an error: the user would get whichever
    /// mounted second and nothing would say so.
    pub fn add(&self, set: Arc<dyn CommandSet>) -> Result<(), String> {
        let mut sets = self.sets.write().expect("commands poisoned");
        let taken_over = set.overrides();
        for c in set.commands() {
            if taken_over.contains(&c.name.as_ref()) {
                continue;
            }
            for existing in sets.iter() {
                if existing.commands().iter().any(|e| e.name == c.name) {
                    return Err(format!(
                        "`{}` and `{}` both define /{}; disable one",
                        existing.id(),
                        set.id(),
                        c.name
                    ));
                }
            }
        }
        // In front, so `owner` and `all` find it before the set it took the
        // name from. Order is how the override is applied; the declaration
        // above is what makes it legal.
        if taken_over.is_empty() {
            sets.push(set);
        } else {
            sets.insert(0, set);
        }
        Ok(())
    }

    pub fn remove(&self, id: &str) {
        self.sets
            .write()
            .expect("commands poisoned")
            .retain(|s| s.id() != id);
    }

    /// Everything available, sorted, for the menu and for the model's view of
    /// what it can ask for. A name two sets offer is listed once, as the set
    /// that runs it: the one mounted first.
    pub fn all(&self) -> Vec<Command> {
        let mut out: Vec<Command> = Vec::new();
        for command in self
            .sets
            .read()
            .expect("commands poisoned")
            .iter()
            .flat_map(|s| s.commands())
        {
            if !out.iter().any(|c| c.name == command.name) {
                out.push(command);
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    /// Matches for what has been typed after the slash — by command name or by
    /// any alias, so `/ne` surfaces the `session` command while it stays one row.
    pub fn matching(&self, prefix: &str) -> Vec<Command> {
        let p = prefix.to_lowercase();
        self.all()
            .into_iter()
            .filter(|c| c.matches_prefix(&p))
            .collect()
    }

    /// The command this name would run, as the registry would run it.
    ///
    /// Asked by the menu's completion, which has to know whether the name it is
    /// about to put on the line wants an argument: completing `/effort` without
    /// the space that says "something goes here" leaves the caret in the wrong
    /// place. The registry is the only thing that knows, and `all` is where the
    /// menu already reads it from.
    pub fn find(&self, name: &str) -> Option<Command> {
        let all = self.all();
        // A real command name beats an alias: an alias is only a fallback way in,
        // so a command literally named `new` wins over `session`'s `new` alias.
        all.iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
            .or_else(|| all.iter().find(|c| c.answers_to(name)))
            .cloned()
    }

    fn owner(&self, name: &str) -> Option<Arc<dyn CommandSet>> {
        self.sets
            .read()
            .expect("commands poisoned")
            .iter()
            .find(|s| {
                s.commands()
                    .iter()
                    .chain(s.hidden().iter())
                    .any(|c| c.answers_to(name))
            })
            .cloned()
    }

    /// Dispatch a whole typed line, slash and all.
    pub async fn dispatch(&self, line: &str, ctx: &Context) -> Outcome {
        let body = line.trim().trim_start_matches('/');
        let (name, args) = match body.split_once(char::is_whitespace) {
            Some((n, a)) => (n, a.trim()),
            None => (body, ""),
        };
        if name.is_empty() {
            return Outcome::Quiet;
        }
        // Resolve an alias (`/exit`, `/new`) to the canonical command it names, so
        // the set's `run` sees the one name it matches on. A name that is not an
        // alias resolves to itself.
        let canonical = self.find(name).map(|c| c.name.to_string());
        // Case folded for a name nothing answers to, so `/Nope` and `/nope`
        // are one miss and not two. A name that resolved is already canonical.
        let typed = name.to_ascii_lowercase();
        let name = canonical.as_deref().unwrap_or(&typed);
        let owner = self.owner(name);
        // Before running it, so a command that then fails is still one the
        // person ran — what `atomcode-tuix` has always reported.
        if let Some(observer) = self.observer.read().expect("commands poisoned").clone() {
            observer.ran(&CommandRun {
                name,
                found: owner.is_some(),
            });
        }
        match owner {
            Some(set) => set.run(name, args, ctx).await,
            None => {
                let near = self.matching(name);
                if near.is_empty() {
                    Outcome::Refused(t(Msg::CmdNoSuch { name }).into_owned())
                } else {
                    let near = near
                        .iter()
                        .map(|c| format!("/{}", c.name))
                        .collect::<Vec<_>>()
                        .join(" ");
                    Outcome::Refused(t(Msg::CmdNoSuchDidYouMean { name, near: &near }).into_owned())
                }
            }
        }
    }
}

/// Whether a submitted line is a slash *command* and not a filesystem path or
/// URL that merely begins with `/`.
///
/// A command is `/name` — `name` being letters, digits, `_`, `-`, or `:` (the
/// last for namespaced skills like `/skills:brainstorming`) — optionally
/// followed by whitespace and arguments. A **non-whitespace** character right
/// after the name (the next `/` of `/Users/me/x`, the `.` of `/x.png`) means the
/// leading `/` was literal: the line is a path the user is sending to the model,
/// not a command, and must NOT be dispatched (which would answer "没有 /Users/…
/// 这条命令"). Arguments that are themselves paths (`/cd /Users/me`) are fine —
/// only the first token is inspected.
#[allow(
    clippy::string_slice,
    reason = "`name_end` is a `find` result on `rest` itself, or `rest.len()`: a char boundary"
)]
pub fn looks_like_command(line: &str) -> bool {
    let Some(rest) = line.trim_start().strip_prefix('/') else {
        return false;
    };
    let name_end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == ':'))
        .unwrap_or(rest.len());
    if name_end == 0 {
        return false; // "/" alone, or "//…" — no command name
    }
    match rest[name_end..].chars().next() {
        None => true,                         // "/help"
        Some(c) if c.is_whitespace() => true, // "/cd ~/x"
        _ => false,                           // "/Users/…", "/x.png", "/a@b"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_that_begins_with_slash_is_not_a_command() {
        // The reported bug: pasting a path must reach the model untouched, not
        // dispatch to "没有 /Users/… 这条命令".
        assert!(!looks_like_command(
            "/Users/theo/Desktop/企业微信20260919-160158@2x.png"
        ));
        assert!(!looks_like_command("/tmp/x"));
        assert!(!looks_like_command("/path/with/中文/pic.png"));
        assert!(!looks_like_command("/x.png")); // a bare filename with a dot
        assert!(!looks_like_command("/")); // slash alone is not a command name
        assert!(!looks_like_command("hello")); // no leading slash
    }

    #[test]
    fn a_real_command_shape_is_a_command() {
        assert!(looks_like_command("/help"));
        assert!(looks_like_command("/cd ~/projects")); // path lives in the args
        assert!(looks_like_command("/model glm5.3"));
        assert!(looks_like_command("/skills:brainstorming")); // namespaced skill
        assert!(looks_like_command("  /clear")); // leading spaces are fine
        assert!(looks_like_command("/reset")); // unknown-but-command-shaped still dispatches (→ "did you mean")
    }

    struct Fake(&'static str, &'static [Command]);

    #[async_trait]
    impl CommandSet for Fake {
        fn id(&self) -> &'static str {
            self.0
        }
        fn commands(&self) -> Vec<Command> {
            self.1.to_vec()
        }
        async fn run(&self, name: &str, args: &str, _ctx: &Context) -> Outcome {
            Outcome::Said(format!("{name}({args})"))
        }
    }

    /// A downstream set that says which names it takes over.
    struct Takeover(&'static str, &'static [Command], &'static [&'static str]);

    #[async_trait]
    impl CommandSet for Takeover {
        fn id(&self) -> &'static str {
            self.0
        }
        fn commands(&self) -> Vec<Command> {
            self.1.to_vec()
        }
        fn overrides(&self) -> Vec<&'static str> {
            self.2.to_vec()
        }
        async fn run(&self, name: &str, args: &str, _ctx: &Context) -> Outcome {
            Outcome::Said(format!("mine:{name}({args})"))
        }
    }

    const A: &[Command] = &[Command::new("alpha", "a"), Command::new("also", "b")];
    const B: &[Command] = &[Command::new("beta", "c")];
    const CLASH: &[Command] = &[Command::new("alpha", "mine now")];
    const ALIASED: &[Command] = &[Command::new("session", "fresh start").with_aliases(&["new"])];

    fn registry() -> Commands {
        let c = Commands::new();
        c.add(Arc::new(Fake("row-a", A))).unwrap();
        c.add(Arc::new(Fake("row-b", B))).unwrap();
        c
    }

    /// An alias shares its command's single row: it is searchable by its own
    /// prefix, `find` resolves it to the canonical command, and the label names
    /// it — `session (new)` — but there is only one entry, not two.
    #[test]
    fn an_alias_is_one_annotated_row_that_resolves_to_its_command() {
        let c = Commands::new();
        c.add(Arc::new(Fake("row", ALIASED))).unwrap();
        // Searchable by the alias's own prefix AND the canonical prefix.
        assert!(c.matching("ne").iter().any(|c| c.name == "session"));
        assert!(c.matching("ses").iter().any(|c| c.name == "session"));
        // Exactly one row, however it was reached.
        assert_eq!(c.all().iter().filter(|c| c.name == "session").count(), 1);
        // The alias resolves to the canonical command, which labels itself.
        let cmd = c.find("new").expect("the alias resolves");
        assert_eq!(cmd.name, "session");
        assert_eq!(cmd.display_name(), "session (new)");
        // A command with no aliases labels itself plainly.
        assert_eq!(Command::new("alpha", "a").display_name(), "alpha");
        // Prefix matching is case-insensitive, like `answers_to`: a mixed-case
        // command name still surfaces for a lowercased prefix.
        assert!(Command::new("MySkill", "x").matches_prefix("my"));
        assert!(!Command::new("MySkill", "x").matches_prefix("zz"));
    }

    #[test]
    fn two_rows_claiming_one_name_is_caught_at_mount() {
        let c = registry();
        let err = c.add(Arc::new(Fake("rival", CLASH))).unwrap_err();
        assert!(err.contains("/alpha"), "{err}");
        assert!(err.contains("row-a") && err.contains("rival"), "{err}");
    }

    /// A downstream build can put its own `/alpha` in place of the shipped one
    /// without losing the rest of the set it lived in, and the menu shows one
    /// `/alpha` — its own. The clash test above is this one's negative control:
    /// the same collision without the declaration is still refused.
    #[tokio::test]
    async fn a_row_takes_over_one_name_and_leaves_the_rest_of_the_set_standing() {
        let c = registry();
        c.add(Arc::new(Takeover("downstream", CLASH, &["alpha"])))
            .unwrap();
        let app = atomcode_plexus::App::new(
            atomcode_plexus::PluginRegistry::new(),
            atomcode_plexus::ConfigTree::default(),
        );
        let ctx = app.context();
        assert_eq!(
            c.dispatch("/alpha x", &ctx).await,
            Outcome::Said("mine:alpha(x)".into())
        );
        // The set it took the name from still answers for its other command.
        assert_eq!(
            c.dispatch("/also", &ctx).await,
            Outcome::Said("also()".into())
        );
        let menu = c.all();
        assert_eq!(
            menu.iter().map(|x| x.name.clone()).collect::<Vec<_>>(),
            vec!["alpha", "also", "beta"]
        );
        let alpha = menu.iter().find(|x| x.name == "alpha").expect("alpha");
        assert_eq!(alpha.about, "mine now");
    }

    #[test]
    fn the_menu_is_sorted_and_prefix_filtered() {
        let c = registry();
        assert_eq!(
            c.all().iter().map(|x| x.name.clone()).collect::<Vec<_>>(),
            vec!["alpha", "also", "beta"]
        );
        assert_eq!(
            c.matching("al")
                .iter()
                .map(|x| x.name.clone())
                .collect::<Vec<_>>(),
            vec!["alpha", "also"]
        );
        assert!(c.matching("zz").is_empty());
    }

    /// Every command a person runs is told to the observer, under the name it
    /// will be counted by.
    ///
    /// The name is the contract, not a detail: `atomcode-tuix` has always
    /// reported the canonical, case-folded, slash-less name
    /// (`src/event_loop/commands.rs:1615` + `canonical_command_name`), so
    /// `/new` and `/session` are one series and `/QUIT` is not a second
    /// `quit`. A front end that reported what was typed would split every
    /// aliased command in two, with nothing anywhere saying so.
    #[tokio::test]
    async fn every_command_a_person_runs_is_told_under_its_canonical_name() {
        #[derive(Default)]
        struct Seen(std::sync::Mutex<Vec<(String, bool)>>);
        impl CommandObserver for Seen {
            fn ran(&self, run: &CommandRun<'_>) {
                self.0
                    .lock()
                    .unwrap()
                    .push((run.name.to_string(), run.found));
            }
        }

        let c = Commands::new();
        c.add(Arc::new(Fake("row", ALIASED))).unwrap();
        let seen = Arc::new(Seen::default());
        c.observe(seen.clone());
        let app = atomcode_plexus::App::new(
            atomcode_plexus::PluginRegistry::new(),
            atomcode_plexus::ConfigTree::default(),
        );
        let ctx = app.context();

        c.dispatch("/session", &ctx).await;
        c.dispatch("/new", &ctx).await; // the alias
        c.dispatch("/SESSION", &ctx).await; // shouting
        c.dispatch("/Nope", &ctx).await; // nothing answers to it

        assert_eq!(
            *seen.0.lock().unwrap(),
            vec![
                ("session".to_string(), true),
                ("session".to_string(), true),
                ("session".to_string(), true),
                // Reported as typed, folded — which is what makes the miss
                // worth reporting: it names what the person reached for.
                ("nope".to_string(), false),
            ]
        );
    }

    /// Nothing is told when nobody is listening, and dispatch is unchanged.
    ///
    /// The port is optional on purpose — a launcher with no use for it should
    /// not have to supply a no-op — so the unobserved path is the one that has
    /// to keep working.
    #[tokio::test]
    async fn an_unobserved_registry_still_dispatches() {
        let c = registry();
        let app = atomcode_plexus::App::new(
            atomcode_plexus::PluginRegistry::new(),
            atomcode_plexus::ConfigTree::default(),
        );
        let ctx = app.context();
        assert_eq!(
            c.dispatch("/alpha", &ctx).await,
            Outcome::Said("alpha()".into())
        );
    }

    #[tokio::test]
    async fn a_command_reaches_the_row_that_owns_it_with_its_argument() {
        let c = registry();
        let app = atomcode_plexus::App::new(
            atomcode_plexus::PluginRegistry::new(),
            atomcode_plexus::ConfigTree::default(),
        );
        let ctx = app.context();
        assert_eq!(
            c.dispatch("/beta  some args ", &ctx).await,
            Outcome::Said("beta(some args)".into())
        );
        assert_eq!(
            c.dispatch("/alpha", &ctx).await,
            Outcome::Said("alpha()".into())
        );
    }

    #[tokio::test]
    async fn an_unknown_command_suggests_rather_than_failing_silently() {
        let c = registry();
        let app = atomcode_plexus::App::new(
            atomcode_plexus::PluginRegistry::new(),
            atomcode_plexus::ConfigTree::default(),
        );
        let ctx = app.context();
        match c.dispatch("/al", &ctx).await {
            Outcome::Refused(m) => {
                assert!(m.contains("/alpha") && m.contains("/also"), "{m}")
            }
            other => panic!("{other:?}"),
        }
        match c.dispatch("/nope", &ctx).await {
            Outcome::Refused(m) => assert!(m.contains("/help"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unmounting_a_row_takes_its_commands_with_it() {
        let c = registry();
        c.remove("row-a");
        assert_eq!(c.all().len(), 1);
    }
}
