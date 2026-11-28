//! Read-only team views addressed through their lead's native session.
use atomcode_capabilities::session::{events, SessionActivity, SessionManager};
use atomcode_kernel::session::{
    retained_events, InjectionOrigin, LoggedEvent, MemberHeader, SessionEvent,
};
use axum::{extract::Path, http::StatusCode, response::IntoResponse, Json};
use serde::Serialize;
use std::collections::VecDeque;

#[derive(Serialize)]
struct Member {
    id: String,
    name: String,
    role: String,
    activity: Option<SessionActivity>,
}

#[derive(Serialize)]
struct Entry<'a> {
    seq: u64,
    kind: &'static str,
    text: &'a str,
}

#[derive(Serialize)]
struct Transcript<'a> {
    entries: VecDeque<Entry<'a>>,
    truncated: bool,
}

fn manager(hash: &str, lead: &str) -> Result<SessionManager, StatusCode> {
    if !super::valid_project_bucket(hash) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let dirs = atomcode_coding::config::product_dirs_from_env();
    let manager = SessionManager::with_root(SessionManager::sessions_root(&dirs).join(hash));
    let header = manager
        .read_event_header(lead)
        .map_err(|_| StatusCode::NOT_FOUND)?;
    if header.id != lead || header.parent.is_some() {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(manager)
}

fn member_header(manager: &SessionManager, lead: &str, id: &str) -> Option<MemberHeader> {
    let header = manager.read_event_header(id).ok()?;
    if header.parent.as_deref() != Some(lead) || events::storage_id(&header.id) != id {
        return None;
    }
    header.member
}

pub(super) async fn list(Path((hash, lead)): Path<(String, String)>) -> impl IntoResponse {
    match super::run_session_catalog_io(move || {
        let result = manager(&hash, &lead).map(|manager| {
            manager
                .children(&lead)
                .into_iter()
                .filter_map(|child| {
                    let header = member_header(&manager, &lead, &child.id)?;
                    let activity = manager.activity(&child.id).ok().flatten();
                    Some(Member {
                        id: child.id,
                        name: header.name,
                        role: header.role,
                        activity,
                    })
                })
                .collect::<Vec<_>>()
        });
        Ok(result)
    })
    .await
    {
        Ok(Ok(members)) => Json(members).into_response(),
        Ok(Err(status)) => status.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub(super) async fn detail(
    Path((hash, lead, id)): Path<(String, String, String)>,
) -> impl IntoResponse {
    match super::run_session_catalog_io(move || {
        let result = (|| {
            let manager = manager(&hash, &lead)?;
            member_header(&manager, &lead, &id).ok_or(StatusCode::NOT_FOUND)?;
            let events = manager
                .load_events(&id)
                .map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?;
            Ok::<_, StatusCode>(Json(transcript(&events)).into_response())
        })();
        Ok(result)
    })
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(status)) => status.into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

fn transcript(events: &[LoggedEvent]) -> Transcript<'_> {
    const MAX_ENTRIES: usize = 120;
    const MAX_ENTRY_BYTES: usize = 16 * 1024;
    let mut entries = VecDeque::new();
    let mut truncated = false;
    for item in retained_events(events) {
        let (kind, text) = match &item.event {
            SessionEvent::UserMessage { text, .. } => ("user", text.as_str()),
            SessionEvent::Injected {
                origin: InjectionOrigin::Peer { .. },
                text,
                ..
            } => ("peer", text.as_str()),
            SessionEvent::AssistantMessage { text, .. }
            | SessionEvent::PartialReply { text, .. } => ("assistant", text.as_str()),
            SessionEvent::ToolStarted { call, .. } => ("tool", call.name.as_str()),
            SessionEvent::ToolResultLogged {
                content, is_error, ..
            } => (
                if *is_error {
                    "tool_error"
                } else {
                    "tool_result"
                },
                content.as_str(),
            ),
            _ => continue,
        };
        if text.trim().is_empty() {
            continue;
        }
        if entries.len() == MAX_ENTRIES {
            entries.pop_front();
            truncated = true;
        }
        let end = text.floor_char_boundary(MAX_ENTRY_BYTES);
        truncated |= end != text.len();
        entries.push_back(Entry {
            seq: item.seq,
            kind,
            text: &text[..end],
        });
    }
    Transcript { entries, truncated }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_capabilities::session::{SessionMeta, StorageOwner};
    use atomcode_kernel::session::SessionHeader;

    fn create_child(manager: &SessionManager, parent: &str, name: &str, team: bool) -> String {
        let mut header = SessionHeader::new(format!("{parent}/{name}"));
        header.parent = Some(parent.to_owned());
        header.member = team.then(|| MemberHeader {
            name: name.to_owned(),
            ..Default::default()
        });
        let id = events::storage_id(&header.id);
        let mut meta = SessionMeta::new(&id, "/fixture", 0);
        meta.parent = header.parent.clone();
        meta.owner = StorageOwner::Native;
        let lease = manager.acquire_lease(&id).unwrap();
        manager
            .create_event_session(&lease, &header, &meta)
            .unwrap();
        id
    }

    #[test]
    fn a_member_is_readable_only_through_its_own_lead() {
        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(dir.path().to_path_buf());
        let member = create_child(&manager, "lead", "expert", true);
        let task = create_child(&manager, "lead", "task", false);
        assert_eq!(
            member_header(&manager, "lead", &member).unwrap().name,
            "expert"
        );
        assert!(member_header(&manager, "other-lead", &member).is_none());
        assert!(member_header(&manager, "lead", &task).is_none());
        assert!(member_header(&manager, "lead", "../expert").is_none());
        assert!(manager.list().is_empty());
    }

    #[test]
    fn the_transcript_shows_committed_member_text_without_internal_context() {
        let events = [
            SessionEvent::Injected {
                turn: 1,
                text: "private runtime context".into(),
                origin: InjectionOrigin::Memory,
            },
            SessionEvent::Injected {
                turn: 1,
                text: "Review the tests".into(),
                origin: InjectionOrigin::Peer {
                    from: "lead".into(),
                    source_turn: None,
                    outside: false,
                },
            },
            SessionEvent::ToolResultLogged {
                turn: 1,
                round: 0,
                call_id: "tool".into(),
                content: "输出".repeat(9000),
                is_error: true,
                images: vec![],
            },
        ]
        .into_iter()
        .enumerate()
        .map(|(index, event)| LoggedEvent {
            seq: index as u64 + 1,
            at: 0,
            event,
        })
        .collect::<Vec<_>>();
        let view = transcript(&events);
        assert!(view.truncated);
        assert_eq!(view.entries.len(), 2);
        assert_eq!(view.entries[0].text, "Review the tests");
        assert_eq!(view.entries[1].kind, "tool_error");
        assert!(view.entries[1].text.chars().all(|c| c == '输' || c == '出'));
    }
}
