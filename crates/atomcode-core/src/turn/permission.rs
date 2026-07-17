use crate::tool::{ApprovalRequirement, PermissionDecision, ToolCall};
use async_trait::async_trait;

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
}
