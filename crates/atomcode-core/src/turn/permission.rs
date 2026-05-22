use crate::tool::{ApprovalRequirement, PermissionDecision, ToolCall};
use async_trait::async_trait;
use tokio::sync::mpsc;

/// Permission decision interface. TurnRunner calls this when a tool requires approval.
/// Different implementations support interactive (main agent) and automatic (subagent) modes.
#[async_trait]
pub trait PermissionDecider: Send + Sync {
    async fn decide(&self, call: &ToolCall, reason: &str) -> PermissionDecision;

    /// Quick synchronous check: will this call be auto-approved without
    /// user interaction?  Used by TurnRunner to skip the
    /// `ApprovalRequested` event (and its associated TUI prompt row)
    /// when the PermissionStore already has a session grant or override
    /// that will cause `decide()` to return `Allow` immediately.
    ///
    /// Returning `false` does **not** mean the call will be denied —
    /// only that it *might* need interactive approval.  Returning
    /// `true` guarantees `decide()` will return `Allow` without
    /// prompting.
    fn will_auto_approve(&self, call: &ToolCall, approval: &ApprovalRequirement) -> bool;
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

const EDIT_TOOLS: &[&str] = &["create_file", "edit_file", "search_replace"];

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

    fn will_auto_approve(&self, call: &ToolCall, _approval: &ApprovalRequirement) -> bool {
        // AutoPermissionDecider never prompts the user — it either
        // allows or denies based on its mode.  Return true when the
        // decision will be Allow (no interactive prompt involved).
        match self.mode {
            AutoPermissionMode::BypassAll => true,
            AutoPermissionMode::AcceptEdits => EDIT_TOOLS.contains(&call.name.as_str()),
            AutoPermissionMode::DenyAll => false,
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

    fn will_auto_approve(&self, call: &ToolCall, approval: &ApprovalRequirement) -> bool {
        // Mirror the PermissionStore check that `decide()` performs
        // before falling through to the interactive channel.
        // IMPORTANT: `decide()` constructs a fresh `RequireApproval` from
        // the reason string (ignoring RequireApprovalAlways) before
        // calling `store.check()`.  We must use the same variant here so
        // that `will_auto_approve` and `decide()` stay consistent.
        let check_approval = match approval {
            ApprovalRequirement::RequireApproval(reason)
            | ApprovalRequirement::RequireApprovalAlways(reason) => {
                ApprovalRequirement::RequireApproval(reason.clone())
            }
            // AutoApprove tools never reach this code path (the if-let
            // in execute_single_tool only matches RequireApproval*).
            ApprovalRequirement::AutoApprove => return true,
        };
        if let Ok(store) = self.permission_store.read() {
            matches!(store.check(&call.name, &check_approval), PermissionDecision::Allow)
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_call(name: &str) -> ToolCall {
        ToolCall {
            id: "test".into(),
            name: name.into(),
            arguments: "{}".into(),
        }
    }

    #[tokio::test]
    async fn test_auto_bypass_allows_all() {
        let d = AutoPermissionDecider::new(AutoPermissionMode::BypassAll);
        assert!(matches!(
            d.decide(&make_call("bash"), "dangerous").await,
            PermissionDecision::Allow
        ));
    }

    #[tokio::test]
    async fn test_auto_deny_denies_all() {
        let d = AutoPermissionDecider::new(AutoPermissionMode::DenyAll);
        assert!(matches!(
            d.decide(&make_call("bash"), "dangerous").await,
            PermissionDecision::Deny
        ));
    }

    #[tokio::test]
    async fn test_auto_accept_edits_allows_write() {
        let d = AutoPermissionDecider::new(AutoPermissionMode::AcceptEdits);
        assert!(matches!(
            d.decide(&make_call("create_file"), "write").await,
            PermissionDecision::Allow
        ));
        assert!(matches!(
            d.decide(&make_call("edit_file"), "edit").await,
            PermissionDecision::Allow
        ));
        assert!(matches!(
            d.decide(&make_call("search_replace"), "sr").await,
            PermissionDecision::Allow
        ));
    }

    #[tokio::test]
    async fn test_auto_accept_edits_denies_bash() {
        let d = AutoPermissionDecider::new(AutoPermissionMode::AcceptEdits);
        assert!(matches!(
            d.decide(&make_call("bash"), "dangerous").await,
            PermissionDecision::Deny
        ));
    }

    #[tokio::test]
    async fn test_interactive_allow() {
        let (req_tx, mut req_rx) = mpsc::unbounded_channel();
        let (resp_tx, resp_rx) = mpsc::unbounded_channel();
        let store =
            std::sync::Arc::new(std::sync::RwLock::new(crate::tool::PermissionStore::new()));
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
        let store =
            std::sync::Arc::new(std::sync::RwLock::new(crate::tool::PermissionStore::new()));
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
        let store =
            std::sync::Arc::new(std::sync::RwLock::new(crate::tool::PermissionStore::new()));
        let d = InteractivePermissionDecider::new(req_tx, resp_rx, store);

        drop(req_rx); // close request channel
        let call = make_call("bash");
        assert!(matches!(
            d.decide(&call, "dangerous").await,
            PermissionDecision::Deny
        ));
    }

    #[tokio::test]
    async fn test_interactive_session_grant_skips_channel() {
        let (req_tx, _req_rx) = mpsc::unbounded_channel();
        let (_resp_tx, resp_rx) = mpsc::unbounded_channel();
        let store =
            std::sync::Arc::new(std::sync::RwLock::new(crate::tool::PermissionStore::new()));

        // Grant session permission for "bash" BEFORE creating the decider
        store.write().unwrap().grant_session("bash");

        let d = InteractivePermissionDecider::new(req_tx, resp_rx, store);
        let call = make_call("bash");

        // Should return Allow immediately from PermissionStore,
        // WITHOUT sending a request on the channel (channel is not even read).
        let decision = d.decide(&call, "dangerous").await;
        assert!(matches!(decision, PermissionDecision::Allow));
    }

    // ── will_auto_approve tests ──

    #[test]
    fn test_will_auto_approve_auto_bypass() {
        let d = AutoPermissionDecider::new(AutoPermissionMode::BypassAll);
        let call = make_call("bash");
        assert!(d.will_auto_approve(&call, &ApprovalRequirement::RequireApproval("dangerous".into())));
    }

    #[test]
    fn test_will_auto_approve_auto_deny() {
        let d = AutoPermissionDecider::new(AutoPermissionMode::DenyAll);
        let call = make_call("bash");
        assert!(!d.will_auto_approve(&call, &ApprovalRequirement::RequireApproval("dangerous".into())));
    }

    #[test]
    fn test_will_auto_approve_auto_accept_edits() {
        let d = AutoPermissionDecider::new(AutoPermissionMode::AcceptEdits);
        let edit_call = make_call("edit_file");
        let bash_call = make_call("bash");
        assert!(d.will_auto_approve(&edit_call, &ApprovalRequirement::RequireApproval("write".into())));
        assert!(!d.will_auto_approve(&bash_call, &ApprovalRequirement::RequireApproval("dangerous".into())));
    }

    #[test]
    fn test_will_auto_approve_interactive_no_grant() {
        let (req_tx, _req_rx) = mpsc::unbounded_channel();
        let (_resp_tx, resp_rx) = mpsc::unbounded_channel();
        let store =
            std::sync::Arc::new(std::sync::RwLock::new(crate::tool::PermissionStore::new()));
        let d = InteractivePermissionDecider::new(req_tx, resp_rx, store);
        let call = make_call("bash");
        // No session grant → will NOT auto-approve
        assert!(!d.will_auto_approve(&call, &ApprovalRequirement::RequireApproval("dangerous".into())));
    }

    #[test]
    fn test_will_auto_approve_interactive_with_session_grant() {
        let (req_tx, _req_rx) = mpsc::unbounded_channel();
        let (_resp_tx, resp_rx) = mpsc::unbounded_channel();
        let store =
            std::sync::Arc::new(std::sync::RwLock::new(crate::tool::PermissionStore::new()));
        store.write().unwrap().grant_session("bash");
        let d = InteractivePermissionDecider::new(req_tx, resp_rx, store);
        let call = make_call("bash");
        // Session grant exists → WILL auto-approve
        assert!(d.will_auto_approve(&call, &ApprovalRequirement::RequireApproval("dangerous".into())));
    }

    #[test]
    fn test_will_auto_approve_interactive_require_approval_always_with_grant() {
        // RequireApprovalAlways with a session grant: `decide()` constructs
        // a fresh `RequireApproval` before calling `store.check()`, so the
        // session grant DOES take effect and `decide()` returns Allow.
        // `will_auto_approve` must mirror this by converting
        // RequireApprovalAlways → RequireApproval before checking.
        let (req_tx, _req_rx) = mpsc::unbounded_channel();
        let (_resp_tx, resp_rx) = mpsc::unbounded_channel();
        let store =
            std::sync::Arc::new(std::sync::RwLock::new(crate::tool::PermissionStore::new()));
        store.write().unwrap().grant_session("bash");
        let d = InteractivePermissionDecider::new(req_tx, resp_rx, store);
        let call = make_call("bash");
        assert!(d.will_auto_approve(&call, &ApprovalRequirement::RequireApprovalAlways("sensitive".into())));
    }

    #[test]
    fn test_will_auto_approve_interactive_require_approval_always_no_grant() {
        // RequireApprovalAlways WITHOUT a session grant: still needs prompt.
        let (req_tx, _req_rx) = mpsc::unbounded_channel();
        let (_resp_tx, resp_rx) = mpsc::unbounded_channel();
        let store =
            std::sync::Arc::new(std::sync::RwLock::new(crate::tool::PermissionStore::new()));
        let d = InteractivePermissionDecider::new(req_tx, resp_rx, store);
        let call = make_call("bash");
        assert!(!d.will_auto_approve(&call, &ApprovalRequirement::RequireApprovalAlways("sensitive".into())));
    }

    #[test]
    fn test_will_auto_approve_interactive_different_tool_not_auto() {
        // Session grant for "bash" should NOT auto-approve "mcp__zouwu__query"
        let (req_tx, _req_rx) = mpsc::unbounded_channel();
        let (_resp_tx, resp_rx) = mpsc::unbounded_channel();
        let store =
            std::sync::Arc::new(std::sync::RwLock::new(crate::tool::PermissionStore::new()));
        store.write().unwrap().grant_session("bash");
        let d = InteractivePermissionDecider::new(req_tx, resp_rx, store);
        let call = make_call("mcp__zouwu__query");
        assert!(!d.will_auto_approve(&call, &ApprovalRequirement::RequireApproval("mcp tool".into())));
    }

    #[test]
    fn test_will_auto_approve_interactive_same_tool_auto() {
        // The key scenario from the bug: user presses [A] for an MCP tool,
        // subsequent calls to the same tool should auto-approve.
        let (req_tx, _req_rx) = mpsc::unbounded_channel();
        let (_resp_tx, resp_rx) = mpsc::unbounded_channel();
        let store =
            std::sync::Arc::new(std::sync::RwLock::new(crate::tool::PermissionStore::new()));
        // Simulate: user pressed [A] for this MCP tool in a previous call
        store.write().unwrap().grant_session("mcp__zouwu-mcp-server__query_requirements");
        let d = InteractivePermissionDecider::new(req_tx, resp_rx, store);
        let call = make_call("mcp__zouwu-mcp-server__query_requirements");
        assert!(d.will_auto_approve(&call, &ApprovalRequirement::RequireApproval("mcp tool".into())));
    }
}
