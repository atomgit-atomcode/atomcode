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
///
/// # PANIC CONTRACT (must-not-panic)
///
/// An implementation **MUST NOT panic**. The kernel does **NOT** isolate panics:
/// under the workspace `panic = "abort"` profile a panic ABORTS THE HOST PROCESS
/// (and `catch_unwind` is a no-op there), and under an unwind profile a panicking
/// middleware is not currently caught either — so a panicking `before`/`after`
/// takes down the whole session / process. Treat all injected code as
/// must-not-panic (the SAME trust posture as the tool-sandbox contract — see
/// [`crate::tool`]): to BLOCK a call, return [`BeforeOutcome::Deny`] from `before`; never panic.
#[async_trait]
pub trait ToolMiddleware: Send + Sync {
    /// Before a tool executes (after lookup). May REWRITE the call (`&mut` — change
    /// args; note the tool is already resolved, so rewriting `name` does not
    /// re-route — this is CC's `updatedInput`), round-trip to the driver via `rt`
    /// (e.g. approval), and returns a [`BeforeOutcome`] GATE decision. The first
    /// middleware to return [`BeforeOutcome::Deny`] blocks; `ToolStarted` then never
    /// fires (no ghost row).
    async fn before(
        &self,
        _call: &mut ToolCall,
        _tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> BeforeOutcome {
        BeforeOutcome::Proceed
    }

    /// After a tool executes (or is blocked). Transform / observe the result in place
    /// (truncate / redact — CC's `updatedToolOutput`) and return an [`AfterOutcome`]
    /// CONTINUATION decision. Runs for every middleware in registration order.
    async fn after(&self, _result: &mut ToolResult) -> AfterOutcome {
        AfterOutcome::Proceed
    }
}

/// Gate decision returned by [`ToolMiddleware::before`]. Aligns with Claude Code's
/// PreToolUse `permissionDecision` (allow / deny / ask) plus pass-through. ARG
/// REWRITES still ride on `&mut ToolCall` (CC's `updatedInput`); this enum is the
/// gate decision only.
///
/// FOLD across the chain (see the run loop): the first [`Deny`](BeforeOutcome::Deny)
/// blocks (exact parity with the former `Err`); an [`Allow`](BeforeOutcome::Allow)
/// force-approves THIS call and bypasses the remaining `before` gates;
/// [`Ask`](BeforeOutcome::Ask) requests an approval prompt; [`Proceed`](BeforeOutcome::Proceed)
/// defers to the next middleware / the normal approval flow.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum BeforeOutcome {
    /// Continue the chain + normal approval. (Former `Ok(())`.)
    #[default]
    Proceed,
    /// Force-approve this call: bypass any downstream approval gate without
    /// prompting. (CC `permissionDecision: "allow"`.) `reason` is advisory.
    Allow { reason: Option<String> },
    /// Force an approval prompt for this call even if it would auto-approve.
    /// (CC `permissionDecision: "ask"`.)
    Ask { reason: Option<String> },
    /// Block the call; `reason` is surfaced to the model and the driver.
    /// (Former `Err(reason)`.)
    Deny { reason: String },
}

impl BeforeOutcome {
    /// Block a call with a reason — ergonomic constructor (former `Err(reason)`).
    pub fn deny(reason: impl Into<String>) -> Self {
        BeforeOutcome::Deny {
            reason: reason.into(),
        }
    }
    /// True iff this is a [`Deny`](BeforeOutcome::Deny).
    pub fn is_deny(&self) -> bool {
        matches!(self, BeforeOutcome::Deny { .. })
    }
    /// The deny reason, if this is a [`Deny`](BeforeOutcome::Deny).
    pub fn deny_reason(&self) -> Option<&str> {
        match self {
            BeforeOutcome::Deny { reason } => Some(reason.as_str()),
            _ => None,
        }
    }
}

/// Continuation decision returned by [`ToolMiddleware::after`]. RESULT TRANSFORMS
/// still ride on `&mut ToolResult` (CC's `updatedToolOutput`); this enum is the
/// continuation decision only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum AfterOutcome {
    /// Normal continuation. (Former `()`.)
    #[default]
    Proceed,
    /// Surface `reason` back to the model after this tool result.
    /// (CC PostToolUse `decision: "block"`.)
    Block { reason: String },
}
