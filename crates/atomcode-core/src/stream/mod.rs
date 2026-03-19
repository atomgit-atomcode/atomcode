use crate::tool::ToolCall;

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Delta(String),
    ToolCallStart { id: String, name: String },
    ToolCallDelta(String),
    ToolCallDone(ToolCall),
    Done,
    Error(String),
}
