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

    /// Lenient status parse for incremental `update` calls. Long-context turns
    /// routinely emit near-miss variants (`done`, `in progress`, `InProgress`,
    /// `completed `, `已完成` …) that strict full-list validation rejects;
    /// rejecting them turns a valid update into the confusing
    /// "`update` needs a `status`" error (issue #1456). Trims whitespace and
    /// folds common synonyms onto the three canonical states. Full-list plans
    /// keep the strict [`Self::parse`] gate.
    fn parse_lenient(s: &str) -> Option<TodoStatus> {
        let normalized = s.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "pending" | "todo" | "open" | "waiting" | "not started" | "not_started" | "待办"
            | "未开始" => Some(TodoStatus::Pending),
            "in_progress" | "in-progress" | "in progress" | "inprogress" | "doing" | "started"
            | "active" | "working" | "running" | "进行中" | "开发中" => {
                Some(TodoStatus::InProgress)
            }
            "completed" | "complete" | "done" | "finished" | "closed" | "resolved" | "已完成"
            | "完成" => Some(TodoStatus::Completed),
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

/// Parse the persisted/public full-list shape. This intentionally keeps the
/// historical structural contract: older transcripts may contain vague labels
/// such as `task 1`, and replay must not reinterpret those once-valid calls as
/// missing state. New tool calls additionally pass [`validate_new_todo_plan`].
pub fn parse_todos(args: &str) -> Result<Vec<TodoItem>, String> {
    let mut value: serde_json::Value = serde_json::from_str(args)
        .map_err(|e| format!("todowrite: invalid arguments: {e}. Expected {{\"todos\":[{{\"content\":\"…\",\"status\":\"pending|in_progress|completed\"}}]}}."))?;
    // Keep transcript-derived state aligned with RepairToolArgsMiddleware. The
    // kernel stores the model's original tool call before middleware rewriting,
    // so a provider that emits `{"todos":"[...]"}` must be tolerated here too:
    // live execution, TodoHook, replay, and resume all converge on parse_todos.
    // Decode exactly one layer and only for `todos`, whose public schema is an
    // array; malformed, double-stringified, or wrong-container values still fail.
    if let Some(raw) = value.get("todos").and_then(serde_json::Value::as_str) {
        if let Ok(decoded) = serde_json::from_str::<serde_json::Value>(raw) {
            if decoded.is_array() {
                value["todos"] = decoded;
            }
        }
    }
    let a: Args = serde_json::from_value(value)
        .map_err(|e| format!("todowrite: invalid arguments: {e}. Expected {{\"todos\":[{{\"content\":\"…\",\"status\":\"pending|in_progress|completed\"}}]}}."))?;
    let mut out = Vec::with_capacity(a.todos.len());
    let mut in_progress = 0usize;
    for item in a.todos {
        let content = item.content.trim();
        if content.is_empty() {
            return Err("todowrite: every task needs non-empty `content`.".to_string());
        }
        let status = TodoStatus::parse(&item.status).ok_or_else(|| {
            format!(
                "todowrite: `status` must be one of pending|in_progress|completed (got `{}`).",
                item.status
            )
        })?;
        if status == TodoStatus::InProgress {
            in_progress += 1;
        }
        out.push(TodoItem {
            content: content.to_string(),
            status,
        });
    }
    if in_progress > 1 {
        return Err("todowrite: keep exactly ONE task `in_progress` at a time.".to_string());
    }
    Ok(out)
}

/// Strict quality gate for a newly executed full-list plan. Kept separate from
/// [`parse_todos`] so persisted sessions retain their original interpretation.
pub fn validate_new_todo_plan(args: &str) -> Result<Vec<TodoItem>, String> {
    let todos = parse_todos(args)?;
    for todo in &todos {
        validate_new_todo_content(&todo.content)?;
    }
    Ok(todos)
}

fn validate_new_todo_content(content: &str) -> Result<(), String> {
    if is_placeholder_task_label(content) {
        return Err(format!(
            "todowrite: `{content}` is a placeholder, not an executable task. Name a concrete outcome (for example `design session ownership boundaries` or `verify resume after provider failure`)."
        ));
    }
    Ok(())
}

fn is_placeholder_task_label(content: &str) -> bool {
    let normalized = content
        .trim()
        .trim_matches(|ch: char| ch.is_ascii_punctuation() || ch.is_whitespace())
        .to_ascii_lowercase();
    let compact = normalized
        .chars()
        .filter(|ch| !ch.is_whitespace() && !matches!(ch, '-' | '_' | '#' | '.'))
        .collect::<String>();
    let suffix_is_number = |prefix: &str| {
        compact
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.is_empty() || suffix.chars().all(|ch| ch.is_ascii_digit()))
    };
    [
        "task", "step", "phase", "item", "任务", "步骤", "阶段", "事项",
    ]
    .iter()
    .any(|prefix| suffix_is_number(prefix))
        || matches!(
            compact.as_str(),
            "处理功能" | "实现功能" | "开发功能" | "编写代码" | "修改代码"
        )
}

