//! AtomCode API Service
//!
//! Provides HTTP API for querying conversation history.

use axum::{
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tower_http::cors::{Any, CorsLayer};

use atomcode_core::session::{Session, SessionManager, SessionMeta};

/// Project metadata with working directory resolved
#[derive(Debug, Serialize, Deserialize)]
pub struct ProjectInfo {
    /// Project hash (directory name in sessions/)
    pub hash: String,
    /// Working directory path (from session files)
    pub working_dir: PathBuf,
    /// Number of sessions
    pub session_count: usize,
    /// Last update timestamp
    pub last_updated: u64,
}

/// Search query parameters
#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    /// Search keyword for session name
    pub q: String,
}
/// Session detail response
#[derive(Debug, Serialize)]
pub struct SessionDetail {
    pub id: String,
    pub name: String,
    pub working_dir: PathBuf,
    pub created_at: u64,
    pub updated_at: u64,
    pub message_count: usize,
    pub messages: Vec<MessageInfo>,
}
/// Tool call info for API response
#[derive(Debug, Serialize)]
pub struct ToolCallInfo {
    pub id: String,
    pub name: String,
    pub arguments: String,
    /// Formatted display string (CLI style)
    pub display: String,
}

/// Message info for API response
#[derive(Debug, Serialize)]
pub struct MessageInfo {
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallInfo>>,
    /// Tool result summary (for tool role messages)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_result: Option<ToolResultInfo>,
}

/// Tool result info for API response
#[derive(Debug, Serialize)]
pub struct ToolResultInfo {
    pub success: bool,
    pub summary: String,
    pub line_count: usize,
}

impl From<&atomcode_core::conversation::message::Message> for MessageInfo {
    fn from(msg: &atomcode_core::conversation::message::Message) -> Self {
        let role = match msg.role {
            atomcode_core::conversation::message::Role::System => "system",
            atomcode_core::conversation::message::Role::User => "user",
            atomcode_core::conversation::message::Role::Assistant => "assistant",
            atomcode_core::conversation::message::Role::Tool => "tool",
        };
        
        let (content, tool_calls, tool_result) = match &msg.content {
            atomcode_core::conversation::message::MessageContent::Text(s) => {
                (s.clone(), None, None)
            }
            atomcode_core::conversation::message::MessageContent::AssistantWithToolCalls { text, tool_calls } => {
                let calls: Vec<ToolCallInfo> = tool_calls.iter()
                    .map(|tc| ToolCallInfo {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        arguments: tc.arguments.clone(),
                        display: format_tool_args(&tc.name, &tc.arguments),
                    })
                    .collect();
                (text.clone().unwrap_or_default(), Some(calls), None)
            }
            atomcode_core::conversation::message::MessageContent::ToolResult(r) => {
                let lines = r.output.lines().count();
                let first_line = r.output.lines().next().unwrap_or("");
                let summary = if first_line.len() > 100 {
                    format!("{}...", first_line.chars().take(97).collect::<String>())
                } else {
                    first_line.to_string()
                };
                (r.output.clone(), None, Some(ToolResultInfo {
                    success: r.success,
                    summary,
                    line_count: lines,
                }))
            }
            atomcode_core::conversation::message::MessageContent::ToolResultRef(r) => {
                (r.summary.clone(), None, None)
            }
        };
        
        Self { role: role.to_string(), content, tool_calls, tool_result }
    }
}

