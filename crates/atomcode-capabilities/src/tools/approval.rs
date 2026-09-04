//! A generic, reusable **approval gate** for risky tool calls (L1 MECHANISM; policy
//! injected). The kernel deliberately keeps approval OUT of L0 (see
//! [`atomcode_kernel::tool`]); this is the composable [`ToolMiddleware`] a
//! specialization wires in to turn a tool's advisory `risk()` into an actual gate.
//!
//! For each call it: (1) lets a `Safe` call (arg-aware) through untouched; (2) for a
//! `Risky` call, returns `Ok` if the injected [`PermissionStore`] already granted it;
//! (3) otherwise round-trips the driver via `rt.request(kind, {tool, args})` and maps
//! the decision → allow-once (`Ok`) / allow-always (`Ok` + remember) / deny (`Err`,
//! which blocks the call). The driver owns the actual allow/deny UX; the store + the
//! request `kind` are injected. Register this BEFORE any arg-rewriting middleware so
//! the user approves the bytes that actually execute (see the [`ToolMiddleware`]
//! ordering contract).

use async_trait::async_trait;
use atomcode_kernel::middleware::{BeforeOutcome, ToolMiddleware};
use atomcode_kernel::request::RequestCtx;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolCall};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// The default `AgentEvent::Request.kind` of an approval round-trip — what a driver
/// matches on to render the approval prompt (overridable via
/// [`ApprovalMiddleware::with_kind`]).
pub const APPROVAL_KIND: &str = "approval";

/// The TYPED wire contract of the approval round-trip, so a driver never hand-rolls
/// the JSON shapes. Byte-compatible with what the middleware sends/parses:
/// the `Request.payload` deserializes into [`ApprovalRequest`]; the driver answers
/// `AgentCommand::Respond { value: serde_json::to_value(ApprovalResponse)? }`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    /// The originating model tool-call id. Drivers use it to correlate an approval
    /// prompt with the later started/result events for the same call.
    #[serde(default)]
    pub call_id: String,
    /// The tool about to execute.
    pub tool: String,
    /// The EXACT argument bytes that will execute (approve-what-runs contract).
    pub args: String,
}

/// The driver's answer. `decision` is `"allow"` / `"allow_always"` / `"deny"`
/// (anything else fail-closes to deny); `remember: true` upgrades an `"allow"` to
/// allow-always. See [`PermissionDecision::from_value`] for the parse rules.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalResponse {
    pub decision: String,
    #[serde(default)]
    pub remember: bool,
}

impl ApprovalResponse {
    pub fn allow() -> Self {
        Self {
            decision: "allow".into(),
            remember: false,
        }
    }
    pub fn allow_always() -> Self {
        Self {
            decision: "allow_always".into(),
            remember: false,
        }
    }
    pub fn deny() -> Self {
        Self {
            decision: "deny".into(),
            remember: false,
        }
    }
}

/// The decision a driver returns for an approval round-trip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PermissionDecision {
    /// Allow this one call.
    AllowOnce,
    /// Allow AND remember — the store caches the grant so the identical call is not
    /// asked again this session.
    AllowAlways,
    /// Deny — the middleware blocks the call with `Err`.
    Deny,
}

impl PermissionDecision {
    /// Parse a driver `Respond` value. Accepts `{"decision":"allow"|"allow_always"|
    /// "deny", "remember":bool}`. Anything unrecognized / `Null` (a crashed or
    /// timed-out driver) is treated as `Deny` — FAIL CLOSED.
    pub fn from_value(v: &serde_json::Value) -> Self {
        let decision = v.get("decision").and_then(|x| x.as_str()).unwrap_or("deny");
        let remember = v.get("remember").and_then(|x| x.as_bool()).unwrap_or(false);
        match decision {
            "allow_always" => PermissionDecision::AllowAlways,
            "allow" if remember => PermissionDecision::AllowAlways,
            "allow" => PermissionDecision::AllowOnce,
            _ => PermissionDecision::Deny,
        }
    }
}

