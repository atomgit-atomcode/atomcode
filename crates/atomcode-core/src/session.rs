//! Session management for persistent conversation contexts.
//!
//! Each session represents an independent conversation with its own message history,
//! associated with a specific working directory.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use crate::conversation::message::Message;

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
        }
    }

    /// Update the session's name.
    pub fn rename(&mut self, name: String) {
        self.name = name;
        self.touch();
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
    /// Get the root directory for all sessions (~/.atomcode/sessions/).
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
            eprintln!("[session] Failed to create sessions dir: {}", e);
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
                            eprintln!("[session] Failed to create {:?}: {}", dst, e);
                            continue;
                        }
                        if let Ok(files) = std::fs::read_dir(&src) {
                            for file in files.flatten() {
                                let src_file = file.path();
                                let dst_file = dst.join(file.file_name());
                                if let Err(e) = std::fs::copy(&src_file, &dst_file) {
                                    eprintln!("[session] Failed to copy {:?}: {}", src_file, e);
                                } else {
                                    migrated += 1;
                                }
                            }
                        }
                    }
                }
                if migrated > 0 {
                    eprintln!(
                        "[session] Migrated {} session(s) from legacy location",
                        migrated
                    );
                }
            }
            Err(e) => {
                eprintln!("[session] Failed to read legacy sessions dir: {}", e);
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
        let json = serde_json::to_string_pretty(session)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        std::fs::write(path, json)
    }

    /// Load a session by ID.
    pub fn load(&self, id: &SessionId) -> std::io::Result<Session> {
        let path = self.project_dir().join(format!("{}.json", id));
        let json = std::fs::read_to_string(path)?;
        serde_json::from_str(&json).map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
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
    let normalized = path.to_string_lossy();
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
    fn test_session_new() {
        let session = Session::new(PathBuf::from("/tmp/test"));
        assert!(!session.id.0.is_empty());
        assert!(session.name.starts_with("session-"));
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
}