/// Format tool arguments for display (CLI style)
fn format_tool_args(tool_name: &str, args_json: &str) -> String {
    let args: serde_json::Value = match serde_json::from_str(args_json) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };

    match tool_name {
        "read_file" => {
            let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            let short = short_path(path);
            let mut s = short;
            if let Some(offset) = args.get("offset").and_then(|v| v.as_u64()) {
                if let Some(limit) = args.get("limit").and_then(|v| v.as_u64()) {
                    s.push_str(&format!(" L{}-{}", offset, offset + limit));
                }
            }
            s
        }
        "write_file" => {
            let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            let size = args.get("content").and_then(|v| v.as_str()).map(|s| s.len()).unwrap_or(0);
            format!("{} ({} bytes)", short_path(path), size)
        }
        "edit_file" => {
            let path = args.get("file_path").and_then(|v| v.as_str()).unwrap_or("");
            short_path(path)
        }
        "bash" => {
            let cmd = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if cmd.chars().count() > 80 { 
                format!("`{}...`", cmd.chars().take(77).collect::<String>()) 
            } else { 
                format!("`{}`", cmd) 
            }
        }
        "list_directory" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            short_path(path)
        }
        "grep" => {
            let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            format!("\"{}\" in {}", pattern, short_path(path))
        }
        "glob" => {
            let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            format!("\"{}\"", pattern)
        }
        "web_search" => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            format!("\"{}\"", query)
        }
        "web_fetch" => {
            let url = args.get("url").and_then(|v| v.as_str()).unwrap_or("");
            url.to_string()
        }
        _ => {
            if let Some(obj) = args.as_object() {
                obj.iter()
                    .map(|(k, v)| {
                        let val = match v {
                            serde_json::Value::String(s) if s.chars().count() > 30 => {
                                format!("{}...", s.chars().take(27).collect::<String>())
                            }
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        format!("{}={}", k, val)
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                String::new()
            }
        }
    }
}

fn short_path(path: &str) -> String {
    let parts: Vec<&str> = path.rsplitn(3, '/').collect();
    match parts.len() {
        0 | 1 => path.to_string(),
        2 => format!("{}/{}", parts[1], parts[0]),
        _ => format!(".../{}/{}", parts[1], parts[0]),
    }
}

fn sessions_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("atomcode")
        .join("sessions")
}

/// List all projects (scans sessions directory)
fn list_projects() -> std::io::Result<Vec<ProjectInfo>> {
    let sessions_root = sessions_dir();
    if !sessions_root.exists() {
        return Ok(Vec::new());
    }

    let mut projects = Vec::new();
    
    for entry in std::fs::read_dir(sessions_root)? {
        let entry = entry?;
        let path = entry.path();
        
        if path.is_dir() {
            let hash = path.file_name().unwrap().to_string_lossy().to_string();
            
            // Scan sessions in this project to get working_dir and stats
            let mut session_count = 0;
            let mut last_updated = 0u64;
            let mut working_dir = PathBuf::new();
            
            for session_file in std::fs::read_dir(&path)? {
                let session_file = session_file?;
                let file_path = session_file.path();
                
                if file_path.extension().map_or(false, |ext| ext == "json") {
                    if let Ok(json) = std::fs::read_to_string(&file_path) {
                        if let Ok(session) = serde_json::from_str::<Session>(&json) {
                            // Skip empty sessions
                            if session.messages.is_empty() {
                                continue;
                            }
                            session_count += 1;
                            if session.updated_at > last_updated {
                                last_updated = session.updated_at;
                                working_dir = session.working_dir;
                            }
                        }
                    }
                }
            }
            
            if session_count > 0 {
                projects.push(ProjectInfo {
                    hash,
                    working_dir,
                    session_count,
                    last_updated,
                });
            }
        }
    }
    
    // Sort by last_updated descending
    projects.sort_by(|a, b| b.last_updated.cmp(&a.last_updated));
    Ok(projects)
}

/// Session metadata with project hash for cross-project listing
#[derive(Debug, Serialize)]
pub struct SessionMetaWithProject {
    pub project_hash: String,
    #[serde(flatten)]
    pub meta: SessionMeta,
}

/// List sessions for a project
fn list_sessions(project_hash: &str) -> std::io::Result<Vec<SessionMeta>> {
    let project_dir = sessions_dir().join(project_hash);
    if !project_dir.exists() {
        return Ok(Vec::new());
    }

    let mut sessions = Vec::new();
    
    for entry in std::fs::read_dir(project_dir)? {
        let entry = entry?;
        let path = entry.path();
        
        if path.extension().map_or(false, |ext| ext == "json") {
            let file_size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if let Ok(json) = std::fs::read_to_string(&path) {
                if let Ok(session) = serde_json::from_str::<Session>(&json) {
                    // Skip empty sessions (no messages)
                    if session.messages.is_empty() {
                        continue;
                    }
                    let mut meta = SessionMeta::from(&session);
                    meta.file_size = file_size;
                    sessions.push(meta);
                }
            }
        }
    }
    
    // Sort by updated_at descending
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(sessions)
}

