//! The commands a person can run against an agent, beyond talking to it
//! (`docs/adr/0021` §10).
//!
//! A capability row registers its commands when it mounts and they go when it
//! unmounts; the catalog reaches a front end inside [`super::AgentDescription`],
//! and every entry is run the same way, by
//! [`crate::event::AgentCommand::Invoke`]. So a new capability brings its
//! commands with it, and the contract does not grow a type per command.
//!
//! These are a person's commands, run from a front end. They are not tools: a
//! model never sees them.

use serde::{Deserialize, Serialize};

/// Whether a command takes an argument, from the person's own declaration on
/// disk. Kept in the neutral layer rather than in a capability row: this is
/// the shape a front end needs to decide how to route a keystroke, and a
/// front end must not have to know whether the command came from a skill on
/// disk or a built-in row.
///
/// `Unspecified` is the default and the value a front end uses to say "the
/// disk did not declare this": it keeps a legacy command's behaviour
/// unchanged rather than silently treating "not written" as "optional".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArgsRequirement {
    /// The command must receive a non-empty argument.
    Required,
    /// The command accepts an argument but does not require one.
    Optional,
    /// The command does not accept an argument at all. The menu routes a bare
    /// pick to an immediate dispatch, same as [`Unspecified`](Self::Unspecified):
    /// the command takes nothing after its name. Refusing a surplus argument
    /// is a future strict form.
    None,
    /// The disk did not declare the field. Default; preserves legacy command
    /// behaviour exactly.
    #[default]
    Unspecified,
}

/// One command in the catalog.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandDescription {
    /// What a person types, without any prefix a front end puts before it.
    pub name: String,
    /// How the arguments are written, for a hint beside the name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<String>,
    /// What it does, in a line.
    pub summary: String,
    pub target: CommandTarget,
    /// Whether the command's argument is required. Absent on the wire reads as
    /// [`ArgsRequirement::Unspecified`], so a description written by an older
    /// build still deserialises.
    #[serde(default)]
    pub args_requirement: ArgsRequirement,
}

/// What a command acts on, which is what [`crate::event::AgentCommand::Invoke`]
/// addresses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandTarget {
    /// The session as a whole. The common case — the few commands that act on
    /// one agent (`stop`) are the exception, so this is also the default.
    #[default]
    Session,
    /// One agent in it — a member, say.
    Agent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_requirement_default_is_unspecified() {
        // The state that keeps a legacy command's behaviour unchanged. If this
        // ever drifts to `Optional`, every pre-existing command would start
        // routing to the completion path on the next front end update.
        assert_eq!(ArgsRequirement::default(), ArgsRequirement::Unspecified);
    }

    #[test]
    fn args_requirement_serde_uses_snake_case_tags() {
        // The wire form is the stable contract between a capability row and a
        // front end, so its tag spelling is pinned here.
        assert_eq!(
            serde_json::to_string(&ArgsRequirement::Required).unwrap(),
            "\"required\""
        );
        assert_eq!(
            serde_json::to_string(&ArgsRequirement::Optional).unwrap(),
            "\"optional\""
        );
        assert_eq!(
            serde_json::to_string(&ArgsRequirement::None).unwrap(),
            "\"none\""
        );
        assert_eq!(
            serde_json::to_string(&ArgsRequirement::Unspecified).unwrap(),
            "\"unspecified\""
        );
        assert_eq!(
            serde_json::from_str::<ArgsRequirement>("\"required\"").unwrap(),
            ArgsRequirement::Required
        );
        assert_eq!(
            serde_json::from_str::<ArgsRequirement>("\"unspecified\"").unwrap(),
            ArgsRequirement::Unspecified
        );
        // An unknown tag must not be silently accepted as a default.
        assert!(serde_json::from_str::<ArgsRequirement>("\"bogus\"").is_err());
    }

    #[test]
    fn command_description_without_args_requirement_still_deserialises() {
        // A description written by an older build has no `args_requirement`
        // field. `#[serde(default)]` must make that read as `Unspecified`
        // rather than failing — this is the backward-compatibility guarantee
        // for sessions and remote agent descriptions persisted before the
        // field existed.
        let json = r#"{"name":"cmd","usage":"[x]","summary":"does a thing","target":"Session"}"#;
        let parsed: CommandDescription = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.args_requirement, ArgsRequirement::Unspecified);
        assert_eq!(parsed.name, "cmd");
        assert_eq!(parsed.target, CommandTarget::Session);
    }
}
