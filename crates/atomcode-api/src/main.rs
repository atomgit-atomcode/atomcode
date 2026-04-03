//! AtomCode API Service
//!
//! Provides HTTP API for querying conversation history and streaming chat.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json, sse::Sse},
    routing::{get, post},
    Router,
};
use futures::stream::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_util::sync::CancellationToken;
use tower_http::cors::{Any, CorsLayer};

use atomcode_core::config::Config;
use atomcode_core::session::{Session, SessionId, SessionManager, SessionMeta};
use atomcode_core::conversation::Conversation;
use atomcode_core::provider;
use atomcode_core::tool::{ToolContext, ToolRegistry};
use atomcode_core::turn::runner::TurnRunner;
use atomcode_core::turn::event::{TurnEvent, TurnResult};
use atomcode_core::turn::permission::{AutoPermissionDecider, AutoPermissionMode};
#[derive(Debug, Clone, Serialize)]
pub struct ProjectInfo {
    /// Project hash (directory name in sessions/)
    pub hash: String,
    /// Project name (user-defined or directory name)
    pub name: String,
    /// Working directory path (from session files)
    pub working_dir: PathBuf,
    /// Optional description
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Number of sessions
    pub session_count: usize,
    /// Creation timestamp
    pub created_at: u64,
    /// Last update timestamp
    pub last_updated: u64,
}

/// Current project state (working directory)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectState {
    /// Current working directory
    pub working_dir: PathBuf,
    /// Previous working directory (for /cd -)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_dir: Option<PathBuf>,
    /// Recently visited directories (max 5)
    pub recent_dirs: Vec<PathBuf>,
    /// Project name (derived from directory name)
    pub name: String,
}

/// Request to change working directory
#[derive(Debug, Deserialize)]
pub struct ChangeDirRequest {
    /// New working directory path, or "-" to go back
    pub path: String,
}

/// Response after changing directory
#[derive(Debug, Serialize)]
pub struct ChangeDirResponse {
    pub success: bool,
    pub message: String,
    pub current_dir: PathBuf,
    pub project_hash: String,
}

/// Search query parameters
#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    /// Search keyword for session name
    pub q: String,
}

/// Request to create a new session
#[derive(Debug, Deserialize)]
pub struct CreateSessionRequest {
    /// Optional working directory (uses current project dir if not provided)
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    /// Optional session title
    #[serde(default)]
    pub title: Option<String>,
}

/// Response for created session
#[derive(Debug, Serialize)]
pub struct CreateSessionResponse {
    pub id: String,
    pub name: String,
    pub working_dir: PathBuf,
    pub project_hash: String,
    pub created_at: u64,
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

/// Global project state store (current working directory)
type ProjectStateStore = Arc<RwLock<ProjectState>>;

/// Active chat tasks (session_id -> cancellation token)
type ChatTasksStore = Arc<RwLock<HashMap<String, CancellationToken>>>;

/// Stopped sessions (session_id) - used to prevent saving stopped chats
type StoppedSessionsStore = Arc<RwLock<HashSet<String>>>;

/// Combined app state for Axum
#[derive(Clone)]
pub struct AppState {
    pub sessions: SessionStore,
    pub project: ProjectStateStore,
    /// Active chat tasks that can be cancelled
    pub chat_tasks: ChatTasksStore,
    /// Sessions that were stopped - their messages should not be saved
    pub stopped_sessions: StoppedSessionsStore,
}

/// Get default working directory
fn default_working_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Initialize project state from config or default
fn init_project_state() -> ProjectState {
    let config_path = Config::default_path();
    if let Ok(config) = Config::load(&config_path) {
        if let Some(ref workdir) = config.default_workdir {
            let path = PathBuf::from(workdir);
            if path.exists() {
                let name = path.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "project".to_string());
                return ProjectState {
                    working_dir: path,
                    previous_dir: None,
                    recent_dirs: vec![],
                    name,
                };
            }
        }
    }
    let path = default_working_dir();
    let name = path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string());
    ProjectState {
        working_dir: path,
        previous_dir: None,
        recent_dirs: vec![],
        name,
    }
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

