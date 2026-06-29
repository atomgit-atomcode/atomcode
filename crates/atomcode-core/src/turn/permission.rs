use crate::tool::{ApprovalRequirement, PermissionDecision, ToolCall};
use async_trait::async_trait;
use tokio::sync::mpsc;

/// Permission decision interface. TurnRunner calls this when a tool requires approval.
/// Different implementations support interactive (main agent) and automatic (subagent) modes.
#[async_trait]
pub trait PermissionDecider: Send + Sync {
    async fn decide(&self, call: &ToolCall, approval: &ApprovalRequirement) -> PermissionDecision;

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
    async fn decide(&self, call: &ToolCall, _approval: &ApprovalRequirement) -> PermissionDecision {
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
    /// For `RequireApprovalScoped` calls: the scope key (e.g. canonical file
    /// path) that an [A]lways press should remember for the session, instead
    /// of granting the whole tool. `None` for tool-wide approvals.
    pub scope: Option<String>,
}

/// Interactive permission decider (used by AgentLoop).
/// Checks PermissionStore first (session grants, overrides),
/// then falls back to sending approval request via channel.
///
/// When `dangerously_skip_permissions` is true, all tool calls are
/// auto-approved without prompting the user — equivalent to Claude
/// Code's `--dangerously-skip-permissions` mode.
pub struct InteractivePermissionDecider {
    request_tx: mpsc::UnboundedSender<ApprovalRequest>,
    response_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<PermissionDecision>>,
    /// Shared permission store — checked before sending interactive requests.
    /// AgentLoop writes to this (grant_session on ApproveToolAlways),
    /// TurnRunner reads from it (check before prompting user).
    permission_store: std::sync::Arc<std::sync::RwLock<crate::tool::PermissionStore>>,
    /// When true, auto-approve every tool call without prompting.
    /// Set via the `--dangerously-skip-permissions` CLI flag.
    dangerously_skip_permissions: bool,
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
            dangerously_skip_permissions: false,
        }
    }

    /// Create with the dangerously-skip-permissions flag.
    /// When `skip` is true, `decide()` always returns `Allow` and
    /// `will_auto_approve()` always returns `true`.
    pub fn new_with_skip_permissions(
        request_tx: mpsc::UnboundedSender<ApprovalRequest>,
        response_rx: mpsc::UnboundedReceiver<PermissionDecision>,
        permission_store: std::sync::Arc<std::sync::RwLock<crate::tool::PermissionStore>>,
        skip: bool,
    ) -> Self {
        Self {
            request_tx,
            response_rx: tokio::sync::Mutex::new(response_rx),
            permission_store,
            dangerously_skip_permissions: skip,
        }
    }
}

#[async_trait]
impl PermissionDecider for InteractivePermissionDecider {
    async fn decide(&self, call: &ToolCall, approval: &ApprovalRequirement) -> PermissionDecision {
        // --dangerously-skip-permissions: auto-approve everything.
        if self.dangerously_skip_permissions {
            return PermissionDecision::Allow;
        }

        // Check PermissionStore first — session grants and overrides
        // take effect without prompting the user again. The full
        // `ApprovalRequirement` (including `RequireApprovalAlways`) is
        // passed through so the store can honor variants that must
        // always prompt regardless of prior session grants.
        if let Ok(store) = self.permission_store.read() {
            match store.check(&call.name, approval) {
                PermissionDecision::Allow => return PermissionDecision::Allow,
                // `check()` never returns AllowAlways, but the match must be
                // exhaustive — treat it as Allow defensively.
                PermissionDecision::AllowAlways => return PermissionDecision::Allow,
                PermissionDecision::Deny => return PermissionDecision::Deny,
                PermissionDecision::Ask(_) => {} // fall through to interactive
            }
        }

        let (reason, scope) = match approval {
            ApprovalRequirement::RequireApproval(r)
            | ApprovalRequirement::RequireApprovalAlways(r) => (r.clone(), None),
            ApprovalRequirement::RequireApprovalScoped { reason, scope } => {
                (reason.clone(), Some(scope.clone()))
            }
            ApprovalRequirement::AutoApprove => return PermissionDecision::Allow,
        };

        // Not in store — send interactive approval request
        let request = ApprovalRequest {
            call: call.clone(),
            reason,
            scope: scope.clone(),
        };
        if self.request_tx.send(request).is_err() {
            eprintln!(
                "[permission] approval request channel closed for tool '{}' — \
                 denying due to internal channel failure, not a user denial",
                call.name
            );
            return PermissionDecision::Deny;
        }
        let resp = {
            let mut rx = self.response_rx.lock().await;
            rx.recv().await.unwrap_or_else(|| {
                eprintln!(
                    "[permission] approval response channel closed for tool '{}' — \
                     denying due to internal channel failure, not a user denial",
                    call.name
                );
                PermissionDecision::Deny
            })
        };
        match resp {
            // "Always allow" from the responder: persist the grant for the
            // session (scoped if the approval carried a scope, else tool-wide)
            // then act as a normal Allow for this call.
            PermissionDecision::AllowAlways => {
                if let Ok(mut store) = self.permission_store.write() {
                    match &scope {
                        Some(s) => store.grant_session_scope(s),
                        None => store.grant_session(&call.name),
                    }
                }
                PermissionDecision::Allow
            }
            other => other,
        }
    }

