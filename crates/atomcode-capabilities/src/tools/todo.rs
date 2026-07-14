//! `todowrite` — an AI-driven, full-list-replace task list for the current coding
//! session. STATELESS: the model sends the entire updated list every call; the tool
//! validates + echoes it. Current state is DERIVED from the transcript (last todowrite
//! call), so it persists with the session and survives /resume with zero extra storage.
//! Non-destructive ⇒ always `Safe`.

use super::{err, ok};
use async_trait::async_trait;
use atomcode_kernel::message::Message;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    fn parse(s: &str) -> Option<TodoStatus> {
        match s {
            "pending" => Some(TodoStatus::Pending),
            "in_progress" => Some(TodoStatus::InProgress),
            "completed" => Some(TodoStatus::Completed),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TodoItem {
    pub content: String,
    pub status: TodoStatus,
}

/// Terminal-safe status glyph. Unicode `[•]`/`[✓]` gated behind `unicode`; ASCII
/// `[~]`/`[x]` fallback (mirrors the spinner / hint-marker unicode gating).
pub fn todo_glyph(status: TodoStatus, unicode: bool) -> &'static str {
    match (status, unicode) {
        (TodoStatus::Pending, _) => "[ ]",
        (TodoStatus::InProgress, true) => "[\u{2022}]",
        (TodoStatus::InProgress, false) => "[~]",
        (TodoStatus::Completed, true) => "[\u{2713}]",
        (TodoStatus::Completed, false) => "[x]",
    }
}

/// One line per item: `<glyph> <content>`. Empty list → "(no tasks)".
pub fn render_todos_text(todos: &[TodoItem], unicode: bool) -> String {
    if todos.is_empty() {
        return "(no tasks)".to_string();
    }
    todos
        .iter()
        .map(|t| format!("{} {}", todo_glyph(t.status, unicode), t.content))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One line per item WITH its 1-based id: `<glyph> <id>. <content>`. This is the shape the
/// TUI title cache (`parse_todo_titles_into`) reads to learn `id → title`, so an incremental
/// `todo update id=N` row can render the task NAME, not just `#N`. Empty list → "(no tasks)".
pub fn render_todos_numbered(todos: &[TodoItem], unicode: bool) -> String {
    if todos.is_empty() {
        return "(no tasks)".to_string();
    }
    todos
        .iter()
        .enumerate()
        .map(|(i, t)| format!("{} {}. {}", todo_glyph(t.status, unicode), i + 1, t.content))
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Deserialize)]
struct RawItem {
    content: String,
    status: String,
}

#[derive(Deserialize)]
struct Args {
    todos: Vec<RawItem>,
}

/// Parse + validate the full-list args. Enforces: valid status enum, non-empty
/// content, at most one `in_progress`. Returns a human-readable reason on failure
/// (fed back to the model so it can correct and resend).
pub fn parse_todos(args: &str) -> Result<Vec<TodoItem>, String> {
    let a: Args = serde_json::from_str(args)
        .map_err(|e| format!("todowrite: invalid arguments: {e}. Expected {{\"todos\":[{{\"content\":\"…\",\"status\":\"pending|in_progress|completed\"}}]}}."))?;
    let mut out = Vec::with_capacity(a.todos.len());
    let mut in_progress = 0usize;
    for item in a.todos {
        if item.content.trim().is_empty() {
            return Err("todowrite: every task needs non-empty `content`.".to_string());
        }
        let status = TodoStatus::parse(&item.status)
            .ok_or_else(|| format!("todowrite: `status` must be one of pending|in_progress|completed (got `{}`).", item.status))?;
        if status == TodoStatus::InProgress {
            in_progress += 1;
        }
        out.push(TodoItem { content: item.content, status });
    }
    if in_progress > 1 {
        return Err("todowrite: keep exactly ONE task `in_progress` at a time.".to_string());
    }
    Ok(out)
}

/// `(completed, total)` for a todo list. The footer progress indicator renders
/// this as `N/M`. `total == 0` ⇒ caller omits the indicator (no todos yet).
pub fn todo_counts(todos: &[TodoItem]) -> (usize, usize) {
    let completed = todos
        .iter()
        .filter(|t| t.status == TodoStatus::Completed)
        .count();
    (completed, todos.len())
}

/// Apply ONE incremental `todo` action call's args to `list`. The item `id` is the
/// 1-based POSITION in the list (stable because the list is append-only + status-only:
/// `add` appends, `update` patches in place — nothing reorders or removes). Malformed /
/// unknown-id calls are IGNORED (the tool already returned an error to the model; the
/// derived state must stay consistent). `update` to `in_progress` first clears any OTHER
/// in_progress, so the "exactly one in_progress" invariant holds regardless of the model.
pub fn apply_todo_action(list: &mut Vec<TodoItem>, args: &str) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(args) else {
        return;
    };
    match v.get("action").and_then(|a| a.as_str()) {
        Some("add") => {
            if let Some(content) = v.get("content").and_then(|c| c.as_str()) {
                let content = content.trim();
                if !content.is_empty() {
                    list.push(TodoItem { content: content.to_string(), status: TodoStatus::Pending });
                }
            }
        }
        Some("update") => {
            let (Some(id), Some(status)) = (
                v.get("id").and_then(|x| x.as_u64()),
                v.get("status").and_then(|x| x.as_str()).and_then(TodoStatus::parse),
            ) else {
                return;
            };
            if id == 0 || (id as usize) > list.len() {
                return; // unknown id → ignore (1-based)
            }
            if status == TodoStatus::InProgress {
                for it in list.iter_mut() {
                    if it.status == TodoStatus::InProgress {
                        it.status = TodoStatus::Pending;
                    }
                }
            }
            list[(id - 1) as usize].status = status;
        }
        _ => {}
    }
}

