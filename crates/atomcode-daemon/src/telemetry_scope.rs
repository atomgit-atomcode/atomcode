use std::future::Future;

use atomcode_telemetry::{CurrentContext, SessionMode};

use crate::AppState;

/// Wraps an async closure in a telemetry `CurrentContext::scope` that sets
/// `mode` (from client header), `repo_origin` from `AppState`, and an optional
/// per-request `session_id`. Every handler that emits telemetry events should
/// call this at its entry point.
pub async fn daemon_scope<F, Fut, R>(
    state: &AppState,
    session_id: Option<uuid::Uuid>,
    mode: SessionMode,
    f: F,
) -> R
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = R>,
{
    let ctx = CurrentContext {
        mode: Some(mode),
        repo_origin: Some(state.repo_origin.clone()),
        session_id,
        ..CurrentContext::current()
    };
    CurrentContext::scope(ctx, f).await
}
