//! TodoWrite tool — lets the agent track tasks during a session.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

use super::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

#[derive(Debug, Clone)]
struct TodoItem {
    id: usize,
    content: String,
    status: String, // "pending", "in_progress", "completed"
}

/// Shared todo list, wrapped in Arc<Mutex> so multiple tool calls can access it.
/// State IS shared across calls: TodoTool is constructed once via TodoTool::new()
/// and registered as a single instance in the ToolRegistry. All execute() calls
/// reference the same Arc<Mutex> fields, so items persist across the session.
pub struct TodoTool {
    items: Arc<Mutex<Vec<TodoItem>>>,
    next_id: Arc<Mutex<usize>>,
}

impl TodoTool {
    pub fn new() -> Self {
        Self {
            items: Arc::new(Mutex::new(Vec::new())),
            next_id: Arc::new(Mutex::new(1)),
        }
    }

    /// Format the todo list for display.
    async fn format_list(items: &Arc<Mutex<Vec<TodoItem>>>) -> String {
        let items = items.lock().await;
        if items.is_empty() {
            return "No tasks.".to_string();
        }
        let mut out = String::new();
        for item in items.iter() {
            let icon = match item.status.as_str() {
                "completed" => "[x]",
                "in_progress" => "[>]",
                _ => "[ ]",
            };
            out.push_str(&format!("{} {}. {}\n", icon, item.id, item.content));
        }
        out
    }
}

#[derive(Deserialize)]
struct TodoArgs {
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
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "todo",
            description: "Manage a task list to track progress on multi-step work. Use 'add' to create tasks, 'update' to change status, and 'list' to show all tasks.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["add", "update", "list"],
                        "description": "Action: 'add' a new task, 'update' a task's status, or 'list' all tasks"
                    },
                    "content": {
                        "type": "string",
                        "description": "Task description (required for 'add')"
                    },
                    "id": {
                        "type": "integer",
                        "description": "Task ID (required for 'update')"
                    },
                    "status": {
                        "type": "string",
                        "enum": ["pending", "in_progress", "completed"],
                        "description": "New status (required for 'update')"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, args: &str, _ctx: &ToolContext) -> Result<ToolResult> {
        let parsed: TodoArgs = serde_json::from_str(args)?;

        match parsed.action.as_str() {
            "add" => {
                let content = parsed.content.unwrap_or_else(|| "Untitled task".to_string());
                let mut id_guard = self.next_id.lock().await;
                let id = *id_guard;
                *id_guard += 1;
                drop(id_guard);

                let item = TodoItem {
                    id,
                    content: content.clone(),
                    status: "pending".to_string(),
                };
                self.items.lock().await.push(item);

                Ok(ToolResult {
                    call_id: String::new(),
                    output: format!("Added task #{}: {}", id, content),
                    success: true,
                })
            }
            "update" => {
                let id = parsed.id.ok_or_else(|| anyhow::anyhow!("'id' is required for update"))?;
                let status = parsed.status.unwrap_or_else(|| "in_progress".to_string());

                let mut items = self.items.lock().await;
                if let Some(item) = items.iter_mut().find(|i| i.id == id) {
                    item.status = status.clone();
                    Ok(ToolResult {
                        call_id: String::new(),
                        output: format!("Task #{} updated to '{}'", id, status),
                        success: true,
                    })
                } else {
                    Ok(ToolResult {
                        call_id: String::new(),
                        output: format!("Task #{} not found", id),
                        success: false,
                    })
                }
            }
            "list" => {
                let list = Self::format_list(&self.items).await;
                Ok(ToolResult {
                    call_id: String::new(),
                    output: list,
                    success: true,
                })
            }
            other => Ok(ToolResult {
                call_id: String::new(),
                output: format!("Unknown action: {}. Use 'add', 'update', or 'list'.", other),
                success: false,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn add_and_list_tasks() {
        let tool = TodoTool::new();
        let ctx = ToolContext::new(std::path::PathBuf::from("/tmp"));

        // Add
        let r = tool.execute(r#"{"action":"add","content":"Write tests"}"#, &ctx).await.unwrap();
        assert!(r.success);
        assert!(r.output.contains("#1"));

        let r = tool.execute(r#"{"action":"add","content":"Fix bug"}"#, &ctx).await.unwrap();
        assert!(r.output.contains("#2"));

        // List
        let r = tool.execute(r#"{"action":"list"}"#, &ctx).await.unwrap();
        assert!(r.output.contains("Write tests"));
        assert!(r.output.contains("Fix bug"));
        assert!(r.output.contains("[ ]")); // pending
    }

    #[tokio::test]
    async fn update_task_status() {
        let tool = TodoTool::new();
        let ctx = ToolContext::new(std::path::PathBuf::from("/tmp"));

        tool.execute(r#"{"action":"add","content":"Task 1"}"#, &ctx).await.unwrap();

        let r = tool.execute(r#"{"action":"update","id":1,"status":"completed"}"#, &ctx).await.unwrap();
        assert!(r.success);

        let r = tool.execute(r#"{"action":"list"}"#, &ctx).await.unwrap();
        assert!(r.output.contains("[x]")); // completed
    }

    #[tokio::test]
    async fn update_nonexistent_task_fails() {
        let tool = TodoTool::new();
        let ctx = ToolContext::new(std::path::PathBuf::from("/tmp"));

        let r = tool.execute(r#"{"action":"update","id":99,"status":"completed"}"#, &ctx).await.unwrap();
        assert!(!r.success);
        assert!(r.output.contains("not found"));
    }

    #[tokio::test]
    async fn list_empty_shows_no_tasks() {
        let tool = TodoTool::new();
        let ctx = ToolContext::new(std::path::PathBuf::from("/tmp"));

        let r = tool.execute(r#"{"action":"list"}"#, &ctx).await.unwrap();
        assert!(r.output.contains("No tasks"));
    }
}
