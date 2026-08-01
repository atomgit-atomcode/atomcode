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

/// session_id -> decider 的 response 发送端。
#[derive(Clone, Default)]
pub struct PermissionResponders {
    inner: Arc<RwLock<HashMap<String, UnboundedSender<PermissionDecision>>>>,
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
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记某 session 的决定发送端（`/chat` 启动时调用）。
    pub fn register(&self, session_id: String, tx: UnboundedSender<PermissionDecision>) {
        self.inner.write().unwrap().insert(session_id, tx);
    }

    /// `/chat` 结束时清理。
    pub fn unregister(&self, session_id: &str) {
        self.inner.write().unwrap().remove(session_id);
    }

    /// 把决定送给对应 session 的 decider。返回是否成功（session 是否在等待）。
    pub fn deliver(&self, session_id: &str, decision: PermissionDecision) -> bool {
        if let Some(tx) = self.inner.read().unwrap().get(session_id) {
            tx.send(decision).is_ok()
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
    async fn routes_decision_to_registered_session() {
        let reg = PermissionResponders::new();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        reg.register("sess-1".into(), tx);

        assert!(reg.deliver("sess-1", PermissionDecision::AllowOnce));
        assert!(matches!(
            rx.recv().await,
            Some(PermissionDecision::AllowOnce)
        ));
    }

    #[test]
    fn deliver_to_unknown_session_returns_false() {
        let reg = PermissionResponders::new();
        assert!(!reg.deliver("nope", PermissionDecision::Deny));
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
