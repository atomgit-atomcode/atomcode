//! 把 daemon `/chat` 的交互式权限决策桥接到 HTTP。
//!
//! `InteractivePermissionDecider` 阻塞在 `response_rx` 上等待决定；
//! `/chat/permission` 收到前端决定后，经 `PermissionResponders` 按
//! session_id 路由到对应的 `response_tx`，唤醒 decider。

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
}

impl From<PermissionDecision> for ChatPermission {
    fn from(decision: PermissionDecision) -> Self {
        Self {
            decision,
            persist_mcp_tool: false,
            persist_project_rule: false,
            call_id: None,
        }
    }
}

/// session_id -> decider 的 response 发送端。
#[derive(Clone, Default)]
pub struct PermissionResponders {
    inner: Arc<RwLock<HashMap<String, UnboundedSender<ChatPermission>>>>,
    /// Sessions whose approval request has been handed to the client and not yet
    /// answered.
    ///
    /// `inner` is not this: a decider is registered for the *whole* turn, so its
    /// length says "turns that could ask", not "turns that did ask and are now
    /// blocked". A phone needs the second one — it is the difference between
    /// "working" and "waiting for you".
    outstanding: Arc<RwLock<std::collections::HashSet<String>>>,
    calls: Arc<RwLock<HashMap<String, String>>>,
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
        let mut calls = self.calls.write().unwrap();
        if let Some(call) = call {
            calls.insert(session.into(), call.into());
        } else {
            calls.remove(session);
        }
    }
    pub fn deliver_project(&self, session: &str, call_id: &str) -> bool {
        let mut calls = self.calls.write().unwrap();
        if calls.get(session).map(String::as_str) != Some(call_id) {
            return false;
        }
        calls.remove(session);
        drop(calls);
        self.deliver(
            session,
            ChatPermission {
                decision: PermissionDecision::AllowOnce,
                persist_mcp_tool: false,
                persist_project_rule: true,
                call_id: Some(call_id.into()),
            },
        )
    }
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记某 session 的决定发送端（`/chat` 启动时调用）。
    pub fn register(&self, session_id: String, tx: UnboundedSender<ChatPermission>) {
        self.inner.write().unwrap().insert(session_id, tx);
    }

    /// `/chat` 结束时清理。
    pub fn unregister(&self, session_id: &str) {
        self.pending_call(session_id, None);
        self.inner.write().unwrap().remove(session_id);
        self.outstanding.write().unwrap().remove(session_id);
    }

    /// Record that this session's turn is now blocked on an approval answer.
    ///
    /// Called where the approval request is handed to the client — that is the
    /// moment the turn stops making progress until somebody answers.
    pub fn mark_requested(&self, session_id: &str) {
        self.outstanding
            .write()
            .unwrap()
            .insert(session_id.to_owned());
    }

    /// How many turns are blocked on an approval answer right now.
    pub fn outstanding_count(&self) -> usize {
        self.outstanding.read().unwrap().len()
    }

    /// 把决定送给对应 session 的 decider。返回是否成功（session 是否在等待）。
    pub fn deliver(&self, session_id: &str, answer: impl Into<ChatPermission>) -> bool {
        // An answer was given. Even if the send fails (the turn is gone) the
        // request is no longer waiting for a person, so the mark goes either way.
        self.outstanding.write().unwrap().remove(session_id);
        self.pending_call(session_id, None);
        if let Some(tx) = self.inner.read().unwrap().get(session_id) {
            tx.send(answer.into()).is_ok()
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_capabilities::tools::PermissionDecision;

    #[tokio::test]
    async fn project_save_rejects_stale_duplicate_and_resolved_call_ids() {
        let registry = PermissionResponders::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        registry.register("session".into(), tx);
        registry.pending_call("session", Some("new-call"));
        assert!(!registry.deliver_project("session", "old-call"));
        assert!(rx.try_recv().is_err());
        assert!(registry.deliver_project("session", "new-call"));
        let answer = rx.recv().await.unwrap();
        assert!(answer.persist_project_rule);
        assert_eq!(answer.call_id.as_deref(), Some("new-call"));
        assert!(!registry.deliver_project("session", "new-call"));
        registry.pending_call("session", Some("next-call"));
        registry.deliver("session", PermissionDecision::AllowOnce);
        assert!(!registry.deliver_project("session", "next-call"));
        registry.unregister("session");
        assert!(!registry.deliver_project("session", "next-call"));
    }

    #[tokio::test]
    async fn routes_decision_to_registered_session() {
        let reg = PermissionResponders::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        reg.register("sess-1".into(), tx);

        assert!(reg.deliver("sess-1", PermissionDecision::AllowOnce));
        assert_eq!(
            rx.recv().await,
            Some(ChatPermission::from(PermissionDecision::AllowOnce))
        );
    }

    /// "Always allow this MCP tool" travels with the decision to the runtime that
    /// asked, which carries it out through its own registry.
    #[tokio::test]
    async fn an_always_allow_reaches_the_runtime_with_its_decision() {
        let reg = PermissionResponders::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        reg.register("sess-1".into(), tx);
        let answer = ChatPermission {
            decision: PermissionDecision::AllowOnce,
            persist_mcp_tool: true,
            persist_project_rule: false,
            call_id: None,
        };

        assert!(reg.deliver("sess-1", answer.clone()));
        assert_eq!(rx.recv().await, Some(answer));
    }

    #[test]
    fn deliver_to_unknown_session_returns_false() {
        let reg = PermissionResponders::new();
        assert!(!reg.deliver("nope", PermissionDecision::Deny));
    }

    /// `/health`'s `pending_approvals` must mean "blocked on a person", not "a
    /// decider exists". A decider is registered for the whole turn, so counting
    /// those would report every Build-mode turn as waiting.
    #[test]
    fn only_an_unanswered_request_counts_as_pending() {
        let reg = PermissionResponders::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        reg.register("sess-1".into(), tx);

        // Registered but nothing asked yet: not waiting.
        assert_eq!(reg.outstanding_count(), 0);

        reg.mark_requested("sess-1");
        assert_eq!(reg.outstanding_count(), 1);

        // The answer clears it, whether or not the send lands.
        assert!(reg.deliver("sess-1", PermissionDecision::AllowOnce));
        assert_eq!(reg.outstanding_count(), 0);

        // A turn that ends without an answer must not leave a stale "waiting".
        reg.mark_requested("sess-1");
        reg.unregister("sess-1");
        assert_eq!(reg.outstanding_count(), 0);
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