/// List all sessions across all projects
fn list_all_sessions() -> std::io::Result<Vec<SessionMetaWithProject>> {
    let sessions_root = sessions_dir();
    if !sessions_root.exists() {
        return Ok(Vec::new());
    }

    let mut all_sessions = Vec::new();
    
    for entry in std::fs::read_dir(sessions_root)? {
        let entry = entry?;
        let path = entry.path();
        
        if path.is_dir() {
            let project_hash = path.file_name().unwrap().to_string_lossy().to_string();
            
            for session_file in std::fs::read_dir(&path)? {
                let session_file = session_file?;
                let file_path = session_file.path();
                
                if file_path.extension().map_or(false, |ext| ext == "json") {
                    let file_size = session_file.metadata().map(|m| m.len()).unwrap_or(0);
                    if let Ok(json) = std::fs::read_to_string(&file_path) {
                        if let Ok(session) = serde_json::from_str::<Session>(&json) {
                            // Skip empty sessions (no messages)
                            if session.messages.is_empty() {
                                continue;
                            }
                            let mut meta = SessionMeta::from(&session);
                            meta.file_size = file_size;
                            all_sessions.push(SessionMetaWithProject {
                                project_hash: project_hash.clone(),
                                meta,
                            });
                        }
                    }
                }
            }
        }
    }
    
    // Sort by updated_at descending
    all_sessions.sort_by(|a, b| b.meta.updated_at.cmp(&a.meta.updated_at));
    // Limit to first 50 sessions
    all_sessions.truncate(50);
    Ok(all_sessions)
}

/// Load a specific session
fn load_session(project_hash: &str, session_id: &str) -> std::io::Result<Session> {
    let path = sessions_dir()
        .join(project_hash)
        .join(format!("{}.json", session_id));
    
    let json = std::fs::read_to_string(path)?;
    serde_json::from_str(&json)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
}

// ============== HTTP Handlers ==============

/// GET /projects - List all projects
async fn get_projects() -> impl IntoResponse {
    match list_projects() {
        Ok(projects) => Json(projects).into_response(),
        Err(e) => {
            let msg = format!("Failed to list projects: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(msg)).into_response()
        }
    }
}

/// GET /projects/:hash/sessions - List sessions for a project
async fn get_project_sessions(Path(hash): Path<String>) -> impl IntoResponse {
    match list_sessions(&hash) {
        Ok(sessions) => Json(sessions).into_response(),
        Err(e) => {
            let msg = format!("Failed to list sessions: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(msg)).into_response()
        }
    }
}

/// GET /projects/:hash/sessions/:id - Get session detail
async fn get_session_detail(
    Path((hash, id)): Path<(String, String)>,
) -> impl IntoResponse {
    match load_session(&hash, &id) {
        Ok(session) => {
            let detail = SessionDetail {
                id: session.id.to_string(),
                name: session.name,
                working_dir: session.working_dir,
                created_at: session.created_at,
                updated_at: session.updated_at,
                message_count: session.messages.len(),
                messages: session.messages.iter().map(MessageInfo::from).collect(),
            };
            Json(detail).into_response()
        }
        Err(e) => {
            let msg = format!("Failed to load session: {}", e);
            (StatusCode::NOT_FOUND, Json(msg)).into_response()
        }
    }
}

/// GET /sessions - List all sessions across all projects
async fn get_all_sessions() -> impl IntoResponse {
    match list_all_sessions() {
        Ok(sessions) => Json(sessions).into_response(),
        Err(e) => {
            let msg = format!("Failed to list sessions: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(msg)).into_response()
        }
    }
}

/// Search sessions by name across all projects
fn search_sessions_by_name(keyword: &str) -> std::io::Result<Vec<SessionMetaWithProject>> {
    let sessions_root = sessions_dir();
    if !sessions_root.exists() {
        return Ok(Vec::new());
    }

    let keyword_lower = keyword.to_lowercase();
    let mut results = Vec::new();
    
    for entry in std::fs::read_dir(sessions_root)? {
        let entry = entry?;
        let path = entry.path();
        
        if path.is_dir() {
            let project_hash = path.file_name().unwrap().to_string_lossy().to_string();
            
            for session_file in std::fs::read_dir(&path)? {
                let session_file = session_file?;
                let file_path = session_file.path();
                
                if file_path.extension().map_or(false, |ext| ext == "json") {
                    let file_size = session_file.metadata().map(|m| m.len()).unwrap_or(0);
                    if let Ok(json) = std::fs::read_to_string(&file_path) {
                        if let Ok(session) = serde_json::from_str::<Session>(&json) {
                            // Skip empty sessions
                            if session.messages.is_empty() {
                                continue;
                            }
                            // Match keyword in session name (case-insensitive)
                            if session.name.to_lowercase().contains(&keyword_lower) {
                                let mut meta = SessionMeta::from(&session);
                                meta.file_size = file_size;
                                results.push(SessionMetaWithProject {
                                    project_hash: project_hash.clone(),
                                    meta,
                                });
                            }
                        }
                    }
                }
            }
        }
    }
    
    // Sort by updated_at descending
    results.sort_by(|a, b| b.meta.updated_at.cmp(&a.meta.updated_at));
    Ok(results)
}

/// GET /sessions/search?q=keyword - Search sessions by name
async fn search_sessions(Query(query): Query<SearchQuery>) -> impl IntoResponse {
    if query.q.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, Json("Search keyword cannot be empty")).into_response();
    }
    
    match search_sessions_by_name(&query.q) {
        Ok(sessions) => Json(sessions).into_response(),
        Err(e) => {
            let msg = format!("Failed to search sessions: {}", e);
            (StatusCode::INTERNAL_SERVER_ERROR, Json(msg)).into_response()
        }
    }
}

/// Delete a session file
fn delete_session_file(project_hash: &str, session_id: &str) -> std::io::Result<()> {
    let path = sessions_dir()
        .join(project_hash)
        .join(format!("{}.json", session_id));
    
    if !path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Session not found: {}/{}", project_hash, session_id)
        ));
    }
    
    std::fs::remove_file(path)
}

