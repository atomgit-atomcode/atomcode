//! Tools mounted into the kernel — and the kernel's **trust-model contract**.
//!
//! # Trust model (read before mounting a tool)
//!
//! The kernel is a neutral, embeddable SDK. It does **NOT sandbox** the tools it
//! hosts. MOUNTING a tool GRANTS its `execute` the host process's **full ambient
//! authority** — the same environment variables, filesystem, network, and
//! secrets the host process itself holds. There is no privilege boundary between
//! a mounted tool and the embedder.
//!
//! * [`RiskLevel`] is **advisory metadata only**. A tool declares it; a
//!   specialization's approval middleware MAY read it to decide whether to gate a
//!   call. It is NOT an enforcement boundary — the kernel never blocks, drops, or
//!   confines a call based on its `RiskLevel`. Rating a call `Safe` confines
//!   nothing; rating it `Risky` stops nothing on its own.
//! * The kernel's ONE built-in safety mechanism at this altitude is the
//!   **tool-result size cap** (`agent::AgentBuilder::max_tool_result_bytes`,
//!   default 256 KiB): it bounds how many bytes a tool result can inject into the
//!   context window / host memory. That is the limit of what the kernel can own
//!   here.
//! * **OS-level isolation is the EMBEDDER's responsibility**, not the kernel's.
//!   seccomp, namespaces, containers, a separate child process, a restricted
//!   user, network egress controls — these live at the OS / driver / an L1
//!   capability layer. The kernel deliberately does not implement them: doing so
//!   would be out of its altitude (it has no OS-specific knowledge and must stay
//!   portable). An embedder mounting untrusted tools MUST provide isolation
//!   itself.

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

/// Risk classification a tool declares about itself. **Advisory metadata, NOT an
/// enforcement boundary** (see the module-level trust-model contract): the kernel
/// only *knows* risk; it does nothing about it — it never blocks, drops, or
/// sandboxes a call based on its `RiskLevel`. "Approval" is a specialization
/// concept built on top (see testkit::ApprovalMiddleware) that MAY read this to
/// decide whether to gate. This boundary keeps approval OUT of the kernel.
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
/// services — proving the kernel needs none. NOTE the trust model (module doc):
/// the kernel does not sandbox; `execute` runs with the host process's full
/// ambient authority. The only bound the kernel imposes on a tool is the size of
/// the `ToolResult.content` it may inject (`AgentBuilder::max_tool_result_bytes`).
///
/// `cancel` is the per-turn cooperative-cancellation token. A long-running tool
/// SHOULD poll `ctx.cancel.is_cancelled()` or `select!` on `ctx.cancel.cancelled()`
/// to bail out and RELEASE ITS RESOURCES. On cancel the kernel drops the execute
/// future as a backstop, but dropping only STOPS POLLING — it is NOT cleanup: any
/// subprocess / fd / partial write the tool spawned is the TOOL's responsibility
/// to reclaim, via cooperative cancel-polling or an RAII `Drop` guard on the
/// resource (e.g. a child-process handle that SIGKILLs on drop). A tool that does
/// neither may leak on cancel, and a side effect already in flight when the future
/// is dropped is reported to the model as cancelled even though it may have landed.
pub struct ToolContext {
    pub working_dir: PathBuf,
    pub cancel: tokio_util::sync::CancellationToken,
}

/// A mounted tool. Its `execute` runs with the host process's FULL ambient
/// authority — the kernel does not sandbox it (see the module-level trust-model
/// contract). The kernel's only built-in bound on a tool is the size of the
/// result it may return (`agent::AgentBuilder::max_tool_result_bytes`); a tool
/// returning a huge `ToolResult.content` is TRUNCATED to that cap before the
/// model / history / driver see it, so a runaway tool cannot blow the context
/// window. A tool MAY also self-cap, but need not — the kernel cap is a central
/// backstop for third-party tools that do not.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;
    /// Risk classification for THIS call — arg-aware, so e.g. a bash tool can rate
    /// `rm -rf` Risky and `ls` Safe. Conservative default: Safe. The tool owns this
    /// (intrinsic knowledge of its args); a specialization's approval middleware
    /// reads it to decide whether to gate.
    fn risk(&self, _args: &str) -> RiskLevel {
        RiskLevel::Safe
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
        fn risk(&self, _args: &str) -> RiskLevel { self.1 }
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
