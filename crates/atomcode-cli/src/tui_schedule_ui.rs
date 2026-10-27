//! Keyboard-first management over the shared automation service.
use anyhow::{Context as _, Result};
use async_trait::async_trait;
use atomcode_config::schedule::{Schedule, ScheduleTask};
use atomcode_i18n::screen::{t as tr, Msg};
use atomcode_plexus::Context;
use atomcode_schedule::{
    service::{Mutation, Service},
    Definition, RunEvent,
};
use atomcode_tui::{
    command::{Command, CommandSet, Outcome},
    keymap::Action,
    overlay::Choice,
    plugin::AgentClientSvc,
    sheet::{List, Row, Sheet},
    wizard::{StepDef, StepKind, Wizard},
};
use std::sync::{Arc, Mutex};

pub struct ScheduleCommands {
    editor: Mutex<Option<Editor>>,
}
#[derive(Clone)]
struct Editor {
    token: String,
    wizard: Arc<Wizard>,
    task: ScheduleTask,
    original: Option<Definition>,
    deleting: bool,
}
impl Default for ScheduleCommands {
    fn default() -> Self {
        Self {
            editor: Mutex::new(None),
        }
    }
}
fn service() -> Service {
    Service::system(
        atomcode_coding::config::product_dirs_from_env()
            .user()
            .join("schedules"),
    )
}
fn sheet(title: impl Into<String>, rows: Vec<Row>) -> Outcome {
    Outcome::Do(Action::OpenSheet(Sheet::list(
        List::new("schedule", title, rows).empty(tr(Msg::AutomationEmpty)),
    )))
}
fn route(action: &str, id: &str) -> String {
    format!("schedule {action} {id}")
}
#[async_trait]
impl CommandSet for ScheduleCommands {
    fn id(&self) -> &'static str {
        super::tui_schedule::ROW
    }
    fn commands(&self) -> Vec<Command> {
        vec![Command::said_taking(
            "schedule",
            "[new | task]".into(),
            tr(Msg::AutomationManage).into(),
        )]
    }
    fn hidden(&self) -> Vec<Command> {
        vec![Command::said(
            "schedule-save",
            tr(Msg::AutomationFinish).into(),
        )]
    }
    async fn run(&self, name: &str, args: &str, ctx: &Context) -> Outcome {
        match self.answer(name, args, ctx).await {
            Ok(outcome) => outcome,
            Err(error) => Outcome::Refused(format!("{error:#}")),
        }
    }
}
impl ScheduleCommands {
    async fn answer(&self, name: &str, args: &str, ctx: &Context) -> Result<Outcome> {
        if name == "schedule-save" {
            return self.finish(args.trim()).await;
        }
        let mut words = args.split_whitespace();
        let action = words.next().unwrap_or("");
        let id = words.next().unwrap_or("");
        let observed = words.next().map(str::to_string);
        if action.is_empty() || action == "list" {
            let entries = tokio::task::spawn_blocking(|| service().catalog()).await??;
            let mut rows = vec![Row::new("schedule new", tr(Msg::AutomationNew))];
            rows.extend(entries.into_iter().map(|entry| {
                let task = entry.definition.task;
                Row::new(route("detail", &task.id), task.title)
                    .about(format!(
                        "{} · {}{}",
                        if task.enabled {
                            tr(Msg::AutomationEnabled)
                        } else {
                            tr(Msg::AutomationPaused)
                        },
                        entry.registration,
                        if entry.running { " · running" } else { "" }
                    ))
                    .preview(vec![
                        task.prompt,
                        task.cwd,
                        entry
                            .next_run
                            .map(|n| {
                                format!(
                                    "Estimated next: {} · {}",
                                    super::tui_schedule::when(n.at),
                                    n.time_zone
                                )
                            })
                            .unwrap_or_default(),
                    ])
            }));
            return Ok(sheet(tr(Msg::AutomationTitle), rows));
        }
        if action == "new" {
            let client = ctx
                .require::<AgentClientSvc>()
                .map_err(|e| anyhow::anyhow!(e.to_string()))?;
            let control = client.control().context("host control unavailable")?;
            let reply = control
                .call(atomcode_host_api::HostCommand::Context {
                    session: client.root(),
                    prompt: false,
                })
                .await
                .map_err(|e| anyhow::anyhow!(format!("{e:?}")))?;
            let atomcode_host_api::HostReply::Context { working_dir, .. } = reply else {
                anyhow::bail!("working directory unavailable");
            };
            let task = serde_json::from_value(serde_json::json!({
                "id": uuid::Uuid::new_v4().to_string(), "title":tr(Msg::AutomationNewName), "prompt":"", "cwd":working_dir,
                "schedule":{"kind":"daily","time":"09:00"}
            }))?;
            return Ok(self.edit(task, None, false));
        }
        let task_id = if id.is_empty() { action } else { id };
        let definition = service().store.get(task_id)?;
        let task = &definition.task;
        match action {
            "edit" => Ok(self.edit(task.clone(), Some(definition), false)),
            "delete" => Ok(self.edit(task.clone(), Some(definition), true)),
            "toggle" => {
                let mut task = task.clone();
                task.enabled = !task.enabled;
                let expected = observed.unwrap_or(definition.revision);
                let entry =
                    tokio::task::spawn_blocking(move || service().save(&task, Some(&expected)))
                        .await??;
                Ok(Outcome::Said(
                    entry
                        .registration_error
                        .unwrap_or_else(|| tr(Msg::AutomationUpdated).into()),
                ))
            }
            "run" => {
                let id = task.id.clone();
                let request = uuid::Uuid::new_v4().to_string();
                let result = tokio::task::spawn_blocking(move || {
                    service().launch(&id, &request, &observed.unwrap_or(definition.revision))
                })
                .await??;
                Ok(Outcome::Said(
                    result
                        .error
                        .unwrap_or_else(|| tr(Msg::AutomationAdmitted).into()),
                ))
            }
            "repair" => {
                let id = task.id.clone();
                let entry = tokio::task::spawn_blocking(move || service().reconcile(&id)).await??;
                Ok(Outcome::Said(entry.registration_error.unwrap_or_else(
                    || tr(Msg::AutomationRegistrationChecked).into(),
                )))
            }
            "history" => {
                let cursor = observed.as_deref().map(str::parse::<usize>).transpose()?;
                let page = service().store.history_page(&task.id, cursor, 20)?;
                let events = page.events;
                let mut rows = Vec::new();
                for event in events.into_iter().rev() {
                    match event {
                        RunEvent::Session { session_id, at, .. } => rows.push(Row::new(
                            format!("resume {session_id}"),
                            format!("{} · Open session", super::tui_schedule::when(at)),
                        )),
                        RunEvent::Started {
                            run_id, at, prompt, ..
                        } => rows.push(
                            Row::new(
                                route("history", &task.id),
                                format!("{} · Started · {run_id}", super::tui_schedule::when(at)),
                            )
                            .preview(vec![prompt]),
                        ),
                        RunEvent::Finished {
                            status, error, at, ..
                        } => rows.push(
                            Row::new(
                                route("history", &task.id),
                                format!("{} · {status}", super::tui_schedule::when(at)),
                            )
                            .preview(error.into_iter().collect()),
                        ),
                    }
                }
                if let Some(cursor) = page.next_cursor {
                    rows.push(Row::new(
                        format!("schedule history {} {cursor}", task.id),
                        tr(Msg::AutomationOlder),
                    ));
                }
                rows.push(Row::new(route("detail", &task.id), tr(Msg::AutomationEdit)));
                Ok(sheet(tr(Msg::AutomationHistory), rows))
            }
            _ => Ok(sheet(
                &task.title,
                vec![
                    Row::new(route("edit", &task.id), tr(Msg::AutomationEdit)).preview(vec![
                        task.prompt.clone(),
                        task.title.clone(),
                        task.cwd.clone(),
                        format!("Permissions: {} · Default host model", task.permission_mode),
                    ]),
                    Row::new(
                        format!("{} {}", route("toggle", &task.id), definition.revision),
                        if task.enabled {
                            tr(Msg::AutomationPause)
                        } else {
                            tr(Msg::AutomationEnable)
                        },
                    ),
                    Row::new(
                        format!("{} {}", route("run", &task.id), definition.revision),
                        tr(Msg::AutomationRun),
                    ),
                    Row::new(route("history", &task.id), tr(Msg::AutomationHistory)),
                    Row::new(route("repair", &task.id), tr(Msg::AutomationRepair)),
                    Row::new(route("delete", &task.id), tr(Msg::AutomationDelete)),
                ],
            )),
        }
    }
    fn edit(&self, task: ScheduleTask, original: Option<Definition>, deleting: bool) -> Outcome {
        let token = uuid::Uuid::new_v4().to_string();
        let current_kind = match task.schedule {
            Schedule::Daily { .. } => "daily",
            Schedule::Weekly { .. } => "weekly",
            Schedule::Hourly => "hourly",
            Schedule::Interval { .. } => "interval",
            Schedule::Cron { .. } => "daily",
        };
        let mut rules = vec![
            Choice::new("daily", tr(Msg::AutomationDaily)),
            Choice::new("weekly", tr(Msg::AutomationWeekly)),
            Choice::new("hourly", tr(Msg::AutomationHourly)),
            Choice::new("interval", tr(Msg::AutomationInterval)),
        ];
        rules.sort_by_key(|choice| choice.value != current_kind);
        let mut modes = vec![
            Choice::new("plan", tr(Msg::AutomationReadOnly)),
            Choice::new("accept_edits", tr(Msg::AutomationAllowEdits)),
        ];
        let current_mode = if task.permission_mode == "auto" {
            "accept_edits"
        } else {
            &task.permission_mode
        };
        modes.sort_by_key(|choice| choice.value != current_mode);
        let mut steps = if deleting {
            Vec::new()
        } else {
            vec![
                StepDef::new(
                    "title",
                    tr(Msg::AutomationName),
                    StepKind::Type {
                        placeholder: task.title.clone(),
                    },
                )
                .saying(vec![tr(Msg::AutomationKeep).into()]),
                StepDef::new(
                    "prompt",
                    tr(Msg::AutomationInstructions),
                    StepKind::Type {
                        placeholder: task.prompt.clone(),
                    },
                ),
                StepDef::new("rule", tr(Msg::AutomationRepeat), StepKind::Choose(rules)),
                StepDef::new(
                    "time",
                    tr(Msg::AutomationTime),
                    StepKind::Type {
                        placeholder: tr(Msg::AutomationTimeHint).into(),
                    },
                ),
                StepDef::new(
                    "mode",
                    tr(Msg::AutomationPermissions),
                    StepKind::Choose(modes),
                ),
            ]
        };
        steps.push(
            StepDef::new(
                "confirm",
                if deleting {
                    tr(Msg::AutomationDeleteConfirm)
                } else {
                    tr(Msg::AutomationConfirm)
                },
                StepKind::Note,
            )
            .saying(vec![
                task.title.clone(),
                task.cwd.clone(),
                tr(Msg::AutomationModelHint).into(),
                if deleting {
                    tr(Msg::AutomationDeleteHint)
                } else {
                    tr(Msg::AutomationConfirmHint)
                }
                .into(),
            ]),
        );
        let slot = Arc::new(std::sync::OnceLock::<std::sync::Weak<Wizard>>::new());
        let here = slot.clone();
        let preview = task.clone();
        let wizard = Wizard::new(
            "schedule-editor",
            tr(Msg::AutomationTitle),
            steps,
            Box::new(move |step| {
                if step != "confirm" || deleting {
                    return;
                }
                let Some(wizard) = here.get().and_then(std::sync::Weak::upgrade) else {
                    return;
                };
                let answers = wizard.answers();
                let answer = |key: &str| {
                    answers
                        .iter()
                        .find(|(id, _)| id == key)
                        .and_then(|(_, v)| v.clone())
                        .filter(|s| !s.trim().is_empty())
                };
                let kind = answer("rule").unwrap_or_default();
                let value = answer("time").unwrap_or_else(|| match &preview.schedule {
                    Schedule::Daily { time } => time.clone(),
                    Schedule::Weekly { weekday, time } => format!("{weekday}@{time}"),
                    Schedule::Interval { every_minutes } => every_minutes.to_string(),
                    _ => String::new(),
                });
                let kind = match kind.as_str() {
                    "daily" => tr(Msg::AutomationDaily),
                    "weekly" => tr(Msg::AutomationWeekly),
                    "hourly" => tr(Msg::AutomationHourly),
                    _ => tr(Msg::AutomationInterval),
                };
                let mode = if answer("mode").as_deref() == Some("accept_edits") {
                    tr(Msg::AutomationAllowEdits)
                } else {
                    tr(Msg::AutomationReadOnly)
                };
                wizard.say(vec![
                    format!(
                        "{}: {}",
                        tr(Msg::AutomationName),
                        answer("title").unwrap_or_else(|| preview.title.clone())
                    ),
                    answer("prompt").unwrap_or_else(|| preview.prompt.clone()),
                    format!("{kind} {value}"),
                    mode.into(),
                    preview.cwd.clone(),
                    tr(Msg::AutomationModelHint).into(),
                    tr(Msg::AutomationConfirmHint).into(),
                ]);
            }),
            format!("schedule-save {token}"),
        );
        let _ = slot.set(Arc::downgrade(&wizard));
        *self.editor.lock().expect("task editor poisoned") = Some(Editor {
            token,
            wizard: wizard.clone(),
            task,
            original,
            deleting,
        });
        Outcome::Open(wizard)
    }
    async fn finish(&self, token: &str) -> Result<Outcome> {
        let editor = self
            .editor
            .lock()
            .expect("task editor poisoned")
            .clone()
            .context("No task editor is open")?;
        if editor.token != token {
            anyhow::bail!("task editor was replaced");
        }
        let mutation = editor_mutation(editor)?;
        let token = token.to_string();
        let request_token = token.clone();
        let result =
            tokio::task::spawn_blocking(move || service().apply(&request_token, &mutation))
                .await??;
        let mut current = self.editor.lock().expect("task editor poisoned");
        if current.as_ref().is_some_and(|editor| editor.token == token) {
            *current = None;
        }
        Ok(Outcome::Said(
            result
                .warning
                .or_else(|| result.entry.and_then(|entry| entry.registration_error))
                .unwrap_or_else(|| tr(Msg::AutomationSaved).into()),
        ))
    }
}

