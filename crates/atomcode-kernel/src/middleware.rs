use crate::request::RequestCtx;
use crate::tool::{Tool, ToolCall, ToolResult};
use async_trait::async_trait;
use std::sync::Arc;

/// Composable around-tool wrapper — the SINGLE home for TOOL-level concerns
/// (rewrite args, gate/approve, transform results). The kernel runs every
/// middleware's `before` (in registration order) around each tool execution, then
/// every `after`. Distinct from `LifecycleHooks`, which is TURN-level.
///
/// ORDERING IS LOAD-BEARING. Middlewares run in REGISTRATION ORDER: the `before`
/// chain forward (the first-registered runs first; the first to `Err` blocks and
/// stops the chain), then the `after` chain (also in registration order). This is
/// a documented contract, not an accident of iteration. Concretely: an approval
/// middleware that round-trips the user MUST be registered BEFORE a redaction
/// middleware that rewrites the call's args — otherwise the user approves bytes
/// different from what actually executes. Register the user-facing / gating
/// middleware first, arg-rewriting / transforming middleware after.
#[async_trait]
pub trait ToolMiddleware: Send + Sync {
    /// Before a tool executes (after lookup). May REWRITE the call (`&mut` — change
    /// args; note the tool is already resolved, so rewriting `name` does not
    /// re-route), round-trip to the driver via `rt` (e.g. approval), or block via
    /// `Err`. The first middleware to return `Err` blocks; `ToolStarted` then never
    /// fires (no ghost row).
    async fn before(
        &self,
        _call: &mut ToolCall,
        _tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> Result<(), String> {
        Ok(())
    }

    /// After a tool executes (or is blocked). Transform / observe the result
    /// (truncate / redact). Runs for every middleware in registration order.
    async fn after(&self, _result: &mut ToolResult) {}
}
