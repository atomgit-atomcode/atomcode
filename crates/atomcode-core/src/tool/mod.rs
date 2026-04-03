pub mod auto_fix;
pub mod bash;
pub mod cd;
pub mod devserver;
pub mod edit;
pub mod file_history;
pub mod find_references;
pub mod glob;
pub mod grep;
pub mod list_dir;
pub mod list_symbols;
pub mod read;
pub mod read_symbol;
pub mod result_store;
pub mod search_replace;
pub mod use_skill;
pub mod web_fetch;
pub mod web_search;
pub mod write;

use std::collections::{HashMap, HashSet};

/// Directories to skip when scanning file trees (build artifacts, caches, VCS).
/// Used by glob, list_dir, project_context, and collect_project_files.
pub const SKIP_DIRS: &[&str] = &[
    "node_modules", ".git", "target", "__pycache__", ".next",
    "dist", "build", ".cache", "vendor", ".venv", "venv",
    ".idea", ".vscode", ".DS_Store", ".env",
    "datalog", "logs", "log", ".atomcode",
];
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::{Mutex, RwLock};

#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: String,
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

/// Coarse-grained permission level for a tool, stored in `PermissionStore`.
#[derive(Debug, Clone, PartialEq)]
pub enum PermissionLevel {
    /// Never ask — always execute automatically.
    AlwaysAllow,
    /// Ask every time (default for destructive operations).
    Ask,
    /// Allowed for the duration of the current session.
    SessionAllow,
    /// Never execute.
    AlwaysDeny,
}

/// The resolved decision returned by `PermissionStore::check`.
#[derive(Debug, Clone)]
pub enum PermissionDecision {
    Allow,
    /// Ask the user — carries the reason string from `ApprovalRequirement`.
    Ask(String),
    Deny,
}

/// Stores per-tool permission overrides and session-level grants.
pub struct PermissionStore {
    /// Per-tool level overrides: tool_name → level.
    overrides: HashMap<String, PermissionLevel>,
    /// Session-level grants: tool names approved with [A]lways for this session.
    session_grants: HashSet<String>,
}

impl PermissionStore {
    pub fn new() -> Self {
        Self {
            overrides: HashMap::new(),
            session_grants: HashSet::new(),
        }
    }

    /// Check whether a tool call should be auto-approved, needs asking, or denied.
    pub fn check(&self, tool_name: &str, approval: &ApprovalRequirement) -> PermissionDecision {
        // Destructive commands (RequireApproval) ALWAYS prompt — no override or
        // session grant can bypass this. Prevents a single [A]lways on "bash"
        // from silently executing DROP TABLE, rm -rf, etc.
        if let ApprovalRequirement::RequireApproval(reason) = approval {
            return PermissionDecision::Ask(reason.clone());
        }

        // 1. Explicit per-tool override (only reached for AutoApprove tools).
        if let Some(level) = self.overrides.get(tool_name) {
            match level {
                PermissionLevel::AlwaysAllow | PermissionLevel::SessionAllow => {
                    return PermissionDecision::Allow;
                }
                PermissionLevel::AlwaysDeny => return PermissionDecision::Deny,
                PermissionLevel::Ask => {} // fall through to normal logic
            }
        }
        // 2. Session grant (set by user pressing [A] during a session).
        if self.session_grants.contains(tool_name) {
            return PermissionDecision::Allow;
        }
        // 3. Defer to the tool's own approval requirement.
        PermissionDecision::Allow
    }

    /// Grant session-level permission for a tool (user pressed [A]).
    pub fn grant_session(&mut self, tool_name: &str) {
        self.session_grants.insert(tool_name.to_string());
    }

    /// Set an explicit override level for a tool.
    pub fn set_override(&mut self, tool_name: &str, level: PermissionLevel) {
        self.overrides.insert(tool_name.to_string(), level);
    }
}

/// Shared execution context passed to every tool invocation.
/// Holds a shared working directory that tools can read (and `CdTool` can write).
#[derive(Clone)]
pub struct ToolContext {
    pub working_dir: Arc<RwLock<PathBuf>>,
    pub semantic: Arc<Mutex<crate::semantic::SemanticSearcher>>,
    pub file_history: Arc<Mutex<file_history::FileHistory>>,
}

impl ToolContext {
    pub fn new(working_dir: PathBuf) -> Self {
        Self::with_session(working_dir, "default")
    }

    pub fn with_session(working_dir: PathBuf, session_id: &str) -> Self {
        Self {
            working_dir: Arc::new(RwLock::new(working_dir)),
            semantic: Arc::new(Mutex::new(crate::semantic::SemanticSearcher::new())),
            file_history: Arc::new(Mutex::new(file_history::FileHistory::new(session_id))),
        }
    }