fn hash_path(path: &std::path::Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// List all projects (scans sessions directory)
fn list_projects() -> std::io::Result<Vec<ProjectInfo>> {
    let sessions_root = sessions_dir();
    let mut projects = Vec::new();
    
    if !sessions_root.exists() {
        return Ok(projects);
    }
    
    // Scan sessions directory for actual session data
    for entry in std::fs::read_dir(sessions_root)? {
        let entry = entry?;
        let path = entry.path();
        
        if path.is_dir() {
            let hash = path.file_name().unwrap().to_string_lossy().to_string();
            
            // Scan sessions in this project to get working_dir and stats
            let mut session_count = 0;
            let mut last_updated = 0u64;
            let mut created_at = u64::MAX;
            let mut working_dir = PathBuf::new();
            
            for session_file in std::fs::read_dir(&path)? {
                let session_file = session_file?;
                let file_path = session_file.path();
                
                if file_path.extension().map_or(false, |ext| ext == "json") {
                    if let Ok(json) = std::fs::read_to_string(&file_path) {
                        if let Ok(session) = serde_json::from_str::<Session>(&json) {
                            session_count += 1;
                            last_updated = last_updated.max(session.updated_at);
                            created_at = created_at.min(session.created_at);
                            if working_dir.to_string_lossy().is_empty() {
                                working_dir = session.working_dir;
                            }
                        }
                    }
                }
            }
            
            // Only include projects with at least one session
            if session_count > 0 {
                let name = working_dir
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "unknown".to_string());
                
                projects.push(ProjectInfo {
                    hash,
                    name,
                    working_dir,
                    description: None,
                    session_count,
                    created_at: if created_at == u64::MAX { 0 } else { created_at },
                    last_updated,
                });
            }
        }
    }
    
    // Sort by last updated (most recent first)
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

/// GET /project - Get current project state
async fn get_project_state(
    State(state): State<AppState>,
) -> impl IntoResponse {
    let state = state.project.read().await;
    Json(ProjectState {
        working_dir: state.working_dir.clone(),
        previous_dir: state.previous_dir.clone(),
        recent_dirs: state.recent_dirs.clone(),
        name: state.name.clone(),
    })
}

/// POST /cd - Change working directory (like /cd command)
async fn change_dir(
    State(state): State<AppState>,
    Json(req): Json<ChangeDirRequest>,
) -> impl IntoResponse {
    let mut project = state.project.write().await;
    
    // Handle "-" to go back to previous directory
    let new_path = if req.path == "-" {
        match &project.previous_dir {
            Some(prev) => prev.clone(),
            None => {
                return Json(ChangeDirResponse {
                    success: false,
                    message: "No previous directory to go back to".to_string(),
                    current_dir: project.working_dir.clone(),
                    project_hash: hash_path(&project.working_dir),
                });
            }
        }
    } else {
        // Expand ~ and make absolute
        let expanded = if req.path.starts_with('~') {
            dirs::home_dir()
                .map(|h| h.join(req.path.strip_prefix('~').unwrap_or("").trim_start_matches('/')))
                .unwrap_or_else(|| PathBuf::from(&req.path))
        } else {
            PathBuf::from(&req.path)
        };
        
        let resolved = if expanded.is_absolute() {
            expanded
        } else {
            project.working_dir.join(&expanded)
        };
        
        // Check if directory exists
        if !resolved.exists() {
            return Json(ChangeDirResponse {
                success: false,
                message: format!("Directory does not exist: {}", resolved.display()),
                current_dir: project.working_dir.clone(),
                project_hash: hash_path(&project.working_dir),
            });
        }
        
        if !resolved.is_dir() {
            return Json(ChangeDirResponse {
                success: false,
                message: format!("Not a directory: {}", resolved.display()),
                current_dir: project.working_dir.clone(),
                project_hash: hash_path(&project.working_dir),
            });
        }
        
        resolved
    };
    
    // Update state
    let old_dir = project.working_dir.clone();
    project.previous_dir = Some(old_dir);
    project.working_dir = new_path.clone();
    project.name = new_path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "project".to_string());
    
    // Update recent dirs (max 5, deduplicated)
    project.recent_dirs.retain(|d| d != &new_path);
    project.recent_dirs.insert(0, new_path.clone());
    project.recent_dirs.truncate(5);
    
    // Persist to config
    let config_path = Config::default_path();
    if let Ok(mut config) = Config::load(&config_path) {
        config.default_workdir = Some(new_path.to_string_lossy().to_string());
        let _ = config.save(&config_path);
    }
    
    let hash = hash_path(&new_path);
    Json(ChangeDirResponse {
        success: true,
        message: format!("Changed to {}", new_path.display()),
        current_dir: new_path,
        project_hash: hash,
    })
}

