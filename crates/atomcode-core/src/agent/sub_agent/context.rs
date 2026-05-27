//! Sandbox ToolContext builder for Q&A sub-agents.
//!
//! Sub-agents run in an isolated context that shares the parent's
//! working directory, code graph, telemetry, LSP, and file store but
//! keeps everything else independent.  This is a whitelist approach:
//! `isolate()` only copies the fields that are safe to share; any
//! field added to `ToolContext` in the future is automatically
//! sandboxed to `None`/default unless explicitly added to `isolate()`.

use std::sync::Arc;
use tokio::sync::mpsc;

use crate::tool::{ToolContext, ToolRegistry};
use crate::turn::event::TurnEvent;
use crate::turn::permission::{AutoPermissionDecider, AutoPermissionMode, PermissionDecider};

/// Build an isolated `ToolContext` for a sub-agent.
///
/// Whitelist isolation via `parent_ctx.isolate()`:
/// - Shared: `working_dir`, `graph`, `telemetry`, `lsp`, `file_store`
/// - Sandboxed (None/default): everything else
///
/// The sub-agent is further configured with its own tool registry
/// (a filtered subset of the parent's tools) and its own event
/// sender for streaming tool output.
pub async fn build_sandbox_context(
    parent_ctx: &ToolContext,
    filtered_tools: Arc<ToolRegistry>,
    turn_event_tx: mpsc::UnboundedSender<TurnEvent>,
) -> ToolContext {
    let mut ctx = parent_ctx.isolate().await;
    ctx.tool_registry = Some(filtered_tools);
    ctx.event_tx = Some(Arc::new(turn_event_tx));
    ctx
}

/// Build a permission decider that bypasses all approval prompts.
///
/// Q&A sub-agents only have read-only tools, so `BypassAll` is safe:
/// no tool in the filtered set can mutate the workspace.
pub fn build_permission_decider() -> Box<dyn PermissionDecider> {
    Box::new(AutoPermissionDecider::new(AutoPermissionMode::BypassAll))
}
