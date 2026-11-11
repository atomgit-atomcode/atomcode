//! Bridge daemon `/chat` approval answers to the runtime that owns the request.
//! HTTP answers claim a live (session, tool call, one-shot approval) route once;
//! the waiting runtime independently checks identity and cancellation before acting.

use atomcode_capabilities::tools::PermissionDecision;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

/// A person's answer to a `/chat` approval: the decision, and whether it was
/// "always allow this MCP tool". The runtime waiting on the answer is the one
/// that carries the tool out — through its own registry — so the flag travels
/// with the decision instead of being acted on here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatPermission {
    pub decision: PermissionDecision,
    pub persist_mcp_tool: bool,
    pub persist_project_rule: bool,
    pub call_id: Option<String>,
    pub approval_id: Option<String>,
}

impl From<PermissionDecision> for ChatPermission {
    fn from(decision: PermissionDecision) -> Self {
        Self {
            decision,
            persist_mcp_tool: false,
            persist_project_rule: false,
            call_id: None,
            approval_id: None,
        }
    }
}

/// session_id -> decider 的 response 发送端。
#[derive(Clone, Default)]
pub struct PermissionResponders {
    inner: Arc<RwLock<HashMap<String, PermissionRoute>>>,
}

struct PermissionRoute {
    tx: UnboundedSender<ChatPermission>,
    pending: Option<(String, String)>, // actual tool call ID, fresh driver approval ID
}

/// A pending `/chat` structured-input request, keyed by the native runtime request id.
/// The runtime remains the request owner; this registry is only the HTTP response route.
#[derive(Clone, Default)]
pub struct UserInputResponders {
    inner: Arc<RwLock<HashMap<(String, u64), oneshot::Sender<serde_json::Value>>>>,
}

impl UserInputResponders {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &self,
        session_id: String,
        request_id: u64,
        tx: oneshot::Sender<serde_json::Value>,
    ) {
        self.inner
            .write()
            .unwrap()
            .insert((session_id, request_id), tx);
    }

    pub fn unregister(&self, session_id: &str, request_id: u64) {
        self.inner
            .write()
            .unwrap()
            .remove(&(session_id.to_owned(), request_id));
    }

    pub fn unregister_session(&self, session_id: &str) {
        self.inner
            .write()
            .unwrap()
            .retain(|(registered, _), _| registered != session_id);
    }

    /// Delivers at most one answer. Removing before send rejects duplicate/stale POSTs.
    pub fn deliver(&self, session_id: &str, request_id: u64, value: serde_json::Value) -> bool {
        self.inner
            .write()
            .unwrap()
            .remove(&(session_id.to_owned(), request_id))
            .is_some_and(|tx| tx.send(value).is_ok())
    }
}

impl PermissionResponders {
    pub fn pending_call(&self, session: &str, call: Option<&str>) {
        if let Some(route) = self.inner.write().unwrap().get_mut(session) {
            route.pending = call
                .filter(|call| !call.is_empty())
                .map(|call| (call.to_owned(), uuid::Uuid::new_v4().to_string()));
        }
    }

    pub fn approval_id(&self, session: &str, call: &str) -> Option<String> {
        let routes = self.inner.read().unwrap();
        let (pending_call, approval_id) = routes.get(session)?.pending.as_ref()?;
        (pending_call == call).then(|| approval_id.clone())
    }

    pub fn deliver_project(&self, session: &str, call_id: &str, approval_id: Option<&str>) -> bool {
        self.deliver(
            session,
            ChatPermission {
                decision: PermissionDecision::AllowOnce,
                persist_mcp_tool: false,
                persist_project_rule: true,
                call_id: Some(call_id.into()),
                approval_id: approval_id.map(str::to_owned),
            },
        )
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, session_id: String, tx: UnboundedSender<ChatPermission>) {
        self.inner
            .write()
            .unwrap()
            .insert(session_id, PermissionRoute { tx, pending: None });
    }

    pub fn unregister(&self, session_id: &str) {
        self.inner.write().unwrap().remove(session_id);
    }

