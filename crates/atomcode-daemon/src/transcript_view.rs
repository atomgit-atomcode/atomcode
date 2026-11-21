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
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    user_truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_images: Option<Vec<crate::ImageData>>,
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
    selected: &[u64],
    detail: bool,
    include_images: bool,
) -> SessionResult<Vec<TranscriptTurn>> {
    if !manager.is_event_session(id) {
        // Legacy records contain merged text only. Do not invent boundaries.
        return Ok(manager
            .load_transcript_records(id)?
            .into_iter()
            .filter(|r| selected.is_empty() || selected.contains(&r.turn_id))
            .map(|mut record| {
                let shown = crate::MessageInfo::from_kernel(
                    &atomcode_kernel::message::Message::user(&record.user),
                );
                record.user = shown.content;
                TranscriptTurn {
                    record,
                    steps: None,
                    user_truncated: false,
                    user_images: shown.images,
                }
            })
            .collect());
    }
    let events = manager.load_events(id)?;
    let records = atomcode_capabilities::session::events::turn_records(id, &events);
    let (identities, mut shown): (Vec<_>, Vec<_>) =
        project_user_display(&events, selected, include_images)
            .into_iter()
            .map(|(turn, raw, info)| ((turn, raw), info))
            .unzip();
    if include_images {
        let sidecar = std::fs::read(manager.root().join(format!("{id}.images.json")))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<crate::ImageSidecar>(&bytes).ok())
            .unwrap_or_default();
        for ((turn, raw), info) in identities.iter().zip(shown.iter_mut()) {
            if selected.is_empty() || selected.contains(turn) {
                crate::attach_native_display_images(info, manager, id, raw);
            }
        }
        crate::attach_image_sets(&mut shown, sidecar.sets);
    }
    let mut user_display = HashMap::new();
    for (identity, info) in identities.into_iter().zip(shown) {
        user_display.entry(identity).or_insert(info);
    }
    let mut steps = if detail || !selected.is_empty() {
        project_selected_steps(&events, selected)
    } else {
        HashMap::new()
    };
    Ok(records
        .into_iter()
        .filter(|r| selected.is_empty() || selected.contains(&r.turn_id))
        .map(|mut record| {
            let shown = user_display
                .remove(&(record.turn_id, record.user.clone()))
                .unwrap_or_else(|| {
                    crate::MessageInfo::from_kernel(&atomcode_kernel::message::Message::user(
                        &record.user,
                    ))
                });
            record.user = shown.content;
            let parts = if detail || !selected.is_empty() {
                Some(steps.remove(&record.turn_id).unwrap_or_default())
            } else {
                None
            };
            TranscriptTurn {
                user_images: if include_images { shown.images } else { None },
                record,
                steps: parts,
                user_truncated: false,
            }
        })
        .collect())
}

/// Replay original user facts, including turns omitted by compaction. Images are
/// display-only and never copied into the model's reconstructed conversation.
fn project_user_display(
    events: &[LoggedEvent],
    selected: &[u64],
    include_images: bool,
) -> Vec<(u64, String, crate::MessageInfo)> {
    events
        .iter()
        .filter_map(|logged| match &logged.event {
            SessionEvent::UserMessage { turn, text, images } => {
                let mut info = crate::MessageInfo::from_kernel(
                    &atomcode_kernel::message::Message::user_with_images(
                        text,
                        if include_images && (selected.is_empty() || selected.contains(turn)) {
                            images.clone()
                        } else {
                            vec![]
                        },
                    ),
                );
                info.source_seq = Some(logged.seq);
                Some((*turn, text.clone(), info))
            }
            _ => None,
        })
        .collect()
}

/// Index-only transport projection: do not send merged tool output or reasoning
/// just to populate the navigation rail. The event log remains authoritative.
pub(super) fn summarize(turns: &mut [TranscriptTurn]) {
    for turn in turns {
        turn.user_images = None;
        turn.user_truncated = truncate_preview(&mut turn.record.user);
        truncate_preview(&mut turn.record.assistant);
        turn.record.reasoning.clear();
        turn.record.tools.clear();
        turn.steps = None;
    }
}

fn truncate_preview(text: &mut String) -> bool {
    if let Some((offset, _)) = text.char_indices().nth(160) {
        text.truncate(offset);
        text.push('…');
        true
    } else {
        false
    }
}

#[cfg(test)]
fn project_steps(events: &[LoggedEvent], selected: Option<u64>) -> HashMap<u64, Vec<Step>> {
    project_selected_steps(events, selected.as_slice())
}