/// GET /projects - List all projects (historical, from sessions directory)
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

/// POST /sessions - Create a new session
async fn create_session(
    State(state): State<AppState>,
    Json(req): Json<CreateSessionRequest>,
) -> impl IntoResponse {
    // Determine working directory
    let working_dir = match req.working_dir {
        Some(dir) => dir,
        None => {
            // Use current project's working directory
            let project = state.project.read().await;
            project.working_dir.clone()
        }
    };
    
    // Ensure working directory exists
    if !working_dir.exists() {
        // Create atomchat directory in user's home if default
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let atomchat_dir = home.join("atomchat");
        if atomchat_dir.exists() || std::fs::create_dir_all(&atomchat_dir).is_ok() {
            // Use atomchat directory as working dir
        } else {
            let msg = format!("Working directory does not exist: {:?}", working_dir);
            return (StatusCode::BAD_REQUEST, Json(msg)).into_response();
        }
    }
    
    // Create session manager
    let manager = SessionManager::new(&working_dir);
    
    // Create new session
    let mut session = Session::new(working_dir.clone());
    
    // Set title if provided
    if let Some(title) = req.title {
        session.rename(title);
    }
    
    // Save session
    if let Err(e) = manager.save(&session) {
        let msg = format!("Failed to save session: {}", e);
        return (StatusCode::INTERNAL_SERVER_ERROR, Json(msg)).into_response();
    }
    
    // Calculate project hash for response
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    working_dir.hash(&mut hasher);
    let project_hash = format!("{:016x}", hasher.finish());
    
    let response = CreateSessionResponse {
        id: session.id.to_string(),
        name: session.name.clone(),
        working_dir: session.working_dir.clone(),
        project_hash,
        created_at: session.created_at,
    };
    
    (StatusCode::CREATED, Json(response)).into_response()
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

/// Model info for API response
#[derive(Debug, Serialize)]
pub struct ModelInfo {
    /// Provider name
    pub provider: String,
    /// Model identifier
    pub model: String,
    /// Provider type (claude, openai, ollama)
    pub provider_type: String,
    /// Whether this is the default provider
    pub is_default: bool,
}

/// GET /models - List all available models from configured providers
async fn get_models() -> impl IntoResponse {
    let config_path = Config::default_path();
    let config = match Config::load(&config_path) {
        Ok(c) => c,
        Err(_e) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(Vec::<ModelInfo>::new())).into_response();
        }
    };
    
    let models: Vec<ModelInfo> = config
        .providers
        .iter()
        .map(|(name, p)| ModelInfo {
            provider: name.clone(),
            model: p.model.clone(),
            provider_type: p.provider_type.clone(),
            is_default: name == &config.default_provider,
        })
        .collect();
    
    (StatusCode::OK, Json(models)).into_response()
}

// ============== Streaming Chat API ==============

/// Chat request body
#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    /// User message content
    pub message: String,
    /// Working directory (defaults to current dir)
    #[serde(default)]
    pub working_dir: Option<PathBuf>,
    /// Provider name (defaults to configured default)
    #[serde(default)]
    pub provider: Option<String>,
    /// Session ID to continue (optional, creates new if not provided)
    #[serde(default)]
    pub session_id: Option<String>,
}

/// SSE event types for streaming chat
#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum ChatEvent {
    /// LLM text delta
    #[serde(rename = "text")]
    TextDelta { content: String },
    /// Tool call started
    #[serde(rename = "tool_start")]
    ToolCallStarted { name: String, arguments: String },
    /// Tool call completed
    #[serde(rename = "tool_result")]
    ToolCallResult { name: String, output: String, success: bool, duration_ms: u64 },
    /// Token usage update
    #[serde(rename = "tokens")]
    TokenUsage { prompt: usize, completion: usize, total: usize },
    /// Chat completed
    #[serde(rename = "done")]
    Done { tokens: usize, tool_calls: usize },
    /// Chat was stopped by user
    #[serde(rename = "stopped")]
    Stopped,
    /// Error occurred
    #[serde(rename = "error")]
    Error { message: String },
}

