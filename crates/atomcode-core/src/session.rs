//! Session management for persistent conversation contexts.
//!
//! Each session represents an independent conversation with its own message history,
//! associated with a specific working directory.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use crate::conversation::{
    message::{Message, Role},
    Conversation, ConversationSnapshot,
};
use crate::setup::fs_atomic::atomic_write;

/// Unique identifier for a session.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(String);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Create from an existing string (for loading sessions).
    pub fn from_string(s: String) -> Self {
        Self(s)
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Per-turn summary stats, recorded at each completed turn so `/resume` can
/// re-render the same `✓ … 工具 · tokens` divider the live turn showed
/// (token/duration are otherwise lost — only `messages` is persisted).
///
/// `after_message` anchors the divider to a position in `messages`: it is the
/// conversation message count at the moment the turn completed, so on replay
/// the divider is emitted right after that many messages have been rendered.
/// Anchoring by count (rather than a turn ordinal) stays correct even if some
/// turns were cancelled and produced no stat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnStat {
    /// Number of messages in the conversation when this turn completed.
    pub after_message: usize,
    /// LLM round-trips in the turn.
    pub turn_count: usize,
    /// Tool calls executed in the turn.
    pub tool_call_count: usize,
    /// Wall-clock duration of the turn, milliseconds.
    pub duration_ms: u64,
    /// Total tokens the turn consumed.
    pub total_tokens: usize,
    /// The turn ended in an error (render the ✗ "stopped" variant).
    #[serde(default)]
    pub errored: bool,
}

/// A UI-only message that should be replayed in session history but must not
/// be included in model context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayMessage {
    /// Render after this many real conversation messages.
    pub after_message: usize,
    pub message: Message,
}

/// A session represents an independent conversation context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Unique identifier.
    pub id: SessionId,
    /// Display name (AI-generated or user-specified).
    pub name: String,
    /// Working directory this session is associated with.
    pub working_dir: PathBuf,
    /// Creation timestamp (seconds since UNIX epoch).
    pub created_at: u64,
    /// Last update timestamp.
    pub updated_at: u64,
    /// Conversation messages.
    pub messages: Vec<Message>,
    /// UI-only messages such as locally handled slash command output.
    #[serde(default)]
    pub display_messages: Vec<DisplayMessage>,
    /// Compressed summaries of older conversation history.
    #[serde(default)]
    pub cold_summaries: Vec<String>,
    /// True once the user has explicitly run `/rename`. Drives the
    /// session-name badge above the input box: auto-named sessions
    /// (default / session-* / first-message-derived) stay badge-less
    /// so the chrome doesn't get noisy on every fresh conversation —
    /// the badge is reserved for names the user deliberately chose.
    /// `#[serde(default)]` so sessions saved before this field exists
    /// load as `false` (i.e., behave like auto-named).
    #[serde(default)]
    pub user_renamed: bool,
    /// True once the AI session-namer has set this session's name (see
    /// `agent::session_title`). Persisted so the one-shot naming survives
    /// reconnects/restarts: a fresh bridge instance re-arms its in-memory
    /// `ai_name_attempted` flag, so without this durable marker every
    /// `/resume`-then-message would re-run the namer and overwrite a perfectly
    /// good AI name. Gates `should_accept_ai_name`. A deliberate `/rename`
    /// (`user_renamed`) still wins over an AI name regardless of this flag.
    /// `#[serde(default)]` → sessions saved before this field load as `false`.
    #[serde(default)]
    pub ai_named: bool,
    /// Per-turn summary stats (one per completed turn), so `/resume` can
    /// re-render the `✓ … 工具 · tokens` divider between turns. `#[serde(default)]`
    /// keeps sessions saved before this field loading — they replay with plain
    /// inter-turn dividers (no stats numbers) instead.
    #[serde(default)]
    pub turn_stats: Vec<TurnStat>,
}

impl Session {
    /// Create a new session for the given working directory.
    pub fn new(working_dir: PathBuf) -> Self {
        let now = current_timestamp();
        Self {
            id: SessionId::new(),
            name: format!("session-{}", format_timestamp(now)),
            working_dir,
            created_at: now,
            updated_at: now,
            messages: Vec::new(),
            display_messages: Vec::new(),
            cold_summaries: Vec::new(),
            user_renamed: false,
            ai_named: false,
            turn_stats: Vec::new(),
        }
    }

