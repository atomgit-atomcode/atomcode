//! Plan mode — read-only exploration, no edits.
//!
//! v1 exposed `/plan` (and `SetPlanMode`): the agent explores and presents a plan
//! WITHOUT mutating anything. This re-implements the ENFORCEMENT on the new stack as
//! a [`ToolMiddleware`] that, while active, blocks every `Risky` tool (the kernel's
//! own risk metadata already marks the mutating ones: write/edit/bash). Read-only
//! tools (read_file, grep, list_*, symbols, web, …) stay available.
//!
//! The flag is an `Arc<AtomicBool>` so the driver can toggle it live (the bridge maps
//! `SetPlanMode` onto it) without a respawn — like the shared cwd handle.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::Message;
use atomcode_kernel::middleware::{BeforeOutcome, ToolMiddleware};
use atomcode_kernel::request::RequestCtx;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolCall};

/// Blocks mutating (`Risky`) tools while plan mode is active. Share the same
/// `Arc<AtomicBool>` with the driver to toggle it live.
pub struct PlanModeGate {
    active: Arc<AtomicBool>,
}

impl PlanModeGate {
    pub fn new(active: Arc<AtomicBool>) -> Self {
        Self { active }
    }
}

#[async_trait]
impl ToolMiddleware for PlanModeGate {
    async fn before(
        &self,
        call: &mut ToolCall,
        tool: &Arc<dyn Tool>,
        _rt: &RequestCtx,
    ) -> BeforeOutcome {
        // Block `Risky` tools, AND every MCP tool (`mcp__*`) regardless of its risk:
        // an MCP server is external code whose side effects we can't verify, and a
        // `trust: true` server now reports `Safe` — without the name check a trusted
        // MCP write/exec tool would slip past plan mode's read-only guarantee. (MCP
        // tools were already blocked here before trust existed, since they were all
        // `Risky`; this preserves that.)
        if self.active.load(Ordering::Relaxed)
            && (tool.risk(&call.arguments) == RiskLevel::Risky || call.name.starts_with("mcp__"))
        {
            return BeforeOutcome::deny(format!(
                "plan mode is active — `{}` would modify the workspace and is blocked. Only \
                 read-only tools are allowed: explore and present a plan for the user to approve \
                 before making changes.",
                call.name
            ));
        }
        BeforeOutcome::Proceed
    }
}

/// The standing plan-mode reminder BODY. Kept OUT of the system prompt (so toggling plan
/// mode never perturbs the cached prefix) and carried instead as an EPHEMERAL per-request
/// tail by [`PlanModeReminderHook`], which wraps it via the shared
/// [`system_reminder`](atomcode_capabilities::reminder::system_reminder) constructor so the
/// `<system-reminder>` convention lives in ONE place. The [`PlanModeGate`] blocks mutating
/// TOOLS, but nothing stops the model from writing the implementation straight into its
/// reply — this keeps it planning. (Ported from core's `plan_mode_turn_reminder`.)
const PLAN_MODE_REMINDER_BODY: &str = "\
PLAN MODE is active. Do NOT create, edit, or delete files, and do NOT write out the \
implementation — not even as code blocks in your reply. Investigate with read-only tools, \
then present a concise implementation plan and STOP, waiting for the user to review and \
switch to build mode. Writing the full solution now defeats the purpose of plan mode.";

/// Injects the wrapped [`PLAN_MODE_REMINDER_BODY`] as an ephemeral request tail while plan
/// mode is active.
/// Shares the same `Arc<AtomicBool>` as the [`PlanModeGate`] so they toggle together.
/// Cache-safe: the tail is appended in `pre_request` (not stored), so the cached prefix is
/// untouched and an OFF↔ON toggle only changes ephemeral bytes past the prefix.
pub struct PlanModeReminderHook {
    active: Arc<AtomicBool>,
}

impl PlanModeReminderHook {
    pub fn new(active: Arc<AtomicBool>) -> Self {
        Self { active }
    }
}

#[async_trait]
impl LifecycleHooks for PlanModeReminderHook {
    async fn pre_request(&self, messages: &mut Vec<Message>, _ctx: &TurnCtx) {
        if self.active.load(Ordering::Relaxed) {
            messages.push(Message::user(atomcode_capabilities::reminder::system_reminder(
                PLAN_MODE_REMINDER_BODY,
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::testkit::{EchoTool, RiskyWriteTool};

    fn rt() -> RequestCtx {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        RequestCtx::new(tx, None)
    }

    #[tokio::test]
    async fn blocks_risky_only_when_active() {
        let flag = Arc::new(AtomicBool::new(false));
        let gate = PlanModeGate::new(flag.clone());
        let risky: Arc<dyn Tool> = Arc::new(RiskyWriteTool); // always Risky
        let safe: Arc<dyn Tool> = Arc::new(EchoTool); // Safe
        let mut call =
            ToolCall { id: "c".into(), name: "risky_write".into(), arguments: "{}".into() };

        // Inactive: nothing blocked.
        assert!(!gate.before(&mut call, &risky, &rt()).await.is_deny());

        // Active: Risky blocked, Safe allowed.
        flag.store(true, Ordering::Relaxed);
        assert!(gate.before(&mut call, &risky, &rt()).await.is_deny());
        let mut safe_call = ToolCall { id: "c".into(), name: "echo".into(), arguments: "{}".into() };
        assert!(!gate.before(&mut safe_call, &safe, &rt()).await.is_deny());
    }

    /// A `trust: true` MCP tool now reports `Safe`, but plan mode must STILL block it
    /// (an MCP server's side effects can't be verified). Uses a Safe tool with an
    /// `mcp__*` call name so the only possible reason for the deny is the name check.
    #[tokio::test]
    async fn blocks_mcp_tools_in_plan_mode_even_when_safe() {
        let flag = Arc::new(AtomicBool::new(true));
        let gate = PlanModeGate::new(flag);
        let safe: Arc<dyn Tool> = Arc::new(EchoTool); // risk() == Safe
        let mut mcp_call =
            ToolCall { id: "c".into(), name: "mcp__docs__query".into(), arguments: "{}".into() };
        assert!(gate.before(&mut mcp_call, &safe, &rt()).await.is_deny());
    }

    #[tokio::test]
    async fn reminder_tail_only_when_active_prefix_unchanged() {
        let flag = Arc::new(AtomicBool::new(false));
        let hook = PlanModeReminderHook::new(flag.clone());
        let mut msgs = vec![Message::system("sys"), Message::user("hi")];
        let before = msgs.clone();

        // Build mode: nothing injected — the last user turn stays clean + cacheable.
        hook.pre_request(&mut msgs, &TurnCtx::default()).await;
        assert_eq!(msgs, before, "build mode must not inject a plan reminder");

        // Plan mode: exactly one ephemeral tail; the cached prefix is byte-identical.
        flag.store(true, Ordering::Relaxed);
        hook.pre_request(&mut msgs, &TurnCtx::default()).await;
        assert_eq!(msgs.len(), 3, "exactly one reminder tail appended");
        assert_eq!(msgs[..2], before[..], "the cached prefix must be byte-identical");
        assert!(msgs[2].text.contains("PLAN MODE"), "tail carries the plan reminder: {:?}", msgs[2].text);
        assert!(msgs[2].text.to_lowercase().contains("stop"), "must tell the model to STOP after planning");
    }
}
