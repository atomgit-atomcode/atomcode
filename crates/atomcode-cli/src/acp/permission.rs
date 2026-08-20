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
use agent_client_protocol::schema::v2::{
    PermissionOption as V2PermissionOption, PermissionOptionKind as V2PermissionOptionKind,
    RequestPermissionOutcome as V2RequestPermissionOutcome,
    RequestPermissionRequest as V2RequestPermissionRequest, RequestPermissionSubject,
    SessionId as V2SessionId, ToolCallId as V2ToolCallId, ToolCallPermissionSubject,
    ToolCallUpdate as V2ToolCallUpdate,
};
use agent_client_protocol::{Client, ConnectionTo};
use atomcode_coding::CodingRuntimeHandle;

/// The three standard permission options, each with a stable `option_id` string
/// that `outcome_to_decision` maps back to the kernel's decision JSON.
///
/// There is deliberately NO "always reject" option: the kernel's
/// `PermissionDecision` has no deny-remember variant (only
/// allow / allow-always / deny), so a `reject_always` label would promise
/// persistence the kernel cannot deliver and the same call would prompt again
/// on the next turn.
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
    ]
}

/// The v2 schema's `PermissionOption` type — same three option ids as the v1
/// set, but the v2 types are distinct (per the protocol version's own schema;
/// the v2 `PermissionOptionKind` additionally carries a `RejectAlways` variant
/// the kernel cannot honour — deliberately not offered, mirroring the v1
/// rationale in [`permission_options`]).
pub fn v2_permission_options() -> Vec<V2PermissionOption> {
    vec![
        V2PermissionOption::new(
            "allow_once",
            "Allow once",
            V2PermissionOptionKind::AllowOnce,
        ),
        V2PermissionOption::new(
            "allow_always",
            "Always allow",
            V2PermissionOptionKind::AllowAlways,
        ),
        V2PermissionOption::new(
            "reject_once",
            "Reject once",
            V2PermissionOptionKind::RejectOnce,
        ),
    ]
}

/// Map an ACP option_id to the kernel's `ApprovalResponse` JSON.
///
/// `allow_once`   → `{"decision":"allow"}`
/// `allow_always` → `{"decision":"allow","remember":true}`
/// anything else  → `{"decision":"deny"}` (fail closed — covers `reject_once`,
///                  cancelled outcomes, and unknown ids)
pub fn outcome_to_decision(option_id: &str) -> serde_json::Value {
    match option_id {
        "allow_once" => serde_json::json!({"decision": "allow"}),
        "allow_always" => serde_json::json!({"decision": "allow", "remember": true}),
        _ => serde_json::json!({"decision": "deny"}),
    }
}

/// Handle a kernel approval round-trip.
///
/// Called by the prompt-turn loop when the kernel emits
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

/// Handle a kernel approval round-trip with the **v2** `session/request_permission`
/// wire shape.
///
/// The v2 draft defines its own `RequestPermissionRequest` (required `title`,
/// optional `description`/`subject`) distinct from the v1 shape; this v2 handler
/// sends `title` = tool name and a `ToolCallPermissionSubject` so v2-native
/// clients get the request in the shape their schema expects. The v1 chain's
/// [`handle_approval`] is untouched.
///
/// Fail-closed semantics are identical to the v1 path: the round-trip must NEVER
/// propagate its error (it runs inside the spawned prompt turn — returning `Err`
/// tears the whole ACP connection down). On ANY failure — client cancelled,
/// unexpected response, or a transport error — the kernel is answered with
/// `{"decision":"deny"}` and the turn continues.
pub async fn handle_approval_v2(
    cx: &ConnectionTo<Client>,
    session_id: &V2SessionId,
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

    let subject = RequestPermissionSubject::ToolCall(Box::new(ToolCallPermissionSubject::new(
        V2ToolCallUpdate::new(V2ToolCallId::new(call_id)).title(tool.clone()),
    )));
    let request =
        V2RequestPermissionRequest::new(session_id.clone(), tool, v2_permission_options())
            .subject(subject);

    let decision = match cx.send_request(request).block_task().await {
        Ok(resp) => match resp.outcome {
            V2RequestPermissionOutcome::Selected(sel) => {
                outcome_to_decision(sel.option_id.0.as_ref())
            }
            // Cancelled or any future non-exhaustive variant → fail closed.
            _ => serde_json::json!({"decision": "deny"}),
        },
        Err(e) => {
            eprintln!("acp: v2 request_permission round-trip failed ({e}); denying this call");
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
            outcome_to_decision("anything_else"),
            serde_json::json!({"decision":"deny"})
        );
    }

    #[test]
    fn three_options_offered_without_always_reject() {
        let opts = permission_options();
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[0].option_id.0.as_ref(), "allow_once");
        assert_eq!(opts[1].option_id.0.as_ref(), "allow_always");
        assert_eq!(opts[2].option_id.0.as_ref(), "reject_once");
        // The kernel cannot remember a rejection, so "always reject" must not
        // be offered (it would prompt again on the next identical call).
        assert!(opts
            .iter()
            .all(|o| o.option_id.0.as_ref() != "reject_always"));
    }

    #[test]
    fn v2_options_offer_same_three_ids_with_v2_kinds() {
        let opts = v2_permission_options();
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[0].option_id.0.as_ref(), "allow_once");
        assert_eq!(opts[1].option_id.0.as_ref(), "allow_always");
        assert_eq!(opts[2].option_id.0.as_ref(), "reject_once");
        // The v2 kind enum has a RejectAlways variant the kernel cannot
        // honour — it must NOT be offered, mirroring the v1 rationale.
        assert!(opts
            .iter()
            .all(|o| o.option_id.0.as_ref() != "reject_always"));
        // Kinds must serialize to the v2 wire names (snake_case, same as v1).
        let kinds: Vec<String> = opts
            .iter()
            .map(|o| {
                serde_json::to_value(&o.kind)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(kinds, vec!["allow_once", "allow_always", "reject_once"]);
    }

    #[test]
    fn v2_selected_option_feeds_same_decision_mapping() {
        // The v2 response's `SelectedPermissionOutcome` carries the same
        // `option_id` the v1 path maps — the shared `outcome_to_decision`
        // must produce the same kernel JSON for v2 selections.
        assert_eq!(
            outcome_to_decision("allow_once"),
            serde_json::json!({"decision":"allow"})
        );
        assert_eq!(
            outcome_to_decision("allow_always"),
            serde_json::json!({"decision":"allow","remember":true})
        );
    }
}
