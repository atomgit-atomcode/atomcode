//! Keeping the task list true while the turn runs.
//!
//! `todowrite` tells the model, in as many words, to mark a task `in_progress`
//! the moment it starts and `completed` the moment it is verified. The model
//! reads that once, plans, and then works — and twelve steps later the list on
//! screen still says it is doing the thing it finished ten minutes ago. Nobody
//! lied: the instruction is in a tool description, and a tool description is
//! the furthest thing in the prompt from the step being taken.
//!
//! So the list is checked against the work, and when it has gone stale the
//! agent is told — once, in the same `<system-reminder>` shape everything else
//! model-visible-but-not-said uses. This is the only thing in the tree that
//! injects one: reminders were a mechanism the harness had ([`InjectionOrigin::Reminder`],
//! rendered by `derive_messages`) and never used, because the piece that knew
//! something had gone stale did not exist yet.
//!
//! **An observer, not a policy.** It listens to committed facts, exactly as the
//! team row does, rather than sitting in `turn-stopping` where a side effect
//! would be hiding inside a question about whether to stop. It never changes
//! what runs; it adds a sentence to what the next step sees.
//!
//! The list is folded with the tool's own [`reduce_todos`] — the same function
//! the tool and the panel use — so three things can never disagree about what
//! the plan currently is.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_capabilities::tools::todo::{is_todo_call, reduce_todos, TodoStatus};
use atomcode_plexus::{Context, Plugin};
use serde::Deserialize;
use serde_json::Value;

use crate::events::SessionEventCommitted;
use crate::seams::AgentsSvc;
use crate::session::{Committed, InjectionOrigin, SessionEvent};

#[derive(Debug, Deserialize)]
struct Row {
    /// Steps of silence before the list is treated as stale.
    ///
    /// Not zero-tolerance: a model that reads a file, greps and edits between
    /// two status updates is working normally, and a reminder after every step
    /// is noise the model learns to skim. Three is the distance at which "still
    /// in progress" has stopped being true often enough to be worth a sentence.
    #[serde(default = "default_after")]
    after_steps: u32,
}

impl Default for Row {
    fn default() -> Self {
        Self {
            after_steps: default_after(),
        }
    }
}

fn default_after() -> u32 {
    3
}

/// One agent's list, as its own facts describe it.
#[derive(Default)]
struct Watch {
    /// Todo calls in order: `(call id, tool name, arguments)`.
    calls: Vec<(String, String, String)>,
    /// Calls that came back an error. A rejected plan is not a plan.
    failed: HashSet<String>,
    /// The step the list was last touched at, and the step it was last
    /// mentioned at — so a reminder that went unheeded does not repeat every
    /// step until the turn ends.
    touched_at: u32,
    reminded_at: u32,
}

#[derive(Default)]
struct Watches(Mutex<HashMap<String, Watch>>);

/// What the list still owes, in the words the reminder uses.
enum Stale {
    /// A task has been `in_progress` since before the silence started.
    Doing(String),
    /// There is unfinished work and nothing is marked as being done.
    Idle(usize),
}

fn stale(watch: &Watch) -> Option<Stale> {
    let items = reduce_todos(
        watch
            .calls
            .iter()
            .filter(|(id, _, _)| !watch.failed.contains(id))
            .map(|(_, name, args)| (name.as_str(), args.as_str())),
    );
    if items.is_empty() {
        return None;
    }
    if let Some(doing) = items
        .iter()
        .find(|item| item.status == TodoStatus::InProgress)
    {
        return Some(Stale::Doing(doing.content.clone()));
    }
    let left = items
        .iter()
        .filter(|item| item.status != TodoStatus::Completed)
        .count();
    // A list that is entirely done needs nothing said about it: the model
    // finished and the panel has already retired.
    (left > 0).then_some(Stale::Idle(left))
}

fn reminder(stale: &Stale, quiet_steps: u32) -> String {
    let body = match stale {
        Stale::Doing(what) => format!(
            "The task list still shows \"{what}\" in progress, and has not been updated for \
             {quiet_steps} steps. If it is done, mark it completed \
             (`{{\"action\":\"update\",\"id\":N,\"status\":\"completed\"}}`); if you moved on to \
             something else, mark that one in progress; if the plan changed, send the new list."
        ),
        Stale::Idle(left) => format!(
            "The task list has {left} unfinished task(s) and none of them is marked in progress, \
             {quiet_steps} steps after it was last touched. Mark what you are working on \
             (`{{\"action\":\"update\",\"id\":N,\"status\":\"in_progress\"}}`), or send a new list \
             if the plan changed."
        ),
    };
    // The same envelope every model-visible-but-unsaid note uses, and the same
    // last line: a reminder the model repeats back to the person is a reminder
    // that has become part of the conversation, which is exactly what it is not.
    format!("<system-reminder>{body} Do not mention this reminder to the user.</system-reminder>")
}

pub struct TodoReminderPlugin;