    /// Create a default session (used on first launch).
    pub fn default_session(working_dir: PathBuf) -> Self {
        Self {
            id: SessionId::new(),
            name: "default".to_string(),
            working_dir,
            created_at: current_timestamp(),
            updated_at: current_timestamp(),
            messages: Vec::new(),
            display_messages: Vec::new(),
            cold_summaries: Vec::new(),
            user_renamed: false,
            ai_named: false,
            turn_stats: Vec::new(),
        }
    }

    /// Update the session's name in response to an explicit user
    /// `/rename`. Also flips `user_renamed` so the session-name badge
    /// becomes visible — auto_name_from_messages must NOT call this.
    pub fn rename(&mut self, name: String) {
        self.name = name;
        self.user_renamed = true;
        self.touch();
    }

    pub fn to_conversation(&self) -> Conversation {
        Conversation::from_snapshot(self.to_conversation_snapshot())
    }

    pub fn update_from_conversation(&mut self, conversation: &Conversation) {
        self.update_from_conversation_snapshot(conversation.snapshot());
    }

    pub fn to_conversation_snapshot(&self) -> ConversationSnapshot {
        ConversationSnapshot {
            messages: self.messages.clone(),
            cold_summaries: self.cold_summaries.clone(),
        }
    }

    pub fn update_from_conversation_snapshot(&mut self, snapshot: ConversationSnapshot) {
        self.messages = snapshot.messages;
        self.cold_summaries = snapshot.cold_summaries;
    }

    /// Auto-name an untouched session from the first real user message.
    ///
    /// This mirrors the TUI persistence behavior: default names are replaced
    /// by the first non-synthetic user turn, while user-renamed sessions are
    /// left alone.
    pub fn auto_name_from_messages(&mut self) {
        // If the user explicitly renamed this session (via /rename or the
        // webui rename dialog), never auto-name it — the user's choice
        // must persist across turns.
        if self.user_renamed {
            return;
        }
        if !should_auto_name_session(&self.name) {
            return;
        }

        // Primary: filter by the `synthetic` flag — accurate for sessions
        // written after the field landed. Secondary: bracket-prefix
        // heuristic (`[Context was compressed]` / `[System meta...]`)
        // for legacy session files whose messages were saved before the
        // field existed and so default to `synthetic = false`.
        let first_real_user = self
            .messages
            .iter()
            .filter(|m| matches!(m.role, Role::User) && !m.synthetic)
            .find_map(|m| m.text().filter(|t| !is_synthetic_user_text_legacy(t)));

        if let Some(text) = first_real_user {
            let name: String = text.lines().next().unwrap_or("").chars().take(40).collect();
            if !name.is_empty() {
                self.name = name;
            }
        }
    }

    /// Update the last modified timestamp.
    pub fn touch(&mut self) {
        self.updated_at = current_timestamp();
    }

    /// Get a short display ID (first 8 chars of UUID).
    pub fn short_id(&self) -> &str {
        &self.id.0[..8]
    }
}

pub(crate) fn should_auto_name_session(name: &str) -> bool {
    // Names starting with `[` are legacy auto-names derived from a
    // synthetic user message (pre-`Message.synthetic`-field heuristic).
    // Treat them like default / session-<ts>: candidates for re-naming
    // once a real user message is found.
    name == "default" || name.starts_with("session-") || name.trim_start().starts_with('[')
}

/// Legacy synthetic-message detector kept as a defensive fallback for
/// session JSONs saved before `Message.synthetic` existed. Such messages
/// load with `synthetic = false` (serde default), so the bracket-prefix
/// convention is the only signal we have for them. New code should rely
/// on the `synthetic` field directly; only `auto_name_from_messages`
/// retains this heuristic so legacy `/resume` picker titles stay sane.
fn is_synthetic_user_text_legacy(text: &str) -> bool {
    text.trim_start().starts_with('[')
}

/// Metadata for a session (without full message history).
/// Used for listing sessions efficiently.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: SessionId,
    pub name: String,
    pub working_dir: PathBuf,
    pub created_at: u64,
    pub updated_at: u64,
    pub message_count: usize,
    /// Session file size in bytes
    #[serde(default)]
    pub file_size: u64,
}

