//! Host-local display metadata; this endpoint never changes runtime state.
use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Default, Deserialize)]
pub(crate) struct BranchQuery {
    dir: Option<PathBuf>,
}
#[derive(Serialize)]
struct BranchResponse {
    working_dir: PathBuf,
    branch: Option<String>,
}

pub(crate) async fn get(
    State(state): State<crate::AppState>,
    Query(query): Query<BranchQuery>,
) -> Result<impl axum::response::IntoResponse, StatusCode> {
    let working_dir = match query.dir {
        Some(dir) if dir.is_absolute() => dir,
        Some(_) => return Err(StatusCode::BAD_REQUEST),
        None => state.project.read().await.working_dir.clone(),
    };
    let dir = working_dir.clone();
    let branch = tokio::task::spawn_blocking(move || atomcode_capabilities::git_branch::read(&dir))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(BranchResponse {
        working_dir,
        branch,
    }))
}