/// Fold an ORDERED stream of `(tool_name, args)` todo-affecting calls into the current list.
/// Baseline = the LAST VALID `todowrite` (full-list replace; positions become the stable
/// 1-based ids); then every `todo` action call AFTER that baseline is applied in order. An
/// invalid `todowrite` never wipes an earlier valid list; `todo` events before the baseline
/// are void (a re-plan resets the ids). THE single source of truth for the fold — both the
/// kernel-message reducer below and the TUI's core-message panel derivation call this, so
/// live / replay / injected views never diverge.
pub fn reduce_todos<'a>(calls: impl IntoIterator<Item = (&'a str, &'a str)>) -> Vec<TodoItem> {
    let calls: Vec<(&str, &str)> = calls
        .into_iter()
        .filter(|(n, _)| *n == "todowrite" || *n == "todo")
        .collect();
    let baseline = calls
        .iter()
        .rposition(|(n, a)| *n == "todowrite" && parse_todos(a).is_ok());
    let (mut list, start) = match baseline {
        Some(i) => (parse_todos(calls[i].1).unwrap_or_default(), i + 1),
        None => (Vec::new(), 0),
    };
    for (n, a) in &calls[start..] {
        if *n == "todo" {
            apply_todo_action(&mut list, a);
        }
    }
    list
}

/// The current todo list, folded over the transcript (see [`reduce_todos`]). Returns `vec![]`
/// if there is no valid `todowrite` and no `todo` events.
pub fn derive_current_todos(messages: &[Message]) -> Vec<TodoItem> {
    reduce_todos(
        messages
            .iter()
            .flat_map(|m| m.tool_calls.iter())
            .map(|c| (c.name.as_str(), c.arguments.as_str())),
    )
}

/// Stateless full-list-replace todo tool. No interior state — current list is derived
/// from the transcript (see `derive_current_todos`).
#[derive(Clone, Default)]
pub struct TodoTool;

impl TodoTool {
    pub fn new() -> Self {
        Self
    }
}