impl From<&Session> for SessionMeta {
    fn from(session: &Session) -> Self {
        Self {
            id: session.id.clone(),
            name: session.name.clone(),
            working_dir: session.working_dir.clone(),
            created_at: session.created_at,
            updated_at: session.updated_at,
            message_count: session.messages.len(),
            file_size: 0, // Will be populated by list()
        }
    }
}

/// Session manager handles persistence and lifecycle.
pub struct SessionManager {
    /// Root directory for session storage (~/.atomcode/sessions/).
    sessions_dir: PathBuf,
    /// Hash of the current project's working directory.
    project_hash: String,
}

impl SessionManager {
    /// Get the root directory for all sessions ($ATOMCODE_HOME/sessions/).
    pub fn sessions_root_dir() -> PathBuf {
        crate::config::Config::config_dir().join("sessions")
    }

    /// Get the legacy sessions directory (used on macOS before v4.16).
    /// Returns None on non-macOS platforms.
    fn legacy_sessions_dir() -> Option<PathBuf> {
        if cfg!(target_os = "macos") {
            dirs::data_local_dir().map(|p| p.join("atomcode").join("sessions"))
        } else {
            None
        }
    }

    /// Migrate sessions from legacy location to new location.
    /// This is a no-op if:
    /// - Not on macOS
    /// - Legacy directory doesn't exist
    /// - New directory already has sessions
    pub fn migrate_from_legacy() {
        let Some(legacy_dir) = Self::legacy_sessions_dir() else {
            return; // Not macOS, no migration needed
        };

        if !legacy_dir.exists() {
            return; // No legacy data
        }

        let new_dir = Self::sessions_root_dir();
        if new_dir.exists() && std::fs::read_dir(&new_dir).map_or(false, |mut d| d.next().is_some())
        {
            return; // New location already has data, skip migration
        }

        // Perform migration
        if let Err(e) = std::fs::create_dir_all(&new_dir) {
            tracing::warn!("[session] Failed to create sessions dir: {}", e);
            return;
        }

        match std::fs::read_dir(&legacy_dir) {
            Ok(entries) => {
                let mut migrated = 0;
                for entry in entries.flatten() {
                    let src = entry.path();
                    let dst = new_dir.join(entry.file_name());
                    if src.is_dir() {
                        if let Err(e) = std::fs::create_dir_all(&dst) {
                            tracing::warn!("[session] Failed to create {:?}: {}", dst, e);
                            continue;
                        }
                        if let Ok(files) = std::fs::read_dir(&src) {
                            for file in files.flatten() {
                                let src_file = file.path();
                                let dst_file = dst.join(file.file_name());
                                if let Err(e) = std::fs::copy(&src_file, &dst_file) {
                                    tracing::warn!(
                                        "[session] Failed to copy {:?}: {}",
                                        src_file,
                                        e
                                    );
                                } else {
                                    migrated += 1;
                                }
                            }
                        }
                    }
                }
                if migrated > 0 {
                    tracing::info!(
                        "[session] Migrated {} session(s) from legacy location",
                        migrated
                    );
                }
            }
            Err(e) => {
                tracing::warn!("[session] Failed to read legacy sessions dir: {}", e);
            }
        }
    }

    /// Create a new session manager for the given working directory.
    pub fn new(working_dir: &Path) -> Self {
        // Auto-migrate from legacy location on first use
        Self::migrate_from_legacy();

        let sessions_dir = Self::sessions_root_dir();
        let project_hash = hash_path(working_dir);

        Self {
            sessions_dir,
            project_hash,
        }
    }

    /// Get the directory for this project's sessions.
    fn project_dir(&self) -> PathBuf {
        self.sessions_dir.join(&self.project_hash)
    }