fn project_selected_steps(events: &[LoggedEvent], selected: &[u64]) -> HashMap<u64, Vec<Step>> {
    let mut results = HashMap::new();
    let mut started = HashMap::new();
    for logged in events
        .iter()
        .filter(|e| selected.is_empty() || selected.contains(&e.event.turn()))
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
        .filter(|e| selected.is_empty() || selected.contains(&e.event.turn()))
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
    fn original_images_match_native_source_identity_and_text_not_prompt_order() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(dir.path());
        let lease = manager.acquire_lease("pictures").unwrap();
        let text = "look [Image #1]\n\n[图片内容（由 vl 识别）]\na picture";
        manager
            .write_display_images(
                &lease,
                7,
                text,
                vec![atomcode_kernel::message::ImageContent {
                    media_type: "image/png".into(),
                    data: "eA==".into(),
                }],
            )
            .unwrap();
        let events = vec![LoggedEvent {
            seq: 7,
            at: 1,
            event: SessionEvent::UserMessage {
                turn: 2,
                text: text.into(),
                images: vec![],
            },
        }];
        let (_, _, mut info) = project_user_display(&events, &[2], true).pop().unwrap();
        assert_eq!(info.content, "look [Image #1]");
        crate::attach_native_display_images(&mut info, &manager, "pictures", text);
        assert_eq!(info.images.as_ref().unwrap()[0].data, "eA==");
        let mut missing =
            crate::MessageInfo::from_kernel(&atomcode_kernel::message::Message::user(text));
        missing.source_seq = Some(8);
        crate::attach_native_display_images(&mut missing, &manager, "pictures", text);
        assert!(missing.images.as_ref().unwrap()[0].missing);
        missing.source_seq = Some(7);
        crate::attach_native_display_images(
            &mut missing,
            &manager,
            "pictures",
            "different message at reused sequence",
        );
        assert!(missing.images.as_ref().unwrap()[0].missing);
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
            user_truncated: false,
            user_images: None,
        })
        .unwrap();
        assert_eq!(wire["user"], "prompt");
        assert_eq!(wire["assistant"], "merged reply");
        assert_eq!(wire["undone"], true);
        assert!(wire.get("steps").is_none());
        assert!(wire.get("usage").is_some());
    }

    #[test]
    fn navigation_summary_is_bounded_unicode_safe_and_excludes_tool_payloads() {
        let record = TurnRecord {
            v: 1,
            started_at: Some(1),
            ts: 2,
            iso: String::new(),
            session_id: "s".into(),
            turn_id: 3,
            undone: false,
            user: "测".repeat(1000),
            assistant: "答".repeat(1000),
            reasoning: "private reasoning".into(),
            tools: vec![atomcode_capabilities::session::transcript::ToolRecord {
                name: "Bash".into(),
                args: "large arguments".into(),
                result: "large output".into(),
                is_error: false,
            }],
            usage: Default::default(),
        };
        let mut turns = vec![TranscriptTurn {
            record,
            steps: Some(vec![]),
            user_truncated: false,
            user_images: Some(vec![crate::ImageData {
                media_type: "image/png".into(),
                data: "eA==".into(),
                missing: false,
            }]),
        }];
        summarize(&mut turns);
        let wire = serde_json::to_value(&turns[0]).unwrap();
        assert_eq!(wire["turn_id"], 3);
        assert_eq!(wire["started_at"], 1);
        assert_eq!(wire["user_truncated"], true);
        assert_eq!(wire["user"].as_str().unwrap().chars().count(), 161);
        assert_eq!(wire["assistant"].as_str().unwrap().chars().count(), 161);
        assert!(wire["tools"].as_array().unwrap().is_empty());
        assert!(wire.get("reasoning").is_none());
        assert!(wire.get("steps").is_none());
        assert!(
            wire.get("user_images").is_none(),
            "navigation must not include image payloads"
        );
    }

    #[test]
    fn a_detail_batch_projects_only_selected_turns_in_original_order() {
        let events = vec![
            log(assistant(1, 1, "first", vec![])),
            log(assistant(2, 1, "not requested", vec![])),
            log(assistant(3, 1, "third", vec![])),
            log(assistant(1, 2, "first continued", vec![])),
        ];
        let steps = project_selected_steps(&events, &[1, 3]);
        assert_eq!(steps.len(), 2);
        assert_eq!(
            steps[&1]
                .iter()
                .map(|step| step.text.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "first continued"]
        );
        assert_eq!(steps[&3][0].text, "third");
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