/// Parse the wire string used by the daemon's permission endpoints
/// (`/chat/permission`, `/live/permission`) into a decision. Mirrors the
/// retired `core::tool::parse_permission_decision` wire mapping.
pub fn parse_permission_decision(s: &str) -> PermissionDecision {
    match s {
        "allow" => PermissionDecision::AllowOnce,
        "always_allow" => PermissionDecision::AllowAlways,
        _ => PermissionDecision::Deny,
    }
}

/// Session-scoped grant cache. The middleware consults it before round-tripping and
/// records `AllowAlways` grants into it. Pluggable so a specialization can back it
/// with anything (in-memory, persisted, per-project policy, …).
pub trait PermissionStore: Send + Sync {
    /// Has this exact `(tool, args)` key already been granted "always"?
    fn is_granted(&self, key: &str) -> bool;
    /// Record an "always" grant for this key.
    fn grant(&self, key: &str);
}

/// Default in-memory grant cache — one session's "remember" set.
#[derive(Default)]
pub struct InMemoryPermissionStore {
    granted: Mutex<HashSet<String>>,
}

impl InMemoryPermissionStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl PermissionStore for InMemoryPermissionStore {
    fn is_granted(&self, key: &str) -> bool {
        // Poison-recover instead of unwrap: an approval gate runs in the tool path and
        // MUST NOT panic (the kernel runs panic=abort). The critical section only touches
        // an infallible HashSet, so the guarded set is never left inconsistent.
        self.granted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(key)
    }
    fn grant(&self, key: &str) {
        self.granted
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_string());
    }
}

/// The generic approval gate. Clone-cheap (Arc-backed store).
pub struct ApprovalMiddleware {
    store: Arc<dyn PermissionStore>,
    kind: String,
}

impl ApprovalMiddleware {
    /// Build over an injected store. `kind` defaults to `"approval"` (the driver
    /// matches `AgentEvent::Request.kind` on it).
    pub fn new(store: Arc<dyn PermissionStore>) -> Self {
        Self {
            store,
            kind: APPROVAL_KIND.to_string(),
        }
    }
    /// Convenience: gate with a fresh in-memory store.
    pub fn in_memory() -> Self {
        Self::new(Arc::new(InMemoryPermissionStore::new()))
    }
    /// Override the round-trip request `kind`.
    pub fn with_kind(mut self, kind: impl Into<String>) -> Self {
        self.kind = kind.into();
        self
    }
    /// Key under which an `AllowAlways` grant is cached. The SCOPE comes from the
    /// tool (`Tool::always_grant_scope`), NOT blindly from the raw args: a
    /// file-mutation tool reports a tool-wide scope so "Always" covers every later
    /// edit this session (v1 parity), while `bash` keeps the default per-command
    /// scope so approving one destructive command never blanket-approves others.
    fn grant_key(call: &ToolCall, tool: &dyn Tool) -> String {
        format!(
            "{}::{}",
            call.name,
            tool.always_grant_scope(&call.arguments)
        )
    }
}

/// The fail-closed `Deny` for a DEGRADED approval round-trip — a `Null` response, which
/// the kernel emits when the driver's oneshot sender was dropped, the bounded round-trip
/// timed out, or the turn was cancelled (see `RequestCtx::request` / `cancel_pending`). A
/// genuine user "deny" arrives as `{"decision":"deny"}` (non-null); this is NOT that. We
/// fail closed either way, but surface the difference — on stderr AND in the deny reason
/// the model/UI sees — so an internal channel failure can be told apart from a real user
/// denial (issue #173). SHARED by every gate that round-trips the driver so the contract +
/// wording can never drift between copies.
pub fn approval_channel_failure_deny(tool_name: &str) -> BeforeOutcome {
    eprintln!(
        "[approval] no decision received for tool '{tool_name}' (driver disconnected, \
         timed out, or cancelled); denying due to internal channel failure, not a user decision"
    );
    BeforeOutcome::deny(format!(
        "approval unresolved for '{tool_name}': no decision received (driver disconnected, \
         timed out, or cancelled) — internal channel failure, not a user denial"
    ))
}