/// Global chat sessions store (in-memory for now)
type SessionStore = Arc<RwLock<std::collections::HashMap<String, Conversation>>>;

/// POST /chat - Stream chat response with SSE
async fn chat_stream(
    State(state): State<AppState>,
    Json(mut req): Json<ChatRequest>,
) -> impl IntoResponse {
// Use current project working directory if not specified
    if req.working_dir.is_none() {
        let project = state.project.read().await;
        req.working_dir = Some(project.working_dir.clone());
    }
    
    let (tx, rx) = mpsc::unbounded_channel::<ChatEvent>();
    
    // Create cancellation token for this chat
    let cancel_token = CancellationToken::new();
    
    // Register this chat task if we have a session_id
    let session_id = req.session_id.clone();
    if let Some(ref sid) = session_id {
        state.chat_tasks.write().await.insert(sid.clone(), cancel_token.clone());
    }
    
    // Clone state for the spawned task
    let chat_tasks = state.chat_tasks.clone();
    let stopped_sessions = state.stopped_sessions.clone();
    
    // Spawn the chat processing task
    tokio::spawn(async move {
        if let Err(e) = process_chat_request(req, tx.clone(), cancel_token, stopped_sessions.clone()).await {
            let _ = tx.send(ChatEvent::Error { message: e.to_string() });
        }
        
        // Cleanup: remove from chat_tasks
        if let Some(sid) = session_id {
            chat_tasks.write().await.remove(&sid);
        }
    });
    let stream = UnboundedReceiverStream::new(rx).map(|event| {
        let json = serde_json::to_string(&event).unwrap_or_default();
        Ok::<_, std::convert::Infallible>(
            axum::response::sse::Event::default().data(json)
        )
    });
    
    Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("ping"),
    )
}