    fn will_auto_approve(&self, call: &ToolCall, approval: &ApprovalRequirement) -> bool {
        // --dangerously-skip-permissions: everything is auto-approved.
        if self.dangerously_skip_permissions {
            return true;
        }

        // Mirror the PermissionStore check that `decide()` performs
        // before falling through to the interactive channel. The full
        // variant is forwarded so `RequireApprovalAlways` continues to
        // prompt even when a session grant exists for the tool.
        if matches!(approval, ApprovalRequirement::AutoApprove) {
            return true;
        }
        if let Ok(store) = self.permission_store.read() {
            matches!(store.check(&call.name, approval), PermissionDecision::Allow)
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
            d.decide(&make_call("bash"), &ApprovalRequirement::RequireApproval("dangerous".into())).await,
            PermissionDecision::Allow
        ));
    }

    #[tokio::test]
    async fn test_auto_deny_denies_all() {
        let d = AutoPermissionDecider::new(AutoPermissionMode::DenyAll);
        assert!(matches!(
            d.decide(&make_call("bash"), &ApprovalRequirement::RequireApproval("dangerous".into())).await,
            PermissionDecision::Deny
        ));
    }

    #[tokio::test]
    async fn test_auto_accept_edits_allows_write() {
        let d = AutoPermissionDecider::new(AutoPermissionMode::AcceptEdits);
        assert!(matches!(
            d.decide(&make_call("create_file"), &ApprovalRequirement::RequireApproval("write".into())).await,
            PermissionDecision::Allow
        ));
        assert!(matches!(
            d.decide(&make_call("edit_file"), &ApprovalRequirement::RequireApproval("edit".into())).await,
            PermissionDecision::Allow
        ));
        assert!(matches!(
            d.decide(&make_call("search_replace"), &ApprovalRequirement::RequireApproval("sr".into())).await,
            PermissionDecision::Allow
        ));
    }

    #[tokio::test]
    async fn test_auto_accept_edits_denies_bash() {
        let d = AutoPermissionDecider::new(AutoPermissionMode::AcceptEdits);
        assert!(matches!(
            d.decide(&make_call("bash"), &ApprovalRequirement::RequireApproval("dangerous".into())).await,
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
        let approval = ApprovalRequirement::RequireApproval("dangerous".into());
        let fut = d.decide(&call, &approval);

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
        let approval = ApprovalRequirement::RequireApproval("dangerous".into());
        let fut = d.decide(&call, &approval);

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
            d.decide(&call, &ApprovalRequirement::RequireApproval("dangerous".into())).await,
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
        let decision = d.decide(&call, &ApprovalRequirement::RequireApproval("dangerous".into())).await;
        assert!(matches!(decision, PermissionDecision::Allow));
    }

    #[tokio::test]
    async fn test_interactive_require_approval_always_with_grant_still_prompts() {
        // RequireApprovalAlways must always prompt the user — even if [A] was
        // pressed earlier for this tool. The session grant must NOT bypass it.
        let (req_tx, mut req_rx) = mpsc::unbounded_channel();
        let (resp_tx, resp_rx) = mpsc::unbounded_channel();
        let store =
            std::sync::Arc::new(std::sync::RwLock::new(crate::tool::PermissionStore::new()));
        store.write().unwrap().grant_session("bash");
        let d = InteractivePermissionDecider::new(req_tx, resp_rx, store);
        let call = make_call("bash");
        let approval = ApprovalRequirement::RequireApprovalAlways("sensitive".into());
        let fut = d.decide(&call, &approval);

        // Expect a request on the channel (NOT auto-approved by the store).
        // Deny it and confirm decide() returns Deny.
        tokio::spawn(async move {
            let _req = req_rx.recv().await.expect("channel must receive request");
            resp_tx.send(PermissionDecision::Deny).unwrap();
        });

        assert!(matches!(fut.await, PermissionDecision::Deny));
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
        // RequireApprovalAlways means "always prompt, even if [A] was pressed
        // earlier for this tool". A session grant must NOT bypass it.
        let (req_tx, _req_rx) = mpsc::unbounded_channel();
        let (_resp_tx, resp_rx) = mpsc::unbounded_channel();
        let store =
            std::sync::Arc::new(std::sync::RwLock::new(crate::tool::PermissionStore::new()));
        store.write().unwrap().grant_session("bash");
        let d = InteractivePermissionDecider::new(req_tx, resp_rx, store);
        let call = make_call("bash");
        assert!(!d.will_auto_approve(&call, &ApprovalRequirement::RequireApprovalAlways("sensitive".into())));
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

    #[test]
    fn test_will_auto_approve_scoped_same_path_after_grant() {
        // The reported bug: after pressing [A] on a sensitive read, re-reading
        // the SAME file in segments (any offset/limit → identical scope) must
        // auto-approve without re-prompting — while a DIFFERENT sensitive file
        // still requires its own confirmation.
        let (req_tx, _req_rx) = mpsc::unbounded_channel();
        let (_resp_tx, resp_rx) = mpsc::unbounded_channel();
        let store =
            std::sync::Arc::new(std::sync::RwLock::new(crate::tool::PermissionStore::new()));
        store
            .write()
            .unwrap()
            .grant_session_scope("/home/u/.config/notes.md");
        let d = InteractivePermissionDecider::new(req_tx, resp_rx, store);
        let call = make_call("read_file");

        let same = ApprovalRequirement::RequireApprovalScoped {
            reason: "sensitive".into(),
            scope: "/home/u/.config/notes.md".into(),
        };
        assert!(
            d.will_auto_approve(&call, &same),
            "re-reading the granted file must not re-prompt"
        );

        let other = ApprovalRequirement::RequireApprovalScoped {
            reason: "sensitive".into(),
            scope: "/home/u/.ssh/id_rsa".into(),
        };
        assert!(
            !d.will_auto_approve(&call, &other),
            "a different sensitive file must still require approval"
        );
    }

    // ── dangerously-skip-permissions tests ──

    /// Helper: create an InteractivePermissionDecider with skip_permissions=true.
    fn make_skip_decider() -> InteractivePermissionDecider {
        let (req_tx, _req_rx) = mpsc::unbounded_channel();
        let (_resp_tx, resp_rx) = mpsc::unbounded_channel();
        let store =
            std::sync::Arc::new(std::sync::RwLock::new(crate::tool::PermissionStore::new()));
        InteractivePermissionDecider::new_with_skip_permissions(req_tx, resp_rx, store, true)
    }

    /// Helper: create an InteractivePermissionDecider with skip_permissions=false.
    fn make_normal_decider() -> InteractivePermissionDecider {
        let (req_tx, _req_rx) = mpsc::unbounded_channel();
        let (_resp_tx, resp_rx) = mpsc::unbounded_channel();
        let store =
            std::sync::Arc::new(std::sync::RwLock::new(crate::tool::PermissionStore::new()));
        InteractivePermissionDecider::new(req_tx, resp_rx, store)
    }

    // ── decide() tests ──

    #[tokio::test]
    async fn test_skip_permissions_auto_approves_require_approval() {
        let d = make_skip_decider();
        let call = make_call("bash");
        let decision = d
            .decide(&call, &ApprovalRequirement::RequireApproval("needs approval".into()))
            .await;
        assert!(
            matches!(decision, PermissionDecision::Allow),
            "skip_permissions should auto-approve RequireApproval"
        );
    }

    #[tokio::test]
    async fn test_skip_permissions_auto_approves_require_approval_always() {
        // This is the critical safety test: --dangerously-skip-permissions
        // even bypasses RequireApprovalAlways (e.g. rm -rf, git push --force).
        let d = make_skip_decider();
        let call = make_call("bash");
        let decision = d
            .decide(&call, &ApprovalRequirement::RequireApprovalAlways("sensitive".into()))
            .await;
        assert!(
            matches!(decision, PermissionDecision::Allow),
            "skip_permissions should auto-approve even RequireApprovalAlways"
        );
    }

    #[tokio::test]
    async fn test_skip_permissions_auto_approves_auto_approve() {
        let d = make_skip_decider();
        let call = make_call("read_file");
        let decision = d
            .decide(&call, &ApprovalRequirement::AutoApprove)
            .await;
        assert!(
            matches!(decision, PermissionDecision::Allow),
            "skip_permissions should auto-approve AutoApprove"
        );
    }

    #[tokio::test]
    async fn test_skip_permissions_auto_approves_mcp_tool() {
        let d = make_skip_decider();
        let call = make_call("mcp__zouwu__query");
        let decision = d
            .decide(&call, &ApprovalRequirement::RequireApproval("mcp tool".into()))
            .await;
        assert!(
            matches!(decision, PermissionDecision::Allow),
            "skip_permissions should auto-approve MCP tools"
        );
    }

    #[tokio::test]
    async fn test_normal_mode_still_prompts_require_approval() {
        // Regression: ensure normal mode (skip=false) still requires approval.
        // We can't fully test the interactive channel here (would block),
        // but we can verify that it does NOT return Allow immediately.
        let (req_tx, mut req_rx) = mpsc::unbounded_channel();
        let (resp_tx, resp_rx) = mpsc::unbounded_channel();
        let store =
            std::sync::Arc::new(std::sync::RwLock::new(crate::tool::PermissionStore::new()));
        let d = InteractivePermissionDecider::new(req_tx, resp_rx, store);

        let call = make_call("bash");
        let approval = ApprovalRequirement::RequireApproval("dangerous".into());
        let fut = d.decide(&call, &approval);

        // Normal mode should send a request on the channel (not auto-approve).
        tokio::spawn(async move {
            let req = req_rx.recv().await.expect("should receive approval request");
            assert_eq!(req.call.name, "bash");
            resp_tx.send(PermissionDecision::Deny).unwrap();
        });

        assert!(
            matches!(fut.await, PermissionDecision::Deny),
            "normal mode should not auto-approve RequireApproval"
        );
    }

    // ── will_auto_approve() tests ──

    #[test]
    fn test_will_auto_approve_skip_permissions_require_approval() {
        let d = make_skip_decider();
        let call = make_call("bash");
        assert!(
            d.will_auto_approve(&call, &ApprovalRequirement::RequireApproval("dangerous".into())),
            "skip_permissions: will_auto_approve should return true for RequireApproval"
        );
    }

    #[test]
    fn test_will_auto_approve_skip_permissions_require_approval_always() {
        let d = make_skip_decider();
        let call = make_call("bash");
        assert!(
            d.will_auto_approve(&call, &ApprovalRequirement::RequireApprovalAlways("sensitive".into())),
            "skip_permissions: will_auto_approve should return true even for RequireApprovalAlways"
        );
    }

    #[test]
    fn test_will_auto_approve_skip_permissions_auto_approve() {
        let d = make_skip_decider();
        let call = make_call("read_file");
        assert!(
            d.will_auto_approve(&call, &ApprovalRequirement::AutoApprove),
            "skip_permissions: will_auto_approve should return true for AutoApprove"
        );
    }

    #[test]
    fn test_will_auto_approve_skip_permissions_mcp_tool() {
        let d = make_skip_decider();
        let call = make_call("mcp__custom__query");
        assert!(
            d.will_auto_approve(&call, &ApprovalRequirement::RequireApproval("mcp".into())),
            "skip_permissions: will_auto_approve should return true for MCP tools"
        );
    }

    #[test]
    fn test_will_auto_approve_normal_mode_require_approval_is_false() {
        let d = make_normal_decider();
        let call = make_call("bash");
        assert!(
            !d.will_auto_approve(&call, &ApprovalRequirement::RequireApproval("dangerous".into())),
            "normal mode: will_auto_approve should return false for RequireApproval without session grant"
        );
    }

    #[test]
    fn test_will_auto_approve_normal_mode_require_approval_always_is_false() {
        let d = make_normal_decider();
        let call = make_call("bash");
        assert!(
            !d.will_auto_approve(&call, &ApprovalRequirement::RequireApprovalAlways("sensitive".into())),
            "normal mode: will_auto_approve should return false for RequireApprovalAlways"
        );
    }

    // ── constructor tests ──

    #[test]
    fn test_new_defaults_to_skip_false() {
        let d = make_normal_decider();
        let call = make_call("bash");
        // If skip_permissions were true, this would return true even without session grant.
        assert!(
            !d.will_auto_approve(&call, &ApprovalRequirement::RequireApproval("test".into())),
            "new() should default to skip_permissions=false"
        );
    }

    #[test]
    fn test_new_with_skip_permissions_true() {
        let d = make_skip_decider();
        let call = make_call("bash");
        assert!(
            d.will_auto_approve(&call, &ApprovalRequirement::RequireApproval("test".into())),
            "new_with_skip_permissions(true) should set skip_permissions=true"
        );
    }

    #[tokio::test]
    async fn allow_always_grants_session_for_tool() {
        let (req_tx, mut req_rx) = mpsc::unbounded_channel();
        let (resp_tx, resp_rx) = mpsc::unbounded_channel();
        let store = std::sync::Arc::new(std::sync::RwLock::new(crate::tool::PermissionStore::new()));
        let d = InteractivePermissionDecider::new(req_tx, resp_rx, store.clone());
        let call = make_call("mcp__srv__q");
        let approval = ApprovalRequirement::RequireApproval("mcp".into());
        let fut = d.decide(&call, &approval);
        tokio::spawn(async move {
            let _req = req_rx.recv().await.unwrap();
            resp_tx.send(PermissionDecision::AllowAlways).unwrap();
        });
        assert!(matches!(fut.await, PermissionDecision::Allow));
        let call2 = make_call("mcp__srv__q");
        assert!(d.will_auto_approve(&call2, &ApprovalRequirement::RequireApproval("mcp".into())));
    }

    #[tokio::test]
    async fn allow_always_grants_scope_for_scoped_approval() {
        let (req_tx, mut req_rx) = mpsc::unbounded_channel();
        let (resp_tx, resp_rx) = mpsc::unbounded_channel();
        let store = std::sync::Arc::new(std::sync::RwLock::new(crate::tool::PermissionStore::new()));
        let d = InteractivePermissionDecider::new(req_tx, resp_rx, store.clone());
        let call = make_call("read_file");
        let approval = ApprovalRequirement::RequireApprovalScoped { reason: "sensitive".into(), scope: "/home/u/.ssh/id_rsa".into() };
        let fut = d.decide(&call, &approval);
        tokio::spawn(async move {
            let _req = req_rx.recv().await.unwrap();
            resp_tx.send(PermissionDecision::AllowAlways).unwrap();
        });
        assert!(matches!(fut.await, PermissionDecision::Allow));
        assert!(d.will_auto_approve(&call, &ApprovalRequirement::RequireApprovalScoped { reason: "sensitive".into(), scope: "/home/u/.ssh/id_rsa".into() }));
        assert!(!d.will_auto_approve(&call, &ApprovalRequirement::RequireApproval("x".into())));
    }
}
