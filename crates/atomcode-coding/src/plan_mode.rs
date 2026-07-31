//! Plan mode — read-only exploration, no edits.
//!
//! v1 exposed `/plan` (and `SetPlanMode`): the agent explores and presents a plan
//! WITHOUT mutating anything. This re-implements the ENFORCEMENT on the new stack as
//! a [`ToolMiddleware`] that, while active, blocks every `Risky` tool (the kernel's
//! own risk metadata already marks the mutating ones: write/edit/bash). Read-only
//! tools (read_file, grep, list_*, symbols, web, …) stay available.
//!
//! The flag is an `Arc<AtomicBool>` so `CodingRuntime::set_mode` can toggle it live
//! without a respawn — like the shared cwd handle.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use atomcode_capabilities::tools::{
    ApprovalRequest, PermissionDecision, PermissionStore, APPROVAL_KIND,
};
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::Message;
use atomcode_kernel::middleware::{BeforeOutcome, ToolMiddleware};
use atomcode_kernel::request::RequestCtx;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolCall};

/// Enforces plan mode (read-only exploration) while active. Share the `Arc<AtomicBool>`
/// with the driver to toggle it live.
///
/// Policy while active (mirrors codex `readOnlyHint` + Claude Code's prompt):
/// - built-in **`Risky`** tools (bash/edit/write) → **hard-blocked** (plan's local
///   read-only guarantee — the model must present a plan first);
/// - **MCP tools declared `readOnlyHint: true`** → **allowed** (an external read-only
///   query has no side effects and is exactly what planning research needs);
/// - **other MCP tools** (mutating / unannotated, incl. `trust: true` servers) →
///   **prompt** instead of hard-block, so the user can allow a needed external call or
///   deny a risky one. A trusted server can't silently write here because we prompt
///   regardless of trust.
pub struct PlanModeGate {
    active: Arc<AtomicBool>,
    /// Session grants for mutating MCP tools the user approved "always" while in plan
    /// mode — keyed by the tool's full name so a repeat call skips the prompt. Supplied
    /// by `CodingParts` (shared, not rebuilt in `assemble`) so it survives a respawn /
    /// model-swap, matching how the write gate and approval middleware persist grants.
    mcp_grants: Arc<dyn PermissionStore>,
}

impl PlanModeGate {
    pub fn new(active: Arc<AtomicBool>, mcp_grants: Arc<dyn PermissionStore>) -> Self {
        Self { active, mcp_grants }
    }

    /// The hard-block message for a built-in mutating tool under plan mode.
    fn blocked(name: &str) -> BeforeOutcome {
        BeforeOutcome::deny(format!(
            "plan mode is active — `{name}` would modify the workspace and is blocked. Only \
             read-only tools are allowed: explore and present a plan for the user to approve \
             before making changes."
        ))
    }
}