/// Process a chat request and stream events
async fn process_chat_request(
    req: ChatRequest,
    event_tx: mpsc::UnboundedSender<ChatEvent>,
    cancel_token: CancellationToken,
    stopped_sessions: StoppedSessionsStore,
) -> anyhow::Result<()> {
    use atomcode_core::tool::{
        bash::BashTool, read::ReadFileTool, write::WriteFileTool, edit::EditFileTool,
        grep::GrepTool, glob::GlobTool, list_dir::ListDirTool,
        web_search::WebSearchTool, web_fetch::WebFetchTool, search_replace::SearchReplaceTool,
    };
    // Load config
    let config_path = Config::default_path();
    let config = Config::load(&config_path)?;
    
    // Determine provider
    let provider_name = req.provider.unwrap_or_else(|| config.default_provider.clone());
    let provider_config = config.providers.get(&provider_name)
        .ok_or_else(|| anyhow::anyhow!("Provider '{}' not found", provider_name))?;
    
    // Create provider instance
    let provider = provider::create_provider(provider_config)?;
    
    // Get working directory
    let working_dir = req.working_dir.unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    
    // Create session manager for this working directory
let session_manager = SessionManager::new(&working_dir);
    
    // Load or create session
// Load or create session
    let mut session = if let Some(ref session_id_str) = req.session_id {
        // Try to load existing session
        let session_id = SessionId::from_string(session_id_str.clone());
        match session_manager.load(&session_id) {
            Ok(session) => session,
            Err(_) => {
                // Session not found, create new one
                Session::new(working_dir.clone())
            }
        }
    } else {
        // Create new session
        Session::new(working_dir.clone())
    };

    // Create conversation from session messages
    let conversation = Arc::new(tokio::sync::Mutex::new({
        let mut conv = Conversation::new();
        conv.messages = session.messages.clone();
        conv
    }));
    conversation.lock().await.add_user_message(&req.message);
    // Build tool registry and context
    let tool_context = ToolContext::new(working_dir.clone());
    let mut tool_registry = ToolRegistry::new();
    tool_registry.register(Box::new(ReadFileTool));
    tool_registry.register(Box::new(WriteFileTool));
    tool_registry.register(Box::new(EditFileTool));
    tool_registry.register(Box::new(BashTool));
    tool_registry.register(Box::new(GrepTool));
    tool_registry.register(Box::new(GlobTool));
    tool_registry.register(Box::new(ListDirTool));
    tool_registry.register(Box::new(WebSearchTool));
    tool_registry.register(Box::new(WebFetchTool));
    tool_registry.register(Box::new(SearchReplaceTool));
    let shared_tools = Arc::new(tool_registry);
    
    // Create turn runner with auto-bypass permission (API mode - no interactive approval)
    let permission = Box::new(AutoPermissionDecider::new(AutoPermissionMode::BypassAll));
    let turn_runner = TurnRunner {
        provider,
        tools: shared_tools,
        context: tool_context,
        config: config.clone(),
        permission,
        result_store: atomcode_core::tool::result_store::ToolResultStore::new(
            atomcode_core::tool::result_store::ToolResultStore::default_dir()
        ),
    };
    
    // Build system prompt (minimal for API)
    let system_prompt = build_api_system_prompt(&working_dir);
    // Create turn event channel
    let (turn_tx, mut turn_rx) = mpsc::unbounded_channel::<TurnEvent>();
    
    // Check if session was stopped before we started
    if stopped_sessions.read().await.contains(&req.session_id.clone().unwrap_or_default()) {
        let _ = event_tx.send(ChatEvent::Stopped);
        let _ = event_tx.send(ChatEvent::Done { tokens: 0, tool_calls: 0 });
        return Ok(());
    }
    
    // Clone conversation Arc for the spawn task
    let conversation_clone = conversation.clone();
    
    // Run turn(s) in background task - may need multiple turns if tools are used
    tokio::spawn(async move {
        let mut conv = conversation_clone.lock().await;
        
        // Loop until LLM produces text without tool calls
        loop {
            let result = turn_runner.run(&mut conv, &system_prompt, &turn_tx, cancel_token.clone()).await;
            
            match result {
                TurnResult::Responded { .. } => {
                    // LLM produced text, turn is complete
                    break;
                }
                TurnResult::UsedTools { .. } => {
                    // Tools were executed, continue to next LLM call
                    // The tool results are already added to conversation
                    continue;
                }
                TurnResult::Failed(e) => {
                    let _ = turn_tx.send(TurnEvent::Error(e));
                    break;
                }
                TurnResult::Cancelled => {
                    break;
                }
            }
        }
    });
    
    // Forward turn events to chat events
    let mut total_tokens = 0usize;
    let mut tool_call_count = 0usize;
    
    while let Some(event) = turn_rx.recv().await {
        match event {
            TurnEvent::TextDelta(text) => {
                let _ = event_tx.send(ChatEvent::TextDelta { content: text });
            }
            TurnEvent::ToolCallStarted { name, arguments } => {
                tool_call_count += 1;
                let _ = event_tx.send(ChatEvent::ToolCallStarted { name, arguments });
            }
            TurnEvent::ToolCallResult { name, output, success, duration } => {
                let _ = event_tx.send(ChatEvent::ToolCallResult {
                    name,
                    output,
                    success,
                    duration_ms: duration.as_millis() as u64,
                });
            }
            TurnEvent::TokenUsage { prompt_tokens, completion_tokens, total_tokens: tt } => {
                total_tokens = tt;
                let _ = event_tx.send(ChatEvent::TokenUsage {
                    prompt: prompt_tokens,
                    completion: completion_tokens,
                    total: tt,
                });
            }
            TurnEvent::Error(e) => {
                let _ = event_tx.send(ChatEvent::Error { message: e });
            }
            TurnEvent::ContextStats { .. } => {
                // Ignore context stats in API mode
            }
        }
}
    
    // Save session after conversation completes (unless stopped)
    let session_id_str = req.session_id.clone().unwrap_or_default();
    let was_stopped = stopped_sessions.read().await.contains(&session_id_str);
    
    if was_stopped {
        // Session was stopped, don't save the messages
        eprintln!("Session {} was stopped, skipping save", session_id_str);
    } else {
        let conv = conversation.lock().await;
        session.messages = conv.messages.clone();
        session.touch();
        if let Err(e) = session_manager.save(&session) {
            eprintln!("Warning: Failed to save session: {}", e);
        }
    }
    
    // Clean up stopped sessions marker if present
    if was_stopped {
        stopped_sessions.write().await.remove(&session_id_str);
    }
    
    // Send done event
    let _ = event_tx.send(ChatEvent::Done {
        tokens: total_tokens,
        tool_calls: tool_call_count,
    });
    Ok(())
}

