//! Disposable state bootstrap for a bounded transcript. Never persisted or sent
//! to a model. Old transcript bodies are not transported just to seed sidebars.
use atomcode_kernel::session::{Committed, InjectionOrigin, LoggedEvent, SessionEvent};
use std::collections::HashSet;

pub fn state_seed(session: &str, events: &[LoggedEvent]) -> Vec<Committed> {
    let mut todo_calls = HashSet::new();
    events.iter().filter_map(|entry| {
        let event = match &entry.event {
            SessionEvent::TurnStart { .. } | SessionEvent::TurnEnd { .. }
            | SessionEvent::Usage { .. } | SessionEvent::StepEnd { .. }
            | SessionEvent::RequestHeader { .. } | SessionEvent::Rewound { .. }
            | SessionEvent::Interrupted { .. } | SessionEvent::Titled { .. } => entry.event.clone(),
            SessionEvent::AssistantMessage { turn, round, tool_calls, .. } => {
                let calls: Vec<_> = tool_calls.iter().filter(|c|
                    atomcode_capabilities::tools::todo::is_todo_call(&c.name)).cloned().collect();
                if calls.is_empty() { return None; }
                todo_calls.extend(calls.iter().map(|c| c.id.clone()));
                SessionEvent::AssistantMessage { turn:*turn, round:*round, text:String::new(), reasoning:String::new(),
                    tool_calls:calls, reasoning_blocks:vec![], meta:None }
            }
            SessionEvent::ToolResultLogged { turn, round, call_id, is_error:true, .. } if todo_calls.contains(call_id) =>
                SessionEvent::ToolResultLogged { turn:*turn, round:*round, call_id:call_id.clone(), content:String::new(), is_error:true, images:vec![] },
            SessionEvent::Injected { turn, origin: origin @ (InjectionOrigin::Skill { .. } | InjectionOrigin::CompactionSummary), .. } =>
                SessionEvent::Injected { turn:*turn, origin:origin.clone(), text:String::new() },
            SessionEvent::Compacted { turn, through, from, .. } =>
                SessionEvent::Compacted { turn:*turn, through:*through, from:*from,
                    // Preserve the summary/no-op distinction used by status
                    // counters without transporting the actual summary body.
                    summary:if atomcode_kernel::session::is_context_summary(&entry.event) { "[display seed]".into() } else { String::new() } },
            _ => return None,
        };
        Some(Committed { session:session.into(), seq:entry.seq, at:entry.at, event })
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn state_seed_keeps_todo_and_skill_indexes_without_old_bodies() {
        let facts = vec![
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::UserMessage {
                turn: 1,
                text: "private old prompt".into(),
                images: vec![],
            },
            SessionEvent::Injected {
                turn: 1,
                text: "large skill body".into(),
                origin: InjectionOrigin::Skill {
                    name: "review".into(),
                },
            },
            SessionEvent::AssistantMessage {
                turn: 1,
                round: 1,
                text: "large old answer".into(),
                reasoning: "large thinking".into(),
                tool_calls: vec![atomcode_kernel::tool::ToolCall {
                    id: "todo".into(),
                    name: "todowrite".into(),
                    arguments: r#"{"todos":[{"content":"keep this plan","status":"pending"}]}"#
                        .into(),
                }],
                reasoning_blocks: vec![],
                meta: None,
            },
            SessionEvent::TurnEnd {
                turn: 1,
                stop: atomcode_kernel::event::StopReason::Stopped,
                error: None,
            },
        ];
        let events: Vec<_> = facts
            .into_iter()
            .enumerate()
            .map(|(i, event)| LoggedEvent {
                seq: i as u64 + 1,
                at: 0,
                event,
            })
            .collect();
        let seed = state_seed("lead", &events);
        let text = serde_json::to_string(&seed).unwrap();
        assert!(!text.contains("private old prompt"));
        assert!(!text.contains("large"));
        assert!(text.contains("review"));
        let mut full_plan = crate::modules::todo::Plan::default();
        for e in &events {
            full_plan.absorb(&e.event);
        }
        let mut seeded = crate::modules::todo::Plan::default();
        for e in &seed {
            seeded.absorb(&e.event);
        }
        let rows = |plan: &crate::modules::todo::Plan| {
            plan.fold(&Default::default())
                .into_iter()
                .map(|i| (i.content, format!("{:?}", i.status)))
                .collect::<Vec<_>>()
        };
        assert_eq!(rows(&seeded), rows(&full_plan));
    }
    #[test]
    fn state_seed_preserves_real_summary_counts_without_summary_bodies() {
        let events = [
            SessionEvent::Compacted {
                turn: 1,
                through: 5,
                from: 0,
                summary: "old private summary".into(),
            },
            SessionEvent::Compacted {
                turn: 1,
                through: 5,
                from: 0,
                summary: String::new(),
            },
            SessionEvent::Compacted {
                turn: 1,
                through: 5,
                from: 5,
                summary: "tool cleanup".into(),
            },
        ]
        .into_iter()
        .enumerate()
        .map(|(i, event)| LoggedEvent {
            seq: i as u64 + 1,
            at: 0,
            event,
        })
        .collect::<Vec<_>>();
        let seed = state_seed("lead", &events);
        let logged = seed.iter().map(Committed::logged).collect::<Vec<_>>();
        assert_eq!(atomcode_kernel::session::context_summary_count(&events), 1);
        assert_eq!(atomcode_kernel::session::context_summary_count(&logged), 1);
        assert!(!serde_json::to_string(&seed)
            .unwrap()
            .contains("old private summary"));
    }
    #[test]
    fn a_thousand_long_turns_transport_only_the_recent_bodies() {
        let body = "x".repeat(16 * 1024);
        let mut events = Vec::new();
        for turn in 1..=1000 {
            for event in [
                SessionEvent::TurnStart { turn },
                SessionEvent::UserMessage {
                    turn,
                    text: body.clone(),
                    images: vec![],
                },
                SessionEvent::TurnEnd {
                    turn,
                    stop: atomcode_kernel::event::StopReason::Stopped,
                    error: None,
                },
            ] {
                events.push(LoggedEvent {
                    seq: events.len() as u64 + 1,
                    at: 0,
                    event,
                });
            }
        }
        let (range, page) =
            atomcode_kernel::session::history::range("lead", &events, None, 8).unwrap();
        let seed = state_seed("lead", &events[..range.start]);
        let bytes =
            serde_json::to_vec(&page).unwrap().len() + serde_json::to_vec(&seed).unwrap().len();
        assert_eq!(page.events.len(), 24);
        assert!(
            bytes < 1000 * body.len() / 20,
            "display bootstrap was {bytes} bytes"
        );
        eprintln!("1000 x 16KiB prompts: display bootstrap {bytes} bytes (original prompt bodies {} bytes)",1000*body.len());
    }
}