#[async_trait]
impl ToolMiddleware for PlanModeGate {
    async fn before(
        &self,
        call: &mut ToolCall,
        tool: &Arc<dyn Tool>,
        rt: &RequestCtx,
    ) -> BeforeOutcome {
        if !self.active.load(Ordering::Relaxed) {
            return BeforeOutcome::Proceed;
        }

        if call.name.starts_with("mcp__") {
            // A server-declared read-only external query can't modify anything → proceed.
            // It is `Safe`, so the ApprovalMiddleware won't prompt for it either; a later
            // guard (e.g. SensitivePathGate) may still fire if the args touch a secret path,
            // which is the intended exfiltration guard — same as outside plan mode.
            if tool.read_only_hint() {
                return BeforeOutcome::Proceed;
            }
            // Mutating / unannotated MCP tool: prompt instead of hard-blocking. Owns the
            // decision (returns Allow/Deny) so the generic ApprovalMiddleware after it
            // never double-prompts — same pattern as the write gate.
            if self.mcp_grants.is_granted(&call.name) {
                return BeforeOutcome::Allow {
                    reason: Some("approved this session".into()),
                };
            }
            let payload = serde_json::to_value(ApprovalRequest {
                call_id: call.id.clone(),
                tool: tool.name().to_string(),
                args: call.arguments.clone(),
            })
            .unwrap_or(serde_json::Value::Null);
            return match PermissionDecision::from_value(&rt.request(APPROVAL_KIND, payload).await) {
                PermissionDecision::AllowOnce => BeforeOutcome::Allow {
                    reason: Some("approved once (plan mode)".into()),
                },
                PermissionDecision::AllowAlways => {
                    self.mcp_grants.grant(&call.name);
                    BeforeOutcome::Allow {
                        reason: Some("approved always (plan mode)".into()),
                    }
                }
                PermissionDecision::Deny => BeforeOutcome::deny(format!(
                    "plan mode: `{}` was not approved — present a plan and switch to build mode \
                     to run it.",
                    call.name
                )),
            };
        }

        // Built-in mutating tools (bash/edit/write) stay hard-blocked.
        if tool.risk(&call.arguments) == RiskLevel::Risky {
            return Self::blocked(&call.name);
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
            messages.push(Message::user(
                atomcode_capabilities::reminder::system_reminder(PLAN_MODE_REMINDER_BODY),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_capabilities::tools::InMemoryPermissionStore;
    use atomcode_kernel::testkit::{EchoTool, RiskyWriteTool};
    use atomcode_kernel::tool::{ToolContext, ToolResult};

    fn rt() -> RequestCtx {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        RequestCtx::new(tx, None)
    }

    fn grants() -> Arc<dyn PermissionStore> {
        Arc::new(InMemoryPermissionStore::new())
    }

    /// An rt whose approval requests time out fast (no driver answers in tests),
    /// degrading to `Null` → `Deny` — so the prompt path resolves instead of hanging.
    fn rt_timeout() -> RequestCtx {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        RequestCtx::new(tx, Some(std::time::Duration::from_millis(20)))
    }

    /// A `Safe`, read-only tool with an `mcp__*` name — mimics an MCP tool the server
    /// annotated `readOnlyHint: true`.
    struct ReadOnlyMcpTool;
    #[async_trait]
    impl Tool for ReadOnlyMcpTool {
        fn name(&self) -> &str {
            "mcp__docs__query"
        }
        fn description(&self) -> &str {
            "read-only query"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        fn read_only_hint(&self) -> bool {
            true
        }
        async fn execute(&self, _args: &str, _ctx: &ToolContext) -> ToolResult {
            ToolResult {
                call_id: String::new(),
                content: String::new(),
                is_error: false,
                images: vec![],
            }
        }
    }

    #[tokio::test]
    async fn blocks_risky_only_when_active() {
        let flag = Arc::new(AtomicBool::new(false));
        let gate = PlanModeGate::new(flag.clone(), grants());
        let risky: Arc<dyn Tool> = Arc::new(RiskyWriteTool); // always Risky
        let safe: Arc<dyn Tool> = Arc::new(EchoTool); // Safe
        let mut call = ToolCall {
            id: "c".into(),
            name: "risky_write".into(),
            arguments: "{}".into(),
        };

        // Inactive: nothing blocked.
        assert!(!gate.before(&mut call, &risky, &rt()).await.is_deny());

        // Active: Risky blocked, Safe allowed.
        flag.store(true, Ordering::Relaxed);
        assert!(gate.before(&mut call, &risky, &rt()).await.is_deny());
        let mut safe_call = ToolCall {
            id: "c".into(),
            name: "echo".into(),
            arguments: "{}".into(),
        };
        assert!(!gate.before(&mut safe_call, &safe, &rt()).await.is_deny());
    }

    /// A read-only MCP tool (`readOnlyHint: true`) is ALLOWED in plan mode — an external
    /// read-only query can't modify anything, and it's exactly what planning research needs.
    #[tokio::test]
    async fn read_only_mcp_allowed_in_plan_mode() {
        let flag = Arc::new(AtomicBool::new(true));
        let gate = PlanModeGate::new(flag, grants());
        let ro: Arc<dyn Tool> = Arc::new(ReadOnlyMcpTool);
        let mut call = ToolCall {
            id: "c".into(),
            name: "mcp__docs__query".into(),
            arguments: "{}".into(),
        };
        let out = gate.before(&mut call, &ro, &rt()).await;
        assert!(
            !out.is_deny(),
            "read-only MCP must be allowed in plan mode, got {out:?}"
        );
    }

    /// A mutating / unannotated MCP tool is NOT hard-blocked in plan mode — it PROMPTS
    /// (Claude Code parity). With no driver to answer, the prompt times out → deny, but
    /// the deny reason proves it went through the approval path ("not approved"), not the
    /// hard-block path ("blocked").
    #[tokio::test]
    async fn mutating_mcp_prompts_not_hard_blocked_in_plan_mode() {
        let flag = Arc::new(AtomicBool::new(true));
        let gate = PlanModeGate::new(flag, grants());
        let safe: Arc<dyn Tool> = Arc::new(EchoTool); // read_only_hint()==false, mcp__ name
        let mut call = ToolCall {
            id: "c".into(),
            name: "mcp__docs__delete".into(),
            arguments: "{}".into(),
        };
        let out = gate.before(&mut call, &safe, &rt_timeout()).await;
        assert!(out.is_deny(), "un-answered prompt degrades to deny");
        assert!(
            out.deny_reason().unwrap().contains("not approved"),
            "must reach the PROMPT path, not the hard-block path: {:?}",
            out.deny_reason()
        );
    }

    /// The "always allow" grant lives in a store supplied by `CodingParts`, so it
    /// survives a respawn (model-swap / MCP-reload) that rebuilds the gate. Two gates
    /// sharing ONE store model that: a grant seen by the second gate skips the prompt.
    #[tokio::test]
    async fn always_grant_survives_gate_rebuild_via_shared_store() {
        let flag = Arc::new(AtomicBool::new(true));
        let store = grants();
        // First gate records an "always" grant (as the AllowAlways arm would).
        store.grant("mcp__docs__delete");
        // A freshly-built gate (post-respawn) sharing the SAME store honors it — and
        // short-circuits BEFORE rt.request, so the no-timeout rt() can't hang.
        let gate = PlanModeGate::new(flag, store);
        let safe: Arc<dyn Tool> = Arc::new(EchoTool);
        let mut call = ToolCall {
            id: "c".into(),
            name: "mcp__docs__delete".into(),
            arguments: "{}".into(),
        };
        let out = gate.before(&mut call, &safe, &rt()).await;
        assert!(
            matches!(out, BeforeOutcome::Allow { .. }),
            "shared grant must skip the prompt after a respawn, got {out:?}"
        );
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
        assert_eq!(
            msgs[..2],
            before[..],
            "the cached prefix must be byte-identical"
        );
        assert!(
            msgs[2].text.contains("PLAN MODE"),
            "tail carries the plan reminder: {:?}",
            msgs[2].text
        );
        assert!(
            msgs[2].text.to_lowercase().contains("stop"),
            "must tell the model to STOP after planning"
        );
    }
}