/// Build minimal system prompt for API mode
fn build_api_system_prompt(working_dir: &PathBuf) -> String {
    let cwd = working_dir.to_string_lossy();
    format!(
        r#"You are AtomCode, an expert coding agent. You solve tasks efficiently with minimal tool calls.

## WORKING DIRECTORY
{cwd}

## PRINCIPLES:
1. ACT, DON'T INSTRUCT — DO IT, don't tell the user how.
2. BE CONCISE — State what you did. No unsolicited advice.
3. ONE SIGNAL IS ENOUGH — Success once → move on.

## WORKFLOW:
1. INVESTIGATE: Read code and logs. Don't ask the user — find the answer yourself.
2. LOCATE: Use project context to find the right files.
3. EDIT: Make targeted changes.
4. VERIFY: After EACH edit, compile/build. Fix errors before moving on.
5. SUMMARIZE: Tell the user what you changed.
"#
    )
}

/// Request to stop a chat session
#[derive(Debug, Deserialize)]
struct StopChatRequest {
    session_id: String,
}

/// Response for stop chat request
#[derive(Debug, Serialize)]
struct StopChatResponse {
    success: bool,
    message: String,
}

/// POST /chat/stop - Stop a running chat session
async fn stop_chat(
    State(state): State<AppState>,
    Json(req): Json<StopChatRequest>,
) -> impl IntoResponse {
    // Add to stopped sessions set
    state.stopped_sessions.write().await.insert(req.session_id.clone());
    
    // Cancel the chat task if it exists
    if let Some(cancel_token) = state.chat_tasks.read().await.get(&req.session_id) {
        cancel_token.cancel();
        (axum::http::StatusCode::OK, Json(StopChatResponse {
            success: true,
            message: format!("Chat session {} stopped", req.session_id),
        }))
    } else {
        // Session wasn't running, but we marked it as stopped
        (axum::http::StatusCode::OK, Json(StopChatResponse {
            success: true,
            message: format!("Chat session {} marked as stopped (was not running)", req.session_id),
        }))
    }
}

#[tokio::main]
async fn main() {
    use axum::routing::patch;
    
    let state = AppState {
        sessions: Arc::new(RwLock::new(std::collections::HashMap::new())),
        project: Arc::new(RwLock::new(init_project_state())),
        chat_tasks: Arc::new(RwLock::new(HashMap::new())),
        stopped_sessions: Arc::new(RwLock::new(HashSet::new())),
    };
    
    let app = Router::new()
        // Session APIs
        .route("/sessions", get(get_all_sessions).post(create_session))
        .route("/sessions/search", get(search_sessions))
        // Current project state (working directory)
        .route("/project", get(get_project_state))
        .route("/cd", post(change_dir))
        // Historical projects (from sessions directory)
        .route("/projects", get(get_projects))
        .route("/projects/:hash/sessions", get(get_project_sessions))
.route("/projects/:hash/sessions/:id", get(get_session_detail).delete(delete_session))
        .route("/projects/:hash/sessions/:id/rename", patch(rename_session))
        // Model API
        .route("/models", get(get_models))
        // Chat API
        .route("/chat", post(chat_stream))
        .route("/chat/stop", post(stop_chat))
        .with_state(state)
        .layer(CorsLayer::new().allow_origin(Any).allow_methods(Any));

    let addr = "0.0.0.0:13456";
    println!("AtomCode API server listening on http://{}", addr);
    println!("\nAPI endpoints:");
    println!("  GET    /project                        - Get current working directory");
    println!("  POST   /cd                             - Change working directory (like /cd command)");
    println!("  GET    /projects                       - List historical projects");
    println!("  GET    /projects/:hash/sessions        - List sessions in a project");
    println!("  GET    /projects/:hash/sessions/:id    - Get session detail");
    println!("  DELETE /projects/:hash/sessions/:id    - Delete a session");
    println!("  PATCH  /projects/:hash/sessions/:id/rename - Rename a session");
    println!("  GET    /sessions                       - List all sessions (cross-project)");
    println!("  GET    /sessions/search?q=<keyword>    - Search sessions by name");
    println!("  GET    /models                         - List available models");
    println!("  POST   /chat                           - Stream chat response (SSE)");
    println!("\nChange directory body:");
    println!("  {{\"path\": \"/path/to/project\"}}  or {{\"path\": \"-\"}} to go back");
    println!("\nChat request body:");
    println!("  {{\"message\": \"your question\", \"provider\": \"optional\"}}");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
