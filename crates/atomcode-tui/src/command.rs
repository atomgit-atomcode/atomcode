//! Slash commands: contributed by rows, discovered from the registry.
//!
//! A command is not a TUI feature. `/model` belongs to whoever owns the model
//! row, `/compact` to whoever owns compaction — the UI only knows how to
//! *discover* them, *show* them and *dispatch* them. That split is the same
//! three-role convention the seams use, applied to the command surface, and it
//! is why adding a capability adds its command without touching this file.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use atomcode_plexus::Context;

use crate::keymap::Action;

/// What a command looks like in the menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Command {
    pub name: &'static str,
    pub about: &'static str,
    /// Shown after the name when it takes something, e.g. `<id>`.
    pub takes: Option<&'static str>,
}

impl Command {
    pub const fn new(name: &'static str, about: &'static str) -> Self {
        Self {
            name,
            about,
            takes: None,
        }
    }
    pub const fn taking(name: &'static str, takes: &'static str, about: &'static str) -> Self {
        Self {
            name,
            about,
            takes: Some(takes),
        }
    }
}

/// What running one produced.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// Say this on screen.
    Said(String),
    /// Do this instead — so a command and a key can share one implementation.
    Do(Action),
    /// Nothing to report.
    Quiet,
    /// It could not run, and this is why. Never a panic, never a silent no-op.
    Refused(String),
}

/// A row's contribution to the command surface.
#[async_trait]
pub trait CommandSet: Send + Sync {
    fn id(&self) -> &'static str;
    fn commands(&self) -> Vec<Command>;
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
        for c in set.commands() {
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
        sets.push(set);
        Ok(())
    }

    pub fn remove(&self, id: &str) {
        self.sets
            .write()
            .expect("commands poisoned")
            .retain(|s| s.id() != id);
    }

    /// Everything available, sorted, for the menu and for the model's view of
    /// what it can ask for.
    pub fn all(&self) -> Vec<Command> {
        let mut out: Vec<Command> = self
            .sets
            .read()
            .expect("commands poisoned")
            .iter()
            .flat_map(|s| s.commands())
            .collect();
        out.sort_by_key(|c| c.name);
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
            .find(|s| s.commands().iter().any(|c| c.name == name))
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

    #[test]
    fn the_menu_is_sorted_and_prefix_filtered() {
        let c = registry();
        assert_eq!(
            c.all().iter().map(|x| x.name).collect::<Vec<_>>(),
            vec!["alpha", "also", "beta"]
        );
        assert_eq!(
            c.matching("al").iter().map(|x| x.name).collect::<Vec<_>>(),
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
