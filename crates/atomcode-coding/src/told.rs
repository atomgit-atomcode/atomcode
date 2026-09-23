//! What the model is told about what the person did.
//!
//! A person changes the session from outside the conversation — `/model`,
//! `/plan`, `/mcp`, `/undo` — and the model sees the result without the cause:
//! the identity line names a new model, a tool is gone, the history is shorter
//! while the files are not. Each sentence here says the cause once, as a
//! `<system-reminder>` note committed where it happened (`Agent::note`), so it is
//! in the log and a resumed session still says it.
//!
//! Not every command is told. The rule, per runtime control:
//!
//! - **Told** — it changes what the model is or may do, and nothing the model
//!   already sees says the person did it.
//! - **Already a fact** — the log already carries it in a form the model reads:
//!   a compaction summary, a partial reply, a new session with its own prompt.
//! - **Nothing** — a read, or something about the screen.
//!
//! Every [`CodingRuntimeControl`] is sorted into one of the three by
//! [`tests::heard`], which matches them without a wildcard: a new control does
//! not compile until someone has decided what the model hears of it.
//!
//! What is never told: credentials, and anything whose only audience is the
//! screen. A sign-in that ends on another model is told as the model switch it
//! is, not as the sign-in.

use crate::parts::McpAction;
use crate::runtime::RuntimeMode;
use crate::CodingAgentConfig;

fn reminder(body: &str) -> String {
    format!("<system-reminder>{body}</system-reminder>")
}

/// What changed between two configurations the conversation ran on — the
/// model, the reasoning effort, thinking — or `None` when none of those did.
pub(crate) fn reconfigured(from: &CodingAgentConfig, to: &CodingAgentConfig) -> Option<String> {
    let mut said = Vec::new();
    if from.provider_name != to.provider_name || from.model != to.model {
        let name = |config: &CodingAgentConfig| {
            if from.provider_name == to.provider_name {
                format!("`{}`", config.model)
            } else {
                format!("`{}` (provider {})", config.model, config.provider_name)
            }
        };
        let (from, to) = (name(from), name(to));
        said.push(format!(
            "The model for this conversation was switched from {from} to {to}. The replies \
             above this point were written by {from}; from here on you are {to}."
        ));
    }
    let effort = from.chat_options.reasoning_effort;
    let next_effort = to.chat_options.reasoning_effort;
    if effort != next_effort && to.supports_reasoning_effort {
        let level = |e: Option<atomcode_kernel::provider::ReasoningEffort>| {
            e.map_or("the default", |e| e.as_str())
        };
        said.push(format!(
            "The reasoning effort was changed from {} to {}.",
            level(effort),
            level(next_effort)
        ));
    }
    if from.thinking_enabled != to.thinking_enabled {
        said.push(
            match to.thinking_enabled {
                Some(true) => "Extended thinking was turned on.",
                Some(false) => "Extended thinking was turned off.",
                None => "Extended thinking was set back to the model's default.",
            }
            .to_string(),
        );
    }
    (!said.is_empty()).then(|| reminder(&said.join(" ")))
}

fn mode_name(mode: RuntimeMode) -> &'static str {
    match mode {
        RuntimeMode::Plan => "plan",
        RuntimeMode::Build => "build",
        RuntimeMode::AcceptEdits => "accept-edits",
        RuntimeMode::Auto => "auto",
    }
}

fn mode_means(mode: RuntimeMode) -> &'static str {
    match mode {
        RuntimeMode::Plan => {
            "investigate with read-only tools and present a plan; do not edit files"
        }
        RuntimeMode::Build => {
            "you may edit files and run commands; risky actions ask the person for approval \
             first"
        }
        RuntimeMode::AcceptEdits => {
            "you may edit files and run commands; file edits are applied without asking, \
             other risky actions still ask"
        }
        RuntimeMode::Auto => {
            "you may edit files and run commands; the person is no longer asked before tools \
             run"
        }
    }
}

/// The execution mode moved. Plan mode also has its own standing reminder while
/// it is on; what that cannot say is that it has just been switched OFF, which
/// is when the model may start writing.
pub(crate) fn mode_changed(from: RuntimeMode, to: RuntimeMode) -> Option<String> {
    (from != to).then(|| {
        reminder(&format!(
            "The person switched from {} mode to {} mode: {}.",
            mode_name(from),
            mode_name(to),
            mode_means(to)
        ))
    })
}

pub(crate) fn mcp_acted(server: &str, action: McpAction) -> String {
    reminder(&match action {
        McpAction::Trust => format!(
            "The person trusted the MCP server `{server}`; its tools are offered once it \
             connects."
        ),
        McpAction::Untrust => format!(
            "The person revoked trust in the MCP server `{server}`; its tools are no longer \
             available."
        ),
        McpAction::Logout => format!(
            "The person signed out of the MCP server `{server}`; its tools are no longer \
             available."
        ),
        McpAction::Enable => format!(
            "The person enabled the MCP server `{server}`; its tools are offered once it \
             connects."
        ),
        McpAction::Disable => format!(
            "The person disabled the MCP server `{server}`; its tools are no longer available."
        ),
    })
}

pub(crate) fn tool_switched(pattern: &str, on: bool) -> String {
    reminder(&if on {
        format!("The person turned on the tools matching `{pattern}`.")
    } else {
        format!(
            "The person turned off the tools matching `{pattern}`; they are no longer \
             available."
        )
    })
}

