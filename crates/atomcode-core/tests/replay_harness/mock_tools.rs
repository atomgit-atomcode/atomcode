//! Mock tool implementations for replay — each `execute()` call pops
//! the next pre-recorded `RecordedToolResult` from a shared queue. Tool
//! arguments are deliberately ignored (the replay test asserts on
//! framework behavior, not on the tool's own logic; bash/edit/read are
//! already unit-tested elsewhere).
//!
//! **Assumption**: tool calls during replay happen in the same order as
//! recorded. For the single-tool-per-turn sessions used as fixtures today
//! this holds; if a future fixture has multi-tool turns dispatched
//! concurrently, the queue may hand out mismatched results and the test
//! will fail with a mismatching output — treat that as a signal to
//! refactor this mock to match by `(tool_name, args_hash)` instead.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use async_trait::async_trait;

use atomcode_core::tool::{ApprovalRequirement, Tool, ToolContext, ToolDef, ToolResult};

use super::fixture_loader::RecordedToolResult;

/// Shared queue of recorded results. All mock tools in a test share
/// a single instance so order is preserved across tool kinds.
#[derive(Clone)]
pub struct MockToolQueue {
    inner: Arc<Mutex<VecDeque<RecordedToolResult>>>,
}

impl MockToolQueue {
    pub fn new(results: Vec<RecordedToolResult>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(results.into())),
        }
    }

    fn pop(&self, tool_name: &str) -> Result<ToolResult> {
        let mut q = self.inner.lock().expect("queue lock");
        let rec = q.pop_front().ok_or_else(|| {
            anyhow!(
                "mock tool queue exhausted: `{}` called but no more recorded results. \
                 Framework is dispatching more tool calls than the fixture has — possibly \
                 a replay-harness regression or a real fixture-vs-behavior drift.",
                tool_name
            )
        })?;
        Ok(ToolResult {
            call_id: rec.call_id,
            output: rec.output,
            success: rec.success,
        })
    }
}

/// Generic mock: same behavior for every tool name. Each concrete type
/// below is a thin wrapper that reports a `&'static str` name.
pub struct MockTool {
    queue: MockToolQueue,
    name: &'static str,
}

impl MockTool {
    pub fn new(name: &'static str, queue: MockToolQueue) -> Self {
        Self { queue, name }
    }
}

#[async_trait]
impl Tool for MockTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: self.name,
            description: format!("Replay mock for `{}` — returns recorded outputs.", self.name),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": true,
            }),
        }
    }

    fn approval(&self, _args: &str) -> ApprovalRequirement {
        // Replay mocks always auto-approve so the test doesn't block on a
        // permission dialog that has no real user to service.
        ApprovalRequirement::AutoApprove
    }

    async fn execute(&self, _args: &str, _ctx: &ToolContext) -> Result<ToolResult> {
        self.queue.pop(self.name)
    }
}

/// Factory: register one `MockTool` per distinct tool name observed in
/// the fixture. Returns a registry ready to pass to `AgentLoop::new`.
///
/// Names are `&'static str`, so we accept them as an explicit list of
/// literals — Rust's `ToolDef` schema requires static lifetimes and the
/// fixture-discovered names can't satisfy that without leaking. Passing
/// literal names up front is cleaner than the leak workaround.
pub fn build_registry(
    queue: MockToolQueue,
    tool_names: &[&'static str],
) -> atomcode_core::tool::ToolRegistry {
    let mut reg = atomcode_core::tool::ToolRegistry::new();
    for name in tool_names {
        reg.register(Box::new(MockTool::new(name, queue.clone())));
    }
    reg
}
