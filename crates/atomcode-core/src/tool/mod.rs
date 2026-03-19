pub mod bash;
pub mod cd;
pub mod edit;
pub mod read;
pub mod write;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub output: String,
    pub success: bool,
}

#[derive(Debug, Clone)]
pub struct ToolCallBuffer {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

pub enum ApprovalRequirement {
    AutoApprove,
    RequireApproval(String),
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDef;
    fn approval(&self, args: &str) -> ApprovalRequirement;
    async fn execute(&self, args: &str) -> Result<ToolResult>;
}

pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let name = tool.definition().name.to_string();
        self.tools.insert(name, Arc::from(tool));
    }

    pub fn get_definitions(&self) -> Vec<ToolDef> {
        self.tools.values().map(|t| t.definition()).collect()
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    /// Get an Arc clone of a tool by name (for sending across threads).
    pub fn get_arc(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyTool;

    #[async_trait::async_trait]
    impl Tool for DummyTool {
        fn definition(&self) -> ToolDef {
            ToolDef {
                name: "dummy",
                description: "A dummy tool",
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {},
                }),
            }
        }

        fn approval(&self, _args: &str) -> ApprovalRequirement {
            ApprovalRequirement::AutoApprove
        }

        async fn execute(&self, _args: &str) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                call_id: "test".to_string(),
                output: "ok".to_string(),
                success: true,
            })
        }
    }

    #[test]
    fn test_registry_register_and_get() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(DummyTool));
        assert!(reg.get("dummy").is_some());
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn test_registry_definitions() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(DummyTool));
        let defs = reg.get_definitions();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "dummy");
    }

    #[tokio::test]
    async fn test_tool_execute() {
        let tool = DummyTool;
        let result = tool.execute("{}").await.unwrap();
        assert!(result.success);
        assert_eq!(result.output, "ok");
    }
}
