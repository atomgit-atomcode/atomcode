use crate::tool::ToolCall;

/// Minimal provider stream surface. A1 carries the production StreamEvent (with
/// reasoning/thinking/usage/runaway) into this slot; the loop only depends on
/// this shape.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    TextDelta(String),
    ToolCall(ToolCall),
    Done,
}
