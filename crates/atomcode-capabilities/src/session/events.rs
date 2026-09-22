//! Sessions whose content is their event log (`docs/adr/0024`).
//!
//! The store keeps everything it had — project buckets, the catalog, leases,
//! rename, delete, fork, the metadata index, ownership checks — and swaps what a
//! session *is* from a snapshot rewritten every turn to the harness's facts,
//! appended as they are committed. A snapshot is still what several readers
//! want, so reading one projects it from the log.
//!
//! Every file of an event session is named so that a build from before this
//! format finds nothing it recognises (`docs/adr/0024` §16): the log is
//! `<id>.events`, the metadata `<id>.index`, and the sidecars drop the `.json`
//! and `.jsonl` suffixes an older scan claims. Rolling back to such a build
//! loses sight of these sessions; it never reads half of one and writes a
//! diverging history over it.
//!
//! `<id>.events` is JSON lines: a `{"header": …}` line written when the session
//! is created, then one `{"seq", "at", "event"}` record per committed fact.
//! Streamed chunks are never written — the complete message follows them
//! (`docs/adr/0024` §7) — so the numbering on disk has gaps.

use std::fs;
use std::io::Write;
use std::path::PathBuf;

use atomcode_kernel::message::{Message, Role, SessionSnapshot};
use atomcode_kernel::session::{
    derive_messages_with_meta, InjectionOrigin, LoggedEvent, RewindScope, SeqNo, SessionEvent,
    SessionHeader, SESSION_FORMAT_VERSION,
};

use super::manager::{
    io_at, open_append_file, read_regular_file_bounded, retry_transient_file_access, SessionLease,
    SessionManager, SessionMeta, SessionResult, SessionStoreError, StorageOwner, MAX_JSONL_BYTES,
    MAX_JSONL_LINE_BYTES,
};
use super::transcript::TurnRecord;

/// What an event log line holds.
#[derive(serde::Deserialize)]
struct Line {
    #[serde(default)]
    header: Option<SessionHeader>,
    #[serde(default)]
    seq: Option<u64>,
    #[serde(default)]
    at: u64,
    #[serde(default)]
    event: Option<serde_json::Value>,
}

/// The id a session is stored under. A team member's session id is
/// `<lead>/<name>` (`docs/adr/0023` §5), and a path separator cannot be part of
/// a file name; `~` never appears in a minted id or a member name, so the
/// mapping cannot collide.
pub fn storage_id(session_id: &str) -> String {
    session_id.replace('/', "~")
}

impl SessionManager {
    /// Whether `id` is stored as an event log. The index is the commit point of
    /// such a session, so it is what decides.
    pub fn is_event_session(&self, id: &str) -> bool {
        self.index_path(id)
            .ok()
            .is_some_and(|path| fs::symlink_metadata(path).is_ok())
    }

    /// `<id>.events`.
    pub fn events_path(&self, id: &str) -> SessionResult<PathBuf> {
        self.path_for(id, "events")
    }

    /// `<id>.index`: an event session's metadata.
    pub fn index_path(&self, id: &str) -> SessionResult<PathBuf> {
        self.path_for(id, "index")
    }

    /// Create an event session under its lease: the log with its header first,
    /// then the index, which is when the catalog can see it. A session that
    /// already has any content is refused rather than overwritten.
    pub fn create_event_session(
        &self,
        lease: &SessionLease,
        header: &SessionHeader,
        meta: &SessionMeta,
    ) -> SessionResult<()> {
        self.validate_active_lease(lease)?;
        let id = lease.id();
        if storage_id(&header.id) != id || meta.id != id {
            return Err(SessionStoreError::Corrupt {
                kind: "session events",
                message: format!(
                    "creating {id:?} with header {:?} and meta {:?}",
                    header.id, meta.id
                ),
            });
        }
        if meta.owner != StorageOwner::Native {
            return Err(SessionStoreError::OwnershipConflict {
                id: id.to_string(),
                owner: meta.owner.clone(),
                operation: "create event session",
            });
        }
        let header_line = header_line(header)?;
        self.with_meta_lock(id, || {
            for existing in [
                self.index_path(id)?,
                self.events_path(id)?,
                self.path_for(id, "meta")?,
                self.path_for(id, "snapshot")?,
            ] {
                if fs::symlink_metadata(&existing).is_ok() {
                    return Err(SessionStoreError::Corrupt {
                        kind: "session events",
                        message: format!("{} already exists", existing.display()),
                    });
                }
            }
            super::manager::atomic_write(&self.events_path(id)?, &header_line)?;
            self.write_index_unlocked(meta)
        })
    }

    /// Append committed facts under the session's lease. Chunks are skipped.
    ///
    /// One write, under the file's exclusive lock, so records from two writers
    /// can never interleave; the lease is what keeps there being one writer.
    pub fn append_events(&self, lease: &SessionLease, events: &[LoggedEvent]) -> SessionResult<()> {
        self.validate_active_lease(lease)?;
        let id = lease.id();
        if !self.is_event_session(id) {
            return Err(SessionStoreError::NotFound {
                path: self.index_path(id)?,
            });
        }
        let mut buffer = Vec::new();
        for logged in events {
            if matches!(logged.event, SessionEvent::AssistantChunk { .. }) {
                continue;
            }
            buffer.extend_from_slice(&record_line(logged)?);
        }
        if buffer.is_empty() {
            return Ok(());
        }
        let path = self.events_path(id)?;
        let mut file = retry_transient_file_access(|| open_append_file(&path))?;
        retry_transient_file_access(|| {
            fs2::FileExt::lock_exclusive(&file).map_err(|e| io_at(&path, e))
        })?;
        let current = usize::try_from(file.metadata().map_err(|e| io_at(&path, e))?.len())
            .unwrap_or(usize::MAX);
        let next = current.saturating_add(buffer.len());
        if next > MAX_JSONL_BYTES {
            return Err(SessionStoreError::TooLarge {
                kind: "session events",
                limit: MAX_JSONL_BYTES,
                actual: next,
            });
        }
        file.write_all(&buffer).map_err(|e| io_at(&path, e))
    }

    /// The header an event session was created with.
    pub fn read_event_header(&self, id: &str) -> SessionResult<SessionHeader> {
        let (header, _) = self.read_event_log(id, true)?;
        header.ok_or_else(|| SessionStoreError::Corrupt {
            kind: "session events",
            message: format!("{id}: no header"),
        })
    }

    /// Every stored fact of an event session, in file order.
    pub fn load_events(&self, id: &str) -> SessionResult<Vec<LoggedEvent>> {
        Ok(self.read_event_log(id, false)?.1)
    }

    /// The conversation an event session's log projects to, as a snapshot.
    ///
    /// Messages only: the system prompt is assembled per request and was never
    /// a fact.
    pub fn project_snapshot(&self, id: &str) -> SessionResult<SessionSnapshot> {
        Ok(snapshot_of(&self.load_events(id)?))
    }

