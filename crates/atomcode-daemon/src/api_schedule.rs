//! Authenticated automation management. The browser never owns a scheduler.
use crate::{json_error, ProjectStateStore};
use atomcode_schedule::service::{Mutation, Service};
use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Extension, Json, Router,
};
use serde::Deserialize;

pub(crate) fn system_service() -> Service {
    Service::system(
        atomcode_coding::config::product_dirs_from_env()
            .user()
            .join("schedules"),
    )
}
pub(crate) fn routes<S: Clone + Send + Sync + 'static>(
    project: ProjectStateStore,
    service: Service,
) -> Router<S> {
    Router::new()
        .route("/schedules", get(catalog).post(change))
        .route("/schedules/events", get(events))
        .route("/schedules/:id", get(detail))
        .route("/schedules/:id/history", get(history))
        .route("/schedules/:id/reconcile", post(reconcile))
        .route("/schedules/:id/run", post(run))
        .layer(Extension(project))
        .layer(Extension(service))
}
async fn blocking<T: serde::Serialize + Send + 'static>(
    operation: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Response {
    match tokio::task::spawn_blocking(operation).await {
        Ok(Ok(result)) => Json(result).into_response(),
        Ok(Err(error)) => {
            let message = format!("{error:#}");
            let status = if message.contains("not found") {
                StatusCode::NOT_FOUND
            } else if message.contains("changed") || message.contains("already") {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            };
            json_error(status, message).into_response()
        }
        Err(_) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Automation service failed",
        )
        .into_response(),
    }
}
async fn catalog(Extension(service): Extension<Service>) -> Response {
    blocking(move || service.catalog()).await
}
async fn detail(Path(id): Path<String>, Extension(service): Extension<Service>) -> Response {
    blocking(move || service.inspect(&id)).await
}
#[derive(Deserialize)]
struct HistoryQuery {
    cursor: Option<usize>,
    limit: Option<usize>,
}
async fn history(
    Path(id): Path<String>,
    Query(query): Query<HistoryQuery>,
    Extension(service): Extension<Service>,
) -> Response {
    blocking(move || {
        service
            .store
            .history_page(&id, query.cursor, query.limit.unwrap_or(20))
    })
    .await
}
#[derive(Deserialize)]
pub struct Change {
    pub request_id: String,
    #[serde(flatten)]
    pub mutation: Mutation,
}
fn scoped(cwd: &str, project: &std::path::Path) -> anyhow::Result<()> {
    let target = std::fs::canonicalize(cwd)?;
    let project = std::fs::canonicalize(project)?;
    if target != project {
        anyhow::bail!("switch to the task's project before changing its automation");
    }
    if !target.is_dir() {
        anyhow::bail!("working directory is not a directory");
    }
    Ok(())
}
pub async fn change(
    Extension(project): Extension<ProjectStateStore>,
    Extension(service): Extension<Service>,
    Json(change): Json<Change>,
) -> Response {
    let project = project.read().await.working_dir.clone();
    blocking(move || {
        match &change.mutation {
            Mutation::Create { task } => scoped(&task.cwd, &project)?,
            Mutation::Update { task, .. } => {
                if let Ok(current) = service.store.get(&task.id) {
                    scoped(&current.task.cwd, &project)?;
                }
                scoped(&task.cwd, &project)?;
            }
            Mutation::Delete { id, .. } => {
                if let Ok(current) = service.store.get(id) {
                    scoped(&current.task.cwd, &project)?;
                }
            }
        }
        service.apply(&change.request_id, &change.mutation)
    })
    .await
}
pub async fn reconcile(
    Extension(project): Extension<ProjectStateStore>,
    Extension(service): Extension<Service>,
    Path(id): Path<String>,
) -> Response {
    let project = project.read().await.working_dir.clone();
    blocking(move || {
        scoped(&service.store.get(&id)?.task.cwd, &project)?;
        service.reconcile(&id)
    })
    .await
}

#[derive(Deserialize)]
pub struct RunRequest {
    pub request_id: String,
    pub expected: String,
}
pub async fn run(
    Extension(project): Extension<ProjectStateStore>,
    Extension(service): Extension<Service>,
    Path(id): Path<String>,
    Json(request): Json<RunRequest>,
) -> Response {
    let project = project.read().await.working_dir.clone();
    blocking(move || {
        scoped(&service.store.get(&id)?.task.cwd, &project)?;
        service.launch(&id, &request.request_id, &request.expected)
    })
    .await
}

async fn events(Extension(service): Extension<Service>) -> Response {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use futures::StreamExt;
    let (tx, rx) = tokio::sync::mpsc::channel::<String>(8);
    tokio::spawn(async move {
        let mut previous = String::new();
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3));
        loop {
            tokio::select! {
                _=tx.closed()=>break,
                _=interval.tick()=> {
                    let store=service.store.clone();
                    let stamp=tokio::task::spawn_blocking(move ||store.change_stamp()).await;
                    match stamp {
                        Ok(Ok(stamp)) if stamp!=previous=> {
                            previous=stamp.clone();
                            if tx.send(serde_json::json!({"revision":stamp}).to_string()).await.is_err(){break;}
                        }
                        Ok(Ok(_))=>{},
                        _=>{ if tx.send(serde_json::json!({"error":"Automation change detection failed"}).to_string()).await.is_err(){break;} }
                    }
                }
            }
        }
    });
    let stream = tokio_stream::wrappers::ReceiverStream::new(rx)
        .map(|data| Ok::<_, std::convert::Infallible>(Event::default().data(data)));
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(std::time::Duration::from_secs(15)))
        .into_response()
}
