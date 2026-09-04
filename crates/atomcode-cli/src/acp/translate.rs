use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, MessageId, SessionUpdate, TextContent, ToolCall as AcpToolCall,
    ToolCallId, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind, UsageUpdate,
};
use atomcode_kernel::event::AgentEvent;

const POLICY_INTERVENTION_NOTICE: &str =
    "Credential protection blocked an unsafe shell operation. Complete the authenticated step in a separate terminal and then ask AtomCode to continue, skip the blocked step, or end the task. Do not paste credentials into chat.";

pub fn tool_kind(name: &str) -> ToolKind {
    let n = name.to_ascii_lowercase();
    if n.contains("read") || n.contains("cat") {
        ToolKind::Read
    } else if n.contains("edit")
        || n.contains("write")
        || n.contains("replace")
        || n.contains("apply")
    {
        ToolKind::Edit
    } else if n.contains("delete") || n.contains("rm") {
        ToolKind::Delete
    } else if n.contains("move") || n.contains("mv") || n.contains("rename") {
        ToolKind::Move
    } else if n.contains("grep") || n.contains("search") || n.contains("glob") || n.contains("find")
    {
        ToolKind::Search
    } else if n.contains("fetch") || n.contains("http") || n.contains("web") {
        ToolKind::Fetch
    } else if n.contains("bash") || n.contains("shell") || n.contains("exec") || n.contains("run") {
        ToolKind::Execute
    } else {
        ToolKind::Other
    }
}

/// Translate one kernel event to an optional v1 `session/update`.
///
/// `message_id` (v1-optional, per the protocol) is stamped onto message and
/// thought chunks: chunks sharing one id belong to the same message, a changed
/// id starts a new one. The caller allocates one id per LLM output round and
/// advances it at every kernel `Usage` event (see
/// [`crate::acp::dispatch::run_prompt_turn`]), so one model response's stream
/// is one message — the same convention the v2 chain already uses.
pub fn event_to_update(ev: &AgentEvent, message_id: Option<&str>) -> Option<SessionUpdate> {
    let msg = message_id.map(MessageId::new);
    match ev {
        AgentEvent::TextDelta(s) => Some(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new(s.clone())))
                .message_id(msg.clone()),
        )),
        AgentEvent::Reasoning(s) => Some(SessionUpdate::AgentThoughtChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new(s.clone())))
                .message_id(msg.clone()),
        )),
        AgentEvent::ToolStarted { call } => Some(SessionUpdate::ToolCall(
            AcpToolCall::new(ToolCallId::new(call.id.clone()), call.name.clone())
                .kind(tool_kind(&call.name))
                .status(ToolCallStatus::InProgress)
                .raw_input(crate::acp::replay::raw_input_from_arguments(
                    &call.arguments,
                )),
        )),
        AgentEvent::ToolResult { result } => {
            let status = if result.is_error {
                ToolCallStatus::Failed
            } else {
                ToolCallStatus::Completed
            };
            // The protocol's `diff` content block (structured old/new text)
            // is NOT emitted here: the kernel `ToolResult` carries only a
            // string `content`, with no structured old/new pair, so there is
            // nothing to map. Edit tools render their changes as text in the
            // result; a true `diff` block needs an event-side structure first
            // (tracked as a later-phase item in the ACP roadmap).
            let content: ToolCallContent = result.content.clone().into();
            let fields = ToolCallUpdateFields::new()
                .status(status)
                .content(vec![content]);
            Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::new(result.call_id.clone()),
                fields,
            )))
        }
        AgentEvent::PolicyIntervention { .. } => Some(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new(
                POLICY_INTERVENTION_NOTICE.to_string(),
            )))
            .message_id(msg),
        )),
        // `used` and `size` are the protocol-required, non-null token counts.
        // `cost` (optional) is deliberately NOT populated: the kernel
        // `MessageMeta` carries token usage but no price, and there is no
        // pricing table on this path — fabricating a currency amount would be
        // misleading. See `commands.rs::usage_text` ("cost requires a pricing
        // table") for the same stance on the text side.
        AgentEvent::Usage(meta) => Some(SessionUpdate::UsageUpdate(UsageUpdate::new(
            u64::from(meta.used_tokens),
            u64::from(meta.ctx_window),
        ))),
        _ => None,
    }
}

use agent_client_protocol::schema::v1::StopReason as AcpStop;
use agent_client_protocol::schema::v1::ToolCallContent;
use atomcode_kernel::event::StopReason as KStop;

