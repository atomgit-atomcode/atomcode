//! `todo` — a session task list the agent manages to track multi-step work. Stateful:
//! the list lives in the tool instance (interior-mutable `Arc<Mutex<…>>`), so a SINGLE
//! registered `TodoTool` accumulates items across calls within a session. Non-destructive
//! to the filesystem ⇒ always `Safe`. Neutral port of the production `todo` tool.

use super::{err, ok};
use async_trait::async_trait;
use atomcode_kernel::tool::{Tool, ToolContext, ToolResult};
use serde::Deserialize;
use serde_json::json;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
struct TodoItem {
    id: usize,
    content: String,
    status: Status,
}

#[derive(Clone, Copy, PartialEq)]
enum Status {
    Pending,
    InProgress,
    Completed,
}

impl Status {
    fn parse(s: &str) -> Option<Status> {
        match s {
            "pending" => Some(Status::Pending),
            "in_progress" => Some(Status::InProgress),
            "completed" => Some(Status::Completed),
            _ => None,
        }
    }
    fn icon(self) -> &'static str {
        match self {
            Status::Pending => "[ ]",
            Status::InProgress => "[>]",
            Status::Completed => "[x]",
        }
    }
    fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::InProgress => "in_progress",
            Status::Completed => "completed",
        }
    }
}

/// Session task list. State is shared across `execute` calls because the tool is
/// registered ONCE as a single `Arc<dyn Tool>` instance; all calls hit the same inner
/// `Mutex`. Clone is cheap (shares the inner `Arc`).
#[derive(Clone, Default)]
pub struct TodoTool {
    items: Arc<Mutex<Vec<TodoItem>>>,
    next_id: Arc<Mutex<usize>>,
}

impl TodoTool {
    pub fn new() -> Self {
        Self::default()
    }

    fn render(items: &[TodoItem]) -> String {
        if items.is_empty() {
            return "No tasks.".to_string();
        }
        let mut out = String::new();
        for it in items {
            out.push_str(&format!("{} {}. {}\n", it.status.icon(), it.id, it.content));
        }
        out.truncate(out.trim_end().len());
        out
    }
}