/// `(completed, in_progress, total)` for a todo list. The footer progress
/// indicator renders the counts in its header. `total == 0` ⇒ caller omits
/// the indicator (no todos yet). All three counts are derived from a single
/// pass so callers don't need separate `filter` scans.
pub fn todo_counts(todos: &[TodoItem]) -> (usize, usize, usize) {
    let mut completed = 0;
    let mut in_progress = 0;
    for t in todos {
        match t.status {
            TodoStatus::Completed => completed += 1,
            TodoStatus::InProgress => in_progress += 1,
            TodoStatus::Pending => {}
        }
    }
    (completed, in_progress, todos.len())
}

/// Apply ONE incremental `todo` action call's args to `list`. The item `id` is the
/// 1-based POSITION in the list (stable because the list is append-only + status-only:
/// `add` appends, `update` patches in place — nothing reorders or removes). Malformed /
/// unknown-id calls are IGNORED (the tool already returned an error to the model; the
/// derived state must stay consistent). `update` to `in_progress` first clears any OTHER
/// in_progress, so the "exactly one in_progress" invariant holds regardless of the model.
pub fn apply_todo_action(list: &mut Vec<TodoItem>, args: &str) {
    apply_todo_action_inner(list, args, false);
}

/// Apply a newly executed incremental action with the same quality gate used by
/// new full plans. Historical replay continues to use [`apply_todo_action`].
pub fn apply_new_todo_action(list: &mut Vec<TodoItem>, args: &str) {
    apply_todo_action_inner(list, args, true);
}

