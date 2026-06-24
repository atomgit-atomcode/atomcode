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
use atomcode_kernel::event::{AgentCommand as KCmd, RequestId};

use super::event::UiEvent;

/// `--dangerously-skip-permissions`: auto-approve without prompting. `None` ⇒
/// prompt the user as usual.
pub(crate) fn bypass_auto_approval(skip_permissions: bool) -> Option<ApprovalResponse> {
    skip_permissions.then(ApprovalResponse::allow)
}

/// TAKE a parked approval out of the adapter's mirror and return the kernel command
/// that releases its round-trip as a fail-closed DENY. `None` when nothing is parked.
///
/// Used on EVERY teardown of an in-flight turn (Cancel, respawn / model swap):
/// denying the parked request lets the kernel backfill the cancelled tool's result
/// (so the conversation has no dangling tool_use), and `take()` clears the mirror so
/// a stale approval can't re-fire after a respawn re-reads the snapshot.
pub(crate) fn take_deny_cmd(pending: &mut Option<(RequestId, String)>) -> Option<KCmd> {
    pending.take().map(|(id, _tool)| {
        let value =
            serde_json::to_value(ApprovalResponse::deny()).unwrap_or(serde_json::Value::Null);
        KCmd::Respond { id, value }
    })
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
    fn cancel_releases_a_parked_approval_as_deny_and_clears_the_mirror() {
        use atomcode_capabilities::tools::PermissionDecision;
        // A tool is parked awaiting approval (request id 7). Cancelling the turn must
        // release that round-trip as a DENY *and* clear the adapter's mirror — so the
        // kernel backfills the cancelled tool's result and a subsequent respawn/model
        // swap can't find a lingering approval to re-trigger on the next prompt.
        let mut pending = Some((7u64, "geocoding".to_string()));
        let cmd = take_deny_cmd(&mut pending).expect("a parked approval must be released on cancel");
        assert!(pending.is_none(), "the adapter's pending-approval mirror must be cleared");
        match cmd {
            KCmd::Respond { id, value } => {
                assert_eq!(id, 7);
                assert_eq!(
                    PermissionDecision::from_value(&value),
                    PermissionDecision::Deny,
                    "the released approval must read as a fail-closed DENY"
                );
            }
            other => panic!("expected Respond(deny), got {other:?}"),
        }
    }

    #[test]
    fn nothing_to_release_when_no_approval_is_parked() {
        let mut pending: Option<(u64, String)> = None;
        assert!(take_deny_cmd(&mut pending).is_none());
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