/// DELETE /projects/:hash/sessions/:id - Delete a session
async fn delete_session(
    Path((hash, id)): Path<(String, String)>,
) -> impl IntoResponse {
    match delete_session_file(&hash, &id) {
        Ok(()) => {
            let msg = format!("Session {} deleted successfully", id);
            (StatusCode::OK, Json(msg)).into_response()
        }
        Err(e) => {
            let msg = format!("Failed to delete session: {}", e);
            (StatusCode::NOT_FOUND, Json(msg)).into_response()
        }
    }
}

/// Rename request body
#[derive(Debug, Deserialize)]
pub struct RenameRequest {
    pub name: String,
}

/// Rename a session
fn rename_session_file(project_hash: &str, session_id: &str, new_name: &str) -> std::io::Result<()> {
    let path = sessions_dir()
        .join(project_hash)
        .join(format!("{}.json", session_id));
    
    if !path.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Session not found: {}/{}", project_hash, session_id)
        ));
    }
    
    // Load, rename, and save
    let json = std::fs::read_to_string(&path)?;
    let mut session: Session = serde_json::from_str(&json)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    
    session.rename(new_name.to_string());
    
    let manager = SessionManager::new(&PathBuf::from(&session.working_dir));
    manager.save(&session)
}

/// PATCH /projects/:hash/sessions/:id/rename - Rename a session
async fn rename_session(
    Path((hash, id)): Path<(String, String)>,
    Json(req): Json<RenameRequest>,
) -> impl IntoResponse {
    match rename_session_file(&hash, &id, &req.name) {
        Ok(()) => {
            let msg = format!("Session {} renamed to '{}'", id, req.name);
            (StatusCode::OK, Json(msg)).into_response()
        }
        Err(e) => {
            let msg = format!("Failed to rename session: {}", e);
            (StatusCode::NOT_FOUND, Json(msg)).into_response()
        }
    }
}

#[tokio::main]
async fn main() {
    use axum::routing::patch;
    
    let app = Router::new()
        .route("/sessions", get(get_all_sessions))
        .route("/sessions/search", get(search_sessions))
        .route("/projects", get(get_projects))
        .route("/projects/:hash/sessions", get(get_project_sessions))
        .route("/projects/:hash/sessions/:id", get(get_session_detail).delete(delete_session))
        .route("/projects/:hash/sessions/:id/rename", patch(rename_session))
        .layer(CorsLayer::new().allow_origin(Any).allow_methods(Any));

    let addr = "0.0.0.0:3456";
    println!("AtomCode API server listening on http://{}", addr);
    println!("\nAPI endpoints:");
    println!("  GET /sessions                       - List all sessions (cross-project)");
    println!("  GET /sessions/search?q=<keyword>    - Search sessions by name");
    println!("  GET /projects                       - List all projects");
    println!("  GET /projects/:hash/sessions        - List sessions in a project");
    println!("  GET /projects/:hash/sessions/:id    - Get session detail with messages");
    println!("  DELETE /projects/:hash/sessions/:id - Delete a session");
    println!("  PATCH /projects/:hash/sessions/:id/rename - Rename a session (body: {{\"name\": \"new name\"}})");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
