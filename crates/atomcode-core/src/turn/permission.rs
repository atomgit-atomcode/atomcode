use async_trait::async_trait;
use tokio::sync::mpsc;
use crate::tool::{PermissionDecision, ToolCall};

/// Permission decision interface. TurnRunner calls this when a tool requires approval.
/// Different implementations support interactive (main agent) and automatic (subagent) modes.
#[async_trait]
pub trait PermissionDecider: Send + Sync {
    async fn decide(&self, call: &ToolCall, reason: &str) -> PermissionDecision;
}

/// Auto-permission modes for subagents
#[derive(Debug, Clone)]
pub enum AutoPermissionMode {
    /// Allow all tools
    BypassAll,
    /// Allow edit tools (write_file, edit_file, search_replace), deny others
    AcceptEdits,
    /// Deny all tools that require approval
    DenyAll,
}

const EDIT_TOOLS: &[&str] = &["write_file", "edit_file", "search_replace"];

/// Automatic permission decider (used by SubagentLoop)
pub struct AutoPermissionDecider {
    mode: AutoPermissionMode,
}

impl AutoPermissionDecider {
    pub fn new(mode: AutoPermissionMode) -> Self {
        Self { mode }
    }
}

#[async_trait]
impl PermissionDecider for AutoPermissionDecider {
    async fn decide(&self, call: &ToolCall, _reason: &str) -> PermissionDecision {
        match self.mode {
            AutoPermissionMode::BypassAll => PermissionDecision::Allow,
            AutoPermissionMode::AcceptEdits => {
                if EDIT_TOOLS.contains(&call.name.as_str()) {
                    PermissionDecision::Allow
                } else {
                    PermissionDecision::Deny
                }
            }
            AutoPermissionMode::DenyAll => PermissionDecision::Deny,
        }
    }
}

/// Approval request sent to AgentLoop's command loop
#[derive(Debug)]
pub struct ApprovalRequest {
    pub call: ToolCall,
    pub reason: String,
}

/// Interactive permission decider (used by AgentLoop).
/// Checks PermissionStore first (session grants, overrides),
/// then falls back to sending approval request via channel.
pub struct InteractivePermissionDecider {
    request_tx: mpsc::UnboundedSender<ApprovalRequest>,
    response_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<PermissionDecision>>,
    /// Shared permission store — checked before sending interactive requests.
    /// AgentLoop writes to this (grant_session on ApproveToolAlways),
    /// TurnRunner reads from it (check before prompting user).
    permission_store: std::sync::Arc<std::sync::RwLock<crate::tool::PermissionStore>>,
}

impl InteractivePermissionDecider {
    pub fn new(
        request_tx: mpsc::UnboundedSender<ApprovalRequest>,
        response_rx: mpsc::UnboundedReceiver<PermissionDecision>,
        permission_store: std::sync::Arc<std::sync::RwLock<crate::tool::PermissionStore>>,
    ) -> Self {
        Self {
            request_tx,
            response_rx: tokio::sync::Mutex::new(response_rx),
            permission_store,
        }
    }
}