pub(crate) fn mcp_withdrawn() -> String {
    reminder("The person withdrew every MCP tool from this session.")
}

pub(crate) fn reloaded() -> String {
    reminder(
        "The person reloaded plugins, skills and MCP servers; the tools and skills available \
         to you may have changed.",
    )
}

/// The conversation went back and the files did not — what `/undo` does, and a
/// rewind that keeps the code.
pub(crate) fn conversation_rewound_code_kept() -> String {
    reminder(
        "The person rewound this conversation to before one of their earlier messages; the \
         turns after it are gone from the conversation. Changes those turns made to files \
         were NOT reverted and are still on disk.",
    )
}

/// The files went back and the conversation did not.
pub(crate) fn code_restored_conversation_kept(files: &[String]) -> String {
    const SHOWN: usize = 20;
    let mut list = files
        .iter()
        .take(SHOWN)
        .map(|file| format!("`{file}`"))
        .collect::<Vec<_>>()
        .join(", ");
    if files.len() > SHOWN {
        list.push_str(&format!(" and {} more", files.len() - SHOWN));
    }
    reminder(&format!(
        "The person restored files to an earlier point while keeping this conversation, so \
         edits described above may no longer be on disk. Restored: {list}."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{CodingRuntimeControl, ReprepareTarget};

    pub(super) enum Heard {
        Told,
        AlreadyAFact,
        Nothing,
    }

    /// What the model hears of each control. No wildcard, on purpose: see the
    /// module doc.
    pub(super) fn heard(control: &CodingRuntimeControl) -> Heard {
        use CodingRuntimeControl as C;
        match control {
            C::ReassembleProvider { .. }
            | C::SetMode { .. }
            | C::McpAct { .. }
            | C::SwitchTool { .. }
            | C::WithdrawMcpTools { .. }
            | C::ApplyUndo { .. }
            | C::FinishRewind { .. } => Heard::Told,
            C::Reprepare { target, .. } => match target {
                ReprepareTarget::Reload { .. } | ReprepareTarget::ReloadConfig(_) => Heard::Told,
                // A new session: its prompt says where it is, and there is no
                // conversation above it to be told about.
                ReprepareTarget::Fresh
                | ReprepareTarget::Resume(_)
                | ReprepareTarget::ResumeWithLease { .. }
                | ReprepareTarget::ChangeDirectory(_) => Heard::AlreadyAFact,
            },
            // The person's own words, or a fact of their own in the log: a
            // compaction summary, a partial reply, an answer, a goal or loop
            // prompt, a restored conversation.
            C::Compact { .. }
            | C::Submit { .. }
            | C::Respond { .. }
            | C::ResolvePolicyIntervention { .. }
            | C::Cancel { .. }
            | C::QueueLocalContext { .. }
            | C::RestoreSnapshot { .. }
            | C::StartGoal { .. }
            | C::AdjustGoalRounds { .. }
            | C::StopGoal { .. }
            | C::PauseGoal { .. }
            | C::StartLoop { .. }
            | C::StopLoop { .. } => Heard::AlreadyAFact,
            // Credentials are never told; the model it leaves is told by the
            // reassemble that follows a sign-in.
            C::DeactivateProvider { .. } => Heard::Nothing,
            // Opens a transaction `FinishRewind` commits; told there.
            C::BeginRewind { .. } => Heard::Nothing,
            C::Shutdown { .. }
            | C::Snapshot { .. }
            | C::ContextStats { .. }
            | C::Mode { .. }
            | C::WaitMcpReady { .. }
            | C::McpStatus { .. }
            | C::McpTools { .. }
            | C::McpRows { .. }
            | C::McpDetail { .. }
            | C::ToolCatalog { .. }
            | C::PendingPolicyIntervention { .. }
            | C::RewindCatalog { .. }
            | C::Autonomy { .. }
            | C::Usage { .. }
            | C::WorkspaceChanges { .. } => Heard::Nothing,
        }
    }

    #[test]
    fn a_mode_switch_is_told_and_a_read_of_it_is_not() {
        let (done, _) = tokio::sync::oneshot::channel();
        assert!(matches!(
            heard(&CodingRuntimeControl::SetMode {
                generation: 0,
                mode: RuntimeMode::Plan,
                done,
            }),
            Heard::Told
        ));
        let (done, _) = tokio::sync::oneshot::channel();
        assert!(matches!(
            heard(&CodingRuntimeControl::Mode {
                generation: 0,
                done
            }),
            Heard::Nothing
        ));
    }

    #[test]
    fn choosing_what_is_already_in_use_says_nothing() {
        let config = CodingAgentConfig::new("key", "https://example.test/v1", "m", ".");
        assert_eq!(reconfigured(&config, &config), None);
        assert_eq!(mode_changed(RuntimeMode::Plan, RuntimeMode::Plan), None);
    }

    #[test]
    fn a_long_restore_names_the_first_files_and_counts_the_rest() {
        let files: Vec<String> = (0..25).map(|i| format!("f{i}.rs")).collect();
        let said = code_restored_conversation_kept(&files);
        assert!(
            said.contains("`f19.rs`") && !said.contains("`f20.rs`"),
            "{said}"
        );
        assert!(said.contains("and 5 more"), "{said}");
    }
}
