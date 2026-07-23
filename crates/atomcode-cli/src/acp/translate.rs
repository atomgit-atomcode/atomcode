use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, SessionUpdate, TextContent, ToolCall as AcpToolCall, ToolCallId,
    ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use atomcode_kernel::event::AgentEvent;

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

pub fn event_to_update(ev: &AgentEvent) -> Option<SessionUpdate> {
    match ev {
        AgentEvent::TextDelta(s) => Some(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(s.clone())),
        ))),
        AgentEvent::Reasoning(s) => Some(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(s.clone())),
        ))),
        AgentEvent::ToolStarted { call } => Some(SessionUpdate::ToolCall(
            AcpToolCall::new(ToolCallId::new(call.id.clone()), call.name.clone())
                .kind(tool_kind(&call.name))
                .status(ToolCallStatus::InProgress)
                .raw_input(serde_json::from_str::<serde_json::Value>(&call.arguments).ok()),
        )),
        AgentEvent::ToolResult { result } => {
            let status = if result.is_error {
                ToolCallStatus::Failed
            } else {
                ToolCallStatus::Completed
            };
            let content: ToolCallContent = result.content.clone().into();
            let fields = ToolCallUpdateFields::new()
                .status(status)
                .content(vec![content]);
            Some(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::new(result.call_id.clone()),
                fields,
            )))
        }
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
        KStop::PromptRejected => Ok(AcpStop::Refusal),
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
        let u = event_to_update(&AgentEvent::TextDelta("hi".into())).unwrap();
        assert_eq!(tag(&u), "agent_message_chunk");
        let v = serde_json::to_value(&u).unwrap();
        assert_eq!(v["content"]["text"], "hi");
    }

    #[test]
    fn reasoning_maps_to_agent_thought_chunk() {
        let u = event_to_update(&AgentEvent::Reasoning("why".into())).unwrap();
        assert_eq!(tag(&u), "agent_thought_chunk");
    }

    #[test]
    fn usage_has_no_update() {
        assert!(event_to_update(&AgentEvent::TurnStarted).is_none());
    }

    #[test]
    fn tool_started_maps_to_tool_call_with_kind() {
        use atomcode_kernel::tool::ToolCall;
        let call = ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        };
        let u = event_to_update(&AgentEvent::ToolStarted { call }).unwrap();
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
        let u = event_to_update(&AgentEvent::ToolResult { result }).unwrap();
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
        assert!(stop_reason(K::ProviderError).is_err());
        assert!(stop_reason(K::Timeout).is_err());
    }
}