    fn read_event_log(
        &self,
        id: &str,
        header_only: bool,
    ) -> SessionResult<(Option<SessionHeader>, Vec<LoggedEvent>)> {
        let path = self.events_path(id)?;
        let bytes = read_regular_file_bounded(&path, "session events", MAX_JSONL_BYTES)?;
        let mut header = None;
        let mut events = Vec::new();
        for (index, raw) in bytes.split(|b| *b == b'\n').enumerate() {
            if raw.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let line: Line =
                serde_json::from_slice(raw).map_err(|error| SessionStoreError::Corrupt {
                    kind: "session events",
                    message: format!("{}:{}: {error}", path.display(), index + 1),
                })?;
            if let Some(found) = line.header {
                if found.version > SESSION_FORMAT_VERSION {
                    return Err(SessionStoreError::FutureSchema {
                        kind: "session events",
                        found: found.version,
                        supported: SESSION_FORMAT_VERSION,
                    });
                }
                header = Some(found);
                if header_only {
                    break;
                }
                continue;
            }
            match (line.seq, line.event) {
                (Some(seq), Some(event)) => events.push(LoggedEvent {
                    seq,
                    at: line.at,
                    event: event_of(event, &path, index, header.as_ref())?,
                }),
                _ => {
                    return Err(SessionStoreError::Corrupt {
                        kind: "session events",
                        message: format!("{}:{}: not a record", path.display(), index + 1),
                    })
                }
            }
        }
        Ok((header, events))
    }

    fn write_index_unlocked(&self, meta: &SessionMeta) -> SessionResult<()> {
        let mut meta = meta.clone();
        meta.format_version = meta.format_version.max(SESSION_FORMAT_VERSION);
        let meta = &meta;
        super::manager::validate_meta(meta)?;
        let bytes = super::manager::serialize_pretty_bounded(
            meta,
            "session index",
            super::manager::MAX_META_BYTES,
        )?;
        super::manager::atomic_write(&self.index_path(&meta.id)?, &bytes)
    }

    /// Fork an event session: the new one starts from every fact the source has,
    /// with the source as its parent and all of them counted as inherited.
    pub(crate) fn fork_event_session(
        &self,
        source_id: &str,
        destination: &SessionLease,
        meta: &SessionMeta,
        now_ms: i64,
    ) -> SessionResult<()> {
        let source_header = self.read_event_header(source_id)?;
        let events = self.load_events(source_id)?;
        let mut header = source_header;
        header.id = destination.id().to_string();
        header.parent = Some(source_id.to_string());
        header.inherited = events.len();
        header.created_at = u64::try_from(now_ms).unwrap_or(0);
        self.create_event_session(destination, &header, meta)?;
        self.append_events(destination, &events)
    }

    /// Turn timestamps of an event session: when each turn started and ended.
    pub(crate) fn event_turn_timestamps(
        &self,
        id: &str,
    ) -> SessionResult<std::collections::BTreeMap<u64, super::transcript::TurnTimestamp>> {
        let mut out: std::collections::BTreeMap<u64, super::transcript::TurnTimestamp> =
            std::collections::BTreeMap::new();
        for logged in self.load_events(id)? {
            let at = i64::try_from(logged.at).unwrap_or(i64::MAX);
            match logged.event {
                SessionEvent::TurnStart { turn } => {
                    out.entry(turn)
                        .or_insert(super::transcript::TurnTimestamp {
                            started_at: None,
                            completed_at: at,
                        })
                        .started_at = Some(at);
                }
                SessionEvent::TurnEnd { turn, .. } => {
                    out.entry(turn)
                        .or_insert(super::transcript::TurnTimestamp {
                            started_at: None,
                            completed_at: at,
                        })
                        .completed_at = at;
                }
                _ => {}
            }
        }
        Ok(out)
    }
}

impl SessionManager {
    /// How long the log is now: a point [`Self::truncate_events`] can take an
    /// append back to.
    pub fn events_mark(&self, id: &str) -> SessionResult<u64> {
        let path = self.events_path(id)?;
        Ok(fs::symlink_metadata(&path)
            .map_err(|error| io_at(&path, error))?
            .len())
    }

    /// Take back an append nobody has read yet: the log is cut to `mark`.
    ///
    /// The log is append-only for everyone who reads it. This exists for the one
    /// writer that appended a change and then failed to put the rest of it in
    /// place — a rebuilt agent that would not assemble after an undo — and is
    /// rolling its own transaction back under the lease, with no agent running.
    pub fn truncate_events(&self, lease: &SessionLease, mark: u64) -> SessionResult<()> {
        self.validate_active_lease(lease)?;
        let path = self.events_path(lease.id())?;
        let file = retry_transient_file_access(|| {
            fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .map_err(|error| io_at(&path, error))
        })?;
        fs2::FileExt::lock_exclusive(&file).map_err(|error| io_at(&path, error))?;
        let len = file.metadata().map_err(|error| io_at(&path, error))?.len();
        if mark > len {
            return Err(SessionStoreError::Corrupt {
                kind: "session events",
                message: format!(
                    "{}: cannot cut a {len}-byte log back to {mark} bytes",
                    path.display()
                ),
            });
        }
        file.set_len(mark).map_err(|error| io_at(&path, error))
    }

    /// What making this session's conversation `target` would append, stamped
    /// `at` — planned, not appended, so what is kept beside the log can be
    /// brought along before the one write that commits it.
    pub fn plan_conversation_change(
        &self,
        id: &str,
        target: &[Message],
        at: u64,
    ) -> SessionResult<ConversationChange> {
        let mark = self.events_mark(id)?;
        let mut after = self.load_events(id)?;
        let next = after.iter().map(|e| e.seq).max().unwrap_or(0) + 1;
        let change: Vec<LoggedEvent> = events_to_become(&after, target)
            .into_iter()
            .enumerate()
            .map(|(offset, event)| LoggedEvent {
                seq: next + offset as SeqNo,
                at,
                event,
            })
            .collect();
        after.extend(change.iter().cloned());
        Ok(ConversationChange {
            mark,
            change,
            after,
        })
    }

    /// Append what makes this session's conversation `target`, stamped `at`.
    /// Returns where the log stood before, for [`Self::truncate_events`].
    pub fn append_conversation_change(
        &self,
        lease: &SessionLease,
        target: &[Message],
        at: u64,
    ) -> SessionResult<u64> {
        let plan = self.plan_conversation_change(lease.id(), target, at)?;
        self.append_events(lease, &plan.change)?;
        Ok(plan.mark)
    }

    /// Open a session as an event log, converting it once if it is still a
    /// snapshot (`docs/adr/0024` §10). Returns whether it converted.
    ///
    /// The log, the sidecars and then the index are written — the index is the
    /// commit point — and only then are the files a build from before the
    /// format reads moved aside with a `.migrated` suffix, never deleted. A
    /// conversion that stopped half way is finished the next time: files left
    /// behind are moved aside again.
    pub fn open_as_events(&self, lease: &SessionLease) -> SessionResult<bool> {
        self.validate_active_lease(lease)?;
        let id = lease.id();
        self.refuse_newer(id)?;
        if self.is_event_session(id) {
            self.move_snapshot_files_aside(id)?;
            return Ok(false);
        }
        self.convert(lease, self.load_native_session(id)?)?;
        Ok(true)
    }

    /// [`Self::open_as_events`] for a resume: a prompt a snapshot-format
    /// session had accepted but not answered when its process died is carried
    /// across the conversion and handed back, the way a resume of one always
    /// has.
    pub fn open_for_resume(
        &self,
        lease: &SessionLease,
    ) -> SessionResult<(super::manager::LoadedSession, Option<Message>)> {
        self.validate_active_lease(lease)?;
        let id = lease.id();
        self.refuse_newer(id)?;
        if self.is_event_session(id) {
            self.move_snapshot_files_aside(id)?;
            return self.load_native_session_for_resume(lease);
        }
        let (loaded, pending) = self.load_native_session_for_resume(lease)?;
        self.convert(lease, loaded)?;
        Ok((self.load_native_session(id)?, pending))
    }

