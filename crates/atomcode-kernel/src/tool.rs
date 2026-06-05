use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON arguments string from the model.
    pub arguments: String,
}

use async_trait::async_trait;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Risk classification a tool declares about itself. The kernel only *knows*
/// risk; it does nothing about it. "Approval" is a specialization concept built
/// on top (see testkit::ApprovalMiddleware). This boundary keeps approval OUT
/// of the kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RiskLevel {
    Safe,
    Risky,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub content: String,
    pub is_error: bool,
}

/// What the LLM sees for a mounted tool.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Execution context passed to tools. Deliberately minimal: NO semantic/graph/lsp
/// services — proving the kernel needs none.
pub struct ToolContext {
    pub working_dir: PathBuf,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;
    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }
    /// Whether this tool is safe to run concurrently with other tools in the same
    /// batch. Conservative default: false (serial). Read-only tools opt in to true.
    fn parallel_safe(&self) -> bool {
        false
    }
    async fn execute(&self, args: &str, ctx: &ToolContext) -> ToolResult;
}

/// Holds *all* available tools. BTreeMap for deterministic ordering (prompt-cache
/// stability — same discipline as the production registry).
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }
    /// Select the subset exposed to the LLM. Unmounted tools never produce a
    /// ToolDef and are not resolvable during a turn → zero effect on the agent.
    pub fn mount(&self, names: &[&str]) -> MountedTools {
        let selected = names
            .iter()
            .filter_map(|n| self.tools.get(*n).map(|t| (n.to_string(), t.clone())))
            .collect();
        MountedTools { selected }
    }
}

pub struct MountedTools {
    selected: BTreeMap<String, Arc<dyn Tool>>,
}

impl MountedTools {
    pub fn defs(&self) -> Vec<ToolDef> {
        self.selected
            .values()
            .map(|t| ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters_schema(),
            })
            .collect()
    }
    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.selected.get(name).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Arc;

    struct Dummy(&'static str, RiskLevel);

    #[async_trait]
    impl Tool for Dummy {
        fn name(&self) -> &str { self.0 }
        fn description(&self) -> &str { "dummy" }
        fn parameters_schema(&self) -> serde_json::Value { serde_json::json!({"type": "object"}) }
        fn risk(&self) -> RiskLevel { self.1 }
        async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolResult {
            ToolResult { call_id: String::new(), content: "ok".into(), is_error: false }
        }
    }

    #[test]
    fn only_mounted_tools_are_exposed_or_resolvable() {
        let mut reg = ToolRegistry::new();
        reg.register(Arc::new(Dummy("echo", RiskLevel::Safe)));
        reg.register(Arc::new(Dummy("risky_write", RiskLevel::Risky)));

        let mounted = reg.mount(&["echo"]);

        let defs = mounted.defs();
        assert_eq!(defs.len(), 1, "unmounted tool must not appear in ToolDefs");
        assert_eq!(defs[0].name, "echo");

        assert!(mounted.get("echo").is_some());
        assert!(mounted.get("risky_write").is_none(), "unmounted tool must be inert/invisible");
    }
}