const TODOWRITE_DESCRIPTION: &str = "Create and maintain a structured task list for the current coding session. \
Send the ENTIRE updated list every call (full replace). \
When to use: the work spans three or more DISTINCT steps (count real steps, not tool calls — several edits for one \
conceptual change is still one step), the user gives multiple tasks, or you are planning a non-trivial refactor. \
USE it for e.g. `add a --retry flag and cover it with a test`, or `rename resolve_path across the crate` (grep shows \
many hits). SKIP it for e.g. `add a doc comment to parse_args` (single edit), `what does this function do?` \
(informational), or `run cargo test and show the output` (one command). \
Each task is ONE specific, verifiable action — write `add error handling to load_config`, not `handle errors` or \
`make it work`; break large work into small steps, no filler. \
Rules: update statuses in real time; keep EXACTLY ONE task in_progress at a time; mark a task completed ONLY after the \
work is actually done (including any required verification), never on intent.";

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &str {
        "todowrite"
    }
    fn description(&self) -> &str {
        TODOWRITE_DESCRIPTION
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The full, updated task list (replaces the previous list).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string", "description": "Brief, actionable task description." },
                            "status": { "type": "string", "enum": ["pending", "in_progress", "completed"], "description": "Task status." }
                        },
                        "required": ["content", "status"]
                    }
                }
            },
            "required": ["todos"]
        })
    }
    // Never touches the filesystem → risk() defaults to Safe.
    fn always_grant_scope(&self, _args: &str) -> String {
        // Tool-wide: planning is harmless; one grant covers all calls this session.
        String::new()
    }
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        match parse_todos(args) {
            // Echo an ASCII-safe NUMBERED list as the tool result (goes into the transcript;
            // the renderer draws the pretty version separately). The `<glyph> <id>. <content>`
            // shape seeds the TUI title cache so later `todo update id=N` rows show task names.
            Ok(todos) => ok(render_todos_numbered(&todos, false)),
            Err(e) => err(e),
        }
    }
}

/// Description for the incremental [`TodoActionTool`].
const TODO_DESCRIPTION: &str = "Incrementally update the task list WITHOUT resending the whole list. \
Use this for EVERY status change after the initial plan. \
`{\"action\":\"update\",\"id\":N,\"status\":\"in_progress|completed|pending\"}` changes ONE task — `id` is its number in the list (as shown, e.g. `#3`). \
The MOMENT you START a task, set it `in_progress`; the MOMENT it is actually done (verified), set it `completed`. \
Exactly ONE task is `in_progress` at a time — this is enforced automatically, so you never need to clear the previous one. \
`{\"action\":\"add\",\"content\":\"…\"}` appends a new pending task. \
Prefer this over `todowrite` for updates: `todowrite` is only for the INITIAL plan or a full re-plan.";

/// Incremental, single-item task-list updates (`todo`). Sibling to [`TodoTool`] (`todowrite`,
/// full replace). Stateless: the reducer [`derive_current_todos`] folds these action calls over
/// the transcript. Non-destructive ⇒ always `Safe`.
#[derive(Clone, Default)]
pub struct TodoActionTool;

