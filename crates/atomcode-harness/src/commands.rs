//! The command catalog (`docs/adr/0021` §10): what a person can run from a
//! front end against a session, or one agent in it.
//!
//! A row that has such a command registers it here when it mounts, and it is
//! gone when the row unloads — the same two halves a tool mount is. A front end
//! learns what is on offer from an agent's description and runs one with
//! `Invoke`. These are not tools: the model never sees them, and nothing but a
//! person's command runs them.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use atomcode_kernel::agent::CommandDescription;
use atomcode_plexus::Context;

use crate::agent::Agent;

/// One command a row puts in the catalog.
#[async_trait]
pub trait CatalogCommand: Send + Sync {
    fn describe(&self) -> CommandDescription;

    /// Whether it is on offer for `agent`: a command about team members is not
    /// offered for the lead.
    fn offered_for(&self, _agent: &Agent) -> bool {
        true
    }

    /// Run it against `agent`. What comes back is for a person to read; an
    /// `Err` says why nothing was done.
    async fn run(&self, agent: Arc<Agent>, args: &str) -> Result<String, String>;

    /// Whether this command was generated from what a person put on disk (a
    /// skill's SKILL.md), rather than named by a built-in row. Such a command
    /// **yields its name to a built-in** of the same name — a skill someone
    /// dropped in a directory must never take a built-in's name, and must never
    /// bring the whole tree down for it (`register`). Default `false`: a row
    /// naming its own command owns that name.
    fn is_skill(&self) -> bool {
        false
    }
}

/// Every command registered, by name. Two **built-in** rows claiming one name
/// is refused at mount: a person would get whichever registered first and
/// nothing would say so. A **skill** from disk (`is_skill`) never wins a name a
/// built-in holds — it yields, so no file someone dropped in a directory can
/// take a built-in's command or break the mount over it, whatever order the two
/// happen to register in.
#[derive(Default)]
pub struct CommandCatalog {
    commands: RwLock<Vec<Arc<dyn CatalogCommand>>>,
}

impl CommandCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, command: Arc<dyn CatalogCommand>) -> Result<(), String> {
        let name = command.describe().name;
        let mut commands = self.commands.write().expect("commands poisoned");
        if let Some(pos) = commands.iter().position(|c| c.describe().name == name) {
            let existing_is_skill = commands[pos].is_skill();
            // A built-in takes the name from a skill that got there first — the
            // skill is still reachable through `use_skill`, it just is not this
            // slash command. This is what keeps a skill named `memory` (or
            // `compact`, `todo`, …) from failing the whole assembly, no matter
            // whether the skill row or the built-in row applied first.
            if existing_is_skill && !command.is_skill() {
                commands[pos] = command;
                return Ok(());
            }
            // A skill yields to whatever holds the name (built-in or another
            // skill): keep what is there, drop this one, never error.
            if command.is_skill() {
                return Ok(());
            }
            // Two built-in rows on one name is a real bug, refused as before.
            return Err(format!(
                "two rows register the command `{name}`; disable one"
            ));
        }
        commands.push(command);
        Ok(())
    }

    /// Remove every command of `name`, by name.
    ///
    /// **Not for a row's unmount cleanup** — use [`remove_exact`] there. Removing
    /// by name would take a built-in that had evicted a same-named skill along
    /// with the skill row it belonged to (see [`register`]). By-name removal is
    /// for a caller that means "drop whatever holds this name", not "drop the one
    /// thing I registered".
    ///
    /// [`remove_exact`]: Self::remove_exact
    /// [`register`]: Self::register
    pub fn unregister(&self, name: &str) {
        self.commands
            .write()
            .expect("commands poisoned")
            .retain(|c| c.describe().name != name);
    }

    /// Remove exactly the command that was registered, by identity — not by
    /// name. The removal a row files on mount uses this so that a skill whose
    /// name a built-in later took (see [`register`]) does not, on the skill
    /// row's unload, take the built-in's command with it: `unregister` is by
    /// name and would.
    ///
    /// [`register`]: Self::register
    pub fn remove_exact(&self, command: &Arc<dyn CatalogCommand>) {
        self.commands
            .write()
            .expect("commands poisoned")
            .retain(|c| !Arc::ptr_eq(c, command));
    }

    /// Whether a command of this name is registered — for a row that generates
    /// commands from what it finds on disk and must not take a name that is
    /// already somebody's.
    pub fn has(&self, name: &str) -> bool {
        self.commands
            .read()
            .expect("commands poisoned")
            .iter()
            .any(|c| c.describe().name == name)
    }

    /// What is on offer for `agent`, by name.
    pub fn offered_for(&self, agent: &Agent) -> Vec<CommandDescription> {
        let mut offered: Vec<CommandDescription> = self
            .commands
            .read()
            .expect("commands poisoned")
            .iter()
            .filter(|c| c.offered_for(agent))
            .map(|c| c.describe())
            .collect();
        offered.sort_by(|a, b| a.name.cmp(&b.name));
        offered
    }

    /// The command by `name`, when it is on offer for `agent`.
    pub fn find(&self, name: &str, agent: &Agent) -> Option<Arc<dyn CatalogCommand>> {
        self.commands
            .read()
            .expect("commands poisoned")
            .iter()
            .find(|c| c.describe().name == name && c.offered_for(agent))
            .cloned()
    }
}