    /// Ensure the project session directory exists.
    fn ensure_dir(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.project_dir())
    }

    /// Save a session to disk.
    pub fn save(&self, session: &Session) -> std::io::Result<()> {
        self.ensure_dir()?;
        let path = self.project_dir().join(format!("{}.json", session.id));
        let json = serde_json::to_vec_pretty(session)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        atomic_write(&path, &json, 0o644).map_err(std::io::Error::other)
    }

    /// Load a session by ID.
    pub fn load(&self, id: &SessionId) -> std::io::Result<Session> {
        let path = self.project_dir().join(format!("{}.json", id));
        let json = std::fs::read_to_string(path)?;
        serde_json::from_str(&json).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
    }

    /// Find and load a session by ID across **all** project buckets.
    ///
    /// Sessions are sharded into `<sessions_root>/<project_hash>/<id>.json` by
    /// the working dir's hash, so `load` needs to know the project. A webui
    /// session-switch broadcast carries only the id (the TUI may even be in a
    /// different project), so here we scan every bucket for a matching file and
    /// return the first hit — the loaded session's own `working_dir` then tells
    /// the caller which project it belongs to. Errs `NotFound` when no bucket
    /// contains the id.
    pub fn load_any(id: &SessionId) -> std::io::Result<Session> {
        let root = Self::sessions_root_dir();
        let file_name = format!("{}.json", id);
        for entry in std::fs::read_dir(&root)? {
            let path = entry?.path();
            if !path.is_dir() {
                continue;
            }
            let candidate = path.join(&file_name);
            if candidate.is_file() {
                let json = std::fs::read_to_string(&candidate)?;
                return serde_json::from_str(&json)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e));
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("session {id} not found in any project"),
        ))
    }

    /// List all sessions for this project (metadata only).
    pub fn list(&self) -> std::io::Result<Vec<SessionMeta>> {
        let project_dir = self.project_dir();
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
                        let mut meta = SessionMeta::from(&session);
                        meta.file_size = file_size;
                        sessions.push(meta);
                    }
                }
            }
        }

        // Sort by updated_at descending (most recent first)
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(sessions)
    }

    /// Delete a session by ID.
    pub fn delete(&self, id: &SessionId) -> std::io::Result<()> {
        let path = self.project_dir().join(format!("{}.json", id));
        std::fs::remove_file(path)
    }

    /// Check if any sessions exist for this project.
    pub fn has_sessions(&self) -> bool {
        let project_dir = self.project_dir();
        project_dir.exists()
            && std::fs::read_dir(project_dir).map_or(false, |mut d| d.next().is_some())
    }

    /// Get the most recently updated session.
    pub fn latest(&self) -> std::io::Result<Option<Session>> {
        let metas = self.list()?;
        if let Some(latest) = metas.first() {
            return self.load(&latest.id).map(Some);
        }
        Ok(None)
    }
}

/// Generate a hash for a path (used as directory name).
///
/// Normalizes the path before hashing to ensure consistent results across:
/// - Different path separators (Windows: `\` vs `/`)
/// - Case sensitivity (Windows paths are case-insensitive)
/// - Trailing slashes
fn hash_path(path: &Path) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    // Normalize the path:
    // 1. Convert to string representation
    // 2. Replace backslashes with forward slashes (Windows)
    // 3. Remove trailing slash (but keep root "/" or "C:/")
    // 4. Lowercase on Windows (case-insensitive filesystem)
    let normalized = crate::tool::strip_verbatim_prefix(&path.to_string_lossy()).into_owned();
    let mut normalized = normalized.replace('\\', "/");

    if normalized.len() > 1 && normalized.ends_with('/') {
        normalized.pop();
    }

    #[cfg(windows)]
    let normalized = normalized.to_lowercase();

    // IMPORTANT: hash through `Path::hash`, not `str::hash`. `Path`
    // hashes its components with length prefixes, which is NOT the
    // same as hashing the whole string. All sessions saved before
    // the normalization pass was added went into buckets keyed by
    // `Path::hash`; feeding the normalized string back through a
    // `PathBuf` keeps us on that same bucket so /resume still finds
    // legacy sessions. Hashing the raw `&str` here would silently
    // orphan every pre-normalization session — see the "where did
    // my /resume history go?" regression.
    let mut hasher = DefaultHasher::new();
    let p: PathBuf = PathBuf::from(normalized);
    p.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Get current timestamp in seconds.
fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Format timestamp as YYYYMMDD-HHMMSS.
fn format_timestamp(ts: u64) -> String {
    use chrono::{TimeZone, Utc};
    let dt = Utc
        .timestamp_opt(ts as i64, 0)
        .single()
        .unwrap_or_else(|| Utc::now());
    dt.format("%Y%m%d-%H%M%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_id_is_unique() {
        let id1 = SessionId::new();
        let id2 = SessionId::new();
        assert_ne!(id1, id2);
    }

    #[test]
    fn turn_stats_round_trip_and_default_empty_for_old_sessions() {
        let mut s = Session::new(PathBuf::from("/tmp/x"));
        s.turn_stats.push(TurnStat {
            after_message: 4,
            turn_count: 3,
            tool_call_count: 5,
            duration_ms: 6800,
            total_tokens: 1651,
            errored: false,
        });
        let json = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(back.turn_stats.len(), 1);
        assert_eq!(back.turn_stats[0].after_message, 4);
        assert_eq!(back.turn_stats[0].total_tokens, 1651);

        // A session saved before this field existed (no `turn_stats` key) must
        // still load — `#[serde(default)]` → empty vec, replay falls back to
        // plain dividers.
        let old = r#"{"id":"abc","name":"x","working_dir":"/tmp/x","created_at":0,"updated_at":0,"messages":[]}"#;
        let loaded: Session = serde_json::from_str(old).unwrap();
        assert!(loaded.turn_stats.is_empty());
    }

    #[test]
    fn cold_summaries_round_trip_and_default_empty_for_old_sessions() {
        let mut s = Session::new(PathBuf::from("/tmp/x"));
        s.cold_summaries
            .push("older compressed context".to_string());

        let json = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(back.cold_summaries, vec!["older compressed context"]);

        let old = r#"{"id":"abc","name":"x","working_dir":"/tmp/x","created_at":0,"updated_at":0,"messages":[]}"#;
        let loaded: Session = serde_json::from_str(old).unwrap();
        assert!(loaded.cold_summaries.is_empty());
    }

    #[test]
    fn session_conversation_conversion_preserves_cold_summaries() {
        let mut session = Session::new(PathBuf::from("/tmp/x"));
        session
            .messages
            .push(Message::new(Role::User, "current task"));
        session
            .cold_summaries
            .push("compressed history".to_string());

        let mut conversation = session.to_conversation();
        conversation
            .cold_summaries
            .push("new compressed history".to_string());

        session.update_from_conversation(&conversation);

        assert_eq!(
            session.cold_summaries,
            vec!["compressed history", "new compressed history"]
        );
    }

    #[test]
    fn session_reload_preserves_turn_stats_across_saves() {
        use tempfile::tempdir;
        let dir = tempdir().unwrap();
        let wd = dir.path().to_path_buf();
        let manager = SessionManager::new(&wd);

        // Simulate first turn: create + save with turn_stats
        let mut s = Session::new(wd.clone());
        s.id = SessionId::from_string("test-turn-accum".to_string());
        s.messages.push(Message::new(Role::User, "hello"));
        s.turn_stats.push(TurnStat {
            after_message: 1,
            duration_ms: 500,
            total_tokens: 100,
            tool_call_count: 0,
            turn_count: 1,
            errored: false,
        });
        s.touch();
        manager.save(&s).unwrap();

        // Simulate second turn: load existing, update, save — should keep first turn_stats
        let mut loaded = manager.load(&s.id).unwrap();
        assert_eq!(
            loaded.turn_stats.len(),
            1,
            "first turn_stats must survive load"
        );
        loaded
            .messages
            .push(Message::new(Role::Assistant, "hi back"));
        loaded.turn_stats.push(TurnStat {
            after_message: 2,
            duration_ms: 300,
            total_tokens: 50,
            tool_call_count: 1,
            turn_count: 2,
            errored: false,
        });
        loaded.touch();
        manager.save(&loaded).unwrap();

        // Reload: both turn_stats must still be there
        let reloaded = manager.load(&s.id).unwrap();
        assert_eq!(
            reloaded.turn_stats.len(),
            2,
            "accumulated turn_stats must survive save/load cycle"
        );
        assert_eq!(reloaded.turn_stats[0].after_message, 1);
        assert_eq!(reloaded.turn_stats[1].after_message, 2);
        assert_eq!(reloaded.messages.len(), 2);
    }

    #[test]
    fn session_save_overwrites_existing_json_and_remains_loadable() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let wd = dir.path().to_path_buf();
        let manager = SessionManager::new(&wd);

        let mut session = Session::new(wd.clone());
        session.id = SessionId::from_string("atomic-overwrite".to_string());
        session.messages.push(Message::new(Role::User, "first"));
        manager.save(&session).unwrap();

        session
            .messages
            .push(Message::new(Role::Assistant, "second"));
        session.touch();
        manager.save(&session).unwrap();

        let loaded = manager.load(&session.id).unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[0].text(), Some("first"));
        assert_eq!(loaded.messages[1].text(), Some("second"));
    }

    #[test]
    fn session_save_leaves_no_temporary_files_behind() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let wd = dir.path().to_path_buf();
        let manager = SessionManager::new(&wd);

        let mut session = Session::new(wd.clone());
        session.id = SessionId::from_string("atomic-no-tmp".to_string());
        manager.save(&session).unwrap();

        let entries: Vec<_> = std::fs::read_dir(manager.project_dir())
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "successful atomic save must leave only the session json"
        );
        assert_eq!(
            entries[0].path().extension().and_then(|ext| ext.to_str()),
            Some("json")
        );
    }

    #[test]
    fn test_session_new() {
        let session = Session::new(PathBuf::from("/tmp/test"));
        assert!(!session.id.0.is_empty());
        assert!(session.name.starts_with("session-"));
    }

    #[test]
    fn auto_name_uses_first_real_user_message() {
        let mut session = Session::new(PathBuf::from("/tmp/test"));
        session.messages.push(Message::new(
            Role::User,
            "[System meta · not a user message]\nignored",
        ));
        session.messages.push(Message::new(
            Role::User,
            "帮我修复 VS Code 会话标题自动命名的问题\n更多内容",
        ));

        session.auto_name_from_messages();

        assert_eq!(session.name, "帮我修复 VS Code 会话标题自动命名的问题");
    }

    #[test]
    fn auto_name_preserves_user_renamed_session() {
        let mut session = Session::new(PathBuf::from("/tmp/test"));
        session.rename("手动命名".to_string());
        session
            .messages
            .push(Message::new(Role::User, "新的用户消息"));

        session.auto_name_from_messages();

        assert_eq!(session.name, "手动命名");
    }

    #[test]
    fn rename_sets_user_renamed_flag() {
        let mut session = Session::new(PathBuf::from("/tmp/test"));
        assert!(
            !session.user_renamed,
            "fresh session must not be flagged as user-renamed"
        );
        session.rename("我的会话".to_string());
        assert!(
            session.user_renamed,
            "rename() must mark the session as user-renamed"
        );
    }

    #[test]
    fn auto_name_does_not_set_user_renamed_flag() {
        let mut session = Session::new(PathBuf::from("/tmp/test"));
        session
            .messages
            .push(Message::new(Role::User, "first message body"));
        session.auto_name_from_messages();
        assert_eq!(session.name, "first message body");
        assert!(
            !session.user_renamed,
            "auto_name_from_messages must NOT flag the session as user-renamed; only /rename should"
        );
    }

    #[test]
    fn test_hash_path_consistent() {
        let path = Path::new("/Users/test/project");
        let hash1 = hash_path(path);
        let hash2 = hash_path(path);
        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), 16);
    }

    #[test]
    fn test_hash_path_normalized() {
        // Same path with different representations should produce the same hash
        // Note: on non-Windows, case sensitivity is preserved

        // Test trailing slash normalization
        let path1 = Path::new("/Users/test/project");
        let path2 = Path::new("/Users/test/project/");
        assert_eq!(
            hash_path(path1),
            hash_path(path2),
            "Trailing slash should not affect hash"
        );

        // Test backslash normalization (Windows-style paths)
        let path3 = Path::new("C:\\Users\\test\\project");
        let path4 = Path::new("C:/Users/test/project");
        assert_eq!(
            hash_path(path3),
            hash_path(path4),
            "Backslashes should be normalized to forward slashes"
        );

        // Test combined: backslash + trailing slash
        let path5 = Path::new("C:\\Users\\test\\project\\");
        assert_eq!(
            hash_path(path4),
            hash_path(path5),
            "Backslashes and trailing slash should both be normalized"
        );
    }

    #[test]
    fn test_hash_path_strips_windows_verbatim_prefix() {
        // Regression: webui's /fs/list ran the chosen dir through
        // `canonicalize()`, which on Windows yields a `\\?\D:\path` verbatim
        // path. That round-tripped into the session cwd and hashed into a
        // different bucket than the TUI's plain `D:\path`, so the two ends
        // could never see each other's sessions. The verbatim and plain forms
        // must now hash identically.
        let verbatim = Path::new(r"\\?\D:\code\project");
        let plain = Path::new(r"D:\code\project");
        assert_eq!(
            hash_path(verbatim),
            hash_path(plain),
            r"`\\?\` extended-length prefix must not change the session hash"
        );

        // UNC verbatim form `\\?\UNC\server\share` collapses to `\\server\share`.
        let unc_verbatim = Path::new(r"\\?\UNC\server\share\proj");
        let unc_plain = Path::new(r"\\server\share\proj");
        assert_eq!(
            hash_path(unc_verbatim),
            hash_path(unc_plain),
            r"`\\?\UNC\` prefix must collapse to a plain UNC path before hashing"
        );
    }

    #[test]
    fn hash_path_matches_legacy_path_hash_on_unix() {
        // Regression guard: the pre-normalization implementation just did
        // `path.hash(&mut hasher)`. Every session saved before the
        // normalization pass lives in a bucket keyed by that hash. If
        // `hash_path` stops matching `Path::hash` for a plain-ASCII Unix
        // path with no trailing slash / backslashes, every legacy
        // `/resume` session becomes invisible. See the "where did my
        // /resume history go?" regression.
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let p = Path::new("/Users/theo/Documents/workspace/atomcode");
        let mut expected = DefaultHasher::new();
        p.hash(&mut expected);
        let legacy = format!("{:016x}", expected.finish());
        assert_eq!(hash_path(p), legacy);
    }

    // --- P0-4: Session persistence fault-tolerance tests ---

    #[test]
    fn concurrent_writes_from_same_manager_do_not_corrupt() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let wd = dir.path().to_path_buf();
        let manager = SessionManager::new(&wd);

        let mut session = Session::new(wd.clone());
        session.id = SessionId::from_string("concurrent-test".to_string());

        // Simulate rapid successive saves (as if multiple turns completed quickly)
        for i in 0..20 {
            session
                .messages
                .push(Message::new(Role::User, &format!("msg {}", i)));
            session.touch();
            manager.save(&session).unwrap();
        }

        // Verify final state is consistent
        let loaded = manager.load(&session.id).unwrap();
        assert_eq!(loaded.messages.len(), 20);
        assert_eq!(loaded.messages[19].text(), Some("msg 19"));
    }

    #[test]
    fn corrupted_session_file_returns_error_on_load() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let wd = dir.path().to_path_buf();
        let manager = SessionManager::new(&wd);

        let session = Session::new(wd.clone());
        manager.save(&session).unwrap();

        // Corrupt the file
        let path = manager.project_dir().join(format!("{}.json", session.id));
        std::fs::write(&path, "{{invalid json!!!").unwrap();

        // Load should fail, not panic
        let result = manager.load(&session.id);
        assert!(result.is_err());
    }

    #[test]
    fn partial_write_does_not_destroy_previous_valid_session() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let wd = dir.path().to_path_buf();
        let manager = SessionManager::new(&wd);

        // Save a valid session
        let mut session = Session::new(wd.clone());
        session.id = SessionId::from_string("partial-write-test".to_string());
        session
            .messages
            .push(Message::new(Role::User, "original content"));
        manager.save(&session).unwrap();

        // Simulate a failed write by corrupting the file manually
        let path = manager.project_dir().join(format!("{}.json", session.id));
        std::fs::write(&path, "{truncated").unwrap();

        // Load should fail for corrupted file
        assert!(manager.load(&session.id).is_err());

        // Restore valid content and verify load works
        manager.save(&session).unwrap();
        let loaded = manager.load(&session.id).unwrap();
        assert_eq!(loaded.messages[0].text(), Some("original content"));
    }

    #[test]
    fn session_delete_removes_file() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let wd = dir.path().to_path_buf();
        let manager = SessionManager::new(&wd);

        let session = Session::new(wd.clone());
        manager.save(&session).unwrap();
        assert!(manager.load(&session.id).is_ok());

        manager.delete(&session.id).unwrap();
        assert!(manager.load(&session.id).is_err());
    }

    #[test]
    fn session_list_returns_all_sessions_sorted_by_updated_at() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let wd = dir.path().to_path_buf();
        let manager = SessionManager::new(&wd);

        // Create 3 sessions with different timestamps
        for i in 0..3 {
            let mut session = Session::new(wd.clone());
            session.id = SessionId::from_string(format!("list-{}", i));
            // Stagger updated_at so sorting is deterministic
            session.updated_at = 1000 + i as u64;
            manager.save(&session).unwrap();
        }

        let sessions = manager.list().unwrap();
        assert_eq!(sessions.len(), 3);
        // Should be sorted descending by updated_at
        assert!(sessions[0].updated_at >= sessions[1].updated_at);
        assert!(sessions[1].updated_at >= sessions[2].updated_at);
    }

    #[test]
    fn session_list_returns_empty_for_new_project() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let wd = dir.path().to_path_buf();
        let manager = SessionManager::new(&wd);

        let sessions = manager.list().unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn latest_returns_most_recent_session() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let wd = dir.path().to_path_buf();
        let manager = SessionManager::new(&wd);

        // No sessions yet
        assert!(manager.latest().unwrap().is_none());

        // Create two sessions
        let mut s1 = Session::new(wd.clone());
        s1.id = SessionId::from_string("older".to_string());
        s1.updated_at = 1000;
        manager.save(&s1).unwrap();

        let mut s2 = Session::new(wd.clone());
        s2.id = SessionId::from_string("newer".to_string());
        s2.updated_at = 2000;
        manager.save(&s2).unwrap();

        let latest = manager.latest().unwrap().unwrap();
        assert_eq!(latest.id, s2.id);
    }

    #[test]
    fn has_sessions_returns_correctly() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let wd = dir.path().to_path_buf();
        let manager = SessionManager::new(&wd);

        assert!(!manager.has_sessions());

        let session = Session::new(wd.clone());
        manager.save(&session).unwrap();

        assert!(manager.has_sessions());
    }

    #[test]
    fn session_meta_from_session_preserves_fields() {
        let mut session = Session::new(PathBuf::from("/tmp/test"));
        session.name = "custom name".to_string();
        session.messages.push(Message::new(Role::User, "hello"));

        let meta = SessionMeta::from(&session);
        assert_eq!(meta.id, session.id);
        assert_eq!(meta.name, "custom name");
        assert_eq!(meta.message_count, 1);
        assert_eq!(meta.working_dir, PathBuf::from("/tmp/test"));
    }

    #[test]
    fn session_save_shorter_content_leaves_no_trailing_bytes_on_disk() {
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let wd = dir.path().to_path_buf();
        let manager = SessionManager::new(&wd);

        // 1. Create session with many long messages → large JSON
        let mut session = Session::new(wd.clone());
        session.id = SessionId::from_string("no-trailing-test".to_string());
        for i in 0..20 {
            session.messages.push(Message::new(
                Role::User,
                &format!("msg {}: {}", i, "x".repeat(200)),
            ));
        }
        manager.save(&session).unwrap();
        let path = manager.project_dir().join(format!("{}.json", session.id));
        let size_big = std::fs::metadata(&path).unwrap().len();
        assert!(
            size_big > 2000,
            "big session should be >2KB, got {}",
            size_big
        );

        // 2. Clear all messages → small JSON
        session.messages.clear();
        session.touch();
        manager.save(&session).unwrap();

        // 3. File must be much smaller now
        let size_small = std::fs::metadata(&path).unwrap().len();
        assert!(
            size_small < size_big / 2,
            "small session should be < half of big: {} vs {}",
            size_small,
            size_big
        );

        // 4. Raw bytes must be valid JSON with no trailing content
        let raw = std::fs::read(&path).unwrap();
        // Last meaningful byte must be '}' or whitespace before it
        let trimmed_end = raw
            .iter()
            .rposition(|&b| b != b'\n' && b != b' ')
            .unwrap_or(0);
        assert_eq!(
            raw[trimmed_end],
            b'}',
            "file must end with closing brace, got {:?}",
            &raw[trimmed_end.saturating_sub(5)..]
        );

        // 5. Deserialized content must match
        let loaded: Session = serde_json::from_slice(&raw).unwrap();
        assert!(
            loaded.messages.is_empty(),
            "loaded session must have 0 messages"
        );
    }
}
