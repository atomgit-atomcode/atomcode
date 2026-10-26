//! Read-only display projection from one native event-log read. Never persisted.
use atomcode_capabilities::session::{SessionManager, SessionResult, TurnRecord};
use atomcode_kernel::session::{LoggedEvent, SessionEvent};
use serde::Serialize;
use std::collections::HashMap;

#[derive(Serialize)]
pub(super) struct TranscriptTurn {
    #[serde(flatten)]
    record: TurnRecord,
    #[serde(skip_serializing_if = "Option::is_none")]
    steps: Option<Vec<Step>>,
}
#[derive(Serialize)]
struct Step {
    text: String,
    reasoning: String,
    tools: Vec<Tool>,
}
#[derive(Serialize)]
struct Tool {
    id: String,
    name: String,
    args: String,
    result: Option<String>,
    is_error: Option<bool>,
}

pub(super) fn load(
    manager: &SessionManager,
    id: &str,
    selected: Option<u64>,
    detail: bool,
) -> SessionResult<Vec<TranscriptTurn>> {
    if !manager.is_event_session(id) {
        // Legacy records contain merged text only. Do not invent boundaries.
        return Ok(manager
            .load_transcript_records(id)?
            .into_iter()
            .filter(|r| selected.is_none_or(|turn| r.turn_id == turn))
            .map(|record| TranscriptTurn {
                record,
                steps: None,
            })
            .collect());
    }
    let events = manager.load_events(id)?;
    let records = atomcode_capabilities::session::events::turn_records(id, &events);
    let mut steps = if detail || selected.is_some() {
        project_steps(&events, selected)
    } else {
        HashMap::new()
    };
    Ok(records
        .into_iter()
        .filter(|r| selected.is_none_or(|turn| r.turn_id == turn))
        .map(|record| {
            let parts = if detail || selected.is_some() {
                Some(steps.remove(&record.turn_id).unwrap_or_default())
            } else {
                None
            };
            TranscriptTurn {
                record,
                steps: parts,
            }
        })
        .collect())
}

fn project_steps(events: &[LoggedEvent], selected: Option<u64>) -> HashMap<u64, Vec<Step>> {
    let mut results = HashMap::new();
    let mut started = HashMap::new();
    for logged in events
        .iter()
        .filter(|e| selected.is_none_or(|turn| e.event.turn() == turn))
    {
        match &logged.event {
            SessionEvent::ToolResultLogged {
                turn,
                round,
                call_id,
                content,
                is_error,
                ..
            } => {
                results.insert((*turn, *round, call_id.as_str()), (content, *is_error));
            }
            SessionEvent::ToolStarted { turn, round, call } => {
                started.insert((*turn, *round, call.id.as_str()), call);
            }
            _ => {}
        }
    }
    let mut steps: HashMap<u64, Vec<Step>> = HashMap::new();
    for logged in events
        .iter()
        .filter(|e| selected.is_none_or(|turn| e.event.turn() == turn))
    {
        match &logged.event {
            SessionEvent::AssistantMessage {
                turn,
                round,
                text,
                reasoning,
                tool_calls,
                ..
            } => {
                let tools = tool_calls
                    .iter()
                    .map(|asked| {
                        let key = (*turn, *round, asked.id.as_str());
                        let call = started.get(&key).copied().unwrap_or(asked);
                        let result = results.get(&key);
                        Tool {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            args: call.arguments.clone(),
                            result: result.map(|(text, _)| (*text).clone()),
                            is_error: result.map(|(_, failed)| *failed),
                        }
                    })
                    .collect::<Vec<_>>();
                if !text.is_empty() || !reasoning.is_empty() || !tools.is_empty() {
                    steps.entry(*turn).or_default().push(Step {
                        text: text.clone(),
                        reasoning: reasoning.clone(),
                        tools,
                    });
                }
            }
            SessionEvent::PartialReply {
                turn,
                text,
                reasoning,
                ..
            } => {
                if !text.is_empty() || !reasoning.is_empty() {
                    steps.entry(*turn).or_default().push(Step {
                        text: text.clone(),
                        reasoning: reasoning.clone(),
                        tools: vec![],
                    });
                }
            }
            _ => {}
        }
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::tool::ToolCall;
    fn log(event: SessionEvent) -> LoggedEvent {
        LoggedEvent {
            seq: 0,
            at: 0,
            event,
        }
    }
    fn call(args: &str) -> ToolCall {
        ToolCall {
            id: "same".into(),
            name: "Bash".into(),
            arguments: args.into(),
        }
    }
    fn assistant(turn: u64, round: u32, text: &str, calls: Vec<ToolCall>) -> SessionEvent {
        SessionEvent::AssistantMessage {
            turn,
            round,
            text: text.into(),
            reasoning: String::new(),
            tool_calls: calls,
            reasoning_blocks: vec![],
            meta: None,
        }
    }
    #[test]
    fn summary_wire_shape_keeps_existing_fields_without_loading_steps() {
        let record = TurnRecord {
            v: 1,
            started_at: None,
            ts: 0,
            iso: String::new(),
            session_id: "s".into(),
            turn_id: 1,
            undone: true,
            user: "prompt".into(),
            assistant: "merged reply".into(),
            reasoning: String::new(),
            tools: vec![],
            usage: Default::default(),
        };
        let wire = serde_json::to_value(TranscriptTurn {
            record,
            steps: None,
        })
        .unwrap();
        assert_eq!(wire["user"], "prompt");
        assert_eq!(wire["assistant"], "merged reply");
        assert_eq!(wire["undone"], true);
        assert!(wire.get("steps").is_none());
        assert!(wire.get("usage").is_some());
    }

    #[test]
    fn ordered_steps_keep_text_tool_pairing_and_actual_approved_arguments() {
        let events = vec![
            log(assistant(1, 1, "Inspect first", vec![call("requested")])),
            log(SessionEvent::ToolStarted {
                turn: 1,
                round: 1,
                call: call("approved"),
            }),
            log(SessionEvent::ToolResultLogged {
                turn: 1,
                round: 1,
                call_id: "same".into(),
                content: "first result".into(),
                is_error: false,
                images: vec![],
            }),
            log(assistant(1, 2, "Then verify", vec![call("second")])),
            log(SessionEvent::ToolResultLogged {
                turn: 1,
                round: 2,
                call_id: "same".into(),
                content: "second result".into(),
                is_error: true,
                images: vec![],
            }),
            log(SessionEvent::PartialReply {
                turn: 1,
                round: 3,
                text: "Partial tail".into(),
                reasoning: String::new(),
            }),
            log(assistant(2, 1, "Other turn", vec![call("third")])),
        ];
        let mut projected = project_steps(&events, Some(1));
        assert_eq!(projected.len(), 1);
        let steps = projected.remove(&1).unwrap();
        assert_eq!(
            steps.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
            ["Inspect first", "Then verify", "Partial tail"]
        );
        assert_eq!(steps[0].tools[0].args, "approved");
        assert_eq!(steps[0].tools[0].result.as_deref(), Some("first result"));
        assert_eq!(steps[1].tools[0].result.as_deref(), Some("second result"));
        assert_eq!(steps[1].tools[0].is_error, Some(true));
        let projected = project_steps(&events, None);
        assert!(
            projected[&2][0].tools[0].result.is_none(),
            "unfinished calls are not fabricated as complete"
        );
    }
}
