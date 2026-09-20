//! ACP slash commands: `available_commands_update` advertisement and local
//! execution.
//!
//! **The table lives here now** (2026-09-18, plan M6.3). It used to be the
//! `acp: true` subset of `atomcode_tuix::commands` — the last thing outside
//! `main.rs`'s deliberate `--classic` hatch that reached into the old front
//! end, and so the last thing standing between the plan and 6.4.
//!
//! Sourcing it there never bought what the old comment claimed. The handlers
//! were always ACP's own; what came across was 16 names and 16 sentences, and
//! a flag on a table belonging to a front end that ACP does not run. What is
//! advertised on this channel is this channel's business.
//!
//! **Interim, on purpose.** The end state in the plan is to project this from
//! the command catalog that travels with `AgentDescription`, which needs the
//! rest of 6.3 (this session holding a `HostConnection` rather than a
//! `CodingRuntimeHandle`). Until then the table is here, pinned by a criterion
//! against the set ACP advertised before the move, so the wire does not shift
//! under a client while the plumbing changes.
//!
//! Handlers never render TUI chrome; they return plain text that the turn loop
//! replies with before ending the turn (no model round-trip). Mode / effort /
//! model changes reuse the `session/set_config_option` path so slash state and
//! the config catalog can never diverge.

use agent_client_protocol::schema::v1::{
    AvailableCommand, AvailableCommandInput, SessionConfigKind, SessionConfigOption,
    SessionConfigOptionValue, SessionId, SessionUpdate, SetSessionConfigOptionRequest,
    UnstructuredCommandInput,
};
use agent_client_protocol::{Client, ConnectionTo};
use atomcode_capabilities::tools::todo::{TodoItem, TodoStatus};
use atomcode_coding::RuntimeMode;
use atomcode_host_api::{HostCommand, HostReply};
use atomcode_kernel::event::AgentCommand;

use crate::acp::options::{
    handle_set_session_config_option, MODEL_CONFIG_ID, MODE_CONFIG_ID, REASONING_EFFORT_CONFIG_ID,
};
use crate::acp::sessions::Sessions;

/// One command this channel advertises and runs itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AcpCommand {
    pub name: &'static str,
    pub desc: &'static str,
}

/// What the ACP channel offers. Identical to the set that used to come from
/// the old front end's table, name for name and word for word — see
/// `the_advertised_commands_are_the_ones_acp_can_actually_run`.
const ACP_COMMANDS: &[AcpCommand] = &[
    AcpCommand {
        name: "model",
        desc: "Switch provider / model",
    },
    AcpCommand {
        name: "status",
        desc: "Show session status",
    },
    AcpCommand {
        name: "config",
        desc: "Show config path",
    },
    AcpCommand {
        name: "diff",
        desc: "Show git diff",
    },
    AcpCommand {
        name: "usage",
        desc: "Show CodingPlan usage (tabs: current / overview / models)",
    },
    AcpCommand {
        name: "cost",
        desc: "Show this session's token usage (any model)",
    },
    AcpCommand {
        name: "context",
        desc: "Show context budget breakdown",
    },
    AcpCommand {
        name: "compact",
        desc: "Compact conversation history",
    },
    AcpCommand {
        name: "undo",
        desc: "Undo a turn (memory rollback): /undo or /undo N",
    },
    AcpCommand {
        name: "plan",
        desc: "Switch to Plan mode (read-only exploration)",
    },
    AcpCommand {
        name: "build",
        desc: "Switch to Build mode (full execution)",
    },
    AcpCommand {
        name: "auto",
        desc: "Switch to Auto mode (auto-approve all tools)",
    },
    AcpCommand {
        name: "effort",
        desc: "Model reasoning effort control (low / medium / high / xhigh / max / default)",
    },
    AcpCommand {
        name: "help",
        desc: "Show this help",
    },
    AcpCommand {
        name: "todo",
        desc: "Show the todo list; /todo add <task> appends one, /todo clear wipes it",
    },
];

