//! Tool-approval decision logic for the native adapter.
//!
//! The kernel is neutral about approval: it round-trips every Risky tool call as a
//! generic `Request { kind: APPROVAL_KIND, payload: ApprovalRequest }` and waits for
//! a `Respond`. The driver owns the policy. The pure pieces here are:
//! - [`bypass_auto_approval`]: under `--dangerously-skip-permissions`, answer `allow`
//!   WITHOUT prompting (matches v1's pre-prompt auto-allow);
//! - [`approval_needed_event`]: turn an [`ApprovalRequest`] into the `ApprovalNeeded`
//!   UI event the renderer prompts from.
//!
//! The round-trip state machine itself (one pending approval at a time, displacing
//! an older one fail-closed, sending the `Respond`) lives in the adapter run loop.

use atomcode_capabilities::tools::{ApprovalRequest, ApprovalResponse};
use atomcode_core::conversation::ConversationSnapshot;
use atomcode_core::tool::ToolCall;

use super::event::UiEvent;

/// `--dangerously-skip-permissions`: auto-approve without prompting. `None` ⇒
/// prompt the user as usual.
pub(crate) fn bypass_auto_approval(skip_permissions: bool) -> Option<ApprovalResponse> {
    skip_permissions.then(ApprovalResponse::allow)
}

/// Build the `ApprovalNeeded` UI event from a kernel approval request. The
/// `call_id` correlates the prompt with the later started/result events.
pub(crate) fn approval_needed_event(req: ApprovalRequest) -> UiEvent {
    UiEvent::ApprovalNeeded {
        tool_name: req.tool.clone(),
        reason: "Requires approval".to_string(),
        call: ToolCall {
            id: req.call_id,
            name: req.tool,
            arguments: req.args,
        },
        snapshot: ConversationSnapshot::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bypass_allows_only_when_skip_permissions() {
        assert!(bypass_auto_approval(true).is_some(), "skip ⇒ auto-allow");
        assert!(bypass_auto_approval(false).is_none(), "no skip ⇒ prompt");
    }

    #[test]
    fn approval_request_maps_to_needed_event_preserving_call() {
        let req = ApprovalRequest {
            call_id: "c1".into(),
            tool: "bash".into(),
            args: "{\"cmd\":\"ls\"}".into(),
        };
        let ev = approval_needed_event(req);
        match ev {
            UiEvent::ApprovalNeeded { tool_name, call, .. } => {
                assert_eq!(tool_name, "bash");
                assert_eq!(call.id, "c1");
                assert_eq!(call.name, "bash");
                assert_eq!(call.arguments, "{\"cmd\":\"ls\"}");
            }
            other => panic!("expected ApprovalNeeded, got {other:?}"),
        }
    }
}
