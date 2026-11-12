//! Disposable paging over the current display projection. No runtime or storage
//! mutation. Cursors validate the omitted prefix, so appends are safe while
//! compaction/rewind cannot silently splice two different histories together.
use crate::MessageInfo;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const TURNS: usize = 8;
const BYTES: usize = 2 * 1024 * 1024;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Cursor {
    pub session: String,
    pub before: usize,
    pub digest: String,
}

fn digest(messages: &[MessageInfo]) -> String {
    let mut hash = Sha256::new();
    for message in messages {
        // Serialize one message at a time rather than allocating the full log.
        hash.update(serde_json::to_vec(message).expect("display message serializes"));
        hash.update([0]);
    }
    format!("{:x}", hash.finalize())
}

/// Runtime-only system context may precede the persisted conversation. Nothing
/// else may differ: an in-flight tail must remain in the live snapshot.
pub(crate) fn matches_snapshot(
    live: &atomcode_kernel::message::SessionSnapshot,
    stored: &atomcode_kernel::message::SessionSnapshot,
) -> bool {
    let Some(head) = live.messages.len().checked_sub(stored.messages.len()) else {
        return false;
    };
    live.messages[..head]
        .iter()
        .all(|m| m.role == atomcode_kernel::message::Role::System)
        && live.messages[head..]
            .iter()
            .zip(&stored.messages)
            .all(|(a, b)| {
                a.role == b.role
                    && a.display_text() == b.display_text()
                    && a.tool_calls == b.tool_calls
                    && a.tool_call_id == b.tool_call_id
                    && a.synthetic == b.synthetic
                    && a.internal_origin == b.internal_origin
            })
}

pub(crate) fn window(
    session: &str,
    messages: &mut Vec<MessageInfo>,
    cursor: Option<&Cursor>,
) -> Result<Option<Cursor>, &'static str> {
    let end = if let Some(c) = cursor {
        if c.session != session
            || c.before == 0
            || c.before >= messages.len()
            || messages[c.before].role != "user"
            || messages[c.before].synthetic
            || digest(&messages[..c.before]) != c.digest
        {
            return Err("History changed; reopen the session to refresh its history");
        }
        c.before
    } else {
        messages.len()
    };
    let mut start = 0;
    let mut count = 0;
    let mut bytes = 0usize;
    // Human user boundaries keep an assistant's tool calls/results together.
    for i in (0..end).rev() {
        bytes = bytes.saturating_add(serde_json::to_vec(&messages[i]).map_or(BYTES, |v| v.len()));
        if messages[i].role == "user" && !messages[i].synthetic {
            count += 1;
            start = i;
            if count >= TURNS || bytes >= BYTES {
                break;
            }
        }
    }
    // An internal preamble is not an older conversation page.
    let older = messages[..start]
        .iter()
        .any(|m| m.role == "user" && !m.synthetic)
        .then(|| Cursor {
            session: session.into(),
            before: start,
            digest: digest(&messages[..start]),
        });
    messages.truncate(end);
    messages.drain(..start);
    Ok(older)
}

/// Render-free prefix for the existing todo reducer. Human prompts, prose,
/// images and normal tool outputs are never sent just to restore task state.
pub(crate) fn state_seed(messages: &[MessageInfo]) -> Vec<serde_json::Value> {
    let mut ids = std::collections::HashSet::new();
    let mut seed = Vec::new();
    let mut human_boundary = false;
    for m in messages {
        if m.role == "user" {
            if !m.synthetic {
                human_boundary = true;
            }
        } else if m.role == "assistant" {
            let calls: Vec<_> = m
                .tool_calls
                .iter()
                .flatten()
                .filter(|c| atomcode_capabilities::tools::todo::is_todo_call(&c.name))
                .collect();
            if !calls.is_empty() {
                if human_boundary {
                    seed.push(serde_json::json!({"role":"user", "content":""}));
                    human_boundary = false;
                }
                ids.extend(calls.iter().map(|c| c.id.clone()));
                seed.push(
                    serde_json::json!({"role":"assistant", "content":"", "tool_calls":calls}),
                );
            }
        } else if let Some(result) = &m.tool_result {
            if ids.contains(&result.call_id) {
                seed.push(serde_json::json!({"role":"tool", "content":"", "tool_result":{"call_id":result.call_id, "summary":"", "success":result.success}}));
            }
        }
    }
    if human_boundary {
        seed.push(serde_json::json!({"role":"user", "content":""}));
    }
    seed
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::message::Message;
    #[test]
    fn state_seed_keeps_todos_across_interruption() {
        let messages = [
            Message::assistant(
                "",
                vec![atomcode_kernel::tool::ToolCall {
                    id: "plan".into(),
                    name: "todowrite".into(),
                    arguments: r#"{"todos":[{"content":"keep","status":"in_progress"}]}"#.into(),
                }],
            ),
            Message::user_interruption(),
            Message::user("continue"),
        ]
        .iter()
        .map(MessageInfo::from_kernel)
        .collect::<Vec<_>>();
        let seed = state_seed(&messages);
        assert_eq!(seed[0]["tool_calls"][0]["id"], "plan");
        assert_eq!(seed.len(), 2);
    }

    #[test]
    fn runtime_context_matches_but_live_changes_never_get_replaced() {
        use atomcode_kernel::message::SessionSnapshot;
        let stored = SessionSnapshot::new(vec![
            Message::user("question"),
            Message::assistant("answer", vec![]),
        ]);
        let mut live = stored.clone();
        live.messages.insert(0, Message::system("runtime context"));
        assert!(matches_snapshot(&live, &stored));
        live.messages.push(Message::user("in flight"));
        assert!(!matches_snapshot(&live, &stored));
        live.messages.pop();
        live.messages[2].text = "changed answer".into();
        assert!(!matches_snapshot(&live, &stored));
        live.messages[0] = Message::user("different conversation");
        assert!(!matches_snapshot(&live, &stored));
    }

    #[test]
    fn internal_preamble_does_not_create_an_older_page() {
        let mut reminder = Message::user("internal reminder");
        reminder.synthetic = true;
        let mut messages = [
            Message::system("context"),
            reminder,
            Message::user("question"),
            Message::assistant("answer", vec![]),
        ]
        .iter()
        .map(MessageInfo::from_kernel)
        .collect();
        assert!(window("one", &mut messages, None).unwrap().is_none());
        assert_eq!(messages[0].content, "question");
    }

    #[test]
    fn paging_preserves_turns_and_rejects_changed_prefixes() {
        let make = || {
            (0..25)
                .flat_map(|i| {
                    [
                        Message::user(format!("u{i}")),
                        Message::assistant(format!("a{i}"), vec![]),
                    ]
                })
                .map(|m| MessageInfo::from_kernel(&m))
                .collect::<Vec<_>>()
        };
        let mut messages = make();
        let cursor = window("one", &mut messages, None).unwrap().unwrap();
        assert_eq!(messages.len(), 16);
        assert_eq!(messages[0].content, "u17");
        let mut all = make();
        all.push(MessageInfo::from_kernel(&Message::user("new")));
        let next = window("one", &mut all, Some(&cursor)).unwrap().unwrap();
        assert_eq!(all[0].content, "u9");
        assert_eq!(all.last().unwrap().content, "a16");
        let mut changed = make();
        changed[0].content = "changed".into();
        assert!(window("one", &mut changed, Some(&next)).is_err());
        assert!(window("other", &mut make(), Some(&cursor)).is_err());
    }
}
