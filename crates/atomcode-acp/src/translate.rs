use agent_client_protocol::schema::v1::{ContentBlock, ContentChunk, SessionUpdate, TextContent};
use atomcode_kernel::event::AgentEvent;

pub fn event_to_update(ev: &AgentEvent) -> Option<SessionUpdate> {
    match ev {
        AgentEvent::TextDelta(s) => Some(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new(s.clone()))),
        )),
        AgentEvent::Reasoning(s) => Some(SessionUpdate::AgentThoughtChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new(s.clone()))),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::event::AgentEvent;

    fn tag(u: &agent_client_protocol::schema::v1::SessionUpdate) -> String {
        serde_json::to_value(u).unwrap()["sessionUpdate"].as_str().unwrap().to_string()
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
}