/// The commands advertised on the ACP channel, mapped from the one command
/// table (`acp: true`, not hidden) and sorted by name.
/// One name, one sentence, and how its argument is written — what both wire
/// versions advertise, whoever it came from.
struct Advert {
    name: String,
    about: String,
    hint: Option<String>,
}

const LOCAL_HINTS: &[(&str, &str)] = &[
    ("undo", "N (optional; default 1)"),
    ("effort", "high | max | off"),
    ("model", "<model id>"),
];

/// Everything this session offers: the commands this channel runs itself, and
/// the ones the agent registered.
///
/// Two sources because there are two kinds, and neither can stand for the
/// other. `model`, `undo` and the rest are answered here against session state
/// the agent knows nothing about; `goal`, `review`, `worktree` and whatever a
/// row mounted are the agent's, and this channel only knows them because the
/// description says so. A name in both is the local one — that is what runs
/// when this channel sees it, so advertising the agent's sentence for it would
/// describe something the client will never get.
fn adverts(catalog: &[atomcode_kernel::agent::CommandDescription]) -> Vec<Advert> {
    let mut out: Vec<Advert> = ACP_COMMANDS
        .iter()
        .map(|c| Advert {
            name: c.name.to_string(),
            about: c.desc.to_string(),
            hint: LOCAL_HINTS
                .iter()
                .find(|(name, _)| *name == c.name)
                .map(|(_, hint)| (*hint).to_string()),
        })
        .collect();
    for command in catalog {
        if out.iter().any(|a| a.name == command.name) {
            continue;
        }
        out.push(Advert {
            name: command.name.clone(),
            about: command.summary.clone(),
            hint: command.usage.clone(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// What `sid`'s agent offers right now, or nothing when there is no such
/// session (which the caller is about to find out anyway).
pub async fn catalog_of(
    sessions: &Sessions,
    sid: &str,
) -> Vec<atomcode_kernel::agent::CommandDescription> {
    let held = {
        let map = sessions.lock().await;
        match map.get(sid) {
            Some(state) => state.catalog.clone(),
            None => return Vec::new(),
        }
    };
    let catalog = held.lock().await;
    catalog.clone()
}

pub fn available_acp_commands(
    catalog: &[atomcode_kernel::agent::CommandDescription],
) -> Vec<AvailableCommand> {
    adverts(catalog)
        .into_iter()
        .map(|a| {
            let mut advert = AvailableCommand::new(a.name, a.about);
            if let Some(hint) = a.hint {
                advert = advert.input(AvailableCommandInput::Unstructured(
                    UnstructuredCommandInput::new(hint),
                ));
            }
            advert
        })
        .collect()
}

/// The same ACP command subset as [`available_acp_commands`], but shaped for the
/// v2 wire (`Text` command input instead of v1's `Unstructured`). Sourced from
/// the single built-in command table so the v1 and v2 ads never diverge.
pub fn available_acp_commands_v2(
    catalog: &[atomcode_kernel::agent::CommandDescription],
) -> Vec<agent_client_protocol::schema::v2::AvailableCommand> {
    use agent_client_protocol::schema::v2::{
        AvailableCommand, AvailableCommandInput, TextCommandInput,
    };
    adverts(catalog)
        .into_iter()
        .map(|a| {
            let mut advert = AvailableCommand::new(a.name, a.about);
            if let Some(hint) = a.hint {
                advert = advert.input(AvailableCommandInput::Text(TextCommandInput::new(hint)));
            }
            advert
        })
        .collect()
}

/// Parse `/cmd arg` from a prompt. Returns `(canonical name, argument)` when
/// the input starts with `/` and names a known ACP command; `None` for
/// everything else (including unknown `/…` inputs), so the caller keeps the
/// kernel turn.
pub fn parse_slash_command(text: &str) -> Option<(&'static str, &str)> {
    let rest = text.trim_start();
    if !rest.starts_with('/') {
        return None;
    }
    let rest = &rest[1..];
    let (name, arg) = match rest.find(char::is_whitespace) {
        Some(i) => (&rest[..i], rest[i..].trim()),
        None => (rest, ""),
    };
    // Case-insensitive, as the table it came from was: a client that sends
    // `/Status` means `/status`, and the criterion below has pinned that since
    // before the table moved here.
    ACP_COMMANDS
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(name))
        .map(|c| (c.name, arg))
}

/// Parse `/cmd arg` against the **agent's** catalog — the commands this channel
/// does not run itself.
///
/// Tried after [`parse_slash_command`], so a name this channel implements wins:
/// that is the one that runs when it sees it, and advertising said so. `None`
/// for everything else, including an unknown `/…`, which keeps its meaning: the
/// prompt reaches the model as typed.
pub fn parse_catalog_command<'a>(
    text: &'a str,
    catalog: &[atomcode_kernel::agent::CommandDescription],
) -> Option<(String, &'a str)> {
    let rest = text.trim_start().strip_prefix('/')?;
    let (name, arg) = match rest.find(char::is_whitespace) {
        Some(i) => (&rest[..i], rest[i..].trim()),
        None => (rest, ""),
    };
    if ACP_COMMANDS
        .iter()
        .any(|c| c.name.eq_ignore_ascii_case(name))
    {
        return None;
    }
    catalog
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(name))
        .map(|c| (c.name.clone(), arg))
}

/// Run a parsed slash command against the session. Returns `Some(text)` when
/// the command was handled locally (the caller replies with the text and ends
/// the turn); `None` for commands this layer does not implement (the caller
/// keeps the kernel turn).
pub async fn execute_slash_command(
    cmd: &str,
    arg: &str,
    sessions: &Sessions,
    cx: &ConnectionTo<Client>,
    sid: &SessionId,
) -> Option<String> {
    let out = match cmd {
        "status" => status_text(sessions, sid).await,
        "usage" | "cost" => usage_text(sessions, sid).await,
        "context" => context_text(sessions, sid).await,
        "todo" => todo_text(sessions, sid).await,
        "undo" => undo_text(sessions, sid, arg).await,
        "compact" => compact_text(sessions, sid).await,
        "diff" => diff_text(sessions, sid, arg).await,
        "model" | "effort" | "build" | "auto" | "plan" => {
            set_config_text(cmd, arg, sessions, cx, sid).await
        }
        "help" => Some(help_text()),
        "config" => Some(config_text()),
        _ => return None,
    };
    Some(out.unwrap_or_else(|| "acp: session is no longer available".to_string()))
}

/// The select's current value id from the session catalog, if present.
fn select_current(catalog: &[SessionConfigOption], id: &str) -> Option<String> {
    catalog
        .iter()
        .find(|o| o.id.0.as_ref() == id)
        .and_then(|o| match &o.kind {
            SessionConfigKind::Select(select) => Some(select.current_value.0.as_ref().to_string()),
            _ => None,
        })
}

/// One line: used/window tokens + utilization + model, from the host.
async fn context_text(sessions: &Sessions, sid: &SessionId) -> Option<String> {
    let (control, session) = {
        let map = sessions.lock().await;
        let state = map.get(sid.0.as_ref())?;
        (state.control.clone(), state.native_id.clone())
    };
    let HostReply::Context {
        window,
        used,
        model,
        ..
    } = control.call(HostCommand::Context { session }).await.ok()?
    else {
        return None;
    };
    Some(format!(
        "context: {used}/{window} tokens ({:.1}%)\nmodel: {model}",
        percent(used, window),
    ))
}

/// How full a window is, without dividing by a window nobody knows.
fn percent(used: u32, window: u32) -> f32 {
    if window == 0 {
        0.0
    } else {
        used as f32 / window as f32 * 100.0
    }
}

async fn status_text(sessions: &Sessions, sid: &SessionId) -> Option<String> {
    let (cwd, mode, effort, model, usage, control, session) = {
        let map = sessions.lock().await;
        let state = map.get(sid.0.as_ref())?;
        (
            state.cwd.display().to_string(),
            state.current_mode.label().to_string(),
            select_current(&state.config_options, REASONING_EFFORT_CONFIG_ID)
                .unwrap_or_else(|| "off (API default)".to_string()),
            select_current(&state.config_options, MODEL_CONFIG_ID)
                .unwrap_or_else(|| "(default)".to_string()),
            state.usage,
            state.control.clone(),
            state.native_id.clone(),
        )
    };
    let ctx_note = match control.call(HostCommand::Context { session }).await {
        Ok(HostReply::Context { window, used, .. }) => format!(
            "\ncontext: {used}/{window} tokens ({:.1}%)",
            percent(used, window)
        ),
        _ => String::new(),
    };
    Some(format!(
        "mode: {mode}\nmodel: {model}\ncwd: {cwd}\nreasoning effort: {effort}\nusage: {} prompt + {} completion tokens{ctx_note}",
        usage.0, usage.1,
    ))
}

async fn usage_text(sessions: &Sessions, sid: &SessionId) -> Option<String> {
    let usage = {
        let map = sessions.lock().await;
        map.get(sid.0.as_ref())?.usage
    };
    Some(format!(
        "prompt tokens: {}\ncompletion tokens: {}\ntotal: {}\n(cost requires a pricing table; see /usage)",
        usage.0,
        usage.1,
        usage.0 + usage.1,
    ))
}

async fn todo_text(sessions: &Sessions, sid: &SessionId) -> Option<String> {
    use atomcode_capabilities::tools::todo::{reduce_todos, render_todos_text};
    let todos = {
        let map = sessions.lock().await;
        let state = map.get(sid.0.as_ref())?;
        reduce_todos(
            state
                .todo_calls
                .iter()
                .map(|(n, a)| (n.as_str(), a.as_str())),
        )
    };
    if todos.is_empty() {
        Some("no plan yet — ask the agent to outline steps with the todo tool.".to_string())
    } else {
        Some(render_todos_text(&todos, false))
    }
}

/// `/undo` and `/undo N`.
///
/// The contract names a turn; this command counts backwards. So it asks for the
/// turns first and picks the Nth newest — the same two steps the host would
/// have to do anyway, done where the "Nth" lives. `/undo` with nothing after it
/// is the newest, which the contract expresses as no turn at all.
///
/// `based_on` is `0`: this channel keeps no log of its own, so it has no
/// position to claim. A host whose front end never kept one has nothing for the
/// caller to be stale against and says so (`RuntimeControl::fresh`).
async fn undo_text(sessions: &Sessions, sid: &SessionId, arg: &str) -> Option<String> {
    let (control, session) = {
        let map = sessions.lock().await;
        let state = map.get(sid.0.as_ref())?;
        (state.control.clone(), state.native_id.clone())
    };
    let turn = if arg.is_empty() {
        None
    } else {
        let nth = arg.trim().parse::<usize>().ok()?;
        if nth == 0 {
            return Some("undo: N must be 1 or more".to_string());
        }
        let points = match control
            .call(HostCommand::RewindPoints {
                session: session.clone(),
            })
            .await
        {
            Ok(HostReply::RewindPoints { points, .. }) => points,
            other => return Some(format!("undo failed: {other:?}")),
        };
        match points.get(nth - 1) {
            Some(point) => Some(point.turn),
            None => return Some(format!("undo: only {} turn(s) to go back to", points.len())),
        }
    };
    match control
        .call(HostCommand::Undo {
            session,
            turn,
            based_on: 0,
        })
        .await
    {
        Ok(HostReply::Undone { prompt, .. }) => Some(match prompt {
            Some(text) => format!("undo: back to before `{}`", first_line_capped(&text, 60)),
            None => "undo: back one turn".to_string(),
        }),
        other => Some(format!("undo failed: {other:?}")),
    }
}

/// A prompt's first line, short enough for one line of output.
fn first_line_capped(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() <= max {
        return line.to_string();
    }
    line.chars().take(max).collect::<String>() + "…"
}

async fn compact_text(sessions: &Sessions, sid: &SessionId) -> Option<String> {
    let (commands, cwd) = {
        let map = sessions.lock().await;
        let state = map.get(sid.0.as_ref())?;
        (state.commands.clone(), state.cwd.clone())
    };
    // The kernel compacts the session snapshot; focus is provider-specific and
    // rarely used — default to the whole conversation.
    match commands.send(AgentCommand::Compact { focus: None }) {
        Ok(()) => Some(format!(
            "compact requested for {}; the next request continues on the compacted context",
            cwd.display()
        )),
        Err(e) => Some(format!("compact failed: {e}")),
    }
}

async fn diff_text(sessions: &Sessions, sid: &SessionId, arg: &str) -> Option<String> {
    let cwd = {
        let map = sessions.lock().await;
        map.get(sid.0.as_ref())?.cwd.clone()
    };
    let mut cmd = std::process::Command::new("git");
    cmd.arg("diff").arg("--stat").current_dir(&cwd);
    if !arg.is_empty() {
        cmd.arg("--").arg(arg.trim());
    }
    let raw = cmd
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(o.stdout.as_slice()).into_owned())
        .unwrap_or_default();
    let trimmed: String = raw.chars().take(2000).collect();
    if trimmed.trim().is_empty() {
        Some("git diff: no changes".to_string())
    } else {
        Some(trimmed)
    }
}

async fn set_config_text(
    cmd: &str,
    arg: &str,
    sessions: &Sessions,
    cx: &ConnectionTo<Client>,
    sid: &SessionId,
) -> Option<String> {
    let (config_id, value) = match cmd {
        "model" => (MODEL_CONFIG_ID, arg),
        "effort" => (REASONING_EFFORT_CONFIG_ID, arg),
        // Mode switches ride the same config-option path as the `mode` option
        // in the session catalog, so `current_mode_update` + catalog state
        // stay consistent with `session/set_config_option`.
        "build" => (MODE_CONFIG_ID, RuntimeMode::Build.wire()),
        "auto" => (MODE_CONFIG_ID, RuntimeMode::Auto.wire()),
        _ => (MODE_CONFIG_ID, RuntimeMode::Plan.wire()),
    };
    let req = SetSessionConfigOptionRequest::new(
        sid.clone(),
        config_id,
        SessionConfigOptionValue::value_id(value.to_string()),
    );
    match handle_set_session_config_option(sessions, cx, &req).await {
        Ok(_) => Some(format!("/{cmd}: {value} applied")),
        Err(e) => Some(format!("/{cmd}: {e}")),
    }
}

/// Map the session's derived todo list to the ACP v1 `plan` update.
///
/// Clients replace the whole plan on every update, so this carries the full
/// list with current statuses. Todos have no priority concept — every entry is
/// reported as `low` (a stable `PlanEntryPriority` is required on the wire).
pub fn plan_update_from_todos(todos: &[TodoItem]) -> SessionUpdate {
    use agent_client_protocol::schema::v1::{Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus};
    let entries = todos
        .iter()
        .map(|todo| {
            let status = match todo.status {
                TodoStatus::Completed => PlanEntryStatus::Completed,
                TodoStatus::InProgress => PlanEntryStatus::InProgress,
                TodoStatus::Pending => PlanEntryStatus::Pending,
            };
            PlanEntry::new(todo.content.clone(), PlanEntryPriority::Low, status)
        })
        .collect();
    SessionUpdate::Plan(Plan::new(entries))
}

/// Map the session's derived todo list to the ACP v2 `plan_update`.
///
/// v2 plans are identified by a stable `planId` and every update carries the
/// full item list (clients replace the plan by id). Todos have no priority
/// concept, so each entry is `low` — the same stance as the v1 `plan` mapping.
/// `plan_id` must be stable for the session's lifetime (one todo list = one plan).
pub fn plan_update_from_todos_v2(
    todos: &[TodoItem],
    plan_id: &str,
) -> agent_client_protocol::schema::v2::SessionUpdate {
    use agent_client_protocol::schema::v2::{
        PlanEntry, PlanEntryPriority, PlanEntryStatus, PlanUpdate, PlanUpdateContent,
    };
    let entries = todos
        .iter()
        .map(|todo| {
            let status = match todo.status {
                TodoStatus::Completed => PlanEntryStatus::Completed,
                TodoStatus::InProgress => PlanEntryStatus::InProgress,
                TodoStatus::Pending => PlanEntryStatus::Pending,
            };
            PlanEntry::new(todo.content.clone(), PlanEntryPriority::Low, status)
        })
        .collect();
    agent_client_protocol::schema::v2::SessionUpdate::PlanUpdate(PlanUpdate::new(
        PlanUpdateContent::items(plan_id, entries),
    ))
}

/// `/help` output — the ACP-usable subset of the single command table.
fn help_text() -> String {
    let mut out = String::from("available commands:\n");
    for c in ACP_COMMANDS {
        out.push_str(&format!("  /{} - {}\n", c.name, c.desc));
    }
    out
}

/// `/config` output — where the config file lives (no config handle on the
/// ACP session, so this mirrors the TUI's path report).
fn config_text() -> String {
    let home = std::env::var("ATOMCODE_HOME").unwrap_or_else(|_| "~/.atomcode".to_string());
    format!("config path: {home}/config.toml (set ATOMCODE_HOME to override)")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acp_catalog_is_sorted_and_filtered() {
        let catalog = available_acp_commands(&[]);
        let names: Vec<&str> = catalog.iter().map(|c| c.name.as_str()).collect();
        for expected in ["status", "plan", "todo", "help", "build", "effort"] {
            assert!(names.contains(&expected), "missing {expected}: {names:?}");
        }
        for forbidden in ["login", "quit", "think", "keys", "webui"] {
            assert!(
                !names.contains(&forbidden),
                "{forbidden} must stay off the ACP channel: {names:?}"
            );
        }
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "advertised commands sorted by name");
    }

    #[test]
    fn slash_parse_recognizes_acp_commands_only() {
        assert_eq!(parse_slash_command("/status"), Some(("status", "")));
        assert_eq!(parse_slash_command("/undo 3"), Some(("undo", "3")));
        assert_eq!(parse_slash_command("/Status"), Some(("status", "")));
        assert_eq!(
            parse_slash_command("/model qwen-max"),
            Some(("model", "qwen-max"))
        );
        // Unknown `/…` inputs keep the turn (no acp entry).
        assert_eq!(parse_slash_command("plain text"), None);
        assert_eq!(parse_slash_command("/nope"), None);
        // Known commands that are not ACP-enabled also fall through.
        assert_eq!(parse_slash_command("/login"), None);
        assert_eq!(parse_slash_command("/think"), None);
    }

    #[test]
    fn todos_map_to_plan_update() {
        use atomcode_capabilities::tools::todo::{TodoItem, TodoStatus};
        let todos = vec![
            TodoItem {
                content: "a".into(),
                status: TodoStatus::Pending,
            },
            TodoItem {
                content: "b".into(),
                status: TodoStatus::InProgress,
            },
            TodoItem {
                content: "c".into(),
                status: TodoStatus::Completed,
            },
        ];
        let update = plan_update_from_todos(&todos);
        let json = serde_json::to_value(&update).unwrap();
        assert_eq!(json["sessionUpdate"], "plan");
        let entries = json["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0]["content"], "a");
        assert_eq!(entries[0]["status"], "pending");
        assert_eq!(entries[1]["status"], "in_progress");
        assert_eq!(entries[2]["status"], "completed");
        assert_eq!(entries[0]["priority"], "low");
    }

    /// `/undo N` counts backwards; the contract names a turn. The translation
    /// happens here, where "the Nth one back" is said.
    ///
    /// The ordering is the whole risk: the host lists rewind points **newest
    /// first**, so `/undo 1` is the first entry and `/undo 2` the second. Read
    /// the list the other way round and every undo goes to the wrong turn while
    /// still looking like it worked.
    #[tokio::test]
    async fn undo_counts_backwards_and_the_contract_names_the_turn() {
        use crate::acp::sessions::{RecordingHost, SessionState};
        let host = std::sync::Arc::new(RecordingHost::default());
        let point = |turn: u64, prompt: &str| atomcode_host_api::RewindPoint {
            turn,
            prompt: prompt.to_string(),
            files: 0,
            code: false,
        };
        host.replies.lock().unwrap().extend([
            atomcode_host_api::HostReply::RewindPoints {
                // Newest first, as the host lists them.
                points: vec![point(9, "第三句"), point(7, "第二句"), point(5, "第一句")],
                code_unavailable: None,
            },
            atomcode_host_api::HostReply::Undone {
                prompt: Some("第三句".into()),
                restored_files: Vec::new(),
            },
        ]);
        let (commands, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_ev_tx, events) = tokio::sync::mpsc::unbounded_channel();
        let state = SessionState {
            commands,
            control: host.clone(),
            events: std::sync::Arc::new(tokio::sync::Mutex::new(events)),
            _front_end: atomcode_coding::front_end::FrontEnd::new(),
            persistence_failure: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            native_id: "n1".into(),
            cwd: std::path::PathBuf::from("/work"),
            current_mode: RuntimeMode::Build,
            config_options: Vec::new(),
            usage: (0, 0),
            todo_calls: Vec::new(),
            title: None,
            additional_directories: Vec::new(),
            catalog: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
        };
        let sessions: crate::acp::sessions::Sessions =
            std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::from([
                ("acp-n1".to_string(), state),
            ])));

        // `1` on purpose: with an odd list, "the 2nd from the front" and "the
        // 2nd from the back" are the same entry, so a criterion written that way
        // passes whichever way the list is read. The first entry is the one
        // that tells them apart — newest is 9, oldest is 5.
        let said = undo_text(&sessions, &SessionId::new("acp-n1"), "1")
            .await
            .expect("a live session answers");
        assert!(said.contains("第三句"), "{said}");
        let asked = host.asked.lock().unwrap();
        assert!(matches!(
            asked.first(),
            Some(atomcode_host_api::HostCommand::RewindPoints { .. })
        ));
        assert_eq!(
            asked.get(1),
            Some(&atomcode_host_api::HostCommand::Undo {
                session: "n1".into(),
                // The NEWEST. Read the list the other way and this is 5.
                turn: Some(9),
                based_on: 0,
            })
        );
    }

    /// `/undo` with nothing after it is the newest turn, which the contract
    /// says by naming no turn at all — so it does not ask for the list.
    #[tokio::test]
    async fn undo_with_no_argument_names_no_turn() {
        use crate::acp::sessions::{RecordingHost, SessionState};
        let host = std::sync::Arc::new(RecordingHost::default());
        host.replies
            .lock()
            .unwrap()
            .push_back(atomcode_host_api::HostReply::Undone {
                prompt: None,
                restored_files: Vec::new(),
            });
        let (commands, _cmd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (_ev_tx, events) = tokio::sync::mpsc::unbounded_channel();
        let state = SessionState {
            commands,
            control: host.clone(),
            events: std::sync::Arc::new(tokio::sync::Mutex::new(events)),
            _front_end: atomcode_coding::front_end::FrontEnd::new(),
            persistence_failure: std::sync::Arc::new(tokio::sync::Mutex::new(None)),
            native_id: "n1".into(),
            cwd: std::path::PathBuf::from("/work"),
            current_mode: RuntimeMode::Build,
            config_options: Vec::new(),
            usage: (0, 0),
            todo_calls: Vec::new(),
            title: None,
            additional_directories: Vec::new(),
            catalog: std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new())),
        };
        let sessions: crate::acp::sessions::Sessions =
            std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::from([
                ("acp-n1".to_string(), state),
            ])));

        undo_text(&sessions, &SessionId::new("acp-n1"), "")
            .await
            .expect("a live session answers");
        assert_eq!(
            *host.asked.lock().unwrap(),
            vec![atomcode_host_api::HostCommand::Undo {
                session: "n1".into(),
                turn: None,
                based_on: 0,
            }],
            "no list is asked for when no Nth is named"
        );
    }

    /// What this channel advertises is what it can actually run.
    ///
    /// The table moved here from the old front end's `acp: true` column
    /// (plan M6.3). Two things had to survive the move and neither was checked
    /// before: the wire-visible set must not shift under a client, and an
    /// advertisement must correspond to a handler — the old arrangement could
    /// A name this channel runs itself wins over the agent's, both ways.
    ///
    /// Advertising the agent's sentence for a command this file intercepts
    /// would describe something the client will never get; handing such a name
    /// to the agent would run something the client did not ask for. One rule,
    /// checked on both sides of it.
    #[test]
    fn a_name_this_channel_runs_is_this_channels_both_ways() {
        use atomcode_kernel::agent::{CommandDescription, CommandTarget};
        let catalog = vec![
            CommandDescription {
                name: "goal".into(),
                usage: Some("<要什么>".into()),
                summary: "让这个会话自己干到某个目标".into(),
                target: CommandTarget::Session,
            },
            CommandDescription {
                name: "status".into(),
                usage: None,
                summary: "agent 自己那份说明".into(),
                target: CommandTarget::Session,
            },
        ];

        let advertised = available_acp_commands(&catalog);
        let names: Vec<&str> = advertised.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"goal"), "{names:?}");
        assert_eq!(
            names.iter().filter(|n| **n == "status").count(),
            1,
            "one name, once: {names:?}"
        );
        assert!(names.windows(2).all(|w| w[0] <= w[1]), "sorted: {names:?}");
        assert_eq!(
            advertised
                .iter()
                .find(|c| c.name == "status")
                .map(|c| c.description.clone()),
            Some("Show session status".into()),
            "the local sentence stays for the local command"
        );
        assert_eq!(
            advertised
                .iter()
                .find(|c| c.name == "goal")
                .map(|c| c.description.clone()),
            Some("让这个会话自己干到某个目标".into()),
            "and the agent's comes across for the agent's"
        );
        assert_eq!(
            names,
            available_acp_commands_v2(&catalog)
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            "v1 and v2 advertise the same set"
        );

        assert_eq!(
            parse_catalog_command("/goal ship it", &catalog),
            Some(("goal".to_string(), "ship it"))
        );
        assert_eq!(
            parse_catalog_command("/Goal ship it", &catalog).map(|(n, _)| n),
            Some("goal".to_string()),
            "the same case-insensitivity the local table has"
        );
        assert_eq!(
            parse_catalog_command("/status", &catalog),
            None,
            "a name this channel runs is not handed to the agent"
        );
        assert_eq!(parse_catalog_command("/nobody-has-this", &catalog), None);
        assert_eq!(parse_catalog_command("just talking", &catalog), None);
    }

    /// flag a row `acp: true` in one crate while the arm that runs it lives in
    /// another, and nothing would have noticed.
    #[test]
    fn the_advertised_commands_are_the_ones_acp_can_actually_run() {
        // The set as it was advertised before the table moved, in wire order.
        let before = [
            "auto", "build", "compact", "config", "context", "cost", "diff", "effort", "help",
            "model", "plan", "status", "todo", "undo", "usage",
        ];
        let advertised = available_acp_commands(&[]);
        let now: Vec<&str> = advertised.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(now, before, "the advertised set moved crates, not contents");
        // v1 and v2 advertise the same commands; only the input shape differs.
        let v2: Vec<String> = available_acp_commands_v2(&[])
            .iter()
            .map(|c| c.name.clone())
            .collect();
        assert_eq!(v2, before, "v1 and v2 must advertise the same set");

        // And every one of them parses back to a name this file dispatches.
        // `execute_slash_command` needs a session, so this asserts the arm
        // exists by name rather than by running it — which is the half the
        // cross-crate arrangement could get wrong.
        let dispatched = [
            "status", "usage", "cost", "context", "todo", "undo", "compact", "diff", "model",
            "effort", "build", "auto", "plan", "help", "config",
        ];
        for name in before {
            let typed = format!("/{name} x");
            assert_eq!(
                parse_slash_command(&typed).map(|(n, _)| n),
                Some(name),
                "`/{name}` is advertised but does not parse"
            );
            assert!(
                dispatched.contains(&name),
                "`/{name}` is advertised with no arm to run it"
            );
        }
        // Something nobody advertises falls through to the model rather than
        // being swallowed.
        assert_eq!(parse_slash_command("/nope"), None);
    }
}