pub fn stop_reason(r: KStop) -> Result<AcpStop, &'static str> {
    match r {
        KStop::Stopped => Ok(AcpStop::EndTurn),
        KStop::MaxRounds
        | KStop::MaxContinuations
        | KStop::RepeatLoop
        | KStop::ToolLoopDetected => Ok(AcpStop::MaxTurnRequests),
        KStop::Cancelled => Ok(AcpStop::Cancelled),
        KStop::PromptRejected | KStop::PolicyDenied => Ok(AcpStop::Refusal),
        KStop::ProviderError => Err("provider error"),
        KStop::Timeout => Err("turn timed out"),
        _ => Err("turn ended abnormally"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::event::AgentEvent;

    fn tag(u: &agent_client_protocol::schema::v1::SessionUpdate) -> String {
        serde_json::to_value(u).unwrap()["sessionUpdate"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn text_delta_maps_to_agent_message_chunk() {
        let u = event_to_update(&AgentEvent::TextDelta("hi".into()), None).unwrap();
        assert_eq!(tag(&u), "agent_message_chunk");
        let v = serde_json::to_value(&u).unwrap();
        assert_eq!(v["content"]["text"], "hi");
    }

    #[test]
    fn message_id_is_stamped_on_message_and_thought_chunks() {
        let u = event_to_update(&AgentEvent::TextDelta("hi".into()), Some("m1")).unwrap();
        let v = serde_json::to_value(&u).unwrap();
        assert_eq!(v["messageId"], "m1");
        assert_eq!(v["sessionUpdate"], "agent_message_chunk");

        let u = event_to_update(&AgentEvent::Reasoning("why".into()), Some("m1")).unwrap();
        let v = serde_json::to_value(&u).unwrap();
        assert_eq!(v["messageId"], "m1");
        assert_eq!(v["sessionUpdate"], "agent_thought_chunk");
    }

    #[test]
    fn reasoning_maps_to_agent_thought_chunk() {
        let u = event_to_update(&AgentEvent::Reasoning("why".into()), None).unwrap();
        assert_eq!(tag(&u), "agent_thought_chunk");
    }

    #[test]
    fn usage_maps_to_session_usage_update() {
        let meta = atomcode_kernel::message::MessageMeta {
            tokens: atomcode_kernel::stream::TokenUsage {
                prompt: 100,
                completion: 50,
                cached: 0,
            },
            elapsed_ms: 100,
            reasoning_elapsed_ms: 10,
            ctx_window: 200_000,
            used_tokens: 4_200,
            utilization: 0.02,
            round: 1,
            turn_id: 1,
            request_id: 1,
            provider_response_id: None,
            provider_model: None,
            session_id: None,
            finish_reason: "stop".into(),
        };
        let u = event_to_update(&AgentEvent::Usage(meta), None).unwrap();
        assert_eq!(tag(&u), "usage_update");
        let v = serde_json::to_value(&u).unwrap();
        assert_eq!(v["used"], 4200);
        assert_eq!(v["size"], 200_000);
    }

    #[test]
    fn policy_intervention_exposes_safe_recovery_without_secret_material() {
        let event = AgentEvent::PolicyIntervention {
            intervention: atomcode_kernel::event::PolicyIntervention::credential_shell_blocked(),
        };
        let update = event_to_update(&event, None).expect("policy recovery notice");
        let value = serde_json::to_value(update).unwrap();
        let text = value["content"]["text"].as_str().unwrap();
        assert!(text.contains("separate terminal"));
        assert!(text.contains("skip"));
        assert!(!text.contains("TOKEN="));
    }

    #[test]
    fn tool_started_maps_to_tool_call_with_kind() {
        use atomcode_kernel::tool::ToolCall;
        let call = ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        };
        let u = event_to_update(&AgentEvent::ToolStarted { call }, None).unwrap();
        let v = serde_json::to_value(&u).unwrap();
        assert_eq!(v["sessionUpdate"], "tool_call");
        assert_eq!(v["toolCallId"], "c1");
        assert_eq!(v["kind"], "execute");
    }

    #[test]
    fn tool_result_maps_to_update_with_status() {
        use atomcode_kernel::tool::ToolResult;
        let result = ToolResult {
            call_id: "c1".into(),
            content: "ok".into(),
            is_error: false,
            images: vec![],
        };
        let u = event_to_update(&AgentEvent::ToolResult { result }, None).unwrap();
        let v = serde_json::to_value(&u).unwrap();
        assert_eq!(v["sessionUpdate"], "tool_call_update");
        assert_eq!(v["toolCallId"], "c1");
        assert_eq!(v["status"], "completed");
    }

    #[test]
    fn tool_kind_inference() {
        use agent_client_protocol::schema::v1::ToolKind;
        assert_eq!(tool_kind("read_file"), ToolKind::Read);
        assert_eq!(tool_kind("edit_file"), ToolKind::Edit);
        assert_eq!(tool_kind("bash"), ToolKind::Execute);
        assert_eq!(tool_kind("grep"), ToolKind::Search);
        assert_eq!(tool_kind("web_fetch"), ToolKind::Fetch);
        assert_eq!(tool_kind("totally_unknown"), ToolKind::Other);
    }

    #[test]
    fn stop_reason_mapping() {
        use agent_client_protocol::schema::v1::StopReason as Acp;
        use atomcode_kernel::event::StopReason as K;
        assert_eq!(stop_reason(K::Stopped).unwrap(), Acp::EndTurn);
        assert_eq!(stop_reason(K::MaxRounds).unwrap(), Acp::MaxTurnRequests);
        assert_eq!(
            stop_reason(K::MaxContinuations).unwrap(),
            Acp::MaxTurnRequests
        );
        assert_eq!(
            stop_reason(K::ToolLoopDetected).unwrap(),
            Acp::MaxTurnRequests
        );
        assert_eq!(stop_reason(K::RepeatLoop).unwrap(), Acp::MaxTurnRequests);
        assert_eq!(stop_reason(K::Cancelled).unwrap(), Acp::Cancelled);
        assert_eq!(stop_reason(K::PromptRejected).unwrap(), Acp::Refusal);
        assert_eq!(stop_reason(K::PolicyDenied).unwrap(), Acp::Refusal);
        assert!(stop_reason(K::ProviderError).is_err());
        assert!(stop_reason(K::Timeout).is_err());
    }
}