#[async_trait]
impl Plugin for TodoReminderPlugin {
    fn name(&self) -> &'static str {
        "todo-reminder"
    }
    fn uses(&self) -> &'static [&'static str] {
        &["agents", "tools"]
    }
    fn description(&self) -> &'static str {
        "tell the agent when the task list has stopped describing what it is doing"
    }

    async fn apply(&self, ctx: &Context, config: &Value) -> Result<(), String> {
        let row: Row = if config.is_null() {
            Row::default()
        } else {
            serde_json::from_value(config.clone()).map_err(|e| format!("bad config: {e}"))?
        };
        let watches = Arc::new(Watches::default());
        let ctx_for_listener = ctx.clone();

        let _ = ctx.on_emit::<SessionEventCommitted>(move |committed: &Committed| {
            // Decided under the lock, said outside it. Committing a fact emits
            // `SessionEventCommitted`, which re-enters this very listener — and
            // a `std::sync::Mutex` held across that is a deadlock, not a
            // borrow error, so the lock ends before anything is said.
            let say: Option<(u64, String)> = {
                let mut all = watches.0.lock().expect("watches poisoned");
                let watch = all.entry(committed.session.clone()).or_default();
                match &committed.event {
                    // A fresh turn starts the clock over: the list was true when
                    // the previous turn ended, and the person has spoken since.
                    SessionEvent::TurnStart { .. } => {
                        watch.touched_at = 0;
                        watch.reminded_at = 0;
                        None
                    }

                    SessionEvent::AssistantMessage {
                        tool_calls, round, ..
                    } => {
                        for call in tool_calls.iter().filter(|c| is_todo_call(&c.name)) {
                            watch.calls.push((
                                call.id.clone(),
                                call.name.clone(),
                                call.arguments.clone(),
                            ));
                            watch.touched_at = *round;
                        }
                        None
                    }

                    SessionEvent::ToolResultLogged {
                        call_id, is_error, ..
                    } => {
                        if *is_error {
                            watch.failed.insert(call_id.clone());
                        }
                        None
                    }

                    // The end of a step is when the question is worth asking:
                    // this round's tools have run, and whatever the model did
                    // with them either moved the list or did not.
                    SessionEvent::StepEnd { turn, step, .. } => {
                        let step = *step;
                        let quiet = step.saturating_sub(watch.touched_at);
                        let since_said = step.saturating_sub(watch.reminded_at);
                        if quiet < row.after_steps || since_said < row.after_steps {
                            None
                        } else {
                            stale(watch).map(|stale| {
                                watch.reminded_at = step;
                                (*turn, reminder(&stale, quiet))
                            })
                        }
                    }
                    _ => None,
                }
            };

            let Some((turn, text)) = say else {
                return;
            };
            let Some(agents) = ctx_for_listener.service::<AgentsSvc>() else {
                return;
            };
            let Some(agent) = agents.by_session(&committed.session) else {
                return;
            };
            // Committed, not queued. `Inbox::inject` is for context that rides
            // along with the *next message*, and inside a running turn there is
            // no next message — the injection would sit in the inbox until the
            // person typed again, which is long after it mattered. A fact
            // committed here is in the log and in the next request, which is
            // what the repeat-fuse nudge does for the same reason.
            crate::session::commit(
                &ctx_for_listener,
                &agent.session(),
                SessionEvent::Injected {
                    turn,
                    text,
                    origin: InjectionOrigin::Reminder,
                },
            );
        });

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(id: &str, todos: &str) -> (String, String, String) {
        (
            id.to_string(),
            "todowrite".to_string(),
            format!(r#"{{"todos":{todos}}}"#),
        )
    }

    fn watch_with(calls: Vec<(String, String, String)>) -> Watch {
        Watch {
            calls,
            ..Watch::default()
        }
    }

    #[test]
    fn a_task_in_progress_is_what_goes_stale() {
        let watch = watch_with(vec![plan(
            "c1",
            r#"[{"content":"read the parser","status":"completed"},
                {"content":"fix the parser","status":"in_progress"}]"#,
        )]);
        match stale(&watch) {
            Some(Stale::Doing(what)) => assert_eq!(what, "fix the parser"),
            other => panic!("expected the in-progress task, got {:?}", other.is_some()),
        }
    }

    #[test]
    fn a_list_nobody_has_started_is_stale_a_different_way() {
        let watch = watch_with(vec![plan(
            "c1",
            r#"[{"content":"a","status":"pending"},{"content":"b","status":"pending"}]"#,
        )]);
        match stale(&watch) {
            Some(Stale::Idle(left)) => assert_eq!(left, 2),
            _ => panic!("two pending tasks and nothing in progress is stale"),
        }
    }

    #[test]
    fn a_finished_list_says_nothing() {
        let watch = watch_with(vec![plan(
            "c1",
            r#"[{"content":"a","status":"completed"},{"content":"b","status":"completed"}]"#,
        )]);
        assert!(
            stale(&watch).is_none(),
            "work that is done is not something to nag about"
        );
        assert!(
            stale(&Watch::default()).is_none(),
            "and neither is a session that never planned"
        );
    }

    #[test]
    fn a_rejected_plan_is_not_the_plan() {
        // `todowrite` validates its own arguments, so a plan that came back an
        // error never became the list — reminding about it would be reminding
        // about something that does not exist.
        let mut watch = watch_with(vec![plan(
            "c1",
            r#"[{"content":"a","status":"in_progress"}]"#,
        )]);
        watch.failed.insert("c1".to_string());
        assert!(stale(&watch).is_none());
    }

    #[test]
    fn the_reminder_names_the_task_and_stays_out_of_the_conversation() {
        let text = reminder(&Stale::Doing("fix the parser".into()), 4);
        assert!(text.starts_with("<system-reminder>"), "{text}");
        assert!(text.contains("fix the parser"), "{text}");
        assert!(text.contains("4 steps"), "{text}");
        assert!(text.contains("status\\\":\\\"completed") || text.contains("completed"));
        assert!(
            text.contains("Do not mention this reminder"),
            "a reminder read aloud is not a reminder: {text}"
        );
    }
}
