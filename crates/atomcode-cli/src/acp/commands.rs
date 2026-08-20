//! ACP slash commands: `available_commands_update` advertisement and local
//! execution, sourced from the single built-in command table
//! (`atomcode_tuix::commands`). Mirrors qwen-code's channel-filtered registry:
//! commands are defined once with an `acp: true` flag, the ACP channel
//! advertises that subset and runs its handlers against session-local state,
//! and unknown `/…` inputs fall through to the model (supervised turn).
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
use atomcode_tuix::commands::CommandRegistry;

use crate::acp::options::{
    handle_set_session_config_option, MODEL_CONFIG_ID, MODE_CONFIG_ID, REASONING_EFFORT_CONFIG_ID,
};
use crate::acp::sessions::Sessions;
use crate::acp::SessionModelResolver;

/// The commands advertised on the ACP channel, mapped from the one command
/// table (`acp: true`, not hidden) and sorted by name.
pub fn available_acp_commands() -> Vec<AvailableCommand> {
    let hints = [
        ("undo", "N (optional; default 1)"),
        ("effort", "high | max | off"),
        ("model", "<model id>"),
    ];
    let mut out: Vec<AvailableCommand> = CommandRegistry::builtin()
        .acp_commands()
        .into_iter()
        .map(|c| {
            let mut advert = AvailableCommand::new(c.name, c.desc);
            if let Some((_, hint)) = hints.iter().find(|(name, _)| *name == c.name) {
                advert = advert.input(AvailableCommandInput::Unstructured(
                    UnstructuredCommandInput::new(*hint),
                ));
            }
            advert
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// The same ACP command subset as [`available_acp_commands`], but shaped for the
/// v2 wire (`Text` command input instead of v1's `Unstructured`). Sourced from
/// the single built-in command table so the v1 and v2 ads never diverge.
pub fn available_acp_commands_v2() -> Vec<agent_client_protocol::schema::v2::AvailableCommand> {
    use agent_client_protocol::schema::v2::{
        AvailableCommand, AvailableCommandInput, TextCommandInput,
    };
    let hints = [
        ("undo", "N (optional; default 1)"),
        ("effort", "high | max | off"),
        ("model", "<model id>"),
    ];
    let mut out: Vec<AvailableCommand> = CommandRegistry::builtin()
        .acp_commands()
        .into_iter()
        .map(|c| {
            let mut advert = AvailableCommand::new(c.name, c.desc);
            if let Some((_, hint)) = hints.iter().find(|(name, _)| *name == c.name) {
                advert = advert.input(AvailableCommandInput::Text(TextCommandInput::new(*hint)));
            }
            advert
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
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
    CommandRegistry::builtin()
        .find(name)
        .filter(|c| c.acp)
        .map(|c| (c.name, arg))
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
    model_resolver: Option<&SessionModelResolver>,
    effort_resolver: Option<&SessionModelResolver>,
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
            set_config_text(cmd, arg, sessions, cx, sid, model_resolver, effort_resolver).await
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

/// One line: used/window tokens + utilization + model, from `context_stats`.
async fn context_text(sessions: &Sessions, sid: &SessionId) -> Option<String> {
    let runtime = {
        let map = sessions.lock().await;
        map.get(sid.0.as_ref())?.runtime.clone()
    };
    let stats = runtime.context_stats().await.ok()?;
    Some(format!(
        "context: {}/{} tokens ({:.1}%)\nmodel: {}",
        stats.used_tokens,
        stats.context_window,
        stats.utilization * 100.0,
        stats.model,
    ))
}

async fn status_text(sessions: &Sessions, sid: &SessionId) -> Option<String> {
    let (cwd, mode, effort, model, usage, runtime) = {
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
            state.runtime.clone(),
        )
    };
    let ctx_note = runtime
        .context_stats()
        .await
        .ok()
        .map(|s| {
            format!(
                "\ncontext: {}/{} tokens ({:.1}%)",
                s.used_tokens,
                s.context_window,
                s.utilization * 100.0
            )
        })
        .unwrap_or_default();
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

async fn undo_text(sessions: &Sessions, sid: &SessionId, arg: &str) -> Option<String> {
    let runtime = {
        let map = sessions.lock().await;
        map.get(sid.0.as_ref())?.runtime.clone()
    };
    let nth = if arg.is_empty() {
        None
    } else {
        Some(arg.trim().parse::<usize>().ok()?)
    };
    match runtime.undo_to_prompt(nth).await {
        Ok(result) => Some(format!(
            "undo: reverted to prompt {} ({} prompt(s) before the current turn)",
            result.target_n, result.prompts_before
        )),
        Err(e) => Some(format!("undo failed: {e}")),
    }
}

async fn compact_text(sessions: &Sessions, sid: &SessionId) -> Option<String> {
    let (runtime, cwd) = {
        let map = sessions.lock().await;
        let state = map.get(sid.0.as_ref())?;
        (state.runtime.clone(), state.cwd.clone())
    };
    // The kernel compacts the session snapshot; focus is provider-specific and
    // rarely used — default to the whole conversation.
    match runtime.compact(None) {
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
    model_resolver: Option<&SessionModelResolver>,
    effort_resolver: Option<&SessionModelResolver>,
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
    match handle_set_session_config_option(sessions, cx, &req, model_resolver, effort_resolver)
        .await
    {
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
    for c in CommandRegistry::builtin().acp_commands() {
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
        let catalog = available_acp_commands();
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
}
