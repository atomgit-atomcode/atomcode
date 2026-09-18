//! Slash commands: contributed by rows, discovered from the registry.
//!
//! A command is not a TUI feature. `/model` belongs to whoever owns the model
//! row, `/compact` to whoever owns compaction — the UI only knows how to
//! *discover* them, *show* them and *dispatch* them. That split is the same
//! three-role convention the seams use, applied to the command surface, and it
//! is why adding a capability adds its command without touching this file.

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
    /// Shown after the name when it takes something, e.g. `<id>`.
    pub takes: Option<Cow<'static, str>>,
}

impl Command {
    pub const fn new(name: &'static str, about: &'static str) -> Self {
        Self {
            name: Cow::Borrowed(name),
            about: Cow::Borrowed(about),
            takes: None,
        }
    }
    pub const fn taking(name: &'static str, takes: &'static str, about: &'static str) -> Self {
        Self {
            name: Cow::Borrowed(name),
            about: Cow::Borrowed(about),
            takes: Some(Cow::Borrowed(takes)),
        }
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

/// Every command mounted, with conflicts refused at mount time.
#[derive(Default)]
pub struct Commands {
    sets: RwLock<Vec<Arc<dyn CommandSet>>>,
}

impl Commands {
    pub fn new() -> Self {
        Self::default()
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

    /// Matches for what has been typed after the slash.
    pub fn matching(&self, prefix: &str) -> Vec<Command> {
        let p = prefix.to_lowercase();
        self.all()
            .into_iter()
            .filter(|c| c.name.starts_with(&p))
            .collect()
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
                    .any(|c| c.name == name)
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
        match self.owner(name) {
            Some(set) => set.run(name, args, ctx).await,
            None => {
                let near = self.matching(name);
                if near.is_empty() {
                    Outcome::Refused(format!("没有 /{name} 这条命令,输入 /help 看有哪些"))
                } else {
                    Outcome::Refused(format!(
                        "没有 /{name};你是指 {}?",
                        near.iter()
                            .map(|c| format!("/{}", c.name))
                            .collect::<Vec<_>>()
                            .join(" ")
                    ))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn registry() -> Commands {
        let c = Commands::new();
        c.add(Arc::new(Fake("row-a", A))).unwrap();
        c.add(Arc::new(Fake("row-b", B))).unwrap();
        c
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