    /// Create an isolated copy: same working directory value, independent Arc.
    /// Used when passing context across Agent boundaries (subagents, tests).
    pub async fn isolate(&self) -> Self {
        let wd = self.working_dir.read().await.clone();
        Self::new(wd)
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDef;
    fn approval(&self, args: &str) -> ApprovalRequirement;
    async fn execute(&self, args: &str, ctx: &ToolContext) -> Result<ToolResult>;
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

    /// Register a tool from an Arc (for building filtered registries from parent).
    pub fn register_arc(&mut self, name: String, tool: Arc<dyn Tool>) {
        self.tools.insert(name, tool);
    }

    /// Iterate over all registered tools.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Arc<dyn Tool>)> {
        self.tools.iter().map(|(k, v)| (k.as_str(), v))
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
                description: "A dummy tool".to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {},
                }),
            }
        }

        fn approval(&self, _args: &str) -> ApprovalRequirement {
            ApprovalRequirement::AutoApprove
        }

        async fn execute(&self, _args: &str, _ctx: &ToolContext) -> anyhow::Result<ToolResult> {
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
        let ctx = ToolContext::new(std::env::current_dir().unwrap());
        let result = tool.execute("{}", &ctx).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output, "ok");
    }

    // PermissionStore tests

    #[test]
    fn test_permission_store_auto_approve() {
        let store = PermissionStore::new();
        let decision = store.check("bash", &ApprovalRequirement::AutoApprove);
        assert!(matches!(decision, PermissionDecision::Allow));
    }

    #[test]
    fn test_permission_store_require_approval() {
        let store = PermissionStore::new();
        let decision = store.check("bash", &ApprovalRequirement::RequireApproval("Destructive".into()));
        assert!(matches!(decision, PermissionDecision::Ask(_)));
    }

    #[test]
    fn test_permission_store_session_grant_cannot_bypass_destructive() {
        // Session grant on "bash" must NOT bypass RequireApproval.
        // This prevents [A]lways on bash from silently executing DROP TABLE etc.
        let mut store = PermissionStore::new();
        store.grant_session("bash");
        let decision = store.check("bash", &ApprovalRequirement::RequireApproval("Destructive".into()));
        assert!(matches!(decision, PermissionDecision::Ask(_)));
    }

    #[test]
    fn test_permission_store_session_grant_allows_auto_approve() {
        // Session grant still works for non-destructive (AutoApprove) tools.
        let mut store = PermissionStore::new();
        store.grant_session("bash");
        let decision = store.check("bash", &ApprovalRequirement::AutoApprove);
        assert!(matches!(decision, PermissionDecision::Allow));
    }

    #[test]
    fn test_permission_store_always_deny_override() {
        let mut store = PermissionStore::new();
        store.set_override("bash", PermissionLevel::AlwaysDeny);
        // Even AutoApprove is blocked.
        let decision = store.check("bash", &ApprovalRequirement::AutoApprove);
        assert!(matches!(decision, PermissionDecision::Deny));
    }

    #[test]
    fn test_permission_store_always_allow_cannot_bypass_destructive() {
        // Even AlwaysAllow override must NOT bypass RequireApproval.
        let mut store = PermissionStore::new();
        store.set_override("bash", PermissionLevel::AlwaysAllow);
        let decision = store.check("bash", &ApprovalRequirement::RequireApproval("Destructive".into()));
        assert!(matches!(decision, PermissionDecision::Ask(_)));
    }

    #[tokio::test]
    async fn test_tool_context_isolate() {
        let ctx = ToolContext::new(PathBuf::from("/original"));
        let isolated = ctx.isolate().await;
        // Mutating isolated should not affect original
        *isolated.working_dir.write().await = PathBuf::from("/changed");
        let original_wd = ctx.working_dir.read().await.clone();
        assert_eq!(original_wd, PathBuf::from("/original"));
    }

    #[test]
    fn test_registry_iter() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(DummyTool));
        let items: Vec<_> = reg.iter().collect();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].0, "dummy");
    }

    #[test]
    fn test_registry_register_arc() {
        let mut reg1 = ToolRegistry::new();
        reg1.register(Box::new(DummyTool));
        let mut reg2 = ToolRegistry::new();
        for (name, arc) in reg1.iter() {
            reg2.register_arc(name.to_string(), arc.clone());
        }
        assert!(reg2.get("dummy").is_some());
    }

    #[test]
    fn test_permission_store_session_grant_only_affects_named_tool() {
        let mut store = PermissionStore::new();
        store.grant_session("bash");
        // Other tools are unaffected.
        let decision = store.check("write_file", &ApprovalRequirement::RequireApproval("write".into()));
        assert!(matches!(decision, PermissionDecision::Ask(_)));
    }
}