/// Round-trip the driver for an approval decision on `call` under request `kind`, built
/// from the exported typed [`ApprovalRequest`] contract so the wire shape can never drift.
/// Returns the parsed [`PermissionDecision`], or `Err(BeforeOutcome::Deny)` when the
/// round-trip DEGRADED to `Null` (fail closed via [`approval_channel_failure_deny`]). The
/// decision→outcome MAPPING stays with each caller (they diverge: the generic gate
/// remembers `AllowAlways` in its store and maps allow→`Proceed`, while a hook-forced ask
/// maps allow→`Allow` to short-circuit downstream gates), so only the shared round-trip +
/// degraded-path handling is factored here.
pub async fn request_approval_decision(
    rt: &RequestCtx,
    kind: &str,
    call: &ToolCall,
    tool_name: &str,
) -> Result<PermissionDecision, BeforeOutcome> {
    let payload = serde_json::to_value(ApprovalRequest {
        call_id: call.id.clone(),
        tool: tool_name.to_string(),
        args: call.arguments.clone(),
    })
    .unwrap_or(serde_json::Value::Null);
    let response = rt.request(kind, payload).await;
    if response.is_null() {
        return Err(approval_channel_failure_deny(tool_name));
    }
    Ok(PermissionDecision::from_value(&response))
}