#[derive(Deserialize)]
struct Args {
    action: String,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    id: Option<usize>,
    #[serde(default)]
    status: Option<String>,
}

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &str {
        "todo"
    }
    fn description(&self) -> &str {
        "Manage a task list to track progress on multi-step work. `action:\"add\"` \
         creates a task (needs `content`), `action:\"update\"` changes a task's status \
         (needs `id` and `status`), `action:\"list\"` shows all tasks. Every action \
         returns the full current list."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": { "type": "string", "enum": ["add", "update", "list"], "description": "add a task, update a task's status, or list all tasks" },
                "content": { "type": "string", "description": "Task description (required for add)" },
                "id": { "type": "integer", "description": "Task id (required for update)" },
                "status": { "type": "string", "enum": ["pending", "in_progress", "completed"], "description": "New status (required for update)" }
            },
            "required": ["action"]
        })
    }
    // todo never touches the filesystem → risk() defaults to Safe.
    async fn execute(&self, args: &str, _ctx: &ToolContext) -> ToolResult {
        let a: Args = match serde_json::from_str(args) {
            Ok(a) => a,
            Err(e) => return err(format!("todo: invalid arguments: {e}. Expected {{\"action\":\"add|update|list\"}}.")),
        };
        let mut items = self.items.lock().unwrap();
        match a.action.as_str() {
            "add" => {
                let content = match a.content {
                    Some(c) if !c.trim().is_empty() => c,
                    _ => return err("todo add: `content` is required and must be non-empty."),
                };
                let mut next = self.next_id.lock().unwrap();
                *next += 1;
                let id = *next;
                items.push(TodoItem { id, content: content.clone(), status: Status::Pending });
                ok(format!("Added task #{}: {}\n{}", id, content, Self::render(&items)))
            }
            "update" => {
                let Some(id) = a.id else {
                    return err("todo update: `id` is required.");
                };
                let status = match a.status.as_deref().map(Status::parse) {
                    Some(Some(s)) => s,
                    Some(None) => return err("todo update: `status` must be one of pending|in_progress|completed."),
                    None => return err("todo update: `status` is required."),
                };
                match items.iter_mut().find(|i| i.id == id) {
                    Some(it) => {
                        let content = it.content.clone();
                        it.status = status;
                        ok(format!("Task #{} '{}' updated to '{}'\n{}", id, content, status.as_str(), Self::render(&items)))
                    }
                    None => err(format!("todo update: no task with id {id}.")),
                }
            }
            "list" => ok(Self::render(&items)),
            other => err(format!("todo: unknown action `{other}` (expected add|update|list).")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn ctx() -> ToolContext {
        ToolContext {
            working_dir: std::path::PathBuf::from("."),
            cancel: CancellationToken::new(),
            progress: atomcode_kernel::tool::ProgressSink::noop(),
        }
    }

    #[tokio::test]
    async fn add_then_list_accumulates_across_calls() {
        let t = TodoTool::new();
        let r1 = t.execute(r#"{"action":"add","content":"first"}"#, &ctx()).await;
        assert!(!r1.is_error, "{}", r1.content);
        assert!(r1.content.contains("Added task #1"), "{}", r1.content);
        let r2 = t.execute(r#"{"action":"add","content":"second"}"#, &ctx()).await;
        assert!(r2.content.contains("#2"), "{}", r2.content);
        // STATE persists across calls on the same instance.
        let list = t.execute(r#"{"action":"list"}"#, &ctx()).await;
        assert!(list.content.contains("[ ] 1. first"), "{}", list.content);
        assert!(list.content.contains("[ ] 2. second"), "{}", list.content);
    }

    #[tokio::test]
    async fn update_changes_status_icon() {
        let t = TodoTool::new();
        t.execute(r#"{"action":"add","content":"task"}"#, &ctx()).await;
        let r = t.execute(r#"{"action":"update","id":1,"status":"in_progress"}"#, &ctx()).await;
        assert!(!r.is_error, "{}", r.content);
        assert!(r.content.contains("[>] 1. task"), "{}", r.content);
        let r2 = t.execute(r#"{"action":"update","id":1,"status":"completed"}"#, &ctx()).await;
        assert!(r2.content.contains("[x] 1. task"), "{}", r2.content);
    }

    #[tokio::test]
    async fn empty_list_reports_no_tasks() {
        let t = TodoTool::new();
        let r = t.execute(r#"{"action":"list"}"#, &ctx()).await;
        assert_eq!(r.content, "No tasks.");
    }

    #[tokio::test]
    async fn add_without_content_errors() {
        let t = TodoTool::new();
        let r = t.execute(r#"{"action":"add"}"#, &ctx()).await;
        assert!(r.is_error);
        assert!(r.content.contains("content"), "{}", r.content);
    }

    #[tokio::test]
    async fn update_missing_id_and_bad_status_error() {
        let t = TodoTool::new();
        t.execute(r#"{"action":"add","content":"x"}"#, &ctx()).await;
        let no_id = t.execute(r#"{"action":"update","status":"completed"}"#, &ctx()).await;
        assert!(no_id.is_error && no_id.content.contains("id"), "{}", no_id.content);
        let bad = t.execute(r#"{"action":"update","id":1,"status":"done"}"#, &ctx()).await;
        assert!(bad.is_error && bad.content.contains("pending|in_progress|completed"), "{}", bad.content);
    }

    #[tokio::test]
    async fn update_unknown_id_errors() {
        let t = TodoTool::new();
        let r = t.execute(r#"{"action":"update","id":99,"status":"completed"}"#, &ctx()).await;
        assert!(r.is_error);
        assert!(r.content.contains("no task with id 99"), "{}", r.content);
    }

    #[tokio::test]
    async fn unknown_action_errors() {
        let t = TodoTool::new();
        let r = t.execute(r#"{"action":"delete","id":1}"#, &ctx()).await;
        assert!(r.is_error);
        assert!(r.content.contains("unknown action"), "{}", r.content);
    }
}
