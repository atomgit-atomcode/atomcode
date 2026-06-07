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
use atomcode_kernel::middleware::ToolMiddleware;
use atomcode_kernel::request::RequestCtx;
use atomcode_kernel::tool::{RiskLevel, Tool, ToolCall};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

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
        self.granted.lock().unwrap().contains(key)
    }
    fn grant(&self, key: &str) {
        self.granted.lock().unwrap().insert(key.to_string());
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
        Self { store, kind: "approval".to_string() }
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
    fn grant_key(call: &ToolCall) -> String {
        format!("{}::{}", call.name, call.arguments)
    }
}

#[async_trait]
impl ToolMiddleware for ApprovalMiddleware {
    async fn before(
        &self,
        call: &mut ToolCall,
        tool: &Arc<dyn Tool>,
        rt: &RequestCtx,
    ) -> Result<(), String> {
        // Arg-aware: a Safe call needs no approval.
        if tool.risk(&call.arguments) == RiskLevel::Safe {
            return Ok(());
        }
        // Session grant cache: an identical risky call already approved-always.
        let key = Self::grant_key(call);
        if self.store.is_granted(&key) {
            return Ok(());
        }
        // Round-trip the driver for a decision (the oneshot lives in the kernel's
        // RequestCtx, never in an event → events stay serializable).
        let payload = serde_json::json!({ "tool": tool.name(), "args": call.arguments });
        match PermissionDecision::from_value(&rt.request(&self.kind, payload).await) {
            PermissionDecision::AllowOnce => Ok(()),
            PermissionDecision::AllowAlways => {
                self.store.grant(&key);
                Ok(())
            }
            PermissionDecision::Deny => {
                Err(format!("denied by approval policy: {} {}", tool.name(), call.arguments))
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
        ToolCall { id: "2".into(), name: "read_file".into(), arguments: r#"{"file_path":"a.txt"}"#.into() }
    }

    #[test]
    fn decision_parsing_fails_closed() {
        use serde_json::json;
        assert_eq!(PermissionDecision::from_value(&json!({"decision":"allow"})), PermissionDecision::AllowOnce);
        assert_eq!(
            PermissionDecision::from_value(&json!({"decision":"allow","remember":true})),
            PermissionDecision::AllowAlways
        );
        assert_eq!(PermissionDecision::from_value(&json!({"decision":"allow_always"})), PermissionDecision::AllowAlways);
        assert_eq!(PermissionDecision::from_value(&json!({"decision":"deny"})), PermissionDecision::Deny);
        assert_eq!(PermissionDecision::from_value(&serde_json::Value::Null), PermissionDecision::Deny);
        assert_eq!(PermissionDecision::from_value(&json!({})), PermissionDecision::Deny);
    }

    #[tokio::test]
    async fn safe_call_passes_without_round_trip() {
        let (tx, _rx) = unbounded_channel::<AgentEvent>();
        let rt = RequestCtx::new(tx, Some(Duration::from_millis(50)));
        let mw = ApprovalMiddleware::in_memory();
        let tool: Arc<dyn Tool> = Arc::new(crate::tools::read::ReadFileTool);
        let mut call = safe_call();
        // Safe → Ok without ever awaiting the driver (which never responds here).
        assert!(mw.before(&mut call, &tool, &rt).await.is_ok());
    }

    #[tokio::test]
    async fn pre_granted_risky_call_passes() {
        let (tx, _rx) = unbounded_channel::<AgentEvent>();
        let rt = RequestCtx::new(tx, Some(Duration::from_millis(50)));
        let store = Arc::new(InMemoryPermissionStore::new());
        let call = risky_call();
        store.grant(&ApprovalMiddleware::grant_key(&call));
        let mw = ApprovalMiddleware::new(store);
        let tool: Arc<dyn Tool> = Arc::new(WriteFileTool);
        let mut c = call;
        assert!(mw.before(&mut c, &tool, &rt).await.is_ok());
    }

    #[tokio::test]
    async fn risky_call_denied_when_driver_silent() {
        // No driver drains the request → the bounded round-trip times out → Null →
        // Deny → Err (fail closed).
        let (tx, _rx) = unbounded_channel::<AgentEvent>();
        let rt = RequestCtx::new(tx, Some(Duration::from_millis(20)));
        let mw = ApprovalMiddleware::in_memory();
        let tool: Arc<dyn Tool> = Arc::new(WriteFileTool);
        let mut call = risky_call();
        let res = mw.before(&mut call, &tool, &rt).await;
        assert!(res.is_err(), "silent driver must fail closed");
        assert!(res.unwrap_err().contains("denied"));
    }
}
