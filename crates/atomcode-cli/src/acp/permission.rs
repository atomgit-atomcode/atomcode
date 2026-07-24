//! Approval round-trip: maps kernel `AgentEvent::Request { kind: "approval" }`
//! to ACP `session/request_permission` and feeds the chosen option back as
//! `AgentCommand::Respond`.
//!
//! The wire shapes are read from
//! `atomcode_capabilities::tools::approval::{ApprovalRequest, ApprovalResponse}`.
//! Payload fields: `call_id: String`, `tool: String`, `args: String`.
//! Response JSON: `{"decision": "allow"|"allow_always"|"deny", "remember": bool}`.
//!
//! This module only needs to PRODUCE the response JSON and READ the request
//! payload — both shapes are matched exactly as the kernel's
//! `PermissionDecision::from_value` parses them.

use agent_client_protocol::schema::v1::{
    PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    SessionId, ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Client, ConnectionTo};
use atomcode_coding::CodingRuntimeHandle;

/// The four standard permission options, each with a stable `option_id` string
/// that `outcome_to_decision` maps back to the kernel's decision JSON.
pub fn permission_options() -> Vec<PermissionOption> {
    vec![
        PermissionOption::new("allow_once", "Allow once", PermissionOptionKind::AllowOnce),
        PermissionOption::new(
            "allow_always",
            "Always allow",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new(
            "reject_once",
            "Reject once",
            PermissionOptionKind::RejectOnce,
        ),
        PermissionOption::new(
            "reject_always",
            "Always reject",
            PermissionOptionKind::RejectAlways,
        ),
    ]
}

/// Map an ACP option_id to the kernel's `ApprovalResponse` JSON.
///
/// `allow_once`   → `{"decision":"allow"}`
/// `allow_always` → `{"decision":"allow","remember":true}`
/// anything else  → `{"decision":"deny"}` (fail closed — covers reject_* and unknowns)
pub fn outcome_to_decision(option_id: &str) -> serde_json::Value {
    match option_id {
        "allow_once" => serde_json::json!({"decision": "allow"}),
        "allow_always" => serde_json::json!({"decision": "allow", "remember": true}),
        _ => serde_json::json!({"decision": "deny"}),
    }
}

/// Handle a kernel approval round-trip.
///
/// Called by the prompt-turn loop (Task 7) when the kernel emits
/// `AgentEvent::Request { kind: "approval", payload }`.
///
/// 1. Extracts `tool` and `call_id` from the payload.
/// 2. Sends `session/request_permission` to the ACP client via `cx`.
/// 3. Maps the client's chosen option_id back to the kernel's `ApprovalResponse` JSON.
/// 4. Answers the kernel with `AgentCommand::Respond { id: req_id, value: decision }`.
///
/// `Cancelled` outcome (and any unrecognised option) → deny (fail closed).
pub async fn handle_approval(
    cx: &ConnectionTo<Client>,
    session_id: &SessionId,
    runtime: &CodingRuntimeHandle,
    req_id: u64,
    payload: serde_json::Value,
) -> Result<(), agent_client_protocol::Error> {
    let tool = payload
        .get("tool")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let call_id = payload
        .get("call_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let tc = ToolCallUpdate::new(
        ToolCallId::new(call_id),
        ToolCallUpdateFields::new().title(tool),
    );

    // The `session/request_permission` round-trip must NEVER propagate its error:
    // this runs inside the spawned prompt turn, and returning `Err` there tears the
    // WHOLE ACP connection down (server exits, the Zed thread is wiped). A round-trip
    // failure — the client cancelled/ESC'd the prompt, sent an unexpected message, or
    // hit a transient error — is a single-call event, not a reason to kill the session.
    // On ANY failure, fail closed (deny) so the kernel still gets a decision and unparks
    // (otherwise it reports "no decision received … internal channel failure"), and the
    // turn continues. eprintln goes to stderr (the ACP log channel; stdout is JSON-RPC).
    let decision = match cx
        .send_request(RequestPermissionRequest::new(
            session_id.clone(),
            tc,
            permission_options(),
        ))
        .block_task()
        .await
    {
        Ok(resp) => match resp.outcome {
            RequestPermissionOutcome::Selected(sel) => {
                outcome_to_decision(sel.option_id.0.as_ref())
            }
            // Cancelled or any future non-exhaustive variant → fail closed.
            _ => serde_json::json!({"decision": "deny"}),
        },
        Err(e) => {
            eprintln!("acp: request_permission round-trip failed ({e}); denying this call");
            serde_json::json!({"decision": "deny"})
        }
    };

    let _ = runtime.respond(req_id, decision).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_mapping_is_fail_closed() {
        assert_eq!(
            outcome_to_decision("allow_once"),
            serde_json::json!({"decision":"allow"})
        );
        assert_eq!(
            outcome_to_decision("allow_always"),
            serde_json::json!({"decision":"allow","remember":true})
        );
        assert_eq!(
            outcome_to_decision("reject_once"),
            serde_json::json!({"decision":"deny"})
        );
        assert_eq!(
            outcome_to_decision("reject_always"),
            serde_json::json!({"decision":"deny"})
        );
        assert_eq!(
            outcome_to_decision("anything_else"),
            serde_json::json!({"decision":"deny"})
        );
    }

    #[test]
    fn four_options_offered() {
        let opts = permission_options();
        assert_eq!(opts.len(), 4);
        assert_eq!(opts[0].option_id.0.as_ref(), "allow_once");
    }
}