    /// Claim one exact pending request while holding the routing lock. No answer
    /// may be queued before a request or reused for a later request/registration.
    pub fn deliver(&self, session_id: &str, answer: impl Into<ChatPermission>) -> bool {
        let answer = answer.into();
        let mut routes = self.inner.write().unwrap();
        let Some(route) = routes.get_mut(session_id) else {
            return false;
        };
        let Some((call, approval)) = &route.pending else {
            return false;
        };
        if answer.call_id.as_deref() != Some(call.as_str())
            || answer.approval_id.as_deref() != Some(approval.as_str())
        {
            return false;
        }
        route.pending = None;
        route.tx.send(answer).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_capabilities::tools::PermissionDecision;

    fn answer(reg: &PermissionResponders, session: &str, call: &str) -> ChatPermission {
        ChatPermission {
            call_id: Some(call.into()),
            approval_id: reg.approval_id(session, call),
            ..PermissionDecision::AllowOnce.into()
        }
    }

    #[tokio::test]
    async fn ordinary_answers_require_a_live_matching_call_and_are_consumed_once() {
        let reg = PermissionResponders::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        reg.register("session".into(), tx);
        assert!(!reg.deliver("session", answer(&reg, "session", "not-pending")));
        reg.pending_call("session", Some("current"));
        assert!(!reg.deliver("session", answer(&reg, "session", "stale")));
        assert!(!reg.deliver("session", PermissionDecision::AllowOnce));
        let correct = answer(&reg, "session", "current");
        let mut missing = correct.clone();
        missing.approval_id = None;
        assert!(!reg.deliver("session", missing));
        let mut wrong = correct.clone();
        wrong.approval_id = Some("wrong".into());
        assert!(!reg.deliver("session", wrong));
        assert!(rx.try_recv().is_err());
        assert!(reg.deliver("session", correct.clone()));
        assert_eq!(rx.recv().await, Some(correct.clone()));
        assert!(!reg.deliver("session", correct.clone()));
        // A provider can reuse a tool call ID. A fresh approval ID still fences it.
        reg.pending_call("session", Some("current"));
        assert!(!reg.deliver("session", correct));
        assert!(reg.deliver("session", answer(&reg, "session", "current")));
        assert!(rx.recv().await.is_some());
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn project_save_rejects_stale_duplicate_and_resolved_call_ids() {
        let reg = PermissionResponders::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        reg.register("session".into(), tx);
        reg.pending_call("session", Some("current"));
        let token = reg.approval_id("session", "current").unwrap();
        assert!(!reg.deliver_project("session", "old", Some(&token)));
        assert!(!reg.deliver_project("session", "current", None));
        assert!(reg.deliver_project("session", "current", Some(&token)));
        let received = rx.recv().await.unwrap();
        assert!(received.persist_project_rule);
        assert_eq!(received.call_id.as_deref(), Some("current"));
        assert!(!reg.deliver_project("session", "current", Some(&token)));
        reg.pending_call("session", Some("next"));
        assert!(reg.deliver("session", answer(&reg, "session", "next")));
        assert!(!reg.deliver_project("session", "next", Some(&token)));
        reg.unregister("session");
        assert!(!reg.deliver_project("session", "next", Some(&token)));
    }

    #[tokio::test]
    async fn registration_replacement_and_cancelled_pending_requests_revoke_answers() {
        let reg = PermissionResponders::new();
        let (old_tx, mut old_rx) = tokio::sync::mpsc::unbounded_channel();
        reg.register("session".into(), old_tx);
        reg.pending_call("session", Some("call"));
        let old = answer(&reg, "session", "call");
        reg.pending_call("session", None);
        assert!(!reg.deliver("session", old.clone()));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        reg.register("session".into(), tx);
        reg.pending_call("session", Some("call"));
        assert!(!reg.deliver("session", old));
        let mut current = answer(&reg, "session", "call");
        current.persist_mcp_tool = true;
        assert!(reg.deliver("session", current.clone()));
        assert_eq!(rx.recv().await, Some(current.clone()));
        assert!(old_rx.try_recv().is_err());
        reg.unregister("session");
        assert!(!reg.deliver("session", current));
    }

    #[test]
    fn deliver_to_unknown_session_returns_false() {
        assert!(!PermissionResponders::new().deliver("nope", PermissionDecision::Deny));
    }

    #[tokio::test]
    async fn routes_user_input_once_by_session_and_request() {
        let reg = UserInputResponders::new();
        let (tx, rx) = tokio::sync::oneshot::channel();
        reg.register("sess-1".into(), 42, tx);

        assert!(!reg.deliver("sess-1", 41, serde_json::json!({"text":"wrong"})));
        assert!(reg.deliver("sess-1", 42, serde_json::json!({"text":"ok"})));
        assert!(!reg.deliver("sess-1", 42, serde_json::json!({"text":"late"})));
        assert_eq!(rx.await.unwrap(), serde_json::json!({"text":"ok"}));
    }
}
