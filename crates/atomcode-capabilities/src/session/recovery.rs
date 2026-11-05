//! Recovery is an explicit append under a newly acquired runtime lease, never
//! a side effect of reading history or rebuilding an already live runtime.
use std::collections::{BTreeMap, HashSet};

use atomcode_kernel::{
    event::StopReason,
    session::{InjectionOrigin, LoggedEvent, NoticeKind, SessionEvent},
};

use super::{manager::SessionResult, SessionLease, SessionManager};

const RECOVERED: &str = "上一轮未正常结束，已恢复已保存的输出和执行记录。未收到结果的工具状态未知；继续前请核对现场，避免重复执行。";

fn recovery_facts(events: &[LoggedEvent]) -> Vec<SessionEvent> {
    let Some(turn) = events.iter().rev().find_map(|logged| match logged.event {
        SessionEvent::TurnStart { turn } => Some(turn),
        _ => None,
    }) else {
        return vec![];
    };
    if events
        .iter()
        .any(|logged| matches!(logged.event, SessionEvent::TurnEnd { turn: t, .. } if t == turn))
        || !atomcode_kernel::session::visible_turns(events).contains(&turn)
    {
        return vec![];
    }
    let mut previews: BTreeMap<u32, (String, String)> = BTreeMap::new();
    let mut completed = HashSet::new();
    let mut answered = HashSet::new();
    let mut calls = Vec::new();
    for logged in atomcode_kernel::session::retained_events(events)
        .filter(|logged| logged.event.turn() == turn)
    {
        match &logged.event {
            SessionEvent::ReplyCheckpoint {
                round,
                text,
                reasoning,
                reset,
                ..
            } => {
                let preview = previews.entry(*round).or_default();
                if *reset {
                    *preview = Default::default();
                }
                preview.0.push_str(text);
                preview.1.push_str(reasoning);
            }
            SessionEvent::AssistantMessage {
                round, tool_calls, ..
            } => {
                completed.insert(*round);
                calls.extend(tool_calls.iter().map(|call| (*round, call.id.clone())));
            }
            SessionEvent::PartialReply { round, .. } => {
                completed.insert(*round);
            }
            SessionEvent::ToolResultLogged { call_id, .. } => {
                answered.insert(call_id.clone());
            }
            _ => {}
        }
    }
    let mut facts = Vec::new();
    for (round, call_id) in calls {
        if answered.insert(call_id.clone()) {
            facts.push(SessionEvent::ToolResultLogged {
                turn, round, call_id,
                content: "Runtime exited before a tool result was recorded. Execution outcome is unknown: it may have completed or changed external state. Verify the actual state before retrying; do not assume it failed or rerun automatically.".into(),
                is_error: true, images: vec![],
            });
        }
    }
    if let Some((round, (text, reasoning))) =
        previews
            .into_iter()
            .rev()
            .find(|(round, (text, reasoning))| {
                !completed.contains(round) && (!text.is_empty() || !reasoning.is_empty())
            })
    {
        facts.push(SessionEvent::PartialReply {
            turn,
            round,
            text,
            reasoning,
        });
    }
    facts.push(SessionEvent::Injected {
        turn,
        text: RECOVERED.into(),
        origin: InjectionOrigin::Reminder,
    });
    facts.push(SessionEvent::Notice {
        turn,
        notice: NoticeKind::SessionInterrupted,
        detail: RECOVERED.into(),
        retry: None,
    });
    facts.push(SessionEvent::TurnEnd {
        turn,
        stop: StopReason::InternalError,
        error: Some(
            "Runtime exited before this turn finished; execution was not automatically resumed."
                .into(),
        ),
    });
    facts
}

