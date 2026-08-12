use crate::event::PolicyIntervention;
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
/// a documented contract, not an accident of iteration. Any normalization or
/// repair that changes the arguments which will execute MUST run before every
/// observer, policy gate, and user approval, so all of them inspect the exact
/// bytes the tool receives. A middleware that returns [`BeforeOutcome::Allow`]
/// short-circuits the remaining `before` chain, so execution-affecting argument
/// rewrites must also precede every middleware that can return `Allow`.
///
/// Display-only redaction is different: it must never mutate the executable
/// [`ToolCall`]. Apply it only to a separate presentation payload or result.
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
/// or [`DenyTurn`](BeforeOutcome::DenyTurn) blocks; an [`Allow`](BeforeOutcome::Allow)
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
    /// Block the call and terminate the turn after every call in the current
    /// assistant tool batch has received a paired result. Reserved for hard
    /// policy boundaries where retrying through another shell spelling is unsafe.
    DenyTurn { reason: String },
    /// Hard policy boundary with a machine-readable, non-sensitive recovery
    /// contract for drivers. The rejected operation remains non-overridable.
    DenyTurnWithIntervention {
        reason: String,
        intervention: PolicyIntervention,
    },
}

impl BeforeOutcome {
    /// Block a call with a reason — ergonomic constructor (former `Err(reason)`).
    pub fn deny(reason: impl Into<String>) -> Self {
        BeforeOutcome::Deny {
            reason: reason.into(),
        }
    }
    /// Block a call and terminate the current turn after preserving tool pairing.
    pub fn deny_turn(reason: impl Into<String>) -> Self {
        BeforeOutcome::DenyTurn {
            reason: reason.into(),
        }
    }
    pub fn deny_turn_with_intervention(
        reason: impl Into<String>,
        intervention: PolicyIntervention,
    ) -> Self {
        BeforeOutcome::DenyTurnWithIntervention {
            reason: reason.into(),
            intervention,
        }
    }
    /// True iff this is a blocking decision.
    pub fn is_deny(&self) -> bool {
        matches!(
            self,
            BeforeOutcome::Deny { .. }
                | BeforeOutcome::DenyTurn { .. }
                | BeforeOutcome::DenyTurnWithIntervention { .. }
        )
    }
    /// The deny reason, if this is a [`Deny`](BeforeOutcome::Deny).
    pub fn deny_reason(&self) -> Option<&str> {
        match self {
            BeforeOutcome::Deny { reason }
            | BeforeOutcome::DenyTurn { reason }
            | BeforeOutcome::DenyTurnWithIntervention { reason, .. } => Some(reason.as_str()),
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
