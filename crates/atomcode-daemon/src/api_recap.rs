use std::{path::PathBuf, time::Duration};

use atomcode_capabilities::session::{events::snapshot_of, StorageOwner};
use atomcode_coding::CommittedRecap;
use atomcode_kernel::{event::StopReason, session::SessionEvent};
use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};

use crate::{AppState, NativeSessionManager};

// Recap is optional. Bound its log projection independently of chat and history.
const RECAP_LOG_LIMIT: usize = 32 * 1024 * 1024;
static RECAP_LOG_IO: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

#[derive(Deserialize)]
pub(crate) struct RecapRequest {
    session_id: String,
    turn_id: u64,
}

#[derive(Serialize)]
pub(crate) struct RecapResponse {
    text: Option<String>,
}

pub(crate) async fn recap(
    State(state): State<AppState>,
    Json(request): Json<RecapRequest>,
) -> Json<RecapResponse> {
    // This auxiliary request never owns a turn or keeps a chat stream open.
    let text = tokio::time::timeout(Duration::from_secs(30), async {
        let permit = RECAP_LOG_IO.acquire().await.ok()?;
        let RecapTurn {
            input,
            model,
            working_dir,
        } = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            read_turn(request)
        })
        .await
        .ok()??;
        let provider = if input.needs_provider() {
            let (selection, model) = model?;
            let telemetry = state.telemetry.clone();
            tokio::task::spawn_blocking(move || {
                let config = crate::api_config::load_config().ok()?;
                // Use the selection that served this turn. A changed model or a
                // missing selection must not send the transcript to a fallback.
                if !config.selection_exists(&selection)
                    || config.provider_config_for_selection(&selection)?.model != model
                {
                    return None;
                }
                let runtime = crate::live_api::chat_runtime_config(
                    &config,
                    &selection,
                    &working_dir,
                    telemetry,
                );
                let config = crate::kernel_runtime::coding_config_from_runtime(&runtime);
                crate::coding_provider_factory().build(&config, None).ok()
            })
            .await
            .ok()?
        } else {
            None
        };
        input.generate(provider).await
    })
    .await
    .ok()
    .flatten();
    Json(RecapResponse { text })
}

struct RecapTurn {
    input: CommittedRecap,
    model: Option<(String, String)>,
    working_dir: PathBuf,
}

fn read_turn(request: RecapRequest) -> Option<RecapTurn> {
    if request.turn_id == 0 || request.session_id.is_empty() {
        return None;
    }
    let session = crate::resolve_session_by_id(&request.session_id).ok()??;
    if session.meta.id != request.session_id {
        return None;
    }
    let dirs = atomcode_coding::config::product_dirs_from_env();
    let manager = NativeSessionManager::with_root(
        NativeSessionManager::sessions_root(&dirs).join(session.project_hash),
    );
    read_committed_turn(&manager, &request.session_id, request.turn_id)
}

fn read_committed_turn(
    manager: &NativeSessionManager,
    session_id: &str,
    turn_id: u64,
) -> Option<RecapTurn> {
    let meta = manager.read_meta(session_id).ok()?;
    if meta.owner != StorageOwner::Native || meta.parent.is_some() {
        return None;
    }
    let mark = manager.events_mark(session_id).ok()?;
    let events = manager
        .load_events_bounded(session_id, RECAP_LOG_LIMIT)
        .ok()?;
    let boundary = events.iter().rposition(|item| {
        matches!(
            item.event,
            SessionEvent::TurnStart { .. }
                | SessionEvent::TurnEnd { .. }
                | SessionEvent::Rewound { .. }
        )
    })?;
    if !matches!(
        events[boundary].event,
        SessionEvent::TurnEnd { turn, stop: StopReason::Stopped, .. } if turn == turn_id
    ) {
        return None;
    }
    let model = meta
        .turn_stats
        .iter()
        .find(|stat| stat.turn_id == turn_id)
        .and_then(|stat| match stat.model_usage.as_slice() {
            [usage] => Some((usage.provider_id.clone(), usage.model_id.clone())),
            _ => None,
        });
    let input = CommittedRecap::new(
        &snapshot_of(&events[..=boundary]).messages,
        manager,
        session_id,
    );
    // The todo sidecar may be newer than the event read if another turn was
    // submitted during projection. Skip recovery against mixed revisions.
    if manager.events_mark(session_id).ok()? != mark {
        return None;
    }
    Some(RecapTurn {
        input,
        model,
        working_dir: meta.working_dir.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_capabilities::session::SessionMeta;
    use atomcode_kernel::session::{LoggedEvent, SessionHeader};

    #[test]
    fn only_the_latest_normally_finished_turn_is_available_for_recap() {
        let dir = tempfile::tempdir().unwrap();
        let manager = NativeSessionManager::with_root(dir.path().to_path_buf());
        let id = "recap-fixture";
        let lease = manager.acquire_lease(id).unwrap();
        let mut meta = SessionMeta::new(id, "/fixture", 1);
        meta.owner = StorageOwner::Native;
        manager
            .create_event_session(&lease, &SessionHeader::new(id), &meta)
            .unwrap();

        let append = |seq, event| {
            manager
                .append_events(
                    &lease,
                    &[LoggedEvent {
                        seq,
                        at: seq,
                        event,
                    }],
                )
                .unwrap();
        };
        append(1, SessionEvent::TurnStart { turn: 1 });
        append(
            2,
            SessionEvent::TurnEnd {
                turn: 1,
                stop: StopReason::Stopped,
                error: None,
            },
        );
        assert!(read_committed_turn(&manager, id, 1).is_some());
        assert!(read_committed_turn(&manager, id, 2).is_none());
        assert!(matches!(
            manager.load_events_bounded(id, 1),
            Err(atomcode_capabilities::session::SessionStoreError::TooLarge { .. })
        ));

        append(3, SessionEvent::TurnStart { turn: 2 });
        assert!(read_committed_turn(&manager, id, 1).is_none());
        append(
            4,
            SessionEvent::TurnEnd {
                turn: 2,
                stop: StopReason::Cancelled,
                error: None,
            },
        );
        assert!(read_committed_turn(&manager, id, 2).is_none());
    }
}