impl SessionManager {
    /// Call only after acquiring ownership from a previous runtime, before replay.
    /// A live runtime reusing its lease must not call this during a rebuild.
    pub fn recover_interrupted_turn(&self, lease: &SessionLease) -> SessionResult<bool> {
        let events = self.load_events(lease.id())?;
        let inherited = self.read_event_header(lease.id())?.inherited;
        // An unfinished turn inherited from a fork belongs to its parent, not
        // this agent. Only this session's own work can be recovered here.
        let facts = recovery_facts(&events[inherited.min(events.len())..]);
        if facts.is_empty() {
            return Ok(false);
        }
        let seq = events.last().map_or(1, |event| event.seq + 1);
        let at = chrono::Utc::now().timestamp_millis().max(0) as u64;
        let appended: Vec<_> = facts
            .into_iter()
            .enumerate()
            .map(|(offset, event)| LoggedEvent {
                seq: seq + offset as u64,
                at,
                event,
            })
            .collect();
        self.append_events(lease, &appended)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::manager::{SessionMeta, StorageOwner};
    use atomcode_kernel::{message::Role, session::SessionHeader, tool::ToolCall};

    fn log(events: Vec<SessionEvent>) -> Vec<LoggedEvent> {
        events
            .into_iter()
            .enumerate()
            .map(|(i, event)| LoggedEvent {
                seq: i as u64 + 1,
                at: 10,
                event,
            })
            .collect()
    }
    fn start() -> Vec<SessionEvent> {
        vec![
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::UserMessage {
                turn: 1,
                text: "fix the bug".into(),
                images: vec![],
            },
        ]
    }
    fn checkpoint(text: &str, reset: bool) -> SessionEvent {
        SessionEvent::ReplyCheckpoint {
            turn: 1,
            round: 1,
            text: text.into(),
            reasoning: String::new(),
            reset,
        }
    }

    #[test]
    fn a_cold_resume_keeps_progress_and_is_idempotent_without_replaying_tools() {
        const CHILD: &str = "ATOMCODE_RECOVERY_CRASH_FIXTURE";
        if let Some(root) = std::env::var_os(CHILD) {
            let manager = SessionManager::with_root(std::path::PathBuf::from(root));
            let lease = manager.acquire_lease("recovery").unwrap();
            let mut meta = SessionMeta::new("recovery", "/work", 10);
            meta.owner = StorageOwner::Native;
            manager
                .create_event_session(&lease, &SessionHeader::new("recovery"), &meta)
                .unwrap();
            let mut events = start();
            events.extend([checkpoint("已查到", true), checkpoint("根因", false)]);
            manager.append_events(&lease, &log(events)).unwrap();
            assert!(
                !manager
                    .project_snapshot("recovery")
                    .unwrap()
                    .messages
                    .iter()
                    .any(|m| m.text.contains("已查到")),
                "a checkpoint is not a live assistant message"
            );
            // Exit without dropping the lease or running any runtime shutdown,
            // cancellation, or turn-completion hook. The OS releases the lock.
            std::process::exit(0);
        }
        let root = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(root.path().join("0123456789abcdef"));
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "session::recovery::tests::a_cold_resume_keeps_progress_and_is_idempotent_without_replaying_tools", "--nocapture"])
            .env(CHILD, manager.root())
            .output().unwrap();
        assert!(
            child.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        let lease = manager.acquire_lease("recovery").unwrap();
        assert!(manager.recover_interrupted_turn(&lease).unwrap());
        let stored = manager.load_events("recovery").unwrap();
        assert!(stored.iter().any(
            |e| matches!(&e.event, SessionEvent::PartialReply { text, .. } if text == "已查到根因")
        ));
        let messages = manager.project_snapshot("recovery").unwrap().messages;
        assert_eq!(
            messages
                .iter()
                .filter(|m| m.role == Role::Assistant && m.text == "已查到根因")
                .count(),
            1
        );
        assert!(messages.iter().any(|m| m.text.contains("上一轮未正常结束")));
        assert_eq!(
            super::super::events::turn_records("recovery", &stored).len(),
            1
        );
        assert!(matches!(
            &stored.last().unwrap().event,
            SessionEvent::TurnEnd {
                stop: StopReason::InternalError,
                ..
            }
        ));
        assert!(!manager.recover_interrupted_turn(&lease).unwrap());
        assert_eq!(stored, manager.load_events("recovery").unwrap());
    }

    #[test]
    fn an_inherited_unfinished_turn_is_not_recovered_as_the_childs_work() {
        let root = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(root.path().join("0123456789abcdef"));
        let lease = manager.acquire_lease("fork").unwrap();
        let mut meta = SessionMeta::new("fork", "/work", 10);
        meta.owner = StorageOwner::Native;
        let mut header = SessionHeader::new("fork");
        header.inherited = 2;
        manager
            .create_event_session(&lease, &header, &meta)
            .unwrap();
        manager.append_events(&lease, &log(start())).unwrap();
        assert!(!manager.recover_interrupted_turn(&lease).unwrap());
    }

    #[test]
    fn resets_remove_discarded_attempts_and_completed_messages_supersede_checkpoints() {
        let mut events = start();
        events.extend([
            checkpoint("discard me", true),
            checkpoint("", true),
            checkpoint("keep me", false),
        ]);
        let facts = recovery_facts(&log(events.clone()));
        assert!(facts
            .iter()
            .any(|e| matches!(e, SessionEvent::PartialReply { text, .. } if text == "keep me")));
        assert!(!format!("{facts:?}").contains("discard me"));
        events.push(SessionEvent::AssistantMessage {
            turn: 1,
            round: 1,
            text: "final".into(),
            reasoning: String::new(),
            tool_calls: vec![],
            reasoning_blocks: vec![],
            meta: None,
        });
        assert!(!recovery_facts(&log(events.clone()))
            .iter()
            .any(|e| matches!(e, SessionEvent::PartialReply { .. })));
        events.push(SessionEvent::TurnEnd {
            turn: 1,
            stop: StopReason::Stopped,
            error: None,
        });
        assert!(recovery_facts(&log(events)).is_empty());
    }

    #[test]
    fn conversation_rewind_never_revives_a_discarded_checkpoint() {
        let mut events = start();
        events.push(checkpoint("withdrawn output", true));
        events.push(SessionEvent::Rewound {
            turn: 1,
            to: 3,
            scope: atomcode_kernel::session::RewindScope::Conversation,
        });
        assert!(!recovery_facts(&log(events))
            .iter()
            .any(|e| matches!(e, SessionEvent::PartialReply { .. })));
    }

    #[test]
    fn unresolved_tool_calls_are_unknown_not_successful_or_automatically_reissued() {
        let mut events = start();
        events.push(SessionEvent::AssistantMessage {
            turn: 1,
            round: 1,
            text: String::new(),
            reasoning: String::new(),
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            }],
            reasoning_blocks: vec![],
            meta: None,
        });
        let facts = recovery_facts(&log(events.clone()));
        assert!(facts.iter().any(|e| matches!(e, SessionEvent::ToolResultLogged { content, is_error: true, .. } if content.contains("outcome is unknown"))));
        events.extend(facts);
        let messages = atomcode_kernel::session::derive_messages(&log(events));
        let result = messages.iter().find(|m| m.role == Role::Tool).unwrap();
        assert!(result.text.contains("outcome is unknown"));
    }
}
