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
}

/// Every command registered, by name. Two rows claiming one name is refused
/// at mount: a person would get whichever registered first and nothing would
/// say so.
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
        if commands.iter().any(|c| c.describe().name == name) {
            return Err(format!(
                "two rows register the command `{name}`; disable one"
            ));
        }
        commands.push(command);
        Ok(())
    }

    pub fn unregister(&self, name: &str) {
        self.commands
            .write()
            .expect("commands poisoned")
            .retain(|c| c.describe().name != name);
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
pub fn register(ctx: &Context, command: Arc<dyn CatalogCommand>) -> Result<(), String> {
    let catalog = ctx
        .require::<crate::seams::CommandsSvc>()
        .map_err(|e| e.to_string())?;
    let name = command.describe().name;
    catalog.register(command)?;
    let _ = ctx.effect(move || catalog.unregister(&name));
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
}