impl TodoActionTool {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Tool for TodoActionTool {
    fn name(&self) -> &str {
        "todo"
    }
    fn description(&self) -> &str {
        TODO_DESCRIPTION
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["add", "update"], "description": "`add` a new task, or `update` one task's status." },
                "content": { "type": "string", "description": "For `add`: the new task description." },
                "id": { "type": "integer", "description": "For `update`: the 1-based task number to change." },
                "status": { "type": "string", "enum": ["pending", "in_progress", "completed"], "description": "For `update`: the new status." }
            },
            "required": ["action"]
        })
    }
    // Never touches the filesystem → risk() defaults to Safe.
    fn always_grant_scope(&self, _args: &str) -> String {
        String::new()
    }
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        // Validate + echo a short confirmation. State is derived transcript-side by
        // `derive_current_todos` (which folds this call's args), so execute holds no state.
        let Ok(v) = serde_json::from_str::<serde_json::Value>(args) else {
            return err("todo: invalid JSON arguments.".to_string());
        };
        match v.get("action").and_then(|a| a.as_str()) {
            Some("add") => match v.get("content").and_then(|c| c.as_str()) {
                Some(c) if !c.trim().is_empty() => ok(format!("Added task: {}", c.trim())),
                _ => err("todo: `add` needs non-empty `content`.".to_string()),
            },
            Some("update") => {
                let id = v.get("id").and_then(|x| x.as_u64());
                let status = v.get("status").and_then(|x| x.as_str());
                match (id, status.and_then(TodoStatus::parse)) {
                    (Some(id), Some(_)) if id >= 1 => {
                        // `#<id> → <status>` is the base the TUI `enrich_todo_detail` splices a
                        // title into; keep this exact shape.
                        ok(format!("#{} \u{2192} {}", id, status.unwrap()))
                    }
                    (None, _) => err("todo: `update` needs an `id` (the task number).".to_string()),
                    (Some(_), _) => {
                        err("todo: `update` needs a `status` of pending|in_progress|completed.".to_string())
                    }
                }
            }
            _ => err("todo: `action` must be `add` or `update`.".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::message::Message;
    use atomcode_kernel::tool::ToolCall;
    use tokio_util::sync::CancellationToken;

    fn ctx() -> ToolContext {
        ToolContext {
            working_dir: std::path::PathBuf::from("."),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
        }
    }

    #[test]
    fn parse_valid_full_list() {
        let todos = parse_todos(r#"{"todos":[{"content":"a","status":"pending"},{"content":"b","status":"in_progress"}]}"#).unwrap();
        assert_eq!(todos.len(), 2);
        assert_eq!(todos[0].content, "a");
        assert_eq!(todos[1].status, TodoStatus::InProgress);
    }

    #[test]
    fn parse_rejects_bad_status() {
        let e = parse_todos(r#"{"todos":[{"content":"a","status":"done"}]}"#).unwrap_err();
        assert!(e.contains("pending"), "{e}");
    }

    #[test]
    fn parse_rejects_two_in_progress() {
        let e = parse_todos(r#"{"todos":[{"content":"a","status":"in_progress"},{"content":"b","status":"in_progress"}]}"#).unwrap_err();
        assert!(e.to_ascii_lowercase().contains("in_progress"), "{e}");
    }

    #[test]
    fn parse_rejects_empty_content() {
        let e = parse_todos(r#"{"todos":[{"content":"  ","status":"pending"}]}"#).unwrap_err();
        assert!(e.contains("content"), "{e}");
    }

    #[test]
    fn parse_empty_list_ok() {
        assert_eq!(parse_todos(r#"{"todos":[]}"#).unwrap().len(), 0);
    }

    #[test]
    fn glyph_unicode_vs_ascii() {
        assert_eq!(todo_glyph(TodoStatus::Pending, true), "[ ]");
        assert_eq!(todo_glyph(TodoStatus::InProgress, true), "[\u{2022}]"); // [•]
        assert_eq!(todo_glyph(TodoStatus::Completed, true), "[\u{2713}]");  // [✓]
        assert_eq!(todo_glyph(TodoStatus::Pending, false), "[ ]");
        assert_eq!(todo_glyph(TodoStatus::InProgress, false), "[~]");
        assert_eq!(todo_glyph(TodoStatus::Completed, false), "[x]");
    }

    #[test]
    fn render_text_ascii() {
        let todos = vec![
            TodoItem { content: "first".into(), status: TodoStatus::Completed },
            TodoItem { content: "second".into(), status: TodoStatus::InProgress },
        ];
        let s = render_todos_text(&todos, false);
        assert!(s.contains("[x] first"), "{s}");
        assert!(s.contains("[~] second"), "{s}");
    }

    #[test]
    fn derive_finds_last_todowrite() {
        let msgs = vec![
            Message::user("hi"),
            Message::assistant("", vec![ToolCall { id: "1".into(), name: "todowrite".into(),
                arguments: r#"{"todos":[{"content":"old","status":"pending"}]}"#.into() }]),
            Message::assistant("", vec![ToolCall { id: "2".into(), name: "todowrite".into(),
                arguments: r#"{"todos":[{"content":"new","status":"in_progress"}]}"#.into() }]),
        ];
        let todos = derive_current_todos(&msgs);
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].content, "new"); // LAST wins
        assert_eq!(todos[0].status, TodoStatus::InProgress);
    }

    #[test]
    fn counts_completed_over_total() {
        let todos = parse_todos(
            r#"{"todos":[
                {"content":"a","status":"completed"},
                {"content":"b","status":"completed"},
                {"content":"c","status":"in_progress"},
                {"content":"d","status":"pending"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(todo_counts(&todos), (2, 4));
        assert_eq!(todo_counts(&[]), (0, 0));
    }

    #[test]
    fn derive_none_when_no_todowrite() {
        let msgs = vec![Message::user("hi"), Message::assistant("hello", vec![])];
        assert!(derive_current_todos(&msgs).is_empty());
    }

    #[test]
    fn derive_skips_invalid_and_returns_last_valid() {
        // The LAST todowrite has two in_progress items (invalid); the earlier one
        // is valid. derive_current_todos must return the earlier valid list, not [].
        let msgs = vec![
            Message::assistant("", vec![ToolCall {
                id: "1".into(),
                name: "todowrite".into(),
                arguments: r#"{"todos":[{"content":"keep","status":"pending"}]}"#.into(),
            }]),
            Message::assistant("", vec![ToolCall {
                id: "2".into(),
                name: "todowrite".into(),
                // Invalid: two in_progress items.
                arguments: r#"{"todos":[{"content":"a","status":"in_progress"},{"content":"b","status":"in_progress"}]}"#.into(),
            }]),
        ];
        let todos = derive_current_todos(&msgs);
        assert_eq!(todos.len(), 1, "should skip invalid call and return last valid list");
        assert_eq!(todos[0].content, "keep");
        assert_eq!(todos[0].status, TodoStatus::Pending);
    }

    // ---- incremental `todo` action reducer ----------------------------------------------

    fn todo_call(id: &str, args: &str) -> Message {
        Message::assistant("", vec![ToolCall { id: id.into(), name: "todo".into(), arguments: args.into() }])
    }
    fn write_call(id: &str, args: &str) -> Message {
        Message::assistant("", vec![ToolCall { id: id.into(), name: "todowrite".into(), arguments: args.into() }])
    }
    const PLAN3: &str = r#"{"todos":[{"content":"a","status":"pending"},{"content":"b","status":"pending"},{"content":"c","status":"pending"}]}"#;

    #[test]
    fn reduce_add_appends_after_baseline() {
        let msgs = vec![write_call("1", PLAN3), todo_call("2", r#"{"action":"add","content":"d"}"#)];
        let todos = derive_current_todos(&msgs);
        assert_eq!(todos.len(), 4);
        assert_eq!(todos[3].content, "d");
        assert_eq!(todos[3].status, TodoStatus::Pending);
    }

    #[test]
    fn reduce_update_flips_only_that_id() {
        let msgs = vec![write_call("1", PLAN3), todo_call("2", r#"{"action":"update","id":2,"status":"completed"}"#)];
        let todos = derive_current_todos(&msgs);
        assert_eq!(todos[0].status, TodoStatus::Pending);
        assert_eq!(todos[1].status, TodoStatus::Completed); // #2 (1-based)
        assert_eq!(todos[2].status, TodoStatus::Pending);
    }

    #[test]
    fn reduce_in_progress_clears_previous_in_progress() {
        // Invariant enforced by the reducer: setting #3 in_progress clears #1.
        let msgs = vec![
            write_call("1", PLAN3),
            todo_call("2", r#"{"action":"update","id":1,"status":"in_progress"}"#),
            todo_call("3", r#"{"action":"update","id":3,"status":"in_progress"}"#),
        ];
        let todos = derive_current_todos(&msgs);
        assert_eq!(todos[0].status, TodoStatus::Pending, "#1 must revert to pending");
        assert_eq!(todos[2].status, TodoStatus::InProgress, "#3 is the only in_progress");
        assert_eq!(todos.iter().filter(|t| t.status == TodoStatus::InProgress).count(), 1);
    }

    #[test]
    fn reduce_unknown_id_is_ignored() {
        let msgs = vec![write_call("1", PLAN3), todo_call("2", r#"{"action":"update","id":9,"status":"completed"}"#)];
        let todos = derive_current_todos(&msgs);
        assert_eq!(todos.len(), 3);
        assert!(todos.iter().all(|t| t.status == TodoStatus::Pending), "unknown id must not change anything");
    }

    #[test]
    fn reduce_replan_resets_ids_and_voids_stale_updates() {
        // An update BEFORE a re-plan is void; the re-plan is the new baseline.
        let msgs = vec![
            write_call("1", PLAN3),
            todo_call("2", r#"{"action":"update","id":1,"status":"completed"}"#), // pre-replan → void
            write_call("3", r#"{"todos":[{"content":"x","status":"pending"},{"content":"y","status":"pending"}]}"#),
            todo_call("4", r#"{"action":"update","id":2,"status":"completed"}"#),
        ];
        let todos = derive_current_todos(&msgs);
        assert_eq!(todos.len(), 2, "list is the re-plan, not the old plan");
        assert_eq!(todos[0].content, "x");
        assert_eq!(todos[0].status, TodoStatus::Pending, "pre-replan update on old #1 is void");
        assert_eq!(todos[1].status, TodoStatus::Completed, "post-replan update on new #2 applies");
    }

    #[test]
    fn reduce_todo_only_from_empty() {
        // No todowrite at all — a session that plans purely with `todo add`.
        let msgs = vec![
            todo_call("1", r#"{"action":"add","content":"first"}"#),
            todo_call("2", r#"{"action":"add","content":"second"}"#),
            todo_call("3", r#"{"action":"update","id":1,"status":"in_progress"}"#),
        ];
        let todos = derive_current_todos(&msgs);
        assert_eq!(todos.len(), 2);
        assert_eq!(todos[0].content, "first");
        assert_eq!(todos[0].status, TodoStatus::InProgress);
        assert_eq!(todos[1].status, TodoStatus::Pending);
    }

    #[test]
    fn reduce_no_todo_events_matches_legacy_behavior() {
        // With zero `todo` events the reducer returns exactly the last todowrite list.
        let msgs = vec![write_call("1", PLAN3)];
        assert_eq!(derive_current_todos(&msgs).len(), 3);
    }

    #[tokio::test]
    async fn execute_echoes_normalized_list() {
        let t = TodoTool::new();
        let r = t.execute(r#"{"todos":[{"content":"task","status":"pending"}]}"#, &ctx()).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("task"), "{}", r.content);
    }

    #[tokio::test]
    async fn execute_rejects_invalid() {
        let t = TodoTool::new();
        let r = t.execute(r#"{"todos":[{"content":"a","status":"nope"}]}"#, &ctx()).await;
        assert!(r.is_error);
        assert!(r.content.contains("pending"), "{}", r.content);
    }

    #[test]
    fn tool_name_is_todowrite() {
        assert_eq!(TodoTool::new().name(), "todowrite");
    }

    #[test]
    fn todowrite_result_is_numbered_for_title_cache() {
        // The result must carry `<glyph> <id>. <content>` so the TUI title cache learns ids.
        let out = render_todos_numbered(&parse_todos(PLAN3).unwrap(), false);
        assert!(out.contains("] 1. a"), "{out}");
        assert!(out.contains("] 3. c"), "{out}");
    }

    #[tokio::test]
    async fn todo_action_tool_name_and_add() {
        let t = TodoActionTool::new();
        assert_eq!(t.name(), "todo");
        let r = t.execute(r#"{"action":"add","content":"write tests"}"#, &ctx()).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("write tests"), "{}", r.content);
    }

    #[tokio::test]
    async fn todo_action_update_result_is_enrich_base_shape() {
        // `#<id> → <status>` is the exact base the TUI enrich step splices a title into.
        let t = TodoActionTool::new();
        let r = t.execute(r#"{"action":"update","id":2,"status":"completed"}"#, &ctx()).await;
        assert!(!r.is_error, "{}", r.content);
        assert_eq!(r.content, "#2 \u{2192} completed");
    }

    #[tokio::test]
    async fn todo_action_rejects_bad_args() {
        let t = TodoActionTool::new();
        assert!(t.execute(r#"{"action":"add","content":"  "}"#, &ctx()).await.is_error, "empty add");
        assert!(t.execute(r#"{"action":"update","status":"completed"}"#, &ctx()).await.is_error, "no id");
        assert!(t.execute(r#"{"action":"update","id":2}"#, &ctx()).await.is_error, "no status");
        assert!(t.execute(r#"{"action":"update","id":2,"status":"nope"}"#, &ctx()).await.is_error, "bad status");
        assert!(t.execute(r#"{"action":"frobnicate"}"#, &ctx()).await.is_error, "bad action");
    }

    #[test]
    fn description_calibrates_when_to_use() {
        let t = TodoTool::new();
        let d = t.description();
        // Disambiguates steps from tool calls (the key precision fix).
        assert!(d.contains("not tool calls"), "must clarify steps ≠ tool calls: {d}");
        // Shows both use and skip examples, not just tells.
        assert!(d.contains("USE it for") && d.contains("SKIP it for"), "must give use/skip examples: {d}");
        // Sets an item-quality bar with a good/bad contrast.
        assert!(d.contains("specific, verifiable action"), "must set item quality bar: {d}");
    }
}