fn editor_mutation(editor: Editor) -> Result<Mutation> {
    if editor.wizard.answers().last().map(|(id, _)| id.as_str()) != Some("confirm") {
        anyhow::bail!("task editor was not confirmed");
    }
    let answers = editor.wizard.answers();
    let answer = |id: &str| {
        answers
            .iter()
            .find(|(key, _)| key == id)
            .and_then(|(_, value)| value.clone())
            .filter(|v| !v.trim().is_empty())
    };
    let mutation = if editor.deleting {
        let original = editor.original.context("missing task revision")?;
        Mutation::Delete {
            id: editor.task.id,
            expected: original.revision,
        }
    } else {
        let mut task = editor.task;
        if let Some(title) = answer("title") {
            task.title = title;
        }
        if let Some(prompt) = answer("prompt") {
            task.prompt = prompt;
        }
        let rule = answer("rule").context("repeat rule missing")?;
        let value = answer("time").or_else(|| match (&task.schedule, rule.as_str()) {
            (Schedule::Daily { time }, "daily") => Some(time.clone()),
            (Schedule::Weekly { weekday, time }, "weekly") => Some(format!("{weekday}@{time}")),
            (Schedule::Interval { every_minutes }, "interval") => Some(every_minutes.to_string()),
            _ => None,
        });
        task.schedule = match rule.as_str() {
            "hourly" => Schedule::Hourly,
            "daily" => Schedule::Daily {
                time: value.context("daily time is required")?,
            },
            "weekly" => {
                let value = value.context("weekly day and time required")?;
                let (day, time) = value.split_once('@').context("expected N@HH:MM")?;
                Schedule::Weekly {
                    weekday: day.parse()?,
                    time: time.into(),
                }
            }
            "interval" => Schedule::Interval {
                every_minutes: value.context("interval required")?.parse()?,
            },
            _ => anyhow::bail!("unknown repeat rule"),
        };
        task.permission_mode = answer("mode").context("permission mode missing")?;
        atomcode_schedule::timing::validate(&task)?;
        match editor.original {
            Some(original) => Mutation::Update {
                task,
                expected: original.revision,
            },
            None => Mutation::Create { task },
        }
    };
    Ok(mutation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_tui::{
        overlay::Overlay,
        surface::{Key, KeyPress},
    };
    fn task() -> ScheduleTask {
        serde_json::from_value(serde_json::json!({"id":"wizard-task","title":"Existing task","prompt":"Existing instruction","cwd":std::env::temp_dir(),"schedule":{"kind":"weekly","weekday":3,"time":"10:30"},"permission_mode":"accept_edits"})).unwrap()
    }
    fn enter(wizard: &Wizard) {
        wizard.key(KeyPress::plain(Key::Enter));
    }
    fn type_text(wizard: &Wizard, text: &str) {
        for ch in text.chars() {
            wizard.key(KeyPress::plain(Key::Char(ch)));
        }
        enter(wizard);
    }
    #[test]
    fn keyboard_edit_preserves_rule_permissions_and_revision_until_confirmed() {
        let commands = ScheduleCommands::default();
        let original = Definition {
            task: task(),
            revision: "observed-revision".into(),
        };
        commands.edit(original.task.clone(), Some(original), false);
        let editor = commands.editor.lock().unwrap().clone().unwrap();
        type_text(&editor.wizard, "Changed name");
        enter(&editor.wizard); // keep instruction
        enter(&editor.wizard); // existing weekly rule is selected first
        enter(&editor.wizard); // keep the existing weekday/time
        enter(&editor.wizard); // keep explicit edit permission
        assert!(
            editor_mutation(editor.clone()).is_err(),
            "confirmation has not happened"
        );
        enter(&editor.wizard);
        let Mutation::Update { task, expected } = editor_mutation(editor).unwrap() else {
            panic!("expected update")
        };
        assert_eq!(task.title, "Changed name");
        assert_eq!(task.prompt, "Existing instruction");
        assert!(matches!(task.schedule, Schedule::Weekly { weekday: 3, .. }));
        assert_eq!(task.permission_mode, "accept_edits");
        assert_eq!(expected, "observed-revision");
    }
    #[test]
    fn cancelling_a_delete_does_not_produce_a_mutation() {
        let commands = ScheduleCommands::default();
        commands.edit(
            task(),
            Some(Definition {
                task: task(),
                revision: "rev".into(),
            }),
            true,
        );
        let editor = commands.editor.lock().unwrap().clone().unwrap();
        editor.wizard.key(KeyPress::plain(Key::Esc));
        assert!(editor_mutation(editor).is_err());
    }
    #[tokio::test]
    async fn stale_editor_completion_cannot_save_a_replacement_form() {
        let commands = ScheduleCommands::default();
        commands.edit(task(), None, false);
        let old = commands
            .editor
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .token
            .clone();
        commands.edit(task(), None, false);
        assert!(commands.finish(&old).await.is_err());
    }
}
