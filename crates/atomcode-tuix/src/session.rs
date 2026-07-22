use std::path::PathBuf;

use atomcode_capabilities::session::{DisplayAnchor, PresentationRole};
use atomcode_core::conversation::message::{Message, Role};
use atomcode_core::conversation::ConversationSnapshot;

#[derive(Debug, Clone)]
pub struct DisplayMessage {
    pub after_message: usize,
    pub message: Message,
}

/// UI projection of one completed turn. Durable storage uses the native
/// session metadata type; this shape keeps renderer counters in `usize`.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnStat {
    pub after_message: usize,
    pub position_valid: bool,
    pub turn_count: usize,
    pub tool_call_count: usize,
    pub duration_ms: u64,
    pub total_tokens: usize,
    pub errored: bool,
    pub used_tokens: usize,
    pub ctx_window: usize,
}

impl From<atomcode_capabilities::session::TurnStat> for TurnStat {
    fn from(stat: atomcode_capabilities::session::TurnStat) -> Self {
        Self {
            after_message: stat.after_message,
            position_valid: stat.position_valid,
            turn_count: stat.round_count as usize,
            tool_call_count: stat.tool_call_count as usize,
            duration_ms: stat.duration_ms,
            total_tokens: stat.total_tokens as usize,
            errored: stat.errored,
            used_tokens: stat.used_tokens as usize,
            ctx_window: stat.ctx_window as usize,
        }
    }
}

pub type SessionId = String;
pub type Session = TuiSession;

#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub id: SessionId,
    pub name: String,
    /// Authoritative on-disk catalog location. Session ids are not sufficient:
    /// historical migrations may leave the same id in more than one project bucket.
    pub project_bucket: String,
    pub working_dir: PathBuf,
    pub created_at: i64,
    pub updated_at: i64,
    pub message_count: usize,
    pub file_size: u64,
}

impl From<atomcode_capabilities::session::CatalogEntry> for SessionMeta {
    fn from(entry: atomcode_capabilities::session::CatalogEntry) -> Self {
        Self {
            id: entry.id,
            name: entry.name,
            project_bucket: entry.project_bucket,
            working_dir: entry.working_dir,
            created_at: entry.created_at_ms,
            updated_at: entry.updated_at_ms,
            message_count: entry.message_count,
            file_size: 0,
        }
    }
}

/// TUI-local session state. Persistence belongs to the native session store;
/// core messages remain only because the current live/render transport uses them.
#[derive(Debug, Clone)]
pub struct TuiSession {
    pub id: String,
    pub name: String,
    pub working_dir: PathBuf,
    pub created_at: i64,
    pub updated_at: i64,
    pub messages: Vec<Message>,
    pub display_messages: Vec<DisplayMessage>,
    pub cold_summaries: Vec<String>,
    pub user_renamed: bool,
    pub ai_named: bool,
    pub turn_stats: Vec<TurnStat>,
}

impl TuiSession {
    pub fn new(working_dir: PathBuf) -> Self {
        let now = atomcode_capabilities::session::now_ms();
        let id = uuid::Uuid::new_v4().to_string();
        Self {
            name: format!("session-{now}"),
            id,
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

    pub fn default_session(working_dir: PathBuf) -> Self {
        let mut session = Self::new(working_dir);
        session.name = "default".into();
        session
    }

    pub fn from_catalog_view(
        view: atomcode_daemon::legacy_convert::CatalogSessionView,
    ) -> anyhow::Result<Self> {
        let snapshot = atomcode_daemon::legacy_convert::snapshot_to_core(&view.snapshot);
        let mut display_messages = Vec::with_capacity(view.presentation.entries.len());
        for entry in view.presentation.entries {
            let after_message = match entry.anchor {
                DisplayAnchor::AtStart => 0,
                DisplayAnchor::AfterTurn { turn_id } => view
                    .meta
                    .turn_stats
                    .iter()
                    .find(|stat| stat.position_valid && stat.turn_id == turn_id)
                    .map(|stat| stat.after_message)
                    .ok_or_else(|| {
                        anyhow::anyhow!("presentation references missing turn id {turn_id}")
                    })?,
            };
            let role = match entry.role {
                PresentationRole::User => Role::User,
                PresentationRole::Assistant => Role::Assistant,
            };
            display_messages.push(DisplayMessage {
                after_message,
                message: Message::new(role, entry.text),
            });
        }
        Ok(Self {
            id: view.meta.id,
            name: view.meta.name,
            working_dir: PathBuf::from(view.meta.working_dir),
            created_at: view.meta.created_at,
            updated_at: view.meta.updated_at,
            messages: snapshot.messages,
            display_messages,
            cold_summaries: snapshot.cold_summaries,
            user_renamed: view.meta.user_renamed,
            ai_named: view.meta.ai_named,
            turn_stats: view.meta.turn_stats.into_iter().map(Into::into).collect(),
        })
    }

    pub fn rename(&mut self, name: String) {
        self.name = name;
        self.user_renamed = true;
        self.touch();
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

    pub fn retain_turn_stats_after_undo(&mut self, message_count: usize) {
        self.turn_stats
            .retain(|stat| !stat.position_valid || stat.after_message <= message_count);
    }

    pub fn touch(&mut self) {
        self.updated_at = atomcode_capabilities::session::now_ms();
    }

    pub fn short_id(&self) -> &str {
        self.id.get(..8).unwrap_or(&self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_capabilities::session::{CatalogEntry, CatalogPresence};

    #[test]
    fn catalog_projection_preserves_exact_project_bucket() {
        let entry = CatalogEntry {
            id: "same-id".into(),
            name: "selected".into(),
            project_bucket: "0123456789abcdef".into(),
            working_dir: PathBuf::from("/project"),
            created_at_ms: 1,
            updated_at_ms: 2,
            message_count: 3,
            turn_count: 1,
            presence: CatalogPresence::NativeOnly,
        };

        let projected = SessionMeta::from(entry);

        assert_eq!(projected.project_bucket, "0123456789abcdef");
    }

    #[test]
    fn undo_stat_pruning_preserves_accounting_only_history() {
        let mut session = Session::new(PathBuf::from("/project"));
        let stat = |after_message, position_valid, total_tokens| TurnStat {
            after_message,
            position_valid,
            turn_count: 1,
            tool_call_count: 0,
            duration_ms: 1,
            total_tokens,
            errored: false,
            used_tokens: 1,
            ctx_window: 10,
        };
        session.turn_stats = vec![stat(99, false, 100), stat(2, true, 20), stat(4, true, 40)];

        session.retain_turn_stats_after_undo(2);

        assert_eq!(session.turn_stats.len(), 2);
        assert!(!session.turn_stats[0].position_valid);
        assert_eq!(session.turn_stats[0].total_tokens, 100);
        assert_eq!(session.turn_stats[1].after_message, 2);
    }
}