    fn convert(
        &self,
        lease: &SessionLease,
        loaded: super::manager::LoadedSession,
    ) -> SessionResult<()> {
        let id = lease.id();
        let times = self.load_transcript_timestamps(id).unwrap_or_default();
        let mut events = events_from_snapshot(&loaded.snapshot, 1);
        stamp_turn_times(&mut events, &times);

        let meta = loaded.meta;
        let mut header = SessionHeader::new(id);
        header.created_at = u64::try_from(meta.created_at).unwrap_or(0);
        header.cwd = Some(meta.working_dir.clone());
        header.parent = meta.fork_info.as_ref().map(|fork| fork.parent_id.clone());
        header.context = stored_prompt(&loaded.snapshot);

        let mut log = header_line(&header)?;
        for logged in &events {
            log.extend_from_slice(&record_line(logged)?);
        }
        if log.len() > MAX_JSONL_BYTES {
            return Err(SessionStoreError::TooLarge {
                kind: "session events",
                limit: MAX_JSONL_BYTES,
                actual: log.len(),
            });
        }
        self.with_meta_lock(id, || {
            super::manager::atomic_write(&self.events_path(id)?, &log)?;
            for (from, to) in SIDECARS {
                let source = self.path_for(id, from)?;
                if fs::symlink_metadata(&source).is_err() {
                    continue;
                }
                let bytes = read_regular_file_bounded(
                    &source,
                    "session sidecar",
                    super::manager::MAX_META_BYTES.max(MAX_JSONL_BYTES),
                )?;
                super::manager::atomic_write(&self.path_for(id, to)?, &bytes)?;
            }
            self.write_index_unlocked(&meta)
        })?;
        self.move_snapshot_files_aside(id)
    }

    /// A session a newer build last wrote is listed, never opened here: its log
    /// may hold facts this build would misread. A delegated agent's session is
    /// not opened on its own either.
    fn refuse_newer(&self, id: &str) -> SessionResult<()> {
        if !self.is_event_session(id) {
            return Ok(());
        }
        let meta = self.read_meta(id)?;
        if meta.needs_newer_version() {
            return Err(SessionStoreError::FutureSchema {
                kind: "session events",
                found: meta.format_version,
                supported: SESSION_FORMAT_VERSION,
            });
        }
        // Nor one kept under the session it was delegated from: that session's
        // resume is what brings it back (`docs/adr/0024` §11).
        if meta.parent.is_some() {
            return Err(SessionStoreError::InvalidId {
                id: id.to_string(),
                reason: "is kept under the session it was delegated from; open that one",
            });
        }
        Ok(())
    }

    /// Move what a build from before the event format reads out of its sight.
    fn move_snapshot_files_aside(&self, id: &str) -> SessionResult<()> {
        for extension in SNAPSHOT_FILES {
            let path = self.path_for(id, extension)?;
            if fs::symlink_metadata(&path).is_err() {
                continue;
            }
            let aside = self.path_for(id, &format!("{extension}.migrated"))?;
            fs::rename(&path, &aside).map_err(|error| io_at(&path, error))?;
        }
        Ok(())
    }
}

/// A change to a session's conversation, planned against its log.
#[derive(Clone, Debug)]
pub struct ConversationChange {
    /// Where the log stands before it: what [`SessionManager::truncate_events`]
    /// cuts back to.
    pub mark: u64,
    /// The facts to append.
    pub change: Vec<LoggedEvent>,
    /// The log as it will be once they are.
    pub after: Vec<LoggedEvent>,
}

impl ConversationChange {
    /// The turns the conversation will still show.
    pub fn visible_turns(&self) -> std::collections::BTreeSet<u64> {
        atomcode_kernel::session::visible_turns(&self.after)
    }
}

/// Snapshot-format sidecars and their names beside an event log.
const SIDECARS: [(&str, &str); 4] = [
    ("ui.json", "ui"),
    ("rewind.json", "rewind"),
    ("rewind.txn.json", "rewind.txn"),
    ("todos.json", "todos"),
];

/// Every file a snapshot-format session keeps that a build from before the
/// event format reads.
const SNAPSHOT_FILES: [&str; 8] = [
    "snapshot",
    "snapshot.inflight",
    "meta",
    "jsonl",
    "ui.json",
    "rewind.json",
    "rewind.txn.json",
    "todos.json",
];

/// A converted session's facts carry the times its transcript recorded: a
/// turn's facts when it started, its end when it completed.
fn stamp_turn_times(
    events: &mut [LoggedEvent],
    times: &std::collections::BTreeMap<u64, super::transcript::TurnTimestamp>,
) {
    for logged in events {
        let Some(time) = times.get(&logged.event.turn()) else {
            continue;
        };
        let at = match logged.event {
            SessionEvent::TurnEnd { .. } => time.completed_at,
            _ => time.started_at.unwrap_or(time.completed_at),
        };
        logged.at = u64::try_from(at).unwrap_or(0);
    }
}