#[async_trait]
impl PermissionDecider for InteractivePermissionDecider {
    async fn decide(&self, call: &ToolCall, reason: &str) -> PermissionDecision {
        // Check PermissionStore first — session grants and overrides
        // take effect without prompting the user again.
        if let Ok(store) = self.permission_store.read() {
            let approval = crate::tool::ApprovalRequirement::RequireApproval(reason.to_string());
            match store.check(&call.name, &approval) {
                PermissionDecision::Allow => return PermissionDecision::Allow,
                PermissionDecision::Deny => return PermissionDecision::Deny,
                PermissionDecision::Ask(_) => {} // fall through to interactive
            }
        }

        // Not in store — send interactive approval request
        let request = ApprovalRequest {
            call: call.clone(),
            reason: reason.to_string(),
        };
        if self.request_tx.send(request).is_err() {
            return PermissionDecision::Deny;
        }
        let mut rx = self.response_rx.lock().await;
        rx.recv().await.unwrap_or(PermissionDecision::Deny)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_call(name: &str) -> ToolCall {
        ToolCall { id: "test".into(), name: name.into(), arguments: "{}".into() }
    }

    #[tokio::test]
    async fn test_auto_bypass_allows_all() {
        let d = AutoPermissionDecider::new(AutoPermissionMode::BypassAll);
        assert!(matches!(d.decide(&make_call("bash"), "dangerous").await, PermissionDecision::Allow));
    }

    #[tokio::test]
    async fn test_auto_deny_denies_all() {
        let d = AutoPermissionDecider::new(AutoPermissionMode::DenyAll);
        assert!(matches!(d.decide(&make_call("bash"), "dangerous").await, PermissionDecision::Deny));
    }

    #[tokio::test]
    async fn test_auto_accept_edits_allows_write() {
        let d = AutoPermissionDecider::new(AutoPermissionMode::AcceptEdits);
        assert!(matches!(d.decide(&make_call("write_file"), "write").await, PermissionDecision::Allow));
        assert!(matches!(d.decide(&make_call("edit_file"), "edit").await, PermissionDecision::Allow));
        assert!(matches!(d.decide(&make_call("search_replace"), "sr").await, PermissionDecision::Allow));
    }

    #[tokio::test]
    async fn test_auto_accept_edits_denies_bash() {
        let d = AutoPermissionDecider::new(AutoPermissionMode::AcceptEdits);
        assert!(matches!(d.decide(&make_call("bash"), "dangerous").await, PermissionDecision::Deny));
    }

    #[tokio::test]
    async fn test_interactive_allow() {
        let (req_tx, mut req_rx) = mpsc::unbounded_channel();
        let (resp_tx, resp_rx) = mpsc::unbounded_channel();
        let store = std::sync::Arc::new(std::sync::RwLock::new(crate::tool::PermissionStore::new()));
        let d = InteractivePermissionDecider::new(req_tx, resp_rx, store);

        let call = make_call("bash");
        let fut = d.decide(&call, "dangerous");

        tokio::spawn(async move {
            let _req = req_rx.recv().await.unwrap();
            resp_tx.send(PermissionDecision::Allow).unwrap();
        });

        assert!(matches!(fut.await, PermissionDecision::Allow));
    }

    #[tokio::test]
    async fn test_interactive_deny() {
        let (req_tx, mut req_rx) = mpsc::unbounded_channel();
        let (resp_tx, resp_rx) = mpsc::unbounded_channel();
        let store = std::sync::Arc::new(std::sync::RwLock::new(crate::tool::PermissionStore::new()));
        let d = InteractivePermissionDecider::new(req_tx, resp_rx, store);

        let call = make_call("bash");
        let fut = d.decide(&call, "dangerous");

        tokio::spawn(async move {
            let _req = req_rx.recv().await.unwrap();
            resp_tx.send(PermissionDecision::Deny).unwrap();
        });

        assert!(matches!(fut.await, PermissionDecision::Deny));
    }

    #[tokio::test]
    async fn test_interactive_channel_closed_returns_deny() {
        let (req_tx, req_rx) = mpsc::unbounded_channel();
        let (_resp_tx, resp_rx) = mpsc::unbounded_channel();
        let store = std::sync::Arc::new(std::sync::RwLock::new(crate::tool::PermissionStore::new()));
        let d = InteractivePermissionDecider::new(req_tx, resp_rx, store);

        drop(req_rx); // close request channel
        let call = make_call("bash");
        assert!(matches!(d.decide(&call, "dangerous").await, PermissionDecision::Deny));
    }

    #[tokio::test]
    async fn test_interactive_session_grant_skips_channel() {
        let (req_tx, _req_rx) = mpsc::unbounded_channel();
        let (_resp_tx, resp_rx) = mpsc::unbounded_channel();
        let store = std::sync::Arc::new(std::sync::RwLock::new(crate::tool::PermissionStore::new()));

        // Grant session permission for "bash" BEFORE creating the decider
        store.write().unwrap().grant_session("bash");

        let d = InteractivePermissionDecider::new(req_tx, resp_rx, store);
        let call = make_call("bash");

        // Should return Allow immediately from PermissionStore,
        // WITHOUT sending a request on the channel (channel is not even read).
        let decision = d.decide(&call, "dangerous").await;
        assert!(matches!(decision, PermissionDecision::Allow));
    }
}