/// Register `command` and file its removal with the row mounting it.
///
/// The removal is by identity ([`CommandCatalog::remove_exact`]), not by name:
/// a skill whose name a built-in later took must not, when its row unloads,
/// remove the built-in's command in its place.
pub fn register(ctx: &Context, command: Arc<dyn CatalogCommand>) -> Result<(), String> {
    let catalog = ctx
        .require::<crate::seams::CommandsSvc>()
        .map_err(|e| e.to_string())?;
    catalog.register(command.clone())?;
    let _ = ctx.effect(move || catalog.remove_exact(&command));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::agent::CommandTarget;

    struct Named(&'static str);

    #[async_trait]
    impl CatalogCommand for Named {
        fn describe(&self) -> CommandDescription {
            CommandDescription {
                name: self.0.into(),
                usage: None,
                summary: String::new(),
                target: CommandTarget::Session,
            }
        }
        async fn run(&self, _agent: Arc<Agent>, _args: &str) -> Result<String, String> {
            Ok(String::new())
        }
    }

    /// A command generated from a skill on disk — yields its name to a built-in.
    struct Skilled(&'static str);

    #[async_trait]
    impl CatalogCommand for Skilled {
        fn describe(&self) -> CommandDescription {
            CommandDescription {
                name: self.0.into(),
                usage: None,
                summary: String::new(),
                target: CommandTarget::Session,
            }
        }
        async fn run(&self, _agent: Arc<Agent>, _args: &str) -> Result<String, String> {
            Ok(String::new())
        }
        fn is_skill(&self) -> bool {
            true
        }
    }

    impl CommandCatalog {
        /// `Some(is_skill)` for the command holding `name`, for tests to see
        /// which of a built-in / skill won the name.
        fn kind_of(&self, name: &str) -> Option<bool> {
            self.commands
                .read()
                .unwrap()
                .iter()
                .find(|c| c.describe().name == name)
                .map(|c| c.is_skill())
        }
    }

    /// Two rows claiming one name is refused, not settled by mount order.
    #[test]
    fn a_name_is_registered_once() {
        let catalog = CommandCatalog::new();
        catalog.register(Arc::new(Named("stop"))).unwrap();
        let second = catalog.register(Arc::new(Named("stop")));
        assert!(
            second.as_ref().is_err_and(|e| e.contains("`stop`")),
            "{second:?}"
        );
        catalog.register(Arc::new(Named("halt"))).unwrap();
        catalog.unregister("stop");
        catalog.register(Arc::new(Named("stop"))).unwrap();
    }

    /// A skill named like a built-in yields to it — either registration order —
    /// so a skill someone installed can never take a built-in command or break
    /// the mount. This is the `/memory`-vs-`memory`-skill failure, generalised.
    #[test]
    fn a_skill_yields_its_name_to_a_builtin_either_order() {
        // Skill first (the real failure: skills register before the memory row).
        let catalog = CommandCatalog::new();
        catalog.register(Arc::new(Skilled("memory"))).unwrap();
        catalog
            .register(Arc::new(Named("memory")))
            .expect("the built-in takes the name from the skill, not an error");
        assert_eq!(
            catalog.kind_of("memory"),
            Some(false),
            "the built-in holds it"
        );

        // Built-in first: the skill yields, no error, the built-in stays.
        let catalog = CommandCatalog::new();
        catalog.register(Arc::new(Named("memory"))).unwrap();
        catalog
            .register(Arc::new(Skilled("memory")))
            .expect("the skill yields rather than erroring");
        assert_eq!(
            catalog.kind_of("memory"),
            Some(false),
            "the built-in still holds it"
        );
    }

    /// Two skills that resolve to one name: the first keeps it, the second
    /// yields — never an error, so two installed skills sharing a bare name do
    /// not break the mount either.
    #[test]
    fn two_skills_of_one_name_keep_the_first() {
        let catalog = CommandCatalog::new();
        catalog.register(Arc::new(Skilled("init"))).unwrap();
        catalog
            .register(Arc::new(Skilled("init")))
            .expect("the second skill yields, not an error");
        assert_eq!(
            catalog.kind_of("init"),
            Some(true),
            "a skill still holds it"
        );
        assert_eq!(
            catalog.commands.read().unwrap().len(),
            1,
            "only the first skill is kept"
        );
    }

    /// Removing the skill row must not take the built-in that replaced it: the
    /// row files its removal by identity, so a no-longer-registered skill's
    /// unload is a no-op on the built-in now holding the name.
    #[test]
    fn evicting_a_skill_then_unloading_it_leaves_the_builtin() {
        let catalog = CommandCatalog::new();
        let skill: Arc<dyn CatalogCommand> = Arc::new(Skilled("memory"));
        catalog.register(skill.clone()).unwrap();
        catalog.register(Arc::new(Named("memory"))).unwrap(); // built-in evicts the skill
                                                              // The skill's row unloads — its removal is by identity (`remove_exact`).
        catalog.remove_exact(&skill);
        assert_eq!(
            catalog.kind_of("memory"),
            Some(false),
            "the built-in survives the skill row's unload"
        );
    }
}