fn apply_todo_action_inner(list: &mut Vec<TodoItem>, args: &str, strict: bool) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(args) else {
        return;
    };
    match v.get("action").and_then(|a| a.as_str()) {
        Some("add") => {
            if let Some(content) = v.get("content").and_then(|c| c.as_str()) {
                let content = content.trim();
                if !content.is_empty() && (!strict || validate_new_todo_content(content).is_ok()) {
                    list.push(TodoItem {
                        content: content.to_string(),
                        status: TodoStatus::Pending,
                    });
                }
            }
        }
        Some("update") => {
            let (Some(id), Some(status)) = (
                v.get("id").and_then(|x| x.as_u64()),
                v.get("status")
                    .and_then(|x| x.as_str())
                    .and_then(TodoStatus::parse_lenient),
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

/// Whether a todo call's args are the FULL-LIST (re)plan shape (`{"todos":[…]}`) vs the
/// incremental `{"action":…}` shape. `todowrite` accepts BOTH — the two are distinguished by
/// shape, NOT by tool name, so the fold and the renderers agree with the merged tool.
pub fn is_todo_plan(args: &str) -> bool {
    parse_todos(args).is_ok()
}

/// Fold an ORDERED stream of `(tool_name, args)` todo-affecting calls into the current list.
/// Baseline = the LAST call carrying a valid full LIST (`{"todos":[…]}`; positions become the
/// stable 1-based ids); then every incremental `{"action":…}` call AFTER that baseline is
/// applied in order. Decided by ARG SHAPE, not tool name, so the merged `todowrite` (which
/// sends the list shape to plan and the action shape to patch) folds correctly — and a resumed
/// session's legacy `todo`-named calls fold identically. An invalid list never wipes an earlier
/// valid one; action events before the baseline are void (a re-plan resets the ids). THE single
/// source of truth for the fold — both the kernel-message reducer and the TUI panel derivation
/// use this shape rule, so live / replay / injected views never diverge.
pub fn reduce_todos<'a>(calls: impl IntoIterator<Item = (&'a str, &'a str)>) -> Vec<TodoItem> {
    // Keep both names so a resumed transcript (legacy `todo` + `todowrite`) folds the same.
    let calls: Vec<(&str, &str)> = calls
        .into_iter()
        .filter(|(n, _)| *n == "todowrite" || *n == "todo")
        .collect();
    let baseline = calls.iter().rposition(|(_, a)| is_todo_plan(a));
    let (mut list, start) = match baseline {
        Some(i) => (parse_todos(calls[i].1).unwrap_or_default(), i + 1),
        None => (Vec::new(), 0),
    };
    for (_, a) in &calls[start..] {
        apply_todo_action(&mut list, a); // no-op unless the args carry an `action`
    }
    list
}

/// The current todo list, folded over the transcript (see [`reduce_todos`]). Returns `vec![]`
/// if there is no valid `todowrite` and no `todo` events.
pub fn derive_current_todos(messages: &[Message]) -> Vec<TodoItem> {
    // A user cancel retires the active plan without deleting its history. Fold
    // only calls after the most recent authoritative interruption boundary; an
    // explicit later "continue" can create a fresh full-list plan from history.
    let active_messages = messages
        .iter()
        .rposition(Message::is_user_interruption)
        .map_or(messages, |index| &messages[index + 1..]);
    let failed_call_ids = active_messages
        .iter()
        .filter(|message| message.is_error)
        .filter_map(|message| message.tool_call_id.as_deref())
        .collect::<std::collections::HashSet<_>>();
    reduce_todos(
        active_messages
            .iter()
            .flat_map(|m| m.tool_calls.iter())
            .filter(|call| !failed_call_ids.contains(call.id.as_str()))
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
Call it in one of TWO ways:\n\
• PLAN / RE-PLAN — send the FULL list: `{\"todos\":[{\"content\":\"…\",\"status\":\"pending|in_progress|completed\"}]}` \
(REPLACES the previous list). Use when the work has multiple requests, phases, files, dependencies, ambiguity, or \
requires investigation followed by changes. Also use it for a non-trivial refactor even when the exact steps emerge \
during exploration. The initial list covers the complete work from investigation and architecture/module design \
through implementation and verification where relevant — not only the next action. SKIP only for a genuinely simple single edit, an informational question, or a one-command ask.\n\
• UPDATE ONE ITEM (preferred after the initial plan — do NOT resend the whole list): \
`{\"action\":\"update\",\"id\":N,\"status\":\"in_progress|completed|pending\"}` changes ONE task (`id` is its number in \
the list, e.g. `#3`); `{\"action\":\"add\",\"content\":\"…\"}` appends a new pending task. The MOMENT you START a task \
set it `in_progress`; the MOMENT it is actually done (verified) set it `completed`.\n\
Each task is ONE specific, verifiable action with a concrete outcome a later turn can execute without re-planning — write `add error handling to load_config`, not `handle errors`, `task 1`, or `处理功能`. Keep \
EXACTLY ONE task `in_progress` at a time (enforced automatically). Mark a task `completed` ONLY after the work is \
actually done, never on intent.";

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &str {
        "todowrite"
    }
    fn description(&self) -> &str {
        TODOWRITE_DESCRIPTION
    }
    fn parameters_schema(&self) -> serde_json::Value {
        // Flat union (NOT `oneOf`, which weaker models handle poorly): send `todos` to (re)plan,
        // OR `action`(+`id`/`status`/`content`) to change one item. `execute` picks by shape.
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "PLAN/RE-PLAN: the full task list — REPLACES the previous list.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": { "type": "string", "description": "Brief, actionable task description." },
                            "status": { "type": "string", "enum": ["pending", "in_progress", "completed"], "description": "Task status." }
                        },
                        "required": ["content", "status"]
                    }
                },
                "action": { "type": "string", "enum": ["add", "update"], "description": "UPDATE ONE ITEM (do NOT resend the whole list): `add` a task, or `update` one task's status." },
                "id": { "type": "integer", "description": "For action=update: the 1-based task number to change (as shown, e.g. #3)." },
                "status": { "type": "string", "enum": ["pending", "in_progress", "completed"], "description": "For action=update: the new status." },
                "content": { "type": "string", "description": "For action=add: the new task description." }
            }
        })
    }
    // Never touches the filesystem → risk() defaults to Safe.
    fn always_grant_scope(&self, _args: &str) -> String {
        // Tool-wide: planning is harmless; one grant covers all calls this session.
        String::new()
    }
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(args) else {
            return err("todowrite: invalid JSON arguments.".to_string());
        };
        // Full-list (re)plan shape. Echo an ASCII-safe NUMBERED list as the tool result (goes
        // into the transcript; the renderer draws the pretty version separately). The
        // `<glyph> <id>. <content>` shape seeds the TUI title cache so later `action=update
        // id=N` rows show task names.
        if v.get("todos").is_some() {
            return match validate_new_todo_plan(args) {
                Ok(todos) => ok(render_todos_numbered(&todos, false)),
                Err(e) => err(e),
            };
        }
        // Incremental single-item shape. State is derived transcript-side by
        // `derive_current_todos` (which folds this call's args), so execute holds no state.
        if let Some(action) = v.get("action").and_then(|a| a.as_str()) {
            return match action {
                "add" => match v.get("content").and_then(|c| c.as_str()) {
                    Some(c) if !c.trim().is_empty() => match validate_new_todo_content(c.trim()) {
                        Ok(()) => ok(format!("Added task: {}", c.trim())),
                        Err(error) => err(error),
                    },
                    _ => err("todowrite: `add` needs non-empty `content`.".to_string()),
                },
                "update" => {
                    let id = v.get("id").and_then(|x| x.as_u64());
                    let status = v.get("status").and_then(|x| x.as_str());
                    match (id, status.and_then(TodoStatus::parse_lenient)) {
                        // `#<id> → <status>` is the base the TUI `enrich_todo_detail` splices a
                        // title into; keep this exact shape.
                        (Some(id), Some(_)) if id >= 1 => ok(format!("#{} \u{2192} {}", id, status.unwrap())),
                        (None, _) => err("todowrite: `update` needs an `id` (the task number).".to_string()),
                        (Some(_), _) => {
                            err("todowrite: `update` needs a `status` of pending|in_progress|completed.".to_string())
                        }
                    }
                }
                _ => err("todowrite: `action` must be `add` or `update`.".to_string()),
            };
        }
        err("todowrite: provide either `todos` (full list to plan) or `action` (add|update one item).".to_string())
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
            requester: None,
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
    fn parse_accepts_one_stringified_todos_layer() {
        let todos =
            parse_todos(r#"{"todos":"[{\"content\":\"a\",\"status\":\"in_progress\"}]"}"#).unwrap();
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].content, "a");
        assert_eq!(todos[0].status, TodoStatus::InProgress);
    }

    #[test]
    fn parse_rejects_double_stringified_todos() {
        let error = parse_todos(r#"{"todos":"\"[]\""}"#).unwrap_err();
        assert!(error.contains("invalid arguments"), "{error}");
    }

    #[test]
    fn parse_rejects_bad_status() {
        let e = parse_todos(r#"{"todos":[{"content":"a","status":"done"}]}"#).unwrap_err();
        assert!(e.contains("pending"), "{e}");
    }

    #[test]
    fn parse_lenient_accepts_synonyms_case_and_whitespace() {
        // Issue #1456: long-context updates routinely carry near-miss status
        // variants; the incremental `update` path must not reject them.
        assert_eq!(
            TodoStatus::parse_lenient("done"),
            Some(TodoStatus::Completed)
        );
        assert_eq!(
            TodoStatus::parse_lenient("complete"),
            Some(TodoStatus::Completed)
        );
        assert_eq!(
            TodoStatus::parse_lenient("completed"),
            Some(TodoStatus::Completed)
        );
        assert_eq!(
            TodoStatus::parse_lenient(" finished "),
            Some(TodoStatus::Completed)
        );
        assert_eq!(
            TodoStatus::parse_lenient("in progress"),
            Some(TodoStatus::InProgress)
        );
        assert_eq!(
            TodoStatus::parse_lenient("in-progress"),
            Some(TodoStatus::InProgress)
        );
        assert_eq!(
            TodoStatus::parse_lenient("InProgress"),
            Some(TodoStatus::InProgress)
        );
        assert_eq!(
            TodoStatus::parse_lenient("doing"),
            Some(TodoStatus::InProgress)
        );
        assert_eq!(TodoStatus::parse_lenient("todo"), Some(TodoStatus::Pending));
        assert_eq!(
            TodoStatus::parse_lenient("waiting"),
            Some(TodoStatus::Pending)
        );
        assert_eq!(
            TodoStatus::parse_lenient("  pending  "),
            Some(TodoStatus::Pending)
        );
        // Chinese aliases.
        assert_eq!(
            TodoStatus::parse_lenient("已完成"),
            Some(TodoStatus::Completed)
        );
        assert_eq!(
            TodoStatus::parse_lenient("进行中"),
            Some(TodoStatus::InProgress)
        );
        // Garbage still rejected.
        assert_eq!(TodoStatus::parse_lenient("nope"), None);
    }

    #[tokio::test]
    async fn execute_update_accepts_lenient_status() {
        // Issue #1456: an update with `done` must not fail with
        // "`update` needs a `status` of pending|in_progress|completed".
        let t = TodoTool::new();
        let r = t
            .execute(r#"{"action":"update","id":3,"status":"done"}"#, &ctx())
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.starts_with("#3 \u{2192}"), "{}", r.content);
    }

    #[test]
    fn reduce_update_accepts_lenient_status() {
        let msgs = vec![
            write_call("1", PLAN3),
            todo_call("2", r#"{"action":"update","id":2,"status":"done"}"#),
        ];
        let todos = derive_current_todos(&msgs);
        assert_eq!(todos[1].status, TodoStatus::Completed); // #2 (1-based)
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
    fn new_plan_rejects_placeholder_task_labels() {
        for content in ["task 1", "Step #2", "阶段3", "处理功能"] {
            let args = serde_json::json!({
                "todos": [{"content": content, "status": "pending"}]
            })
            .to_string();
            let error = validate_new_todo_plan(&args).unwrap_err();
            assert!(error.contains("placeholder"), "{content}: {error}");
        }
    }

    #[test]
    fn persisted_plan_keeps_legacy_placeholder_labels() {
        let todos =
            parse_todos(r#"{"todos":[{"content":"task 1","status":"in_progress"}]}"#).unwrap();
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].content, "task 1");
    }

    #[test]
    fn parse_accepts_specific_numbered_tasks() {
        let todos = parse_todos(
            r#"{"todos":[{"content":"Step 1: map runtime owners","status":"in_progress"},{"content":"任务2：验证失败恢复","status":"pending"}]}"#,
        )
        .unwrap();
        assert_eq!(todos.len(), 2);
    }

    #[test]
    fn parse_empty_list_ok() {
        assert_eq!(parse_todos(r#"{"todos":[]}"#).unwrap().len(), 0);
    }

    #[test]
    fn glyph_unicode_vs_ascii() {
        assert_eq!(todo_glyph(TodoStatus::Pending, true), "[ ]");
        assert_eq!(todo_glyph(TodoStatus::InProgress, true), "[\u{2022}]"); // [•]
        assert_eq!(todo_glyph(TodoStatus::Completed, true), "[\u{2713}]"); // [✓]
        assert_eq!(todo_glyph(TodoStatus::Pending, false), "[ ]");
        assert_eq!(todo_glyph(TodoStatus::InProgress, false), "[~]");
        assert_eq!(todo_glyph(TodoStatus::Completed, false), "[x]");
    }

    #[test]
    fn render_text_ascii() {
        let todos = vec![
            TodoItem {
                content: "first".into(),
                status: TodoStatus::Completed,
            },
            TodoItem {
                content: "second".into(),
                status: TodoStatus::InProgress,
            },
        ];
        let s = render_todos_text(&todos, false);
        assert!(s.contains("[x] first"), "{s}");
        assert!(s.contains("[~] second"), "{s}");
    }

    #[test]
    fn derive_finds_last_todowrite() {
        let msgs = vec![
            Message::user("hi"),
            Message::assistant(
                "",
                vec![ToolCall {
                    id: "1".into(),
                    name: "todowrite".into(),
                    arguments: r#"{"todos":[{"content":"old","status":"pending"}]}"#.into(),
                }],
            ),
            Message::assistant(
                "",
                vec![ToolCall {
                    id: "2".into(),
                    name: "todowrite".into(),
                    arguments: r#"{"todos":[{"content":"new","status":"in_progress"}]}"#.into(),
                }],
            ),
        ];
        let todos = derive_current_todos(&msgs);
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].content, "new"); // LAST wins
        assert_eq!(todos[0].status, TodoStatus::InProgress);
    }

    #[test]
    fn derive_recovers_stringified_todowrite_from_transcript() {
        let msgs = vec![
            Message::assistant(
                "",
                vec![ToolCall {
                    id: "1".into(),
                    name: "todowrite".into(),
                    arguments:
                        r#"{"todos":"[{\"content\":\"persisted\",\"status\":\"pending\"}]"}"#.into(),
                }],
            ),
            todo_call("2", r#"{"action":"update","id":1,"status":"completed"}"#),
        ];
        let todos = derive_current_todos(&msgs);
        assert_eq!(todos.len(), 1);
        assert_eq!(todos[0].content, "persisted");
        assert_eq!(todos[0].status, TodoStatus::Completed);
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
        assert_eq!(todo_counts(&todos), (2, 1, 4));
        assert_eq!(todo_counts(&[]), (0, 0, 0));
    }

    #[test]
    fn derive_none_when_no_todowrite() {
        let msgs = vec![Message::user("hi"), Message::assistant("hello", vec![])];
        assert!(derive_current_todos(&msgs).is_empty());
    }

    #[test]
    fn user_interruption_retires_prior_plan_but_allows_a_new_plan() {
        let mut msgs = vec![todo_call(
            "old",
            r#"{"todos":[{"content":"old task","status":"in_progress"}]}"#,
        )];
        msgs.push(Message::user_interruption());
        msgs.push(Message::user("do something unrelated"));
        assert!(
            derive_current_todos(&msgs).is_empty(),
            "a cancelled plan remains historical, not active"
        );

        msgs.push(todo_call(
            "new",
            r#"{"todos":[{"content":"new task","status":"pending"}]}"#,
        ));
        let active = derive_current_todos(&msgs);
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].content, "new task");
    }

    #[test]
    fn legacy_interruption_marker_also_retires_prior_plan() {
        let marker = Message::synthetic_user(
            "[The previous response was interrupted by the user before completing. Reconsider.]",
        );
        let msgs = vec![
            todo_call(
                "old",
                r#"{"todos":[{"content":"old task","status":"pending"}]}"#,
            ),
            marker,
        ];
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
        assert_eq!(
            todos.len(),
            1,
            "should skip invalid call and return last valid list"
        );
        assert_eq!(todos[0].content, "keep");
        assert_eq!(todos[0].status, TodoStatus::Pending);
    }

    // ---- incremental `todo` action reducer ----------------------------------------------

    fn todo_call(id: &str, args: &str) -> Message {
        Message::assistant(
            "",
            vec![ToolCall {
                id: id.into(),
                name: "todo".into(),
                arguments: args.into(),
            }],
        )
    }
    fn write_call(id: &str, args: &str) -> Message {
        Message::assistant(
            "",
            vec![ToolCall {
                id: id.into(),
                name: "todowrite".into(),
                arguments: args.into(),
            }],
        )
    }
    const PLAN3: &str = r#"{"todos":[{"content":"a","status":"pending"},{"content":"b","status":"pending"},{"content":"c","status":"pending"}]}"#;

    #[test]
    fn reduce_add_appends_after_baseline() {
        let msgs = vec![
            write_call("1", PLAN3),
            todo_call("2", r#"{"action":"add","content":"d"}"#),
        ];
        let todos = derive_current_todos(&msgs);
        assert_eq!(todos.len(), 4);
        assert_eq!(todos[3].content, "d");
        assert_eq!(todos[3].status, TodoStatus::Pending);
    }

    #[test]
    fn strict_incremental_add_rejects_placeholder_but_replay_keeps_legacy_call() {
        let args = r#"{"action":"add","content":"task 4"}"#;
        let mut live = Vec::new();
        apply_new_todo_action(&mut live, args);
        assert!(live.is_empty(), "new invalid add must not reach live state");

        let mut replay = Vec::new();
        apply_todo_action(&mut replay, args);
        assert_eq!(replay.len(), 1, "persisted legacy add remains readable");
    }

    #[test]
    fn failed_current_call_does_not_reappear_in_transcript_state() {
        let messages = vec![
            write_call("1", PLAN3),
            todo_call("2", r#"{"action":"add","content":"task 4"}"#),
            Message::tool_result("2", "placeholder task rejected", true),
        ];
        let todos = derive_current_todos(&messages);
        assert_eq!(todos.len(), 3);
        assert!(todos.iter().all(|todo| todo.content != "task 4"));
    }

    #[test]
    fn reduce_update_flips_only_that_id() {
        let msgs = vec![
            write_call("1", PLAN3),
            todo_call("2", r#"{"action":"update","id":2,"status":"completed"}"#),
        ];
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
        assert_eq!(
            todos[0].status,
            TodoStatus::Pending,
            "#1 must revert to pending"
        );
        assert_eq!(
            todos[2].status,
            TodoStatus::InProgress,
            "#3 is the only in_progress"
        );
        assert_eq!(
            todos
                .iter()
                .filter(|t| t.status == TodoStatus::InProgress)
                .count(),
            1
        );
    }

    #[test]
    fn reduce_unknown_id_is_ignored() {
        let msgs = vec![
            write_call("1", PLAN3),
            todo_call("2", r#"{"action":"update","id":9,"status":"completed"}"#),
        ];
        let todos = derive_current_todos(&msgs);
        assert_eq!(todos.len(), 3);
        assert!(
            todos.iter().all(|t| t.status == TodoStatus::Pending),
            "unknown id must not change anything"
        );
    }

    #[test]
    fn reduce_replan_resets_ids_and_voids_stale_updates() {
        // An update BEFORE a re-plan is void; the re-plan is the new baseline.
        let msgs = vec![
            write_call("1", PLAN3),
            todo_call("2", r#"{"action":"update","id":1,"status":"completed"}"#), // pre-replan → void
            write_call(
                "3",
                r#"{"todos":[{"content":"x","status":"pending"},{"content":"y","status":"pending"}]}"#,
            ),
            todo_call("4", r#"{"action":"update","id":2,"status":"completed"}"#),
        ];
        let todos = derive_current_todos(&msgs);
        assert_eq!(todos.len(), 2, "list is the re-plan, not the old plan");
        assert_eq!(todos[0].content, "x");
        assert_eq!(
            todos[0].status,
            TodoStatus::Pending,
            "pre-replan update on old #1 is void"
        );
        assert_eq!(
            todos[1].status,
            TodoStatus::Completed,
            "post-replan update on new #2 applies"
        );
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
        let r = t
            .execute(
                r#"{"todos":[{"content":"verify scheduler registration","status":"pending"}]}"#,
                &ctx(),
            )
            .await;
        assert!(!r.is_error, "{}", r.content);
        assert!(
            r.content.contains("verify scheduler registration"),
            "{}",
            r.content
        );
    }

    #[tokio::test]
    async fn execute_accepts_stringified_todos_from_provider() {
        let result = TodoTool::new()
            .execute(
                r#"{"todos":"[{\"content\":\"verify scheduler registration\",\"status\":\"pending\"}]"}"#,
                &ctx(),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(
            result.content.contains("verify scheduler registration"),
            "{}",
            result.content
        );
    }

    #[tokio::test]
    async fn execute_rejects_invalid() {
        let t = TodoTool::new();
        let r = t
            .execute(r#"{"todos":[{"content":"a","status":"nope"}]}"#, &ctx())
            .await;
        assert!(r.is_error);
        assert!(r.content.contains("pending"), "{}", r.content);
    }

    #[tokio::test]
    async fn execute_rejects_placeholder_incremental_add() {
        let result = TodoTool::new()
            .execute(r#"{"action":"add","content":"task 4"}"#, &ctx())
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("placeholder"), "{}", result.content);
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
    async fn todowrite_accepts_full_list_shape() {
        let t = TodoTool::new();
        assert_eq!(t.name(), "todowrite");
        let r = t.execute(PLAN3, &ctx()).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(
            r.content.contains("1. a"),
            "numbered list echoed: {}",
            r.content
        );
    }

    #[tokio::test]
    async fn todowrite_accepts_incremental_action_shape() {
        // The reported confusion is gone: the SAME `todowrite` tool takes the {action} shape too,
        // so a model that sends `{action}` no longer hits `action must be add or update`.
        let t = TodoTool::new();
        let add = t
            .execute(r#"{"action":"add","content":"write tests"}"#, &ctx())
            .await;
        assert!(
            !add.is_error && add.content.contains("write tests"),
            "{}",
            add.content
        );
        // `#<id> → <status>` is the exact base the TUI enrich step splices a title into.
        let upd = t
            .execute(r#"{"action":"update","id":2,"status":"completed"}"#, &ctx())
            .await;
        assert!(!upd.is_error, "{}", upd.content);
        assert_eq!(upd.content, "#2 \u{2192} completed");
    }

    #[tokio::test]
    async fn todowrite_rejects_bad_args() {
        let t = TodoTool::new();
        assert!(
            t.execute(r#"{"action":"add","content":"  "}"#, &ctx())
                .await
                .is_error,
            "empty add"
        );
        assert!(
            t.execute(r#"{"action":"update","status":"completed"}"#, &ctx())
                .await
                .is_error,
            "no id"
        );
        assert!(
            t.execute(r#"{"action":"update","id":2}"#, &ctx())
                .await
                .is_error,
            "no status"
        );
        assert!(
            t.execute(r#"{"action":"update","id":2,"status":"nope"}"#, &ctx())
                .await
                .is_error,
            "bad status"
        );
        assert!(
            t.execute(r#"{"action":"frobnicate"}"#, &ctx())
                .await
                .is_error,
            "bad action"
        );
        assert!(
            t.execute(r#"{}"#, &ctx()).await.is_error,
            "neither todos nor action"
        );
    }

    #[test]
    fn reducer_folds_action_shape_under_todowrite_name() {
        // Merge regression: an incremental {action} carried by the `todowrite` tool name (not the
        // legacy `todo` name) must still fold — the baseline/patch decision is by SHAPE, not name.
        let list = reduce_todos([
            (
                "todowrite",
                r#"{"todos":[{"content":"a","status":"pending"},{"content":"b","status":"pending"}]}"#,
            ),
            (
                "todowrite",
                r#"{"action":"update","id":1,"status":"completed"}"#,
            ),
            ("todowrite", r#"{"action":"add","content":"c"}"#),
        ]);
        assert_eq!(list.len(), 3);
        assert_eq!(list[0].status, TodoStatus::Completed);
        assert_eq!(list[2].content, "c");
        // Legacy `todo`-named action still folds too (resume compatibility).
        let legacy = reduce_todos([
            (
                "todowrite",
                r#"{"todos":[{"content":"a","status":"pending"}]}"#,
            ),
            (
                "todo",
                r#"{"action":"update","id":1,"status":"in_progress"}"#,
            ),
        ]);
        assert_eq!(legacy[0].status, TodoStatus::InProgress);
    }

    #[test]
    fn description_covers_both_plan_and_update_modes() {
        let t = TodoTool::new();
        let d = t.description();
        assert!(
            d.contains("multiple requests, phases, files, dependencies, ambiguity"),
            "uses semantic complexity triggers: {d}"
        );
        assert!(
            d.contains("PLAN") && d.contains("UPDATE ONE ITEM"),
            "covers both modes: {d}"
        );
        assert!(
            d.contains("specific, verifiable action"),
            "sets item-quality bar: {d}"
        );
    }
}
