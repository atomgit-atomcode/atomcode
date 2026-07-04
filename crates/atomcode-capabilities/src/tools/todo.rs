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

/// The current list = args of the last VALID `todowrite` tool call in history.
/// Skips calls whose args fail validation (e.g. two in_progress) so an invalid
/// last call never wipes an earlier valid list. Returns `vec![]` if no valid
/// call exists.
pub fn derive_current_todos(messages: &[Message]) -> Vec<TodoItem> {
    for m in messages.iter().rev() {
        for call in m.tool_calls.iter().rev().filter(|c| c.name == "todowrite") {
            if let Ok(todos) = parse_todos(&call.arguments) {
                return todos;
            }
        }
    }
    Vec::new()
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
Send the ENTIRE updated list every call (full replace). Use it proactively for multi-step work (3+ distinct steps), \
when the user gives multiple tasks, or to plan a non-trivial refactor. Do NOT use it for a single trivial edit or a \
purely informational/conversational reply. Rules: update statuses in real time; keep EXACTLY ONE task in_progress at \
a time; mark a task completed ONLY after the work is actually done (including any required verification), never on \
intent; keep tasks specific and actionable.";

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
            // Echo an ASCII-safe normalized list as the tool result (goes into the
            // transcript; the renderer draws the pretty version separately).
            Ok(todos) => ok(render_todos_text(&todos, false)),
            Err(e) => err(e),
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
}
