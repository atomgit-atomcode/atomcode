//! Counting the commands a person runs on the new screen.
//!
//! Here and not in `atomcode-tui` for the reason `tui_login` and
//! `tui_onboarding` are here: the screen may know neither the host nor what it
//! reports to (`gates/layers.sh`, "UI 不认 Host,也不认 Product"). The screen
//! states that a command ran; this decides that the fact is worth counting and
//! owns the sink it is counted into.
//!
//! # What it is replacing
//!
//! `atomcode-tuix` has reported one `use_command` per dispatch since the event
//! existed (`src/event_loop/commands.rs:1615`, and `:4127` for a name nothing
//! answers to). The row-assembled screen reported none — it has no telemetry
//! dependency at all — so every command run on it was invisible. The moment
//! that screen becomes the default, a dashboard that has counted commands for
//! two years would simply stop.
//!
//! # Parity, deliberately
//!
//! `success` is `true` for anything that reached a command set, even if the
//! command then refused: tuix reports before it dispatches, precisely so a
//! command that errors still counts as run. A name nothing answers to is the
//! one failure shape, `NotFound`, with the same `error_data` keys tuix sends.
//! The richer thing — reporting whether the command itself succeeded — is
//! available here (`Outcome` says), and is deliberately not taken: it would
//! change what `success = false` has meant in every stored row.

use std::sync::Arc;

use async_trait::async_trait;
use atomcode_plexus::{Context, Plugin};
use serde_json::Value;

/// The row's name.
pub const ROW: &str = "tui-command-meter";

pub fn row_layer() -> String {
    format!("[[insert]]\nname = \"{ROW}\"\n")
}

/// Installs the observer. Mounted only when the launcher has a sink.
pub struct CommandMeterRow {
    pub telemetry: Arc<atomcode_telemetry::Telemetry>,
}

#[async_trait]
impl Plugin for CommandMeterRow {
    fn name(&self) -> &'static str {
        ROW
    }
    fn inject(&self) -> &'static [&'static str] {
        &["tui-commands"]
    }
    fn description(&self) -> &'static str {
        "counts each command a person runs into the host's telemetry"
    }
    async fn apply(&self, ctx: &Context, _config: &Value) -> Result<(), String> {
        let commands = ctx
            .require::<atomcode_tui::plugin::CommandsSvc>()
            .map_err(|e| e.to_string())?;
        commands.observe(Arc::new(UseCommandMeter {
            telemetry: self.telemetry.clone(),
        }));
        Ok(())
    }
}

struct UseCommandMeter {
    telemetry: Arc<atomcode_telemetry::Telemetry>,
}

impl atomcode_tui::command::CommandObserver for UseCommandMeter {
    fn ran(&self, run: &atomcode_tui::command::CommandRun<'_>) {
        self.telemetry.track(use_command(run.name, run.found));
    }
}

/// The record a run becomes.
///
/// A free function so the mapping is testable without a mounted screen: what
/// is worth pinning is the shape, and the shape is all of this.
pub(crate) fn use_command(name: &str, found: bool) -> atomcode_telemetry::Event {
    match found {
        true => atomcode_telemetry::Event::UseCommand {
            type_: name.to_string(),
            success: Some(true),
            error_kind: None,
            error_data: None,
        },
        false => atomcode_telemetry::Event::UseCommand {
            type_: name.to_string(),
            success: Some(false),
            error_kind: Some(atomcode_telemetry::UseCommandErrorKind::NotFound),
            error_data: Some(
                serde_json::json!({
                    "command": name,
                    "duration_ms": 0,
                    "message": format!("Unknown command: {name}"),
                })
                .to_string(),
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The name a command is counted under, and the shape of the record.
    ///
    /// Pinned against `crates/atomcode-telemetry/tests/golden/wire/` — the
    /// same two shapes, `use_command` and `use_command_not_found`.
    #[test]
    fn a_command_that_ran_is_counted_as_a_success() {
        let atomcode_telemetry::Event::UseCommand {
            type_,
            success,
            error_kind,
            error_data,
        } = use_command("session", true)
        else {
            panic!("not a use_command");
        };
        assert_eq!(type_, "session");
        assert_eq!(success, Some(true));
        assert!(error_kind.is_none());
        assert!(error_data.is_none());
    }

    /// The whole path: a person types a line, and a record lands in the sink.
    ///
    /// The two halves are tested apart above and in
    /// `atomcode-tui/src/command.rs`; this is the one that fails if they are
    /// never joined — the mistake that left the row-assembled screen counting
    /// nothing while every piece of the machinery existed.
    #[tokio::test]
    async fn a_command_typed_on_the_screen_lands_in_the_sink() {
        use atomcode_tui::command::{Command, CommandSet, Commands, Outcome};

        struct Anything;
        #[async_trait]
        impl CommandSet for Anything {
            fn id(&self) -> &'static str {
                "test"
            }
            fn commands(&self) -> Vec<Command> {
                vec![Command::new("session", "fresh start").with_aliases(&["new"])]
            }
            async fn run(&self, _n: &str, _a: &str, _c: &Context) -> Outcome {
                Outcome::Quiet
            }
        }

        let (telemetry, captured) = atomcode_telemetry::Telemetry::in_memory("test".into());
        let commands = Commands::new();
        commands.add(Arc::new(Anything)).unwrap();
        commands.observe(Arc::new(UseCommandMeter { telemetry }));
        let app = atomcode_plexus::App::new(
            atomcode_plexus::PluginRegistry::new(),
            atomcode_plexus::ConfigTree::default(),
        );
        let ctx = app.context();

        commands.dispatch("/new", &ctx).await;
        commands.dispatch("/nope", &ctx).await;

        let mut counted = Vec::new();
        for _ in 0..200 {
            counted = captured
                .lock()
                .await
                .iter()
                .filter_map(|record| match &record.event {
                    atomcode_telemetry::Event::UseCommand { type_, success, .. } => {
                        Some((type_.clone(), *success))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            if counted.len() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            counted,
            vec![
                // The alias counted under the command it names.
                ("session".to_string(), Some(true)),
                ("nope".to_string(), Some(false)),
            ]
        );
    }

    /// A name nothing answers to is the one failure shape, with the detail
    /// blob tuix sends — a dashboard reads `command` and `message` out of it.
    #[test]
    fn a_name_nothing_answers_to_is_counted_as_not_found() {
        let atomcode_telemetry::Event::UseCommand {
            type_,
            success,
            error_kind,
            error_data,
        } = use_command("nope", false)
        else {
            panic!("not a use_command");
        };
        assert_eq!(type_, "nope");
        assert_eq!(success, Some(false));
        assert!(matches!(
            error_kind,
            Some(atomcode_telemetry::UseCommandErrorKind::NotFound)
        ));
        let detail: serde_json::Value =
            serde_json::from_str(&error_data.expect("a miss says what was typed")).unwrap();
        assert_eq!(detail["command"], "nope");
        assert_eq!(detail["duration_ms"], 0);
        assert_eq!(detail["message"], "Unknown command: nope");
    }
}