#[async_trait]
impl ToolMiddleware for ApprovalMiddleware {
    async fn before(
        &self,
        call: &mut ToolCall,
        tool: &Arc<dyn Tool>,
        rt: &RequestCtx,
    ) -> BeforeOutcome {
        // Arg-aware: a Safe call needs no approval.
        if tool.risk(&call.arguments) == RiskLevel::Safe {
            return BeforeOutcome::Proceed;
        }
        // Session grant cache: an identical risky call already approved-always.
        let key = Self::grant_key(call, tool.as_ref());
        if self.store.is_granted(&key) {
            return BeforeOutcome::Proceed;
        }
        match request_approval_decision(rt, &self.kind, call, tool.name()).await {
            Err(degraded) => degraded, // Null → fail closed (channel failure, not a user deny).
            Ok(PermissionDecision::AllowOnce) => BeforeOutcome::Proceed,
            Ok(PermissionDecision::AllowAlways) => {
                self.store.grant(&key);
                BeforeOutcome::Proceed
            }
            Ok(PermissionDecision::Deny) => {
                // Deny reason goes back to the model as the tool result; it made the call, so
                // don't echo its full arguments back (token waste + the arg preview already
                // renders in the tool header). Name the tool and the policy — that's the signal.
                BeforeOutcome::deny(format!("denied by approval policy: {}", tool.name()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::write::WriteFileTool;
    use atomcode_kernel::event::AgentEvent;
    use atomcode_kernel::tool::ToolCall;
    use std::time::Duration;
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn parse_permission_decision_maps_daemon_wire() {
        assert_eq!(
            parse_permission_decision("allow"),
            PermissionDecision::AllowOnce
        );
        assert_eq!(
            parse_permission_decision("always_allow"),
            PermissionDecision::AllowAlways
        );
        assert_eq!(parse_permission_decision("deny"), PermissionDecision::Deny);
        assert_eq!(
            parse_permission_decision("garbage"),
            PermissionDecision::Deny
        );
    }

    fn risky_call() -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: "write_file".into(),
            arguments: r#"{"file_path":"a.txt","content":"x"}"#.into(),
        }
    }
    fn safe_call() -> ToolCall {
        // read_file is Safe; use a risk-Safe tool's args. We reuse the write tool's
        // risk via a Safe arg? No — write is always Risky. Use ReadFileTool instead.
        ToolCall {
            id: "2".into(),
            name: "read_file".into(),
            arguments: r#"{"file_path":"a.txt"}"#.into(),
        }
    }

    /// REGRESSION: "总是 / Always" must be tool-wide for file-mutation tools (v1
    /// parity) — approving one edit auto-approves every later edit this session —
    /// while `bash` stays per-command so approving one destructive command never
    /// blanket-approves another. The bug: the grant key included the full args, so
    /// each distinct edit re-prompted ("Always" degraded to "allow once").
    #[test]
    fn always_grant_is_tool_wide_for_edits_and_bash_but_per_command_for_secrets() {
        use crate::tools::bash::BashTool;
        use crate::tools::edit::EditFileTool;

        // edit_file: two DIFFERENT edits collapse to the SAME grant key.
        let edit: Arc<dyn Tool> = Arc::new(EditFileTool);
        let a = ToolCall {
            id: "1".into(),
            name: "edit_file".into(),
            arguments: r#"{"file_path":"a.rs","old_string":"x","new_string":"y"}"#.into(),
        };
        let b = ToolCall {
            id: "2".into(),
            name: "edit_file".into(),
            arguments: r#"{"file_path":"b.rs","old_string":"p","new_string":"q"}"#.into(),
        };
        assert_eq!(
            ApprovalMiddleware::grant_key(&a, edit.as_ref()),
            ApprovalMiddleware::grant_key(&b, edit.as_ref()),
            "edit_file 'Always' must grant the whole tool, not just one exact call"
        );

        // bash: two DIFFERENT ordinary commands SHARE a grant key. "Always" is a decision
        // about this session's shell, and a per-command key made the option useless — the
        // next command always re-prompted.
        let bash: Arc<dyn Tool> = Arc::new(BashTool);
        let c1 = ToolCall {
            id: "3".into(),
            name: "bash".into(),
            arguments: r#"{"command":"rm -rf foo"}"#.into(),
        };
        let c2 = ToolCall {
            id: "4".into(),
            name: "bash".into(),
            arguments: r#"{"command":"rm -rf bar"}"#.into(),
        };
        assert_eq!(
            ApprovalMiddleware::grant_key(&c1, bash.as_ref()),
            ApprovalMiddleware::grant_key(&c2, bash.as_ref()),
            "bash 'Always' must cover the session, not just the one exact command"
        );

        // …but a SENSITIVE target keeps its own per-command key, so a session-wide bash
        // grant can never pre-approve a later secret access.
        let secret = ToolCall {
            id: "5".into(),
            name: "bash".into(),
            arguments: r#"{"command":"cat ~/.ssh/id_rsa"}"#.into(),
        };
        assert_ne!(
            ApprovalMiddleware::grant_key(&c1, bash.as_ref()),
            ApprovalMiddleware::grant_key(&secret, bash.as_ref()),
            "a sensitive bash target must not share the tool-wide grant"
        );
    }

    #[test]
    fn decision_parsing_fails_closed() {
        use serde_json::json;
        assert_eq!(
            PermissionDecision::from_value(&json!({"decision":"allow"})),
            PermissionDecision::AllowOnce
        );
        assert_eq!(
            PermissionDecision::from_value(&json!({"decision":"allow","remember":true})),
            PermissionDecision::AllowAlways
        );
        assert_eq!(
            PermissionDecision::from_value(&json!({"decision":"allow_always"})),
            PermissionDecision::AllowAlways
        );
        assert_eq!(
            PermissionDecision::from_value(&json!({"decision":"deny"})),
            PermissionDecision::Deny
        );
        assert_eq!(
            PermissionDecision::from_value(&serde_json::Value::Null),
            PermissionDecision::Deny
        );
        assert_eq!(
            PermissionDecision::from_value(&json!({})),
            PermissionDecision::Deny
        );
    }

    #[tokio::test]
    async fn safe_call_passes_without_round_trip() {
        let (tx, _rx) = unbounded_channel::<AgentEvent>();
        let rt = RequestCtx::new(tx, Some(Duration::from_millis(50)));
        let mw = ApprovalMiddleware::in_memory();
        let tool: Arc<dyn Tool> = Arc::new(crate::tools::read::ReadFileTool::default());
        let mut call = safe_call();
        // Safe → Proceed without ever awaiting the driver (which never responds here).
        assert!(!mw.before(&mut call, &tool, &rt).await.is_deny());
    }

    #[tokio::test]
    async fn pre_granted_risky_call_passes() {
        let (tx, _rx) = unbounded_channel::<AgentEvent>();
        let rt = RequestCtx::new(tx, Some(Duration::from_millis(50)));
        let store = Arc::new(InMemoryPermissionStore::new());
        let call = risky_call();
        let tool: Arc<dyn Tool> = Arc::new(WriteFileTool);
        store.grant(&ApprovalMiddleware::grant_key(&call, tool.as_ref()));
        let mw = ApprovalMiddleware::new(store);
        let mut c = call;
        assert!(!mw.before(&mut c, &tool, &rt).await.is_deny());
    }

    #[tokio::test]
    async fn risky_call_denied_when_driver_silent() {
        // No driver drains the request → the bounded round-trip times out → Null →
        // Deny → Err (fail closed). The deny reason must mark this as an INTERNAL
        // channel failure (not a user denial) for observability (issue #173).
        let (tx, _rx) = unbounded_channel::<AgentEvent>();
        let rt = RequestCtx::new(tx, Some(Duration::from_millis(20)));
        let mw = ApprovalMiddleware::in_memory();
        let tool: Arc<dyn Tool> = Arc::new(WriteFileTool);
        let mut call = risky_call();
        let res = mw.before(&mut call, &tool, &rt).await;
        assert!(res.is_deny(), "silent driver must fail closed");
        let reason = res.deny_reason().unwrap();
        assert!(
            reason.contains("internal channel failure") && reason.contains("not a user"),
            "a degraded (Null) round-trip must be distinguishable from a user deny: {reason}"
        );
    }

    /// The exported typed contract must stay byte-compatible with the wire shapes
    /// the middleware actually sends / the parser actually accepts.
    #[test]
    fn typed_contract_matches_wire_shapes() {
        // Request side: what the middleware emits parses as ApprovalRequest.
        let payload = serde_json::to_value(ApprovalRequest {
            call_id: "call_1".into(),
            tool: "bash".into(),
            args: "{\"cmd\":\"ls\"}".into(),
        })
        .unwrap();
        assert_eq!(
            payload,
            serde_json::json!({ "call_id": "call_1", "tool": "bash", "args": "{\"cmd\":\"ls\"}" })
        );

        // Older approval payloads without call_id still parse; they simply cannot
        // correlate a pre-execution prompt with a later ToolStarted row.
        let legacy: ApprovalRequest =
            serde_json::from_value(serde_json::json!({ "tool": "bash", "args": "{}" })).unwrap();
        assert_eq!(legacy.call_id, "");

        // Response side: each constructor round-trips through from_value exactly.
        let v = serde_json::to_value(ApprovalResponse::allow()).unwrap();
        assert_eq!(
            PermissionDecision::from_value(&v),
            PermissionDecision::AllowOnce
        );
        let v = serde_json::to_value(ApprovalResponse::allow_always()).unwrap();
        assert_eq!(
            PermissionDecision::from_value(&v),
            PermissionDecision::AllowAlways
        );
        let v = serde_json::to_value(ApprovalResponse {
            decision: "allow".into(),
            remember: true,
        })
        .unwrap();
        assert_eq!(
            PermissionDecision::from_value(&v),
            PermissionDecision::AllowAlways
        );
        let v = serde_json::to_value(ApprovalResponse::deny()).unwrap();
        assert_eq!(PermissionDecision::from_value(&v), PermissionDecision::Deny);
        assert_eq!(APPROVAL_KIND, "approval");
    }
}