/// The prompt a snapshot-format session was stored with: its leading system
/// messages that no compaction wrote.
pub fn stored_prompt(snapshot: &SessionSnapshot) -> Option<String> {
    let prompt = snapshot
        .messages
        .iter()
        .take_while(|m| m.role == Role::System && !m.synthetic)
        .map(|m| m.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    (!prompt.is_empty()).then_some(prompt)
}

/// A conversation as a log projects it: without the system prompt, which is
/// assembled for each request and was never a fact.
pub fn without_system_prompt(messages: &[Message]) -> Vec<Message> {
    messages
        .iter()
        .filter(|m| m.role != Role::System || m.synthetic)
        .cloned()
        .collect()
}

/// Whether two conversations are the same one, stats aside: what a log
/// projects against what a snapshot holds.
pub fn same_conversation(a: &[Message], b: &[Message]) -> bool {
    let strip = |messages: &[Message]| -> Vec<Message> {
        without_system_prompt(messages)
            .into_iter()
            .map(|mut m| {
                m.meta = None;
                m
            })
            .collect()
    };
    strip(a) == strip(b)
}

/// The facts that, appended to `events`, make its projection `target`.
///
/// Nothing when it already is. When `target` is what the log held before one
/// of its turns, a [`SessionEvent::Rewound`] to that turn — an undo, a rewind
/// of the conversation, a restore to an earlier point. When it is a summary
/// followed by the log from one of its turns on, a [`SessionEvent::Compacted`]
/// through the turn before. Otherwise the whole conversation is taken back and
/// `target` is committed after it, so a restore to a conversation this log
/// never held is still a fact, not a rewrite. Either way a memory or a
/// compaction summary already injected stays, as it does for every undo.
pub fn events_to_become(events: &[LoggedEvent], target: &[Message]) -> Vec<SessionEvent> {
    if same_conversation(&derive_messages_with_meta(events), target) {
        return Vec::new();
    }
    let turn = events.iter().map(|e| e.event.turn()).max().unwrap_or(0);
    let next = events.iter().map(|e| e.seq).max().unwrap_or(0) + 1;
    let rewound = |to: SeqNo| SessionEvent::Rewound {
        turn,
        to,
        scope: RewindScope::Conversation,
    };
    let starts: Vec<SeqNo> = events
        .iter()
        .filter_map(|logged| {
            matches!(logged.event, SessionEvent::TurnStart { .. }).then_some(logged.seq)
        })
        .collect();
    let mut probe = events.to_vec();
    for to in starts.iter().rev() {
        probe.push(LoggedEvent {
            seq: next,
            at: 0,
            event: rewound(*to),
        });
        if same_conversation(&derive_messages_with_meta(&probe), target) {
            return vec![rewound(*to)];
        }
        probe.pop();
    }
    if let Some(summary) = target
        .first()
        .filter(|m| m.role == Role::System && m.synthetic)
    {
        for to in starts.iter().rev() {
            let compacted = SessionEvent::Compacted {
                turn,
                through: to.saturating_sub(1),
                summary: summary.text.clone(),
                from: 0,
            };
            probe.push(LoggedEvent {
                seq: next,
                at: 0,
                event: compacted.clone(),
            });
            if same_conversation(&derive_messages_with_meta(&probe), target) {
                return vec![compacted];
            }
            probe.pop();
        }
    }
    let from = starts
        .first()
        .copied()
        .or_else(|| events.first().map(|e| e.seq))
        .unwrap_or(next);
    let mut change = vec![rewound(from)];
    let reseeded = events_from_snapshot(&SessionSnapshot::new(target.to_vec()), 1);
    let offset = reseeded
        .iter()
        .map(|e| e.event.turn())
        .filter(|t| *t > 0)
        .min()
        .map(|first| turn.saturating_add(1).saturating_sub(first))
        .unwrap_or(0);
    change.extend(reseeded.into_iter().map(|logged| {
        let mut event = logged.event;
        shift_turn(&mut event, offset);
        event
    }));
    change
}

/// Move a reseeded fact's turn past the turns the log already used.
fn shift_turn(event: &mut SessionEvent, offset: u64) {
    if offset == 0 {
        return;
    }
    match event {
        SessionEvent::TurnStart { turn }
        | SessionEvent::UserMessage { turn, .. }
        | SessionEvent::AssistantMessage { turn, .. }
        | SessionEvent::ToolResultLogged { turn, .. }
        | SessionEvent::Injected { turn, .. }
        | SessionEvent::TurnEnd { turn, .. } => *turn += offset,
        _ => {}
    }
}

/// The facts a stored conversation is made of, numbered from `first_seq`.
///
/// The one crossing from a snapshot into a log: converting a session stored as
/// a snapshot, restoring one the log never held, and a runtime with no store
/// continuing from what it kept in memory. Lossy by construction — a snapshot
/// holds messages, not what happened — so it is never on the resume path of a
/// session that has a log.
///
/// Non-synthetic system messages are left out: they are regenerated for every
/// request. A synthetic user message cannot say which kind of injection it was,
/// so it comes back as a continuation — the same message to the model, one
/// provenance fewer.
///
/// Turn numbers come from the stored ids where the messages carry them, and are
/// counted otherwise; the last fact's turn is at least the snapshot's own
/// `turn_counter`, so the next turn continues the session's sequence instead of
/// reusing an id a rewind point or a turn stat was filed under.
pub fn events_from_snapshot(snapshot: &SessionSnapshot, first_seq: SeqNo) -> Vec<LoggedEvent> {
    let turns = turn_of_each_prompt(&snapshot.messages);
    let mut events: Vec<SessionEvent> = Vec::new();
    let mut turn = 0u64;
    let mut round = 0u32;
    let mut prompt = 0usize;

    for message in &snapshot.messages {
        match message.role {
            Role::System => {
                if message.synthetic {
                    events.push(SessionEvent::Injected {
                        turn,
                        text: message.text.clone(),
                        origin: InjectionOrigin::CompactionSummary,
                    });
                }
            }
            Role::User if !message.synthetic => {
                turn = turns[prompt];
                prompt += 1;
                round = 0;
                events.push(SessionEvent::TurnStart { turn });
                events.push(SessionEvent::UserMessage {
                    turn,
                    text: message.text.clone(),
                    images: message.images.clone(),
                });
            }
            Role::User => {
                // An empty synthetic user message carrying images right after a
                // tool result is how a tool's picture reaches a vision model:
                // it belongs to that result, not to the conversation.
                let carrier = message.text.is_empty() && !message.images.is_empty();
                if carrier {
                    if let Some(SessionEvent::ToolResultLogged { images, .. }) = events.last_mut() {
                        if images.is_empty() {
                            images.clone_from(&message.images);
                            continue;
                        }
                    }
                }
                events.push(SessionEvent::Injected {
                    turn,
                    text: message.text.clone(),
                    origin: InjectionOrigin::Continuation,
                });
            }
            Role::Assistant => {
                round = message
                    .meta
                    .as_ref()
                    .map(|meta| meta.round)
                    .filter(|r| *r > round)
                    .unwrap_or(round + 1);
                events.push(SessionEvent::AssistantMessage {
                    turn,
                    round,
                    text: message.text.clone(),
                    reasoning: message.reasoning.clone().unwrap_or_default(),
                    tool_calls: message.tool_calls.clone(),
                    reasoning_blocks: message.reasoning_blocks.clone(),
                    meta: message.meta.clone(),
                });
            }
            Role::Tool => {
                events.push(SessionEvent::ToolResultLogged {
                    turn,
                    round,
                    call_id: message.tool_call_id.clone().unwrap_or_default(),
                    content: message.text.clone(),
                    is_error: message.is_error,
                    images: message.images.clone(),
                });
            }
        }
    }

    // A turn that stored nothing (it failed before any message was kept) still
    // consumed its id. Say so with a boundary, which is not model-visible.
    if snapshot.turn_counter > turn {
        events.push(SessionEvent::TurnEnd {
            turn: snapshot.turn_counter,
            stop: atomcode_kernel::event::StopReason::Stopped,
            error: None,
        });
    }

    events
        .into_iter()
        .enumerate()
        .map(|(offset, event)| LoggedEvent {
            seq: first_seq + offset as SeqNo,
            at: 0,
            event,
        })
        .collect()
}

/// The turn id each real user prompt opened, in order.
fn turn_of_each_prompt(messages: &[Message]) -> Vec<u64> {
    let mut turns = Vec::new();
    let mut last = 0u64;
    for (index, message) in messages.iter().enumerate() {
        if message.role != Role::User || message.synthetic {
            continue;
        }
        let stored = messages[index + 1..]
            .iter()
            .take_while(|m| m.role != Role::User || m.synthetic)
            .filter_map(|m| m.meta.as_ref())
            .map(|meta| meta.turn_id)
            .find(|id| *id > 0);
        let turn = stored.filter(|id| *id > last).unwrap_or(last + 1);
        turns.push(turn);
        last = turn;
    }
    turns
}

/// A record's fact. A kind this build does not know was added by a newer one
/// — a later build appends kinds to a file an earlier one created, so the
/// header's version alone cannot say — and the file is refused as newer, not
/// read as corrupt.
fn event_of(
    value: serde_json::Value,
    path: &std::path::Path,
    index: usize,
    header: Option<&SessionHeader>,
) -> SessionResult<SessionEvent> {
    let kind = value
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    serde_json::from_value(value).map_err(|error| {
        if error.to_string().starts_with("unknown variant") {
            SessionStoreError::FutureSchema {
                kind: "session events",
                found: header
                    .map(|h| h.version)
                    .unwrap_or(SESSION_FORMAT_VERSION)
                    .max(SESSION_FORMAT_VERSION)
                    + 1,
                supported: SESSION_FORMAT_VERSION,
            }
        } else {
            SessionStoreError::Corrupt {
                kind: "session events",
                message: format!(
                    "{}:{}: {} {error}",
                    path.display(),
                    index + 1,
                    kind.unwrap_or_default()
                ),
            }
        }
    })
}

/// One record line of the log, newline included.
fn record_line(logged: &LoggedEvent) -> SessionResult<Vec<u8>> {
    let mut line = serde_json::to_vec(&serde_json::json!({
        "seq": logged.seq,
        "at": logged.at,
        "event": logged.event,
    }))
    .map_err(|error| SessionStoreError::Corrupt {
        kind: "session event",
        message: error.to_string(),
    })?;
    if line.len() + 1 > MAX_JSONL_LINE_BYTES {
        return Err(SessionStoreError::TooLarge {
            kind: "session event",
            limit: MAX_JSONL_LINE_BYTES,
            actual: line.len() + 1,
        });
    }
    line.push(b'\n');
    Ok(line)
}

/// Each finished turn of a session's log as the record `recall` and `/worklog`
/// read — the shape the transcript hook used to append, now folded from the
/// facts (`docs/adr/0024` §14).
///
/// Started when its `TurnStart` was committed and finished when its `TurnEnd`
/// was; the prompt, every reply's text and reasoning (a stopped reply's too),
/// each call paired with its result, the turn's usage. A turn taken back is kept
/// and marked `undone`, which the transcript never could. A turn with neither
/// words nor calls — a refused prompt — is left out, as it always was.
pub fn turn_records(session_id: &str, events: &[LoggedEvent]) -> Vec<TurnRecord> {
    use super::transcript::{ToolRecord, UsageRecord, RECORD_VERSION};

    let undone = atomcode_kernel::session::undone_turns(events);
    let results: std::collections::HashMap<&str, (&str, bool)> = events
        .iter()
        .filter_map(|logged| match &logged.event {
            SessionEvent::ToolResultLogged {
                call_id,
                content,
                is_error,
                ..
            } => Some((call_id.as_str(), (content.as_str(), *is_error))),
            _ => None,
        })
        .collect();
    let mut records = Vec::new();
    let mut open: Option<TurnRecord> = None;
    for logged in events {
        let at = i64::try_from(logged.at).unwrap_or(i64::MAX);
        match &logged.event {
            SessionEvent::TurnStart { turn } => {
                open = Some(TurnRecord {
                    v: RECORD_VERSION,
                    started_at: Some(at),
                    ts: at,
                    iso: String::new(),
                    session_id: session_id.to_string(),
                    turn_id: *turn,
                    undone: undone.contains(turn),
                    user: String::new(),
                    assistant: String::new(),
                    reasoning: String::new(),
                    tools: Vec::new(),
                    usage: UsageRecord::default(),
                });
            }
            SessionEvent::UserMessage { text, .. } => {
                if let Some(record) = open.as_mut().filter(|r| r.user.is_empty()) {
                    record.user = text.clone();
                }
            }
            SessionEvent::AssistantMessage {
                text,
                reasoning,
                tool_calls,
                meta,
                ..
            } => {
                let Some(record) = open.as_mut() else {
                    continue;
                };
                record.assistant.push_str(text);
                record.reasoning.push_str(reasoning);
                for call in tool_calls {
                    let (result, is_error) = results
                        .get(call.id.as_str())
                        .map(|(content, is_error)| (content.to_string(), *is_error))
                        .unwrap_or_default();
                    record.tools.push(ToolRecord {
                        name: call.name.clone(),
                        args: call.arguments.clone(),
                        result,
                        is_error,
                    });
                }
                if let Some(meta) = meta {
                    // The last round's prompt is how far the turn's context
                    // reached; output adds up across rounds.
                    record.usage.prompt = meta.tokens.prompt;
                    record.usage.completion = record
                        .usage
                        .completion
                        .saturating_add(meta.tokens.completion);
                    record.usage.cached = record.usage.cached.max(meta.tokens.cached);
                }
            }
            SessionEvent::PartialReply {
                text, reasoning, ..
            } => {
                if let Some(record) = open.as_mut() {
                    record.assistant.push_str(text);
                    record.reasoning.push_str(reasoning);
                }
            }
            SessionEvent::TurnEnd { turn, .. } => {
                let Some(mut record) = open.take().filter(|r| r.turn_id == *turn) else {
                    continue;
                };
                if record.assistant.is_empty() && record.tools.is_empty() {
                    continue;
                }
                record.ts = at;
                record.iso = chrono::DateTime::from_timestamp_millis(at)
                    .map(|d| d.to_rfc3339())
                    .unwrap_or_default();
                records.push(record);
            }
            _ => {}
        }
    }
    records
}

/// A snapshot of what `events` project to.
pub fn snapshot_of(events: &[LoggedEvent]) -> SessionSnapshot {
    let mut snapshot = SessionSnapshot::new(derive_messages_with_meta(events));
    let turns = events.iter().map(|e| e.event.turn()).max().unwrap_or(0);
    snapshot.turn_counter = snapshot.turn_counter.max(turns);
    snapshot
}

fn header_line(header: &SessionHeader) -> SessionResult<Vec<u8>> {
    let mut line =
        serde_json::to_vec(&serde_json::json!({ "header": header })).map_err(|error| {
            SessionStoreError::Corrupt {
                kind: "session header",
                message: error.to_string(),
            }
        })?;
    line.push(b'\n');
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::manager::StorageOwner;
    use atomcode_kernel::event::StopReason;
    use atomcode_kernel::message::Message;

    const BUCKET: &str = "0123456789abcdef";

    fn store() -> (tempfile::TempDir, SessionManager) {
        let dir = tempfile::tempdir().unwrap();
        let manager = SessionManager::with_root(dir.path().join(BUCKET));
        (dir, manager)
    }

    fn meta(id: &str) -> SessionMeta {
        let mut meta = SessionMeta::new(id, "/work", 1_000);
        meta.owner = StorageOwner::Native;
        meta
    }

    fn created(manager: &SessionManager, id: &str) -> SessionLease {
        let lease = manager.acquire_lease(id).unwrap();
        manager
            .create_event_session(&lease, &SessionHeader::new(id), &meta(id))
            .unwrap();
        lease
    }

    fn logged(seq: u64, at: u64, event: SessionEvent) -> LoggedEvent {
        LoggedEvent { seq, at, event }
    }

    /// A delegated agent's session is kept under its parent (`docs/adr/0024`
    /// §11): stored under an id a file can have, found through its parent,
    /// never listed, searched or opened on its own, and deleted with it.
    #[test]
    fn a_delegated_session_is_kept_under_its_parent() {
        let (dir, manager) = store();
        let lead = created(&manager, "lead");
        let said = |text: &str| {
            a_turn()
                .into_iter()
                .map(|mut logged| {
                    if let SessionEvent::UserMessage { text: said, .. } = &mut logged.event {
                        *said = text.into();
                    }
                    logged
                })
                .collect::<Vec<_>>()
        };
        manager
            .append_events(&lead, &said("the lead asked about kiwi"))
            .unwrap();

        let child = storage_id("lead/scout");
        assert_eq!(child, "lead~scout");
        let child_lease = manager.acquire_lease(&child).unwrap();
        let mut header = SessionHeader::new("lead/scout");
        header.parent = Some("lead".into());
        let mut child_meta = meta(&child);
        child_meta.parent = Some("lead".into());
        manager
            .create_event_session(&child_lease, &header, &child_meta)
            .unwrap();
        manager
            .append_events(&child_lease, &said("the scout looked for kiwi"))
            .unwrap();
        assert_eq!(manager.read_event_header(&child).unwrap().id, "lead/scout");

        let ids = |metas: Vec<SessionMeta>| metas.into_iter().map(|m| m.id).collect::<Vec<_>>();
        assert_eq!(ids(manager.list()), ["lead"]);
        assert_eq!(ids(manager.children("lead")), ["lead~scout"]);
        assert_eq!(
            SessionManager::scan_catalog(dir.path())
                .entries
                .into_iter()
                .map(|e| e.id)
                .collect::<Vec<_>>(),
            ["lead"],
            "no catalog offers it"
        );
        let found = crate::session::RecallTool::new()
            .search_dir(manager.root(), "kiwi", None, None, 8)
            .unwrap();
        assert!(
            found.contains("the lead asked") && !found.contains("the scout looked"),
            "{found}"
        );
        assert!(
            matches!(
                manager.open_for_resume(&child_lease),
                Err(SessionStoreError::InvalidId { .. })
            ),
            "it is not opened on its own"
        );

        drop(child_lease);
        manager.delete(&lead).unwrap();
        assert!(!manager.is_event_session(&child), "it went with its parent");
    }

    fn a_turn() -> Vec<LoggedEvent> {
        vec![
            logged(1, 10, SessionEvent::TurnStart { turn: 1 }),
            logged(
                2,
                11,
                SessionEvent::UserMessage {
                    turn: 1,
                    text: "hello".into(),
                    images: vec![],
                },
            ),
            logged(
                3,
                12,
                SessionEvent::AssistantChunk {
                    turn: 1,
                    round: 1,
                    delta: "hi".into(),
                    reasoning: false,
                },
            ),
            logged(
                4,
                13,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    round: 1,
                    text: "hi there".into(),
                    reasoning: String::new(),
                    tool_calls: vec![],
                    reasoning_blocks: Vec::new(),
                    meta: None,
                },
            ),
            logged(
                5,
                14,
                SessionEvent::TurnEnd {
                    turn: 1,
                    stop: StopReason::Stopped,
                    error: None,
                },
            ),
        ]
    }

    /// An event session keeps its facts — not the chunks — with their numbers
    /// and times; a snapshot read of it is the projection of those facts; its
    /// turn times come from them; and the catalog lists it.
    #[test]
    fn an_event_session_is_created_appended_and_read_back() {
        let (dir, manager) = store();
        let lease = created(&manager, "s1");
        manager.append_events(&lease, &a_turn()).unwrap();

        let stored = manager.load_events("s1").unwrap();
        assert_eq!(
            stored.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2, 4, 5],
            "chunks are not written"
        );
        assert_eq!(
            stored.iter().map(|e| e.at).collect::<Vec<_>>(),
            vec![10, 11, 13, 14]
        );
        assert_eq!(manager.read_event_header("s1").unwrap().id, "s1");

        let loaded = manager.load_native_session("s1").unwrap();
        assert_eq!(
            loaded.snapshot.messages,
            derive_messages_with_meta(&stored),
            "a snapshot of an event session is its projection"
        );
        assert_eq!(loaded.meta.id, "s1");

        let times = manager.load_transcript_timestamps("s1").unwrap();
        assert_eq!(times[&1].started_at, Some(10));
        assert_eq!(times[&1].completed_at, 14);

        let scan = SessionManager::scan_catalog(dir.path());
        assert!(
            scan.entries.iter().any(|entry| entry.id == "s1"),
            "{scan:#?}"
        );
        assert!(scan.diagnostics.is_empty(), "{:#?}", scan.diagnostics);
    }

    /// Nothing an event session writes has a name a build from before the
    /// format scans for (`docs/adr/0024` §16): rolling back cannot pick up half
    /// of it.
    #[test]
    fn an_event_session_leaves_nothing_an_older_version_recognises() {
        let (_dir, manager) = store();
        let lease = created(&manager, "s1");
        manager.append_events(&lease, &a_turn()).unwrap();
        manager.rename("s1", "renamed").unwrap();
        manager.write_todo_sidecar("s1", &[], 2).unwrap();
        manager
            .commit_native_runtime_mutation(
                &lease,
                &SessionSnapshot::new(Vec::new()),
                |_, meta, _| {
                    meta.turn_count += 1;
                    Ok(())
                },
            )
            .unwrap();

        let names: Vec<String> = fs::read_dir(manager.root())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        for name in &names {
            for older in [".snapshot", ".meta", ".jsonl", ".json"] {
                assert!(
                    !name.ends_with(older),
                    "`{name}` is a file an older build would read: {names:?}"
                );
            }
        }
        assert!(names.iter().any(|n| n == "s1.events") && names.iter().any(|n| n == "s1.index"));
    }

    /// The log is the content: a runtime mutation writes only what sits beside
    /// it, whatever snapshot it is handed.
    #[test]
    fn a_runtime_mutation_leaves_the_log_alone() {
        let (_dir, manager) = store();
        let lease = created(&manager, "s1");
        manager.append_events(&lease, &a_turn()).unwrap();
        let before = fs::read(manager.events_path("s1").unwrap()).unwrap();

        manager
            .commit_native_runtime_mutation(
                &lease,
                &SessionSnapshot::new(Vec::new()),
                |current, meta, _| {
                    assert_eq!(current.messages.len(), 2, "handed the projection");
                    meta.turn_count = 7;
                    Ok(())
                },
            )
            .unwrap();

        assert_eq!(
            fs::read(manager.events_path("s1").unwrap()).unwrap(),
            before
        );
        assert_eq!(manager.read_meta("s1").unwrap().turn_count, 7);
        assert_eq!(manager.load_snapshot("s1").unwrap().messages.len(), 2);
    }

    /// `recall` and `/worklog` read a session's turns out of its log: each
    /// finished turn with its prompt, what was said (a stopped reply's words
    /// too), each call beside its result and when it ran. A turn taken back is
    /// kept and marked, and the day's recap leaves it out.
    #[test]
    fn a_sessions_turns_are_read_out_of_its_log() {
        use atomcode_kernel::tool::ToolCall;

        let (dir, manager) = store();
        let lease = created(&manager, "s1");
        let call = ToolCall {
            id: "c1".into(),
            name: "read_file".into(),
            arguments: "{}".into(),
        };
        let mut facts = vec![
            logged(1, 1_000, SessionEvent::TurnStart { turn: 1 }),
            logged(
                2,
                1_000,
                SessionEvent::UserMessage {
                    turn: 1,
                    text: "remember kiwi".into(),
                    images: vec![],
                },
            ),
            logged(
                3,
                1_100,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    round: 1,
                    text: "reading".into(),
                    reasoning: String::new(),
                    tool_calls: vec![call],
                    reasoning_blocks: Vec::new(),
                    meta: None,
                },
            ),
            logged(
                4,
                1_200,
                SessionEvent::ToolResultLogged {
                    turn: 1,
                    round: 1,
                    call_id: "c1".into(),
                    content: "kiwi.txt".into(),
                    is_error: false,
                    images: vec![],
                },
            ),
            logged(
                5,
                1_500,
                SessionEvent::TurnEnd {
                    turn: 1,
                    stop: StopReason::Stopped,
                    error: None,
                },
            ),
        ];
        // Turn 2, taken back.
        facts.extend(a_turn().into_iter().map(|mut fact| {
            fact.seq += 10;
            fact.at += 2_000;
            shift_turn(&mut fact.event, 1);
            fact
        }));
        facts.push(logged(
            20,
            2_600,
            SessionEvent::Rewound {
                turn: 2,
                to: 11,
                scope: RewindScope::Conversation,
            },
        ));
        // Turn 3, stopped part way and kept.
        facts.extend([
            logged(21, 3_000, SessionEvent::TurnStart { turn: 3 }),
            logged(
                22,
                3_000,
                SessionEvent::UserMessage {
                    turn: 3,
                    text: "and then?".into(),
                    images: vec![],
                },
            ),
            logged(
                23,
                3_100,
                SessionEvent::PartialReply {
                    turn: 3,
                    round: 1,
                    text: "I was saying".into(),
                    reasoning: String::new(),
                },
            ),
            logged(
                24,
                3_100,
                SessionEvent::Interrupted {
                    turn: 3,
                    undone: false,
                },
            ),
            logged(
                25,
                3_200,
                SessionEvent::TurnEnd {
                    turn: 3,
                    stop: StopReason::Cancelled,
                    error: None,
                },
            ),
        ]);
        manager.append_events(&lease, &facts).unwrap();

        let records = turn_records("s1", &manager.load_events("s1").unwrap());
        assert_eq!(
            records
                .iter()
                .map(|r| (r.turn_id, r.undone))
                .collect::<Vec<_>>(),
            vec![(1, false), (2, true), (3, false)]
        );
        assert_eq!(records[0].user, "remember kiwi");
        assert_eq!((records[0].started_at, records[0].ts), (Some(1_000), 1_500));
        assert_eq!(records[0].tools[0].result, "kiwi.txt");
        assert_eq!(records[2].assistant, "I was saying");

        let recalled = crate::session::RecallTool::new()
            .search_dir(manager.root(), "kiwi", None, None, 8)
            .unwrap();
        assert!(recalled.contains("remember kiwi"), "{recalled}");

        let day = crate::session::collect_day_turns(dir.path(), 0, 10_000);
        assert_eq!(
            day.iter().map(|t| t.user.as_str()).collect::<Vec<_>>(),
            vec!["remember kiwi", "and then?"]
        );
    }

    /// A session a newer build last wrote is listed and marked, and opening it is
    /// refused before its log is read; the rest of the project is untouched.
    #[test]
    fn a_session_a_newer_build_wrote_is_listed_but_not_opened() {
        let (dir, manager) = store();
        let newer = created(&manager, "newer");
        let current = created(&manager, "current");
        manager
            .update_meta("newer", |meta| {
                meta.format_version = SESSION_FORMAT_VERSION + 1
            })
            .unwrap();

        let scan = SessionManager::scan_catalog(dir.path());
        let marked = |id: &str| {
            scan.entries
                .iter()
                .find(|entry| entry.id == id)
                .unwrap_or_else(|| panic!("{id} listed: {scan:#?}"))
                .needs_newer_version
        };
        assert!(marked("newer"));
        assert!(!marked("current"));

        assert!(matches!(
            manager.open_for_resume(&newer),
            Err(SessionStoreError::FutureSchema { .. })
        ));
        manager.open_for_resume(&current).unwrap();

        // What marks a session is this build writing it: at creation, and at
        // every runtime write after one an older build made.
        assert_eq!(
            manager.read_meta("current").unwrap().format_version,
            SESSION_FORMAT_VERSION
        );
        manager
            .update_meta("current", |meta| meta.format_version = 1)
            .unwrap();
        manager
            .commit_native_runtime_mutation(
                &current,
                &SessionSnapshot::new(Vec::new()),
                |_, _, _| Ok(()),
            )
            .unwrap();
        assert_eq!(
            manager.read_meta("current").unwrap().format_version,
            SESSION_FORMAT_VERSION
        );
    }

    /// A journal an earlier build of this line kept under `sessions/harness/`
    /// is never read (`docs/adr/0024` §15): the catalog and recall come out the
    /// same with it there as without it.
    #[test]
    fn an_old_harness_journal_is_not_read() {
        let (dir, manager) = store();
        let lease = created(&manager, "s1");
        manager.append_events(&lease, &a_turn()).unwrap();
        let listed = |dir: &std::path::Path| {
            let scan = super::super::manager::scan_catalog_root(dir);
            (scan.entries, scan.diagnostics)
        };
        let recalled = || {
            crate::session::RecallTool::new()
                .search_dir(manager.root(), "hello", None, None, 8)
                .unwrap()
        };
        let (before_list, before_recall) = (listed(dir.path()), recalled());

        let journals = dir.path().join(SessionManager::JOURNAL_DIR).join(BUCKET);
        fs::create_dir_all(&journals).unwrap();
        fs::write(
            journals.join("1700000000000-9.jsonl"),
            "{\"header\":{\"version\":1,\"id\":\"1700000000000-9\",\"created_at\":1,\"inherited\":0}}\n\
             {\"seq\":1,\"event\":{\"kind\":\"user_message\",\"turn\":1,\"text\":\"hello from a journal\"}}\n",
        )
        .unwrap();

        assert_eq!(listed(dir.path()), before_list);
        assert_eq!(recalled(), before_recall);
    }

    /// A fact too long for one line is refused before anything is written: a
    /// log never holds half a record.
    #[test]
    fn an_oversized_fact_is_refused_and_nothing_is_written() {
        let (_dir, manager) = store();
        let lease = created(&manager, "s1");
        let before = fs::read(manager.events_path("s1").unwrap()).unwrap();
        let huge = logged(
            1,
            0,
            SessionEvent::UserMessage {
                turn: 1,
                text: "x".repeat(MAX_JSONL_LINE_BYTES),
                images: vec![],
            },
        );
        assert!(matches!(
            manager.append_events(&lease, &[a_turn()[0].clone(), huge]),
            Err(SessionStoreError::TooLarge {
                kind: "session event",
                ..
            })
        ));
        assert_eq!(
            fs::read(manager.events_path("s1").unwrap()).unwrap(),
            before
        );
    }

    /// Only the holder of the session's own lease appends, and only to an event
    /// session.
    #[test]
    fn appending_needs_the_sessions_own_lease() {
        let (_dir, manager) = store();
        let _lease = created(&manager, "s1");
        let other = created(&manager, "s2");
        let foreign = SessionManager::with_root(manager.root().join("elsewhere"));
        let stray = foreign.acquire_lease("s1").unwrap();
        assert!(matches!(
            manager.append_events(&stray, &a_turn()),
            Err(SessionStoreError::LeaseMismatch { .. })
        ));
        let snapshot_session = manager.acquire_lease("s3").unwrap();
        assert!(matches!(
            manager.append_events(&snapshot_session, &a_turn()),
            Err(SessionStoreError::NotFound { .. })
        ));
        manager.append_events(&other, &a_turn()).unwrap();
        assert!(manager.load_events("s1").unwrap().is_empty());
    }

    /// A log written by a newer build is refused, not misread.
    #[test]
    fn a_log_from_a_newer_format_is_refused() {
        let (_dir, manager) = store();
        let lease = manager.acquire_lease("s1").unwrap();
        let mut header = SessionHeader::new("s1");
        header.version = SESSION_FORMAT_VERSION + 1;
        manager
            .create_event_session(&lease, &header, &meta("s1"))
            .unwrap();
        assert!(matches!(
            manager.load_events("s1"),
            Err(SessionStoreError::FutureSchema { .. })
        ));
    }

    /// A later build appends kinds of fact to a file an earlier one created:
    /// a kind this build does not know is a newer file, refused as one.
    #[test]
    fn a_fact_of_a_kind_this_build_does_not_know_is_refused_as_newer() {
        let (_dir, manager) = store();
        let lease = created(&manager, "s1");
        manager.append_events(&lease, &a_turn()).unwrap();
        let mut log = fs::OpenOptions::new()
            .append(true)
            .open(manager.events_path("s1").unwrap())
            .unwrap();
        writeln!(
            log,
            "{}",
            serde_json::json!({ "seq": 9, "at": 0, "event": { "kind": "from_the_future", "turn": 1 } })
        )
        .unwrap();
        assert!(matches!(
            manager.load_events("s1"),
            Err(SessionStoreError::FutureSchema { .. })
        ));
    }

    #[test]
    fn deleting_an_event_session_removes_its_files() {
        let (_dir, manager) = store();
        let lease = created(&manager, "s1");
        manager.append_events(&lease, &a_turn()).unwrap();
        manager.write_todo_sidecar("s1", &[], 2).unwrap();
        manager.delete(&lease).unwrap();
        let left: Vec<String> = fs::read_dir(manager.root())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| !name.ends_with(".lease") && !name.ends_with(".lock"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
        assert!(!manager.is_event_session("s1"));
    }

    fn snapshot_session(manager: &SessionManager, id: &str) -> (SessionLease, SessionSnapshot) {
        let lease = manager.acquire_lease(id).unwrap();
        let mut answer = Message::assistant("hi there", vec![]);
        answer.meta = Some(atomcode_kernel::message::MessageMeta {
            turn_id: 3,
            round: 1,
            request_id: 4,
            ..Default::default()
        });
        let mut snapshot = SessionSnapshot::new(vec![
            Message::system("You are AtomCode\n\n=== CONTEXT"),
            Message::user("hello"),
            answer,
        ]);
        snapshot.turn_counter = 3;
        manager
            .commit_native_import(
                &lease,
                Some(&snapshot),
                Some(&crate::session::PresentationFile::default()),
                &meta(id),
            )
            .unwrap();
        fs::write(
            manager.path_for(id, "jsonl").unwrap(),
            "{\"v\":1,\"ts\":2000,\"started_at\":1500,\"session_id\":\"s1\",\"turn_id\":3}\n",
        )
        .unwrap();
        (lease, snapshot)
    }

    fn names(manager: &SessionManager) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(manager.root())
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| !name.ends_with(".lease") && !name.ends_with(".lock"))
            .collect();
        names.sort();
        names
    }

    /// A session a released build stored as a snapshot is converted the first
    /// time it is opened: the same conversation, the prompt it was stored with
    /// and the times its transcript recorded, now as a log — and every file the
    /// released build reads is moved aside, not deleted, so rolling back finds
    /// nothing to continue behind the log's back.
    #[test]
    fn a_snapshot_session_is_converted_once_and_its_old_files_moved_aside() {
        let (_dir, manager) = store();
        let (lease, snapshot) = snapshot_session(&manager, "s1");

        assert!(manager.open_as_events(&lease).unwrap(), "converted");
        assert!(manager.is_event_session("s1"));
        let events = manager.load_events("s1").unwrap();
        assert!(same_conversation(
            &derive_messages_with_meta(&events),
            &snapshot.messages
        ));
        assert!(events.iter().all(|e| e.at >= 1500), "{events:?}");
        let header = manager.read_event_header("s1").unwrap();
        assert_eq!(
            header.context.as_deref(),
            Some("You are AtomCode\n\n=== CONTEXT")
        );

        let after = names(&manager);
        for name in &after {
            for older in [".snapshot", ".meta", ".jsonl", ".json"] {
                assert!(!name.ends_with(older), "`{name}` left behind: {after:?}");
            }
        }
        for moved in [
            "s1.snapshot.migrated",
            "s1.meta.migrated",
            "s1.jsonl.migrated",
        ] {
            assert!(
                after.iter().any(|n| n == moved),
                "{moved} missing: {after:?}"
            );
        }

        assert!(!manager.open_as_events(&lease).unwrap(), "only once");
        fs::remove_file(manager.path_for("s1", "snapshot.migrated").unwrap()).unwrap();
        assert_eq!(manager.load_events("s1").unwrap(), events);
    }

    /// Making a log's conversation some other one appends facts and rewrites
    /// nothing: back to before a turn is one `Rewound`; a summary in front of
    /// the later turns is one `Compacted`; a conversation the log never held is
    /// everything taken back and that conversation committed.
    #[test]
    fn a_conversation_change_is_appended_as_facts() {
        let mut log = a_turn();
        log.extend(a_turn().into_iter().map(|mut logged| {
            logged.seq += 10;
            shift_turn(&mut logged.event, 1);
            logged
        }));
        let first_turn = derive_messages_with_meta(&log[..5]);

        let undo = events_to_become(&log, &first_turn);
        assert_eq!(
            undo,
            vec![SessionEvent::Rewound {
                turn: 2,
                to: 11,
                scope: RewindScope::Conversation
            }]
        );

        let mut summary = Message::system("the first turn, summarised");
        summary.synthetic = true;
        let mut compacted = vec![summary];
        compacted.extend(derive_messages_with_meta(&log[5..]));
        assert_eq!(
            events_to_become(&log, &compacted),
            vec![SessionEvent::Compacted {
                turn: 2,
                through: 10,
                summary: "the first turn, summarised".into(),
                from: 0,
            }]
        );

        let elsewhere = vec![
            Message::user("restored"),
            Message::assistant("noted", vec![]),
        ];
        let change = events_to_become(&log, &elsewhere);
        let mut after = log.clone();
        after.extend(
            change
                .into_iter()
                .enumerate()
                .map(|(i, event)| LoggedEvent {
                    seq: 100 + i as u64,
                    at: 0,
                    event,
                }),
        );
        assert!(same_conversation(
            &derive_messages_with_meta(&after),
            &elsewhere
        ));
        assert!(events_to_become(&after, &elsewhere).is_empty());
    }

    /// The writer that appended a change can take it back while nobody has read
    /// it, and only under the lease.
    #[test]
    fn an_append_can_be_taken_back_to_its_mark() {
        let (_dir, manager) = store();
        let lease = created(&manager, "s1");
        manager.append_events(&lease, &a_turn()).unwrap();
        let before = manager.load_events("s1").unwrap();
        let mark = manager.append_conversation_change(&lease, &[], 99).unwrap();
        assert_ne!(manager.load_events("s1").unwrap(), before);
        manager.truncate_events(&lease, mark).unwrap();
        assert_eq!(manager.load_events("s1").unwrap(), before);
    }

    /// A fork starts from every fact of the source, under its own identity, with
    /// the source as its parent and all of them counted as inherited.
    #[test]
    fn forking_an_event_session_copies_its_log() {
        let (_dir, manager) = store();
        let lease = created(&manager, "s1");
        manager.append_events(&lease, &a_turn()).unwrap();

        let (forked, _fork_lease) = manager.fork_native_session("s1", "s2", 2_000).unwrap();
        assert_eq!(forked.meta.id, "s2");
        assert_eq!(
            forked.meta.fork_info.as_ref().map(|f| f.parent_id.as_str()),
            Some("s1")
        );
        assert!(manager.is_event_session("s2"));
        assert_eq!(
            manager.load_events("s2").unwrap(),
            manager.load_events("s1").unwrap()
        );
        let header = manager.read_event_header("s2").unwrap();
        assert_eq!(header.parent.as_deref(), Some("s1"));
        assert_eq!(header.inherited, 4);
    }
}
