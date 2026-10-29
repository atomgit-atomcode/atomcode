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
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use atomcode_kernel::message::{Message, Role, SessionSnapshot};
use atomcode_kernel::session::{
    derive_messages_with_meta, InjectionOrigin, LoggedEvent, NoticeKind, RewindScope, SeqNo,
    SessionEvent, SessionHeader, SESSION_FORMAT_VERSION,
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

// Only presentation payloads are shortened. Sequence numbers and structural
// facts remain intact so the canonical projector still decides what is visible.
fn reference_hit(text: &str, query: &str) -> Option<usize> {
    if query.is_empty() {
        return None;
    }
    // Prefer the latest mention inside a long answer: an earlier proposal may
    // have been corrected near its end. The result still reports a partial window.
    let target = text.to_lowercase().rfind(&query.to_lowercase())?;
    let mut lowered = 0;
    for (offset, ch) in text.char_indices() {
        let width: usize = ch.to_lowercase().map(char::len_utf8).sum();
        if target < lowered + width {
            return Some(offset);
        }
        lowered += width;
    }
    None
}

fn trim_reference_payload(value: &mut serde_json::Value, clipped: &mut bool, query: &str) {
    match value {
        serde_json::Value::Object(fields) => {
            for (key, value) in fields.iter_mut() {
                if matches!(key.as_str(), "reasoning" | "reasoning_blocks" | "images") {
                    *value = if value.is_array() {
                        serde_json::json!([])
                    } else {
                        serde_json::json!("")
                    };
                } else if key == "arguments" && value.is_string() {
                    // Calls remain paired by id, but their inputs are never quoted.
                    *value = serde_json::json!("{}");
                } else if matches!(key.as_str(), "text" | "content" | "summary") {
                    if let Some(text) = value.as_str() {
                        let count = text.chars().count();
                        if count > 8000 {
                            let hit = reference_hit(text, query);
                            let excerpt: String = if let Some(hit) = hit {
                                let start = text[..hit].chars().count().saturating_sub(2000);
                                text.chars().skip(start).take(8000).collect()
                            } else if key == "summary"
                                || text.starts_with(crate::compaction::ANCHOR_SENTINEL)
                            {
                                text.chars().take(8000).collect()
                            } else {
                                text.chars().skip(count - 8000).collect()
                            };
                            *clipped = true;
                            *value = serde_json::json!(excerpt);
                        }
                    } else {
                        trim_reference_payload(value, clipped, query);
                    }
                } else {
                    trim_reference_payload(value, clipped, query);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                trim_reference_payload(item, clipped, query);
            }
        }
        _ => {}
    }
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
    /// Callers allocate sequence numbers from the log they read before the
    /// write, which is safe only while that lease keeps a single writer; a
    /// second writer would need the allocation moved inside the append lock.
    pub fn append_events(&self, lease: &SessionLease, events: &[LoggedEvent]) -> SessionResult<()> {
        self.append_events_inner(lease, events, true)
    }

    /// [`Self::append_events`], optionally leaving the todo cache untouched: a
    /// fork copies the source log and then carries the source's sidecar, so a
    /// rebuild from the copied facts would only be overwritten — and on a
    /// source with no sidecar it could resurrect calls compaction drained.
    #[cfg_attr(not(feature = "tools"), allow(unused_variables))]
    fn append_events_inner(
        &self,
        lease: &SessionLease,
        events: &[LoggedEvent],
        follow_sidecar: bool,
    ) -> SessionResult<()> {
        self.validate_active_lease(lease)?;
        let id = lease.id();
        if !self.is_event_session(id) {
            return Err(SessionStoreError::NotFound {
                path: self.index_path(id)?,
            });
        }
        let mut buffer = Vec::new();
        for logged in events {
            if logged.event.is_transient() {
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
        Self::drop_unterminated_tail(&mut file, &path)?;
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
        // The handle also carries plain write access so a torn tail can be cut
        // off; anchor the record at the end instead of trusting the cursor.
        file.seek(SeekFrom::End(0)).map_err(|e| io_at(&path, e))?;
        file.write_all(&buffer).map_err(|e| io_at(&path, e))?;
        drop(file);
        self.index_the_chosen_name(id, events);
        #[cfg(feature = "tools")]
        if follow_sidecar
            && events.iter().any(|logged| match &logged.event {
                SessionEvent::Rewound { scope, .. } => scope.takes_back_conversation(),
                SessionEvent::Interrupted { undone, .. } => *undone,
                _ => false,
            })
        {
            // A rewind or a cancelled-and-undone turn takes its todo calls out
            // of the projection; the list the model reads is the log's standing
            // turns, rebuilt so the sidecar cannot keep offering undone work.
            if let Err(error) = self.rebuild_todo_sidecar_after_undo(id) {
                // The append itself stands: a stale sidecar is a cache problem,
                // not a reason to report the committed fact as failed.
                tracing::warn!(
                    %error,
                    session = id,
                    "todo sidecar could not follow a rewind; the log append stands"
                );
            }
        }
        Ok(())
    }

    /// Append one fact whose sequence number is allocated under the log's write
    /// lock.
    ///
    /// `build` runs while the lock is held so it can record the exact sequence,
    /// and `commit` runs under the same lock once the record is serialized and
    /// the log has room for it, immediately before the bytes are written. A
    /// failed `build`, a record that cannot be serialized or a full log thus
    /// writes nothing before `commit`: a caller persisting a retry marker there
    /// never leaves one naming a fact this call decided not to write. A `commit`
    /// that fails leaves the log untouched; a write that fails after it leaves
    /// the caller's marker to be reconciled against the log.
    ///
    /// `floor` is the highest sequence the caller's read of this log already
    /// saw. The allocation is one past the later of it and the file's last
    /// record, so a log whose tail is not its maximum — one an earlier build
    /// appended out of order — is still never given a sequence it already
    /// holds. The two agree for every log written in increasing order, which is
    /// every log this store writes.
    pub(super) fn append_event_allocating<F, C>(
        &self,
        lease: &SessionLease,
        floor: SeqNo,
        build: F,
        commit: C,
    ) -> SessionResult<()>
    where
        F: FnOnce(SeqNo) -> SessionResult<LoggedEvent>,
        C: FnOnce(SeqNo) -> SessionResult<()>,
    {
        self.validate_active_lease(lease)?;
        let id = lease.id();
        if !self.is_event_session(id) {
            return Err(SessionStoreError::NotFound {
                path: self.index_path(id)?,
            });
        }
        let path = self.events_path(id)?;
        let mut file = retry_transient_file_access(|| open_append_file(&path))?;
        retry_transient_file_access(|| {
            fs2::FileExt::lock_exclusive(&file).map_err(|e| io_at(&path, e))
        })?;
        // The same handle reads and writes: Windows locks are mandatory, so a
        // second handle could not read the region this one has locked. An
        // append that never completed is cut off first, so the allocation and
        // the write both see the log a reader would.
        Self::drop_unterminated_tail(&mut file, &path)?;
        let next = Self::last_recorded_seq(&mut file, &path)?
            .unwrap_or(0)
            .max(floor)
            .saturating_add(1);
        let event = build(next)?;
        let buffer = record_line(&event)?;
        let current = usize::try_from(file.metadata().map_err(|e| io_at(&path, e))?.len())
            .unwrap_or(usize::MAX);
        let next_len = current.saturating_add(buffer.len());
        if next_len > MAX_JSONL_BYTES {
            return Err(SessionStoreError::TooLarge {
                kind: "session events",
                limit: MAX_JSONL_BYTES,
                actual: next_len,
            });
        }
        commit(next)?;
        // The handle also carries plain write access so a torn tail can be cut
        // off; anchor the record at the end instead of trusting the cursor.
        file.seek(SeekFrom::End(0)).map_err(|e| io_at(&path, e))?;
        file.write_all(&buffer).map_err(|e| io_at(&path, e))?;
        drop(file);
        self.index_the_chosen_name(id, std::slice::from_ref(&event));
        Ok(())
    }

    /// Cut off a final line that no write completed.
    ///
    /// Every record this store appends ends with a newline, written in the same
    /// buffer, so a last line without one is the prefix a failed `write_all`
    /// left behind — not a fact. Readers skip it (see `read_event_log_bounded`);
    /// appends remove it here, under the log's exclusive lock, so the next
    /// record is not glued onto its prefix. A file with no complete line at all
    /// (a torn header) is refused: there is nothing sound to append behind.
    fn drop_unterminated_tail(file: &mut fs::File, path: &Path) -> SessionResult<()> {
        let len = file.metadata().map_err(|error| io_at(path, error))?.len();
        if len == 0 {
            return Ok(());
        }
        let mut buffer = Vec::new();
        read_log_window(file, path, len - 1, len, &mut buffer)?;
        if buffer.first() == Some(&b'\n') {
            return Ok(());
        }
        // The newline that ends the last complete line is within one widest
        // record of the end: the residue is the prefix of a single record, and
        // a record this store writes never exceeds `MAX_JSONL_LINE_BYTES`.
        let start = len.saturating_sub((MAX_JSONL_LINE_BYTES as u64).saturating_add(1));
        read_log_window(file, path, start, len, &mut buffer)?;
        let Some(position) = buffer.iter().rposition(|byte| *byte == b'\n') else {
            return Err(SessionStoreError::Corrupt {
                kind: "session events",
                message: format!("{}: unreadable last record", path.display()),
            });
        };
        let keep = start + position as u64 + 1;
        file.set_len(keep).map_err(|error| io_at(path, error))?;
        // `set_len` leaves the cursor where it was; on Windows the handle now
        // also carries plain write access, so the next write follows the cursor
        // instead of always appending. Put it back at the new end.
        file.seek(SeekFrom::Start(keep))
            .map_err(|error| io_at(path, error))?;
        Ok(())
    }

    /// The last fact's sequence in the log, read from its tail through the
    /// locked append handle. `None` for a log with only its header (or nothing).
    ///
    /// The last record, not the maximum: [`Self::append_event_allocating`]
    /// raises it to the caller's floor, which is what covers a log an earlier
    /// build appended out of order.
    ///
    /// Trailing blank lines are skipped the way the reader skips them
    /// ([`Self::read_event_log_bounded`]): a log that ends with one still ends
    /// with the record above it, and treating that blank line as the record
    /// would refuse to allocate for a log that reads back fine.
    ///
    /// The lock is held for the whole scan, into one buffer that is reused: the
    /// tail window holds every record an append writes, so a normal log pays one
    /// read and parses the line where it was read. A record wider than the
    /// window — or a tail buried under more blank bytes than one — takes a
    /// second read, of the widest line this store writes
    /// ([`MAX_JSONL_LINE_BYTES`]), which covers it without keeping every window
    /// alive or joining copies. A tail without a line boundary within that
    /// bound is not a line this store wrote.
    fn last_recorded_seq(file: &mut fs::File, path: &Path) -> SessionResult<Option<SeqNo>> {
        #[derive(serde::Deserialize)]
        struct SeqRecord {
            #[serde(default)]
            seq: Option<SeqNo>,
        }

        let len = file.metadata().map_err(|error| io_at(path, error))?.len();
        if len == 0 {
            return Ok(None);
        }
        let corrupt = || SessionStoreError::Corrupt {
            kind: "session events",
            message: format!("{}: unreadable last record", path.display()),
        };
        let parse = |line: &[u8]| {
            serde_json::from_slice::<SeqRecord>(line)
                .map(|record| record.seq)
                .map_err(|_| corrupt())
        };

        // `end` is the position just past the last non-blank byte. Only blanks
        // leave it at the window's start.
        let mut buffer = Vec::new();
        let mut start = len.saturating_sub(TAIL_WINDOW);
        read_log_window(file, path, start, len, &mut buffer)?;
        let mut end = start + trimmed_len(&buffer) as u64;
        if end == 0 {
            return Ok(None);
        }
        if !buffer[..(end - start) as usize].contains(&b'\n') && start > 0 {
            // No line boundary in the window: the last record is wider than it,
            // or the window is all blanks and the record ends before it. Either
            // way the boundary it follows is within one widest line of the end.
            start = len.saturating_sub((MAX_JSONL_LINE_BYTES as u64).saturating_add(1));
            read_log_window(file, path, start, len, &mut buffer)?;
            end = start + trimmed_len(&buffer) as u64;
        }
        if end == start {
            // Blanks all the way back to the start of the file: no record, like
            // an empty log. Anywhere else, the tail is wider than this store
            // writes.
            return if start == 0 { Ok(None) } else { Err(corrupt()) };
        }
        let used = (end - start) as usize;
        let line = match buffer[..used].iter().rposition(|byte| *byte == b'\n') {
            Some(position) => &buffer[position + 1..used],
            // The log's first line is a header, not a record: `seq` stays
            // `None`, and the caller allocates from its own floor.
            None if start == 0 => &buffer[..used],
            None => return Err(corrupt()),
        };
        parse(line)
    }

    /// Rebuild the todo sidecar from the log after a rewind or a cancelled-and-
    /// undone turn.
    ///
    /// The sidecar is the model's list once compaction has drained the
    /// transcript's calls. An undo takes those calls out of the projection but
    /// leaves the facts in the log, so folding the standing turns here is the
    /// only way the cache can agree with what the model is about to be shown.
    #[cfg(feature = "tools")]
    fn rebuild_todo_sidecar_after_undo(&self, id: &str) -> SessionResult<()> {
        use crate::tools::todo::{
            apply_todo_action, is_todo_call, is_todo_plan, reduce_todos, TodoItem, TodoStatus,
        };

        let events = self.load_events(id)?;
        let undone = atomcode_kernel::session::undone_turns(&events);

        // A person's cancel retires the plan without deleting its calls. Match
        // the transcript fold's boundary (`active_todo_calls`): only calls after
        // the most recent standing interruption shape the list.
        let boundary = events
            .iter()
            .filter(|logged| !undone.contains(&logged.event.turn()))
            .filter_map(|logged| match &logged.event {
                SessionEvent::Interrupted {
                    turn,
                    undone: false,
                } => Some((logged.seq, *turn)),
                _ => None,
            })
            .next_back();

        // A failed result excludes the call it answered, keyed by the position
        // that call was made at — the same rule the transcript fold applies by
        // message order. Ids alone would let a provider that reuses them across
        // turns drop a later call the model still sees.
        let failed: std::collections::HashSet<(u64, u32, &str)> = events
            .iter()
            .filter(|logged| !undone.contains(&logged.event.turn()))
            .filter(|logged| boundary.is_none_or(|(seq, _)| logged.seq > seq))
            .filter_map(|logged| match &logged.event {
                SessionEvent::ToolResultLogged {
                    turn,
                    round,
                    call_id,
                    is_error: true,
                    ..
                } => Some((*turn, *round, call_id.as_str())),
                _ => None,
            })
            .collect();

        let mut calls: Vec<(u64, u32, u32, &str, &str)> = Vec::new();
        for logged in &events {
            if undone.contains(&logged.event.turn()) {
                continue;
            }
            if boundary.is_some_and(|(seq, _)| logged.seq <= seq) {
                continue;
            }
            let SessionEvent::AssistantMessage {
                turn,
                round,
                tool_calls,
                ..
            } = &logged.event
            else {
                continue;
            };
            for (index, call) in tool_calls.iter().enumerate() {
                if !is_todo_call(&call.name) || failed.contains(&(*turn, *round, call.id.as_str()))
                {
                    continue;
                }
                calls.push((
                    *turn,
                    *round,
                    u32::try_from(index).unwrap_or(u32::MAX),
                    call.name.as_str(),
                    call.arguments.as_str(),
                ));
            }
        }

        let has_plan = calls
            .iter()
            .any(|(_, _, _, _, arguments)| is_todo_plan(arguments));
        // A converted session's plan can live only in the sidecar: compaction
        // removed it from the snapshot the log was built from, so no full-plan
        // call exists anywhere in this log. Do not let an undone `through` turn
        // erase that list; only an event-sourced plan can be retired that way.
        let had_plan_in_log = events.iter().any(|logged| match &logged.event {
            SessionEvent::AssistantMessage { tool_calls, .. } => tool_calls
                .iter()
                .any(|call| is_todo_call(&call.name) && is_todo_plan(&call.arguments)),
            _ => false,
        });
        let existing = if has_plan {
            None
        } else {
            match self.read_todo_sidecar(id) {
                Ok(sidecar) => sidecar,
                // An unreadable sidecar is not evidence that its plan is gone.
                Err(_) => return Ok(()),
            }
        };
        // Whether the sidecar is still the baseline the log has not retired.
        // A plan the log cannot source at all (a converted or compacted
        // session) is its only copy; no undo or cancel in this log can be
        // about it. A plan the log does hold stands only while its through
        // turn was not undone and is newer than the last standing cancel.
        let sidecar_stands = existing.as_ref().is_some_and(|sidecar| {
            !had_plan_in_log
                || sidecar.through.is_none_or(|through| {
                    !undone.contains(&through.turn)
                        && boundary.is_none_or(|(_, turn)| through.turn > turn)
                })
        });

        let mut items: Vec<TodoItem> = Vec::new();
        let mut through: Option<super::manager::TodoCallPosition> = None;
        let mut legacy_baseline = false;
        if has_plan {
            // A full plan in the log is authoritative for the calls after the
            // boundary; fold them from empty.
            items = reduce_todos(
                calls
                    .iter()
                    .map(|(_, _, _, name, arguments)| (*name, *arguments)),
            );
            through =
                calls.last().map(
                    |(turn, round, index, _, _)| super::manager::TodoCallPosition {
                        turn: *turn,
                        round: *round,
                        index: *index,
                    },
                );
        } else {
            if sidecar_stands {
                if let Some(sidecar) = existing.as_ref() {
                    items = sidecar
                        .todos
                        .iter()
                        .map(|item| TodoItem {
                            content: item.content.clone(),
                            status: match item.status.as_str() {
                                "in_progress" => TodoStatus::InProgress,
                                "completed" => TodoStatus::Completed,
                                _ => TodoStatus::Pending,
                            },
                        })
                        .collect();
                    through = sidecar.through;
                    legacy_baseline = sidecar.through.is_none();
                }
            }
            // A sidecar written before positions existed cannot say which calls
            // it already reflects. Same content means the task it adds is
            // already in the list: advance past the call instead of writing it
            // twice — and without dropping the list to avoid the duplicate.
            let already_listed = |items: &[TodoItem], arguments: &str| -> bool {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(arguments) else {
                    return false;
                };
                if value.get("action").and_then(|action| action.as_str()) != Some("add") {
                    return false;
                }
                value
                    .get("content")
                    .and_then(|content| content.as_str())
                    .map(str::trim)
                    .is_some_and(|content| {
                        !content.is_empty() && items.iter().any(|item| item.content == content)
                    })
            };
            for (turn, round, index, _, arguments) in &calls {
                let position = super::manager::TodoCallPosition {
                    turn: *turn,
                    round: *round,
                    index: *index,
                };
                if through.is_some_and(|current| position <= current) {
                    continue;
                }
                if legacy_baseline && already_listed(&items, arguments) {
                    through = Some(position);
                    continue;
                }
                apply_todo_action(&mut items, arguments);
                through = Some(position);
            }
        }

        if items.is_empty() && through.is_none() && !has_plan {
            let Some(_sidecar) = existing.as_ref() else {
                return Ok(());
            };
            if sidecar_stands {
                // The list stood and no call changed it; there is nothing to
                // rewrite.
                return Ok(());
            }
            // The log holds the plan this sidecar cached and took it back
            // (an undone through turn or a cancel boundary); write the empty
            // list so the cache agrees.
        }

        let todos: Vec<super::manager::TodoSidecarItem> = items
            .into_iter()
            .map(|item| super::manager::TodoSidecarItem {
                content: item.content,
                status: match item.status {
                    TodoStatus::Pending => "pending",
                    TodoStatus::InProgress => "in_progress",
                    TodoStatus::Completed => "completed",
                }
                .to_string(),
            })
            .collect();
        // `message_count` is the conversation size this list was written against,
        // so it must be the projection, not a raw event count. Rewinds are rare;
        // one projection here is cheaper than a marker with the wrong meaning.
        let message_count = atomcode_kernel::session::derive_messages(&events).len();
        self.write_todo_sidecar(id, &todos, message_count, through)
    }

    /// Keep the index's name with the log's when the person names the session.
    ///
    /// A name the person chose is written to the log (`Titled { user_set }`) by
    /// the screen that reads names from the log, and to the index alone by the
    /// classic screen's `/rename`. An open reconciles the two the index's way
    /// ([`Self::record_chosen_name`]) — so an index left holding an older name
    /// would put that name back over a newer one the moment the session was
    /// reopened. Written here, beside the one write every fact goes through, the
    /// index follows the log whoever named it. Best-effort, like the index
    /// updates beside it: the log is the authority, and it has the fact.
    fn index_the_chosen_name(&self, id: &str, events: &[LoggedEvent]) {
        let Some(title) = events.iter().rev().find_map(|logged| match &logged.event {
            SessionEvent::Titled {
                title,
                user_set: true,
                ..
            } => Some(title.trim().to_string()),
            _ => None,
        }) else {
            return;
        };
        if title.is_empty() {
            return;
        }
        let _ = self.update_meta(id, |meta| {
            meta.name = title;
            meta.user_renamed = true;
        });
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
        self.project_snapshot_bounded(id, MAX_JSONL_BYTES)
    }

    pub(super) fn project_snapshot_bounded(
        &self,
        id: &str,
        limit: usize,
    ) -> SessionResult<SessionSnapshot> {
        Ok(snapshot_of(
            &self.read_event_log_bounded(id, false, limit)?.1,
        ))
    }

    /// Read-only reference projection. Scan with the store's line/file guards,
    /// retain bounded text, and use the canonical projector for rewind/compaction.
    pub(super) fn reference_projection(
        &self,
        id: &str,
        retained_limit: usize,
    ) -> SessionResult<(SessionMeta, Vec<(Message, bool)>, bool, Option<u64>)> {
        self.reference_projection_query(id, retained_limit, "", false)
    }

    pub(super) fn reference_projection_query(
        &self,
        id: &str,
        retained_limit: usize,
        query: &str,
        historical: bool,
    ) -> SessionResult<(SessionMeta, Vec<(Message, bool)>, bool, Option<u64>)> {
        let meta = self.read_meta(id)?;
        if meta.needs_newer_version() {
            return Err(SessionStoreError::FutureSchema {
                kind: "session events",
                found: meta.format_version,
                supported: SESSION_FORMAT_VERSION,
            });
        }
        if meta.owner != StorageOwner::Native {
            return Err(SessionStoreError::OwnershipConflict {
                id: id.into(),
                owner: meta.owner,
                operation: "reference session",
            });
        }
        if !self.is_event_session(id) {
            if historical {
                return Err(SessionStoreError::Corrupt {
                    kind: "session reference",
                    message: "raw history is unavailable for an unmigrated snapshot".into(),
                });
            }
            let loaded = self.load_native_session_bounded(id, retained_limit)?;
            return Ok((
                loaded.meta,
                loaded
                    .snapshot
                    .messages
                    .into_iter()
                    .map(|m| (m, false))
                    .collect(),
                false,
                None,
            ));
        }
        let path = self.events_path(id)?;
        let mut header = None;
        let mut events = Vec::new();
        let mut retained = 0usize;
        let mut clipped = false;
        let mut index = 0usize;
        super::manager::for_each_jsonl_line_limited(&path, 64 * 1024 * 1024, true, |raw| {
            index += 1;
            if raw.iter().all(u8::is_ascii_whitespace) {
                return Ok(());
            }
            let line: Line =
                serde_json::from_slice(raw).map_err(|e| SessionStoreError::Corrupt {
                    kind: "session events",
                    message: format!("{}:{index}: {e}", path.display()),
                })?;
            if let Some(found) = line.header {
                if header.is_some() || !events.is_empty() || storage_id(&found.id) != id {
                    return Err(SessionStoreError::Corrupt {
                        kind: "session events",
                        message: "invalid reference session header".into(),
                    });
                }
                if found.version > SESSION_FORMAT_VERSION {
                    return Err(SessionStoreError::FutureSchema {
                        kind: "session events",
                        found: found.version,
                        supported: SESSION_FORMAT_VERSION,
                    });
                }
                header = Some(found);
                return Ok(());
            }
            let (Some(seq), Some(value)) = (line.seq, line.event) else {
                return Err(SessionStoreError::Corrupt {
                    kind: "session events",
                    message: "not an event record".into(),
                });
            };
            if header.is_none()
                || events
                    .last()
                    .is_some_and(|last: &LoggedEvent| last.seq >= seq)
            {
                return Err(SessionStoreError::Corrupt {
                    kind: "session events",
                    message: "missing header or non-increasing sequence".into(),
                });
            }
            // Validate the original event before dropping any non-reference payload.
            let event = event_of(value, &path, index - 1, header.as_ref())?;
            let mut value =
                serde_json::to_value(event).map_err(|e| SessionStoreError::Corrupt {
                    kind: "session events",
                    message: e.to_string(),
                })?;
            trim_reference_payload(&mut value, &mut clipped, query);
            retained = retained
                .saturating_add(
                    serde_json::to_vec(&value)
                        .map_err(|e| SessionStoreError::Corrupt {
                            kind: "session events",
                            message: e.to_string(),
                        })?
                        .len(),
                )
                .saturating_add(64);
            if retained > retained_limit {
                return Err(SessionStoreError::TooLarge {
                    kind: "reference projection",
                    limit: retained_limit,
                    actual: retained,
                });
            }
            let event = event_of(value, &path, index - 1, header.as_ref())?;
            events.push(LoggedEvent {
                seq,
                at: line.at,
                event,
            });
            Ok(())
        })?;
        if header.is_none() {
            return Err(SessionStoreError::Corrupt {
                kind: "session events",
                message: "missing session header".into(),
            });
        }
        let captured_through_seq = events.last().map(|e| e.seq);
        if historical {
            let messages = events
                .iter()
                .filter_map(|logged| match &logged.event {
                    SessionEvent::UserMessage { text, .. } => Some((Message::user(text), false)),
                    SessionEvent::AssistantMessage { text, .. } => {
                        Some((Message::assistant(text, vec![]), false))
                    }
                    _ => None,
                })
                .collect();
            return Ok((meta, messages, clipped, captured_through_seq));
        }
        let messages = atomcode_kernel::session::derive_traced(&events)
            .into_iter()
            .map(|traced| {
                let summary = matches!(
                    traced.source,
                    atomcode_kernel::session::Provenance::Summary(_)
                );
                (traced.message, summary)
            })
            .collect();
        Ok((meta, messages, clipped, captured_through_seq))
    }

    fn read_event_log(
        &self,
        id: &str,
        header_only: bool,
    ) -> SessionResult<(Option<SessionHeader>, Vec<LoggedEvent>)> {
        self.read_event_log_bounded(id, header_only, MAX_JSONL_BYTES)
    }

    pub(super) fn read_event_log_bounded(
        &self,
        id: &str,
        header_only: bool,
        limit: usize,
    ) -> SessionResult<(Option<SessionHeader>, Vec<LoggedEvent>)> {
        let path = self.events_path(id)?;
        let bytes = read_regular_file_bounded(&path, "session events", limit.min(MAX_JSONL_BYTES))?;
        // A failed append can leave the prefix of a record at the end, without
        // the newline that terminates every line this store writes. It is not a
        // fact: reading it as one would turn a single failed write into a
        // session that can never be opened or resumed. The next append cuts it
        // off (see `drop_unterminated_tail`).
        let complete = if bytes.last().is_none_or(|byte| *byte == b'\n') {
            &bytes[..]
        } else {
            bytes
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(&bytes[..0], |position| &bytes[..=position])
        };
        let mut header = None;
        let mut events = Vec::new();
        for (index, raw) in complete.split(|b| *b == b'\n').enumerate() {
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
        presentation: &super::presentation::PresentationFile,
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
        let copied = self
            .append_events_inner(destination, &events, false)
            .and_then(|()| self.write_presentation(destination.id(), presentation));
        if copied.is_err() {
            // Only remove a destination we created, never an existing session
            // rejected by create_event_session. A failed copy is not offered.
            self.delete(destination)?;
        } else {
            #[cfg(feature = "tools")]
            {
                let last_turn = events
                    .iter()
                    .map(|logged| logged.event.turn())
                    .max()
                    .unwrap_or(0);
                self.carry_todo_sidecar(source_id, destination.id(), last_turn);
            }
        }
        copied
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

    /// Whether the bytes a rollback would cut (everything after `mark`) carry an
    /// undo fact. Only then can the todo sidecar differ from its pre-append state.
    #[cfg(feature = "tools")]
    fn truncated_tail_has_undo(&self, id: &str, mark: u64) -> bool {
        let Ok(path) = self.events_path(id) else {
            return true;
        };
        let Ok(mut file) = fs::File::open(&path) else {
            return true;
        };
        let Ok(len) = file.metadata().map(|meta| meta.len()) else {
            return true;
        };
        if mark > len || file.seek(SeekFrom::Start(mark)).is_err() {
            return true;
        }
        // Only the rolled-back tail is read, one line at a time, and only the
        // few fields this predicate asks about are deserialized: the prefix is
        // already validated, and a tool-call-heavy line need not become a JSON
        // tree just to read its `kind`.
        #[derive(serde::Deserialize)]
        struct TailEvent {
            #[serde(default)]
            kind: Option<String>,
            #[serde(default)]
            scope: Option<String>,
            #[serde(default)]
            undone: Option<bool>,
        }
        #[derive(serde::Deserialize)]
        struct TailRecord {
            #[serde(default)]
            event: Option<TailEvent>,
        }

        use std::io::{BufRead, BufReader};
        for line in BufReader::new(file).take(MAX_JSONL_BYTES as u64).lines() {
            let Ok(line) = line else {
                // An unreadable tail is not evidence that no undo was cut.
                return true;
            };
            if line.is_empty() {
                continue;
            }
            let Ok(record) = serde_json::from_str::<TailRecord>(&line) else {
                return true;
            };
            let Some(event) = record.event else {
                continue;
            };
            match event.kind.as_deref() {
                Some("rewound") => {
                    if matches!(
                        event.scope.as_deref(),
                        Some("Conversation" | "conversation" | "Both" | "both")
                    ) {
                        return true;
                    }
                }
                Some("interrupted") if event.undone.unwrap_or(false) => return true,
                _ => {}
            }
        }
        false
    }

    /// Take back an append nobody has read yet: the log is cut to `mark`.
    ///
    /// The log is append-only for everyone who reads it. This exists for the one
    /// writer that appended a change and then failed to put the rest of it in
    /// place — a rebuilt agent that would not assemble after an undo — and is
    /// rolling its own transaction back under the lease, with no agent running.
    pub fn truncate_events(&self, lease: &SessionLease, mark: u64) -> SessionResult<()> {
        self.validate_active_lease(lease)?;
        #[cfg(feature = "tools")]
        let tail_has_undo = self.truncated_tail_has_undo(lease.id(), mark);
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
        file.set_len(mark).map_err(|error| io_at(&path, error))?;
        drop(file);
        #[cfg(feature = "tools")]
        {
            // Only a rollback that cuts an undo fact can have changed the list,
            // and only a session that keeps a sidecar has a cache to follow. The
            // log rollback already landed; a cache failure is reported, not fatal.
            if tail_has_undo && self.read_todo_sidecar(lease.id()).ok().flatten().is_some() {
                if let Err(error) = self.rebuild_todo_sidecar_after_undo(lease.id()) {
                    tracing::warn!(
                        %error,
                        session = lease.id(),
                        "todo sidecar could not follow the truncated log; the log rollback stands"
                    );
                }
            }
        }
        Ok(())
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
            self.record_chosen_name(lease);
            return Ok(false);
        }
        self.convert(lease, self.load_native_session(id)?, None)?;
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
        self.open_for_resume_noting(lease, &mut Vec::new())
    }

    /// [`Self::open_for_resume`], with what the person should be told about
    /// the resume in `notes` (see
    /// [`SessionManager::load_native_session_for_resume_noting`]).
    pub fn open_for_resume_noting(
        &self,
        lease: &SessionLease,
        notes: &mut Vec<String>,
    ) -> SessionResult<(super::manager::LoadedSession, Option<Message>)> {
        self.validate_active_lease(lease)?;
        let id = lease.id();
        self.refuse_newer(id)?;
        if self.is_event_session(id) {
            self.move_snapshot_files_aside(id)?;
            self.record_chosen_name(lease);
            return self.load_native_session_for_resume_noting(lease, notes);
        }
        let (loaded, pending) = self.load_native_session_for_resume_noting(lease, notes)?;
        self.convert(lease, loaded, pending.as_ref())?;
        if pending.is_some() {
            // The accepted turn is now a fact of the log; the log's own resume
            // hands it back, with the marker and pending view every replay of a
            // log fact has.
            return self.load_native_session_for_resume_noting(lease, notes);
        }
        Ok((self.load_native_session(id)?, None))
    }

    fn convert(
        &self,
        lease: &SessionLease,
        loaded: super::manager::LoadedSession,
        accepted: Option<&Message>,
    ) -> SessionResult<()> {
        let id = lease.id();
        let times = self.load_transcript_timestamps(id).unwrap_or_default();
        let mut events = events_from_snapshot(&loaded.snapshot, 1);
        stamp_turn_times(&mut events, &times);

        let meta = loaded.meta;
        // The name travels with the conversation. A snapshot kept it only in its
        // meta, and the log is what a screen reads a session's name from — a
        // session renamed before the log existed came back nameless, its `/rename`
        // gone from the composer until it was renamed again.
        if let Some(titled) = name_from_meta(&meta, &events) {
            let seq = next_seq(&events);
            let at = events.last().map_or(0, |last| last.at);
            events.push(LoggedEvent {
                seq,
                at,
                event: titled,
            });
        }
        if let Some(prompt) = accepted {
            // A log turn means "the person said this", so only a message that
            // is one becomes a turn: the checkpoint's tail can also be a tool
            // result, an injected continuation or a synthetic note, and writing
            // one of those as a `UserMessage` would replay it as the person's
            // next prompt. The resume loader asks the same predicate, so a
            // prompt it hands back is always one this write accepts.
            if is_resumable_prompt(prompt) {
                // The snapshot's in-flight prompt was accepted before the process
                // died but never entered a log. It becomes the turn the log would
                // have held, so its replay retracts a log fact like any other
                // instead of being sent beside a conversation that lacks it. The
                // index is not advanced for it: `turn_count` is the number of turn
                // stats (a runtime commit sets exactly that) and a turn that never
                // ran has none, while turn ids are read from the log, not the index.
                let turn = last_turn(&events) + 1;
                let at = u64::try_from(super::now_ms()).unwrap_or(0);
                let seq = next_seq(&events);
                events.push(LoggedEvent {
                    seq,
                    at,
                    event: SessionEvent::TurnStart { turn },
                });
                events.push(LoggedEvent {
                    seq: seq + 1,
                    at,
                    event: SessionEvent::UserMessage {
                        turn,
                        text: prompt.text.clone(),
                        images: prompt.images.clone(),
                    },
                });
            }
        }
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

    /// Record in the log a name the index holds as the person's own but the log
    /// never did — see [`missing_chosen_name`]. Once: after it the log holds it.
    ///
    /// Appended, not rewritten (`docs/adr/0024`), under the lease the opener
    /// holds, before anything replays the log. Best-effort: a session that
    /// cannot take the fact still opens, only without its name on the composer
    /// — what it did before this existed.
    fn record_chosen_name(&self, lease: &SessionLease) {
        let id = lease.id();
        // The index first: only a session the person named is worth reading the
        // whole log for, and that is a handful — every other open costs nothing.
        let Ok(meta) = self.read_meta(id) else {
            return;
        };
        if !meta.user_renamed {
            return;
        }
        let Ok(events) = self.load_events(id) else {
            return;
        };
        let inherited = self
            .read_event_header(id)
            .map(|header| header.inherited)
            .unwrap_or(0);
        let Some(titled) = missing_chosen_name(&meta, &events, inherited) else {
            return;
        };
        let fact = LoggedEvent {
            seq: events.iter().map(|e| e.seq).max().unwrap_or(0) + 1,
            at: u64::try_from(super::now_ms()).unwrap_or(0),
            event: titled,
        };
        let _ = self.append_events(lease, &[fact]);
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

/// Whether a message the resume paths hand back is one the person said, and so
/// one the log can hold as this turn's `UserMessage`.
///
/// The resume loader and the snapshot conversion both ask here because they
/// must agree: a message the loader hands back but the conversion refuses
/// leaves no turn in the log, and the next resume then finds nothing to hand
/// back — the prompt the person lost their turn to disappears without a trace.
pub(super) fn is_resumable_prompt(message: &Message) -> bool {
    message.role == Role::User
        && message.tool_call_id.is_none()
        && !message.synthetic
        && message.internal_origin.is_none()
        && message.tool_calls.is_empty()
}

/// How much of the log's tail the first read of
/// [`SessionManager::last_recorded_seq`] covers. Fixed so the normal scan holds
/// one window at a time, never the file.
const TAIL_WINDOW: u64 = 64 * 1024;

/// Where the last non-blank byte of `buffer` ends, or zero when it holds only
/// blanks. Trailing blank lines are skipped the way the reader skips them.
fn trimmed_len(buffer: &[u8]) -> usize {
    buffer
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())
        .map_or(0, |position| position + 1)
}

/// Fill `buffer` with bytes `start..end` of the locked append handle.
///
/// Seeking on an append handle is safe: the handle's writes go to the end of the
/// file whatever its position (append semantics), and Windows locks cover the
/// same handle that holds them. The buffer is reused across both tail reads, so
/// the normal one-window scan allocates nothing beyond it.
fn read_log_window(
    file: &mut fs::File,
    path: &Path,
    start: u64,
    end: u64,
    buffer: &mut Vec<u8>,
) -> SessionResult<()> {
    let len =
        usize::try_from(end.saturating_sub(start)).map_err(|_| SessionStoreError::Corrupt {
            kind: "session events",
            message: format!("{}: unreadable last record", path.display()),
        })?;
    file.seek(SeekFrom::Start(start))
        .map_err(|error| io_at(path, error))?;
    buffer.clear();
    buffer.resize(len, 0);
    file.read_exact(buffer).map_err(|error| io_at(path, error))
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
        // A synthetic system block is a compaction summary only when it is not
        // the memory block; memory is reseeded below as an `Injected` fact of its
        // own, so the log keeps its provenance instead of hiding it in a summary.
        .filter(|m| !m.text.starts_with(MEMORY_INJECTION_HEADER))
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

/// The prompt a turn accepted but never answered: the fact a process that died
/// mid-turn leaves behind. A turn qualifies only when nothing after its
/// `TurnStart` shows it ran — turn-start context, titles, checkpoints and a
/// compaction of earlier history are fine; a request, streamed or final
/// output, a tool, usage, a question or a stop means replaying would repeat
/// work the log cannot undo.
///
/// Returns the `TurnStart` sequence the caller rewinds to, the turn, and the
/// prompt to hand back to the runtime.
pub(crate) fn unanswered_prompt(events: &[LoggedEvent]) -> Option<(SeqNo, u64, Message)> {
    // The position is what decides the turn; re-finding it later by sequence
    // would pick the first record carrying that number, which a log read after
    // a truncation can have written twice.
    let (position, seq, turn) =
        events
            .iter()
            .enumerate()
            .rev()
            .find_map(|(position, logged)| match logged.event {
                SessionEvent::TurnStart { turn } => Some((position, logged.seq, turn)),
                _ => None,
            })?;
    unanswered_from(events, position, turn).map(|prompt| (seq, turn, prompt))
}

/// The prompt of the turn at `start` when that turn is still accepted and
/// unanswered. A resume marker names this position, so `prepare` can retract
/// exactly what it handed back instead of re-deciding against a log that moved.
pub(crate) fn unanswered_at(events: &[LoggedEvent], start: SeqNo, turn: u64) -> Option<Message> {
    // Read from the turn's own record in file order, the order the log projects
    // in: a `Rewound` written after the turn answers it however the records
    // were numbered. Filtering by `seq > start` would mix the two orders —
    // skipping a record written after the turn but numbered below it, and
    // reading one written before it but numbered above.
    let (position, found) = turn_start_at(events, start)?;
    if found != turn {
        return None;
    }
    unanswered_from(events, position, turn)
}

/// [`unanswered_at`] for a caller that already holds the `TurnStart`'s position.
fn unanswered_from(events: &[LoggedEvent], position: usize, turn: u64) -> Option<Message> {
    let mut prompt = None;
    for logged in &events[position + 1..] {
        match &logged.event {
            SessionEvent::UserMessage {
                turn: said_in,
                text,
                images,
            } if *said_in == turn && prompt.is_none() => {
                prompt = Some(if images.is_empty() {
                    Message::user(text)
                } else {
                    Message::user_with_images(text, images.clone())
                });
            }
            SessionEvent::Injected { .. }
            | SessionEvent::Titled { .. }
            | SessionEvent::Checkpointed { .. }
            | SessionEvent::Compacted { .. }
            | SessionEvent::ToolResultsStubbed { .. }
            | SessionEvent::MessagesRewritten { .. } => {}
            // Notices that only describe the session or its setup do not mean
            // the model ran. A retry, a truncated answer or a recovered stream
            // does, so those stay on the refusal side.
            SessionEvent::Notice {
                notice:
                    NoticeKind::CompactionDegraded
                    | NoticeKind::ConfigSkipped
                    | NoticeKind::MemberNotRestored,
                ..
            } => {}
            _ => return None,
        }
    }
    prompt
}

/// The position and turn of the `TurnStart` at `seq`, read in file order — the
/// order the log projects in. Sequence numbers name positions; they do not order
/// records, so every walk that starts from a turn locates it through here.
pub(crate) fn turn_start_at(events: &[LoggedEvent], seq: SeqNo) -> Option<(usize, u64)> {
    let position = events.iter().position(|logged| logged.seq == seq)?;
    match events[position].event {
        SessionEvent::TurnStart { turn } => Some((position, turn)),
        _ => None,
    }
}

/// The turn whose `TurnStart` still sits at `seq`, when the log still has one
/// there. A marker records a position; a log that was truncated and rewritten
/// can reuse it for another turn, and a retraction must not follow it there.
pub(crate) fn turn_started_at(events: &[LoggedEvent], seq: SeqNo) -> Option<u64> {
    turn_start_at(events, seq).map(|(_, turn)| turn)
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

/// The `Titled` fact a converted session's stored name becomes, if it has one.
///
/// Any real name — the person's, a model's, or one read off the first prompt —
/// with `user_set` saying which it was, so a screen shows a name the person chose
/// the way it shows one chosen now. A placeholder (`session-…`, `default`) is not
/// a name and becomes nothing.
fn name_from_meta(meta: &SessionMeta, events: &[LoggedEvent]) -> Option<SessionEvent> {
    let name = meta.name.trim();
    if name.is_empty() || SessionMeta::name_needs_fallback(name, &meta.id) {
        return None;
    }
    Some(SessionEvent::Titled {
        turn: last_turn(events),
        title: name.to_string(),
        user_set: meta.user_renamed,
    })
}

/// The `Titled` fact a log is missing when its index says the person named the
/// session (`user_renamed`) and the log's own last name is not that one, chosen.
///
/// How an index and its log come apart: a front end that renames by writing the
/// index only (`SessionManager::rename`, the classic screen's `/rename`), while
/// a screen that reads its session from the log — the name on its composer is
/// the log's last `Titled` — sees none of it. `inherited` facts are a parent's:
/// a fork's name is only the ones it committed itself.
fn missing_chosen_name(
    meta: &SessionMeta,
    events: &[LoggedEvent],
    inherited: usize,
) -> Option<SessionEvent> {
    let name = meta.name.trim();
    if !meta.user_renamed || name.is_empty() {
        return None;
    }
    let last = events
        .iter()
        .skip(inherited)
        .rev()
        .find_map(|logged| match &logged.event {
            SessionEvent::Titled {
                title, user_set, ..
            } => Some((title.trim(), *user_set)),
            _ => None,
        });
    if last == Some((name, true)) {
        return None;
    }
    Some(SessionEvent::Titled {
        turn: last_turn(events),
        title: name.to_string(),
        user_set: true,
    })
}

fn last_turn(events: &[LoggedEvent]) -> u64 {
    events
        .iter()
        .map(|logged| logged.event.turn())
        .max()
        .unwrap_or(0)
}

/// One past the highest sequence the built log holds.
///
/// The append path allocates from the log's maximum, not its last record; a
/// whole-file conversion keeps the same shape so the two cannot disagree if a
/// builder ever stops numbering consecutively.
fn next_seq(events: &[LoggedEvent]) -> SeqNo {
    events
        .iter()
        .map(|logged| logged.seq)
        .max()
        .map_or(1, |highest| highest.saturating_add(1))
}

/// First line of the memory block `MemoryContext` injects. A synthetic system
/// message carrying it is memory, not a compaction summary.
const MEMORY_INJECTION_HEADER: &str = "=== MEMORY ===";

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
                    let origin = if message.text.starts_with(MEMORY_INJECTION_HEADER) {
                        InjectionOrigin::Memory
                    } else {
                        InjectionOrigin::CompactionSummary
                    };
                    events.push(SessionEvent::Injected {
                        turn,
                        text: message.text.clone(),
                        origin,
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
    use crate::session::manager::{PrepareResumeReplay, ResumeReplay, StorageOwner};
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
        let found = crate::session::RecallTool::new(manager.root())
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
        manager.write_todo_sidecar("s1", &[], 2, None).unwrap();
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

        // Exercise the shared history reader used by the WebUI as well as recall's
        // projection: an event header must never be parsed as a legacy TurnRecord.
        let records = manager.load_transcript_records("s1").unwrap();
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

        let recalled = crate::session::RecallTool::new(manager.root())
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
            crate::session::RecallTool::new(manager.root())
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
        manager.write_todo_sidecar("s1", &[], 2, None).unwrap();
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

    fn titles(manager: &SessionManager, id: &str) -> Vec<(String, bool)> {
        manager
            .load_events(id)
            .unwrap()
            .into_iter()
            .filter_map(|logged| match logged.event {
                SessionEvent::Titled {
                    title, user_set, ..
                } => Some((title, user_set)),
                _ => None,
            })
            .collect()
    }

    /// The reported case: a session renamed in a build that stored snapshots
    /// kept its name in the meta only, and converting it dropped the name —
    /// the screen that reads names from the log showed none until it was
    /// renamed again. The name, and whose it was, now come across.
    #[test]
    fn a_converted_session_keeps_the_name_it_was_given() {
        let (_dir, manager) = store();
        let (lease, _) = snapshot_session(&manager, "s1");
        manager.rename("s1", "验证缺陷5155修复效果").unwrap();

        manager.open_for_resume(&lease).unwrap();
        assert_eq!(
            titles(&manager, "s1"),
            vec![("验证缺陷5155修复效果".to_string(), true)]
        );
        // Opened again, nothing more is added.
        manager.open_for_resume(&lease).unwrap();
        assert_eq!(titles(&manager, "s1").len(), 1);
    }

    /// A placeholder is not a name, and a name nobody chose is carried as not
    /// chosen.
    #[test]
    fn a_placeholder_is_not_carried_and_an_automatic_name_is_not_chosen() {
        let (_dir, manager) = store();
        let (lease, _) = snapshot_session(&manager, "s1");
        assert!(manager.open_as_events(&lease).unwrap());
        assert!(titles(&manager, "s1").is_empty(), "`session-s1` is no name");

        let (_dir, manager) = store();
        let (lease, _) = snapshot_session(&manager, "s2");
        manager
            .update_meta("s2", |meta| meta.name = "帮我读取下配置".into())
            .unwrap();
        assert!(manager.open_as_events(&lease).unwrap());
        assert_eq!(
            titles(&manager, "s2"),
            vec![("帮我读取下配置".to_string(), false)]
        );
    }

    /// An event session renamed where only the index is written (the classic
    /// screen's `/rename`): the next open records the name in the log, once.
    #[test]
    fn a_name_only_the_index_knows_is_recorded_in_the_log_on_open() {
        let (_dir, manager) = store();
        let lease = created(&manager, "s1");
        manager.append_events(&lease, &a_turn()).unwrap();
        manager
            .append_events(
                &lease,
                &[logged(
                    100,
                    0,
                    SessionEvent::Titled {
                        turn: 1,
                        title: "hello".into(),
                        user_set: false,
                    },
                )],
            )
            .unwrap();
        manager.rename("s1", "我的会话").unwrap();

        manager.open_for_resume(&lease).unwrap();
        let named = titles(&manager, "s1");
        assert_eq!(
            named.last(),
            Some(&("我的会话".to_string(), true)),
            "{named:?}"
        );
        let seqs: Vec<u64> = manager
            .load_events("s1")
            .unwrap()
            .iter()
            .map(|e| e.seq)
            .collect();
        assert!(
            seqs.windows(2).all(|w| w[0] < w[1]),
            "appended in order: {seqs:?}"
        );

        manager.open_as_events(&lease).unwrap();
        assert_eq!(titles(&manager, "s1").len(), named.len(), "only once");

        // A session nobody renamed is left as it is.
        let (_dir, manager) = store();
        let lease = created(&manager, "s2");
        manager.append_events(&lease, &a_turn()).unwrap();
        manager.open_for_resume(&lease).unwrap();
        assert!(titles(&manager, "s2").is_empty());
    }

    /// The reported regression: a session the classic screen named "A" is renamed
    /// "B" on the screen that writes names to the log. Reopening it must keep
    /// "B" — an index still saying "A" would be read as the person's latest
    /// choice and put back. And a later rename on the classic screen still wins.
    #[test]
    fn a_rename_in_the_log_is_not_undone_by_the_next_open() {
        let (_dir, manager) = store();
        let lease = created(&manager, "s1");
        manager.append_events(&lease, &a_turn()).unwrap();
        manager.rename("s1", "A").unwrap();
        manager.open_for_resume(&lease).unwrap();
        assert_eq!(
            titles(&manager, "s1").last(),
            Some(&("A".to_string(), true))
        );

        // Renamed through the log, the way `/rename` does on the new screen.
        let next = manager.load_events("s1").unwrap().last().unwrap().seq + 1;
        manager
            .append_events(
                &lease,
                &[logged(
                    next,
                    0,
                    SessionEvent::Titled {
                        turn: 1,
                        title: "B".into(),
                        user_set: true,
                    },
                )],
            )
            .unwrap();
        assert_eq!(
            manager.read_meta("s1").unwrap().name,
            "B",
            "the index follows"
        );

        manager.open_for_resume(&lease).unwrap();
        assert_eq!(
            titles(&manager, "s1").last(),
            Some(&("B".to_string(), true)),
            "reopening kept the newer name"
        );

        // The classic screen renames it again: the index's name is the newest
        // choice now, and the next open records it.
        manager.rename("s1", "C").unwrap();
        manager.open_for_resume(&lease).unwrap();
        assert_eq!(
            titles(&manager, "s1").last(),
            Some(&("C".to_string(), true))
        );
    }

    /// A fork's name is the ones it committed itself: a user-set name among
    /// the parent's inherited facts does not count as the fork's.
    #[test]
    fn a_forks_own_facts_are_what_its_name_is_read_from() {
        let named = |title: &str| SessionEvent::Titled {
            turn: 1,
            title: title.into(),
            user_set: true,
        };
        let mut meta = meta("f1");
        meta.name = "我的会话".into();
        meta.user_renamed = true;
        let inherited = vec![logged(1, 0, named("我的会话"))];
        assert!(missing_chosen_name(&meta, &inherited, 0).is_none());
        assert!(missing_chosen_name(&meta, &inherited, 1).is_some());
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
        use crate::session::presentation::{
            DisplayAnchor, PresentationEntry, PresentationFile, PresentationRole,
        };
        let (_dir, manager) = store();
        let lease = created(&manager, "s1");
        manager.append_events(&lease, &a_turn()).unwrap();
        let presentation = PresentationFile {
            entries: vec![PresentationEntry {
                anchor: DisplayAnchor::AtStart,
                role: PresentationRole::Assistant,
                text: "local command output".into(),
            }],
            ..Default::default()
        };
        manager.write_presentation("s1", &presentation).unwrap();

        let (forked, _fork_lease) = manager.fork_native_session("s1", "s2", 2_000).unwrap();
        assert_eq!(forked.meta.id, "s2");
        assert_eq!(forked.presentation, presentation);
        assert_eq!(manager.read_presentation("s1").unwrap(), presentation);
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

    #[test]
    fn a_failed_event_fork_does_not_publish_a_partial_copy() {
        let (_dir, manager) = store();
        let _source = created(&manager, "s1");
        let destination = manager.acquire_lease("s2").unwrap();
        let invalid = super::super::presentation::PresentationFile {
            v: u32::MAX,
            ..Default::default()
        };
        assert!(manager
            .fork_event_session("s1", &destination, &meta("s2"), &invalid, 2_000)
            .is_err());
        assert!(!manager.index_path("s2").unwrap().exists());
        assert!(!manager.events_path("s2").unwrap().exists());
        assert!(manager.load_native_session("s1").is_ok());
    }

    #[test]
    fn snapshot_memory_is_replayed_as_memory_not_as_a_compaction_summary() {
        let mut memory = Message::system("=== MEMORY ===\n- prefers tabs");
        memory.synthetic = true;
        let snapshot = SessionSnapshot::new(vec![
            Message::system("You are AtomCode"),
            memory,
            Message::user("hello"),
        ]);
        let events = events_from_snapshot(&snapshot, 1);
        let origin = events.iter().find_map(|logged| match &logged.event {
            SessionEvent::Injected { text, origin, .. } if text.contains("prefers tabs") => {
                Some(origin)
            }
            _ => None,
        });
        assert_eq!(origin, Some(&InjectionOrigin::Memory));
        assert!(!events.iter().any(|logged| matches!(
            &logged.event,
            SessionEvent::Injected {
                origin: InjectionOrigin::CompactionSummary,
                ..
            }
        )));
    }

    #[test]
    fn events_to_become_keeps_memory_out_of_a_summary() {
        let events = vec![
            logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
            logged(
                2,
                0,
                SessionEvent::Injected {
                    turn: 1,
                    text: "=== MEMORY ===\n- prefers tabs".into(),
                    origin: InjectionOrigin::Memory,
                },
            ),
            logged(
                3,
                0,
                SessionEvent::UserMessage {
                    turn: 1,
                    text: "one".into(),
                    images: vec![],
                },
            ),
            logged(
                4,
                0,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    round: 1,
                    text: "ok1".into(),
                    reasoning: String::new(),
                    tool_calls: vec![],
                    reasoning_blocks: vec![],
                    meta: None,
                },
            ),
            logged(5, 0, SessionEvent::TurnStart { turn: 2 }),
            logged(
                6,
                0,
                SessionEvent::UserMessage {
                    turn: 2,
                    text: "two".into(),
                    images: vec![],
                },
            ),
            logged(
                7,
                0,
                SessionEvent::AssistantMessage {
                    turn: 2,
                    round: 1,
                    text: "ok2".into(),
                    reasoning: String::new(),
                    tool_calls: vec![],
                    reasoning_blocks: vec![],
                    meta: None,
                },
            ),
        ];
        let mut memory = Message::system("=== MEMORY ===\n- prefers tabs");
        memory.synthetic = true;
        let target = vec![
            memory,
            Message::user("two"),
            Message::assistant("ok2", vec![]),
        ];
        let change = events_to_become(&events, &target);
        assert!(!change.iter().any(|event| matches!(
            event,
            SessionEvent::Compacted { summary, .. } if summary.contains("prefers tabs")
        )));
        assert!(change.iter().any(|event| matches!(
            event,
            SessionEvent::Injected {
                origin: InjectionOrigin::Memory,
                ..
            }
        )));
    }

    #[test]
    fn events_to_become_reseeds_a_memory_snapshot_instead_of_summarizing_it() {
        // The log holds no memory fact; the snapshot's leading synthetic system
        // message is a memory block. It must be reseeded as `Injected` memory, not
        // recorded as a `Compacted` summary that a later compaction could drop.
        let events = vec![
            logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
            logged(
                3,
                0,
                SessionEvent::UserMessage {
                    turn: 1,
                    text: "one".into(),
                    images: vec![],
                },
            ),
            logged(
                4,
                0,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    round: 1,
                    text: "ok1".into(),
                    reasoning: String::new(),
                    tool_calls: vec![],
                    reasoning_blocks: vec![],
                    meta: None,
                },
            ),
            logged(5, 0, SessionEvent::TurnStart { turn: 2 }),
            logged(
                6,
                0,
                SessionEvent::UserMessage {
                    turn: 2,
                    text: "two".into(),
                    images: vec![],
                },
            ),
            logged(
                7,
                0,
                SessionEvent::AssistantMessage {
                    turn: 2,
                    round: 1,
                    text: "ok2".into(),
                    reasoning: String::new(),
                    tool_calls: vec![],
                    reasoning_blocks: vec![],
                    meta: None,
                },
            ),
        ];
        let mut memory = Message::system("=== MEMORY ===\n- prefers tabs");
        memory.synthetic = true;
        let target = vec![
            memory,
            Message::user("two"),
            Message::assistant("ok2", vec![]),
        ];
        let change = events_to_become(&events, &target);
        assert!(
            !change.iter().any(|event| matches!(
                event,
                SessionEvent::Compacted { summary, .. } if summary.contains("prefers tabs")
            )),
            "memory must not become a compaction summary: {change:?}"
        );
        assert!(
            change.iter().any(|event| matches!(
                event,
                SessionEvent::Injected {
                    origin: InjectionOrigin::Memory,
                    ..
                }
            )),
            "memory is reseeded as its own fact: {change:?}"
        );
    }

    #[test]
    fn an_unanswered_prompt_is_the_one_to_replay() {
        let accepted = vec![
            logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
            logged(
                2,
                0,
                SessionEvent::Injected {
                    turn: 1,
                    text: "=== MEMORY ===\n- prefers tabs".into(),
                    origin: InjectionOrigin::Memory,
                },
            ),
            logged(
                3,
                0,
                SessionEvent::UserMessage {
                    turn: 1,
                    text: "hello".into(),
                    images: vec![],
                },
            ),
        ];
        let (to, turn, prompt) =
            unanswered_prompt(&accepted).expect("the accepted prompt is replayable");
        assert_eq!((to, turn, prompt.text.as_str()), (1, 1, "hello"));

        for notice in [
            NoticeKind::CompactionDegraded,
            NoticeKind::ConfigSkipped,
            NoticeKind::MemberNotRestored,
        ] {
            let mut events = accepted.clone();
            events.push(logged(
                4,
                0,
                SessionEvent::Notice {
                    turn: 1,
                    notice,
                    detail: String::new(),
                    retry: None,
                },
            ));
            assert!(
                unanswered_prompt(&events).is_some(),
                "a notice about the session is not model progress"
            );
        }

        for progress in [
            SessionEvent::AssistantMessage {
                turn: 1,
                round: 1,
                text: "ok".into(),
                reasoning: String::new(),
                tool_calls: vec![],
                reasoning_blocks: vec![],
                meta: None,
            },
            SessionEvent::StepStart { turn: 1, step: 1 },
            SessionEvent::TurnEnd {
                turn: 1,
                stop: atomcode_kernel::event::StopReason::Stopped,
                error: None,
            },
            SessionEvent::Interrupted {
                turn: 1,
                undone: false,
            },
            SessionEvent::Rewound {
                turn: 1,
                to: 1,
                scope: RewindScope::Conversation,
            },
            SessionEvent::Notice {
                turn: 1,
                notice: NoticeKind::ProviderRetry,
                detail: String::new(),
                retry: None,
            },
        ] {
            let mut events = accepted.clone();
            events.push(logged(4, 0, progress));
            assert!(
                unanswered_prompt(&events).is_none(),
                "a turn that made progress must not be replayed"
            );
        }
        let mut second_prompt = accepted.clone();
        second_prompt.push(logged(
            4,
            0,
            SessionEvent::UserMessage {
                turn: 1,
                text: "again".into(),
                images: vec![],
            },
        ));
        assert!(unanswered_prompt(&second_prompt).is_none());
    }

    #[test]
    fn a_withdrawal_answers_a_turn_whatever_its_sequence_says() {
        // File order is the order the log projects in; the sequence only names
        // positions. A record written after the turn but numbered below it still
        // answers it, and one written before but numbered above it still does not
        // belong to it.
        let events = vec![
            logged(5, 0, SessionEvent::TurnStart { turn: 1 }),
            logged(
                6,
                0,
                SessionEvent::UserMessage {
                    turn: 1,
                    text: "hello".into(),
                    images: vec![],
                },
            ),
            logged(
                2,
                0,
                SessionEvent::Rewound {
                    turn: 1,
                    to: 5,
                    scope: RewindScope::Conversation,
                },
            ),
        ];
        assert!(unanswered_at(&events, 5, 1).is_none());
        assert!(unanswered_prompt(&events).is_none());
        assert_eq!(turn_started_at(&events, 5), Some(1));
        assert_eq!(turn_started_at(&events, 2), None);

        let before_the_withdrawal = &events[..2];
        assert_eq!(
            unanswered_at(before_the_withdrawal, 5, 1).map(|prompt| prompt.text),
            Some("hello".to_string())
        );
    }

    #[test]
    fn a_fork_does_not_replay_its_parents_unstarted_turn() {
        let (_dir, manager) = store();
        let id = "resume-unanswered";
        let lease = created(&manager, id);
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();

        let (_forked, fork_lease) = manager.fork_native_session(id, "fork", 100).unwrap();
        manager.recover_interrupted_turn(&fork_lease).unwrap();
        let (_, pending) = manager.open_for_resume(&fork_lease).unwrap();
        assert!(
            pending.is_none(),
            "the inherited unfinished turn belongs to the parent, not the fork"
        );
        assert_eq!(
            manager
                .prepare_resume_replay(&fork_lease, &Message::user("hello"))
                .unwrap(),
            crate::session::manager::PrepareResumeReplay::Obsolete
        );
        manager
            .append_events(
                &fork_lease,
                &[
                    logged(3, 0, SessionEvent::TurnStart { turn: 2 }),
                    logged(
                        4,
                        0,
                        SessionEvent::UserMessage {
                            turn: 2,
                            text: "the fork's own prompt".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();
        manager.recover_interrupted_turn(&fork_lease).unwrap();
        let (_, pending) = manager.open_for_resume(&fork_lease).unwrap();
        assert_eq!(pending.unwrap().text, "the fork's own prompt");
    }

    #[test]
    fn a_cold_resume_replays_an_unanswered_prompt() {
        let (_dir, manager) = store();
        let id = "resume-unanswered";
        let lease = created(&manager, id);
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();

        manager.recover_interrupted_turn(&lease).unwrap();
        let (loaded, pending) = manager.open_for_resume(&lease).unwrap();
        assert_eq!(
            pending.map(|message| message.text),
            Some("hello".to_string())
        );
        assert!(
            !loaded
                .snapshot
                .messages
                .iter()
                .any(|message| message.text == "hello"),
            "the pending view excludes the prompt it will replay"
        );
        assert!(!manager
            .load_events(id)
            .unwrap()
            .iter()
            .any(|event| matches!(event.event, SessionEvent::Rewound { .. })));

        manager
            .prepare_resume_replay(&lease, &Message::user("hello"))
            .unwrap();

        let (loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert_eq!(
            pending.map(|message| message.text),
            Some("hello".to_string())
        );
        assert!(
            !loaded
                .snapshot
                .messages
                .iter()
                .any(|message| message.text == "hello"),
            "the unanswered turn is retracted once the replay is prepared"
        );
        let events = manager.load_events(id).unwrap();
        let (turn, to, scope) = events
            .iter()
            .find_map(|logged| match &logged.event {
                SessionEvent::Rewound { turn, to, scope } => Some((*turn, *to, *scope)),
                _ => None,
            })
            .expect("the prepared replay records a retraction");
        assert_eq!(turn, 1, "the retraction names the unanswered turn");
        assert_eq!(to, 1, "the retraction rewinds to that turn's TurnStart");
        assert!(matches!(scope, RewindScope::Conversation));
        assert!(
            !atomcode_kernel::session::derive_messages(&events)
                .iter()
                .any(|message| message.text == "hello"),
            "the retraction removes the prompt from the projection"
        );
    }

    #[test]
    fn resume_rewinds_an_unanswered_prompt_for_replay() {
        let (_dir, manager) = store();
        let id = "resume-unanswered";
        let lease = created(&manager, id);
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();

        let (loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert_eq!(
            pending.map(|message| message.text),
            Some("hello".to_string())
        );
        assert!(
            !loaded
                .snapshot
                .messages
                .iter()
                .any(|message| message.text == "hello"),
            "the pending view excludes the prompt it will replay"
        );
        assert!(!manager
            .load_events(id)
            .unwrap()
            .iter()
            .any(|event| matches!(event.event, SessionEvent::Rewound { .. })));

        manager
            .prepare_resume_replay(&lease, &Message::user("hello"))
            .unwrap();

        let (loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert_eq!(
            pending.map(|message| message.text),
            Some("hello".to_string())
        );
        assert!(
            !loaded
                .snapshot
                .messages
                .iter()
                .any(|message| message.text == "hello"),
            "the unanswered turn is retracted once the replay is prepared"
        );
        let events = manager.load_events(id).unwrap();
        let (turn, to, scope) = events
            .iter()
            .find_map(|logged| match &logged.event {
                SessionEvent::Rewound { turn, to, scope } => Some((*turn, *to, *scope)),
                _ => None,
            })
            .expect("the prepared replay records a retraction");
        assert_eq!(turn, 1, "the retraction names the unanswered turn");
        assert_eq!(to, 1, "the retraction rewinds to that turn's TurnStart");
        assert!(matches!(scope, RewindScope::Conversation));
        assert!(
            !atomcode_kernel::session::derive_messages(&events)
                .iter()
                .any(|message| message.text == "hello"),
            "the retraction removes the prompt from the projection"
        );
    }

    #[test]
    fn a_blank_tail_does_not_hide_the_last_record() {
        let (_dir, manager) = store();
        let id = "resume-blank-tail";
        let lease = created(&manager, id);
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();
        // A crash after a partial line, or an editor, can leave blank lines
        // after the last record. The reader skips them, so the allocator must
        // not mistake one for a record and refuse a log that reads back fine.
        let mut log = std::fs::OpenOptions::new()
            .append(true)
            .open(manager.events_path(id).unwrap())
            .unwrap();
        log.write_all(b"\n   \n\t\n").unwrap();
        drop(log);

        let (_loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert!(pending.is_some());
        assert_eq!(
            manager
                .prepare_resume_replay(&lease, &Message::user("hello"))
                .unwrap(),
            PrepareResumeReplay::Prepared
        );
        assert_eq!(
            manager.read_resume_replay(id).unwrap(),
            Some(ResumeReplay {
                to: 1,
                turn: 1,
                rewound: Some(3)
            }),
            "the rewind still takes the sequence after the last record"
        );
    }

    #[test]
    fn a_prepare_allocates_the_sequence_after_a_large_tail() {
        let (_dir, manager) = store();
        let id = "resume-large-tail";
        let lease = created(&manager, id);
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                    // Larger than the tail window the allocator scans, and a
                    // fact the replayable-turn rule tolerates.
                    logged(
                        3,
                        0,
                        SessionEvent::Injected {
                            turn: 1,
                            text: "x".repeat(10 * 1024),
                            origin: atomcode_kernel::session::InjectionOrigin::Continuation,
                        },
                    ),
                ],
            )
            .unwrap();

        let (_loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert!(pending.is_some());
        assert_eq!(
            manager
                .prepare_resume_replay(&lease, &Message::user("hello"))
                .unwrap(),
            PrepareResumeReplay::Prepared
        );
        assert_eq!(
            manager.read_resume_replay(id).unwrap(),
            Some(ResumeReplay {
                to: 1,
                turn: 1,
                rewound: Some(4)
            }),
            "the rewind takes the sequence after the log's actual last record"
        );
    }

    #[test]
    fn an_allocation_never_reuses_a_sequence_the_log_already_holds() {
        let (_dir, manager) = store();
        let id = "resume-out-of-order-tail";
        let lease = created(&manager, id);
        manager.append_events(&lease, &a_turn()).unwrap();
        // The last record is not the file's maximum: an earlier build wrote the
        // tail out of order. Allocating after the tail alone would reuse a
        // sequence the log already holds, so the caller's read of the log is
        // the floor.
        let mut log = std::fs::OpenOptions::new()
            .append(true)
            .open(manager.events_path(id).unwrap())
            .unwrap();
        log.write_all(b"{\"seq\":3,\"at\":0,\"event\":{\"kind\":\"user_message\",\"turn\":1,\"text\":\"late\",\"images\":[]}}\n")
            .unwrap();
        drop(log);

        let mut allocated = None;
        manager
            .append_event_allocating(
                &lease,
                5,
                |seq| {
                    allocated = Some(seq);
                    Ok(logged(seq, 0, SessionEvent::TurnStart { turn: 2 }))
                },
                |_| Ok(()),
            )
            .unwrap();
        assert_eq!(
            allocated,
            Some(6),
            "the allocation is one past the log's maximum, not its tail"
        );
        assert_eq!(manager.load_events(id).unwrap().last().unwrap().seq, 6);
    }

    #[test]
    fn an_allocation_walks_back_a_last_record_larger_than_a_tail_window() {
        let (_dir, manager) = store();
        let id = "resume-long-tail";
        let lease = created(&manager, id);
        manager.append_events(&lease, &a_turn()).unwrap();
        // A record wider than the scan window still ends the file; the scan
        // walks back to its start instead of assuming the whole line is in view.
        manager
            .append_events(
                &lease,
                &[logged(
                    6,
                    15,
                    SessionEvent::UserMessage {
                        turn: 2,
                        text: "x".repeat(200 * 1024),
                        images: vec![],
                    },
                )],
            )
            .unwrap();
        // A crash's trailing blank line is skipped the way the reader skips it.
        let mut log = std::fs::OpenOptions::new()
            .append(true)
            .open(manager.events_path(id).unwrap())
            .unwrap();
        log.write_all(b"\n\n").unwrap();
        drop(log);

        let mut allocated = None;
        manager
            .append_event_allocating(
                &lease,
                5,
                |seq| {
                    allocated = Some(seq);
                    Ok(logged(seq, 16, SessionEvent::TurnStart { turn: 3 }))
                },
                |_| Ok(()),
            )
            .unwrap();
        assert_eq!(allocated, Some(7));
        assert_eq!(manager.load_events(id).unwrap().last().unwrap().seq, 7);
    }

    #[test]
    fn a_blank_tail_wider_than_the_scan_window_does_not_hide_the_last_record() {
        let (_dir, manager) = store();
        let id = "resume-wide-blank-tail";
        let lease = created(&manager, id);
        manager.append_events(&lease, &a_turn()).unwrap();
        // More blank bytes than the tail window holds: the first read sees only
        // blanks, so the allocator has to widen its read to reach the record.
        let mut log = std::fs::OpenOptions::new()
            .append(true)
            .open(manager.events_path(id).unwrap())
            .unwrap();
        log.write_all(&vec![b'\n'; 100 * 1024]).unwrap();
        drop(log);

        let mut allocated = None;
        manager
            .append_event_allocating(
                &lease,
                5,
                |seq| {
                    allocated = Some(seq);
                    Ok(logged(seq, 0, SessionEvent::TurnStart { turn: 2 }))
                },
                |_| Ok(()),
            )
            .unwrap();
        assert_eq!(allocated, Some(6));
        assert_eq!(manager.load_events(id).unwrap().last().unwrap().seq, 6);
    }

    #[test]
    fn a_tail_a_failed_write_left_behind_is_ignored_and_cut_off() {
        let (_dir, manager) = store();
        let id = "resume-torn-tail";
        let lease = created(&manager, id);
        manager.append_events(&lease, &a_turn()).unwrap();
        let path = manager.events_path(id).unwrap();
        let tear = || {
            let mut log = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            // The prefix a write cut short leaves behind: no newline, so not a
            // line this store completed.
            log.write_all(
                b"{\"seq\":6,\"at\":16,\"event\":{\"kind\":\"user_message\",\"turn\":3,\"text\":\"TORN-RESIDUE",
            )
            .unwrap();
        };

        tear();
        // One failed write must not strand the log: the reader ignores the
        // prefix and still hands back every completed record.
        assert_eq!(manager.load_events(id).unwrap().last().unwrap().seq, 5);

        let mut allocated = None;
        manager
            .append_event_allocating(
                &lease,
                5,
                |seq| {
                    allocated = Some(seq);
                    Ok(logged(seq, 17, SessionEvent::TurnStart { turn: 3 }))
                },
                |_| Ok(()),
            )
            .unwrap();
        assert_eq!(allocated, Some(6));
        let raw = std::fs::read(&path).unwrap();
        assert!(
            !raw.windows(12).any(|window| window == b"TORN-RESIDUE"),
            "the prefix is cut off instead of being glued to the new record"
        );
        assert_eq!(raw.last(), Some(&b'\n'));
        assert_eq!(manager.load_events(id).unwrap().last().unwrap().seq, 6);

        // The ordinary append path repairs it the same way.
        tear();
        manager
            .append_events(
                &lease,
                &[logged(7, 18, SessionEvent::TurnStart { turn: 4 })],
            )
            .unwrap();
        let raw = std::fs::read(&path).unwrap();
        assert!(!raw.windows(12).any(|window| window == b"TORN-RESIDUE"));
        assert_eq!(manager.load_events(id).unwrap().last().unwrap().seq, 7);
    }

    #[test]
    fn a_resumable_prompt_is_exactly_a_message_the_log_would_hold() {
        assert!(is_resumable_prompt(&Message::user("hello")));
        assert!(is_resumable_prompt(&Message::user_with_images(
            "hello",
            vec![]
        )));
        // Every other shape the loader can meet at the checkpoint's tail is not
        // a turn the log can hold as the person's prompt.
        assert!(!is_resumable_prompt(&Message::synthetic_user("note")));
        assert!(!is_resumable_prompt(&Message::tool_result(
            "call-1", "out", false
        )));
        assert!(!is_resumable_prompt(&Message::assistant("answer", vec![])));
        let mut tool_call_id = Message::user("hello");
        tool_call_id.tool_call_id = Some("call-1".into());
        assert!(!is_resumable_prompt(&tool_call_id));
        let mut injected = Message::user("hello");
        injected.internal_origin = Some("continuation".into());
        assert!(!is_resumable_prompt(&injected));
    }

    #[test]
    fn loading_for_resume_does_not_retract_the_prompt() {
        let (_dir, manager) = store();
        let id = "resume-preview";
        let lease = created(&manager, id);
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();

        for _ in 0..2 {
            let (loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
            assert_eq!(
                pending.map(|message| message.text),
                Some("hello".to_string())
            );
            assert!(
                !loaded
                    .snapshot
                    .messages
                    .iter()
                    .any(|message| message.text == "hello"),
                "the pending view excludes the prompt it will replay"
            );
            assert_eq!(
                manager.read_resume_replay(id).unwrap(),
                Some(ResumeReplay {
                    to: 1,
                    turn: 1,
                    rewound: None
                })
            );
            assert!(!manager
                .load_events(id)
                .unwrap()
                .iter()
                .any(|event| matches!(event.event, SessionEvent::Rewound { .. })));
        }
    }

    #[test]
    fn an_unreadable_replay_marker_is_replaced_by_a_readable_one() {
        let (_dir, manager) = store();
        let id = "resume-unreadable-marker";
        let lease = created(&manager, id);
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();
        // Another build's marker, or a damaged one: this build cannot read it,
        // so it cannot be left to anchor a retry.
        std::fs::write(manager.root().join(format!("{id}.replay")), b"not a replay").unwrap();

        let mut notes = Vec::new();
        let (_loaded, pending) = manager.open_for_resume_noting(&lease, &mut notes).unwrap();
        assert_eq!(
            pending.map(|message| message.text),
            Some("hello".to_string())
        );
        assert!(
            notes.iter().any(|note| note.contains("could not be read")),
            "the unreadable marker is reported: {notes:?}"
        );
        // The prompt is still in the log, and the replacement names this load's
        // target, so the replay it enables retracts exactly that turn.
        assert_eq!(
            manager.read_resume_replay(id).unwrap(),
            Some(ResumeReplay {
                to: 1,
                turn: 1,
                rewound: None
            })
        );
        assert_eq!(
            manager
                .prepare_resume_replay(&lease, &Message::user("hello"))
                .unwrap(),
            PrepareResumeReplay::Prepared
        );
        assert_eq!(
            manager.read_resume_replay(id).unwrap(),
            Some(ResumeReplay {
                to: 1,
                turn: 1,
                rewound: Some(3)
            })
        );
    }

    #[test]
    fn resume_does_not_replay_an_answered_turn() {
        let (_dir, manager) = store();
        let id = "resume-answered";
        let lease = created(&manager, id);
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                    logged(
                        3,
                        0,
                        SessionEvent::AssistantMessage {
                            turn: 1,
                            round: 1,
                            text: "ok".into(),
                            reasoning: String::new(),
                            tool_calls: vec![],
                            reasoning_blocks: vec![],
                            meta: None,
                        },
                    ),
                    logged(
                        4,
                        0,
                        SessionEvent::TurnEnd {
                            turn: 1,
                            stop: atomcode_kernel::event::StopReason::Stopped,
                            error: None,
                        },
                    ),
                ],
            )
            .unwrap();

        let (loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert!(pending.is_none());
        assert!(loaded
            .snapshot
            .messages
            .iter()
            .any(|message| message.text == "hello"));
        assert!(!manager
            .load_events(id)
            .unwrap()
            .iter()
            .any(|event| matches!(event.event, SessionEvent::Rewound { .. })));
    }

    #[cfg(feature = "tools")]
    #[test]
    fn a_fork_drops_a_sidecar_position_past_its_own_turns() {
        use crate::session::manager::{TodoCallPosition, TodoSidecarItem};

        let (_dir, manager) = store();
        let lease = created(&manager, "s1");
        manager.append_events(&lease, &a_turn()).unwrap();
        manager
            .write_todo_sidecar(
                "s1",
                &[TodoSidecarItem {
                    content: "a".into(),
                    status: "in_progress".into(),
                }],
                4,
                Some(TodoCallPosition {
                    turn: 9,
                    round: 1,
                    index: 0,
                }),
            )
            .unwrap();

        let (_forked, _fork_lease) = manager.fork_native_session("s1", "s2", 2_000).unwrap();

        let sidecar = manager
            .read_todo_sidecar("s2")
            .unwrap()
            .expect("the items still travel");
        assert_eq!(sidecar.todos.len(), 1);
        assert_eq!(sidecar.todos[0].content, "a");
        assert_eq!(sidecar.through, None);
    }

    #[cfg(feature = "tools")]
    #[test]
    fn a_sidecar_only_plan_survives_an_undo_of_its_through_turn() {
        use crate::session::manager::{TodoCallPosition, TodoSidecarItem};

        let (_dir, manager) = store();
        let id = "sidecar-only-plan-undone";
        let lease = created(&manager, id);
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "first".into(),
                            images: vec![],
                        },
                    ),
                    logged(
                        3,
                        0,
                        SessionEvent::AssistantMessage {
                            turn: 1,
                            round: 1,
                            text: "ok".into(),
                            reasoning: String::new(),
                            tool_calls: vec![],
                            reasoning_blocks: vec![],
                            meta: None,
                        },
                    ),
                    logged(
                        4,
                        0,
                        SessionEvent::TurnEnd {
                            turn: 1,
                            stop: StopReason::Stopped,
                            error: None,
                        },
                    ),
                ],
            )
            .unwrap();
        let through = Some(TodoCallPosition {
            turn: 1,
            round: 1,
            index: 0,
        });
        manager
            .write_todo_sidecar(
                id,
                &[TodoSidecarItem {
                    content: "keep me".into(),
                    status: "in_progress".into(),
                }],
                3,
                through,
            )
            .unwrap();

        // The plan never appears in this log (a converted/compacted session);
        // undoing the turn its through points at must not erase the only copy.
        manager
            .append_events(
                &lease,
                &[logged(
                    5,
                    0,
                    SessionEvent::Rewound {
                        turn: 1,
                        to: 1,
                        scope: RewindScope::Conversation,
                    },
                )],
            )
            .unwrap();

        let sidecar = manager
            .read_todo_sidecar(id)
            .unwrap()
            .expect("a sidecar-only plan is not evidence of a taken-back plan");
        assert_eq!(sidecar.todos.len(), 1);
        assert_eq!(sidecar.todos[0].content, "keep me");
        assert_eq!(sidecar.through, through);
    }

    /// A session whose turn 1 plans two tasks and turn 2 completes the second,
    /// with the sidecar already reflecting that completion.
    #[cfg(feature = "tools")]
    fn todo_session(manager: &SessionManager, id: &str) -> SessionLease {
        use crate::session::manager::{TodoCallPosition, TodoSidecarItem};
        use atomcode_kernel::tool::ToolCall;

        let lease = created(manager, id);
        let plan = ToolCall {
            id: "p".into(),
            name: "todowrite".into(),
            arguments: r#"{"todos":[{"content":"a","status":"pending"},{"content":"b","status":"in_progress"}]}"#.into(),
        };
        let update = ToolCall {
            id: "u".into(),
            name: "todowrite".into(),
            arguments: r#"{"action":"update","id":2,"status":"completed"}"#.into(),
        };
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::AssistantMessage {
                            turn: 1,
                            round: 1,
                            text: String::new(),
                            reasoning: String::new(),
                            tool_calls: vec![plan],
                            reasoning_blocks: vec![],
                            meta: None,
                        },
                    ),
                    logged(
                        3,
                        0,
                        SessionEvent::ToolResultLogged {
                            turn: 1,
                            round: 1,
                            call_id: "p".into(),
                            content: "#1 [ ] a\n#2 [~] b".into(),
                            is_error: false,
                            images: vec![],
                        },
                    ),
                    logged(4, 0, SessionEvent::TurnStart { turn: 2 }),
                    logged(
                        5,
                        0,
                        SessionEvent::AssistantMessage {
                            turn: 2,
                            round: 1,
                            text: String::new(),
                            reasoning: String::new(),
                            tool_calls: vec![update],
                            reasoning_blocks: vec![],
                            meta: None,
                        },
                    ),
                    logged(
                        6,
                        0,
                        SessionEvent::ToolResultLogged {
                            turn: 2,
                            round: 1,
                            call_id: "u".into(),
                            content: "#2 [x] b".into(),
                            is_error: false,
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();
        manager
            .write_todo_sidecar(
                id,
                &[
                    TodoSidecarItem {
                        content: "a".into(),
                        status: "pending".into(),
                    },
                    TodoSidecarItem {
                        content: "b".into(),
                        status: "completed".into(),
                    },
                ],
                6,
                Some(TodoCallPosition {
                    turn: 2,
                    round: 1,
                    index: 0,
                }),
            )
            .unwrap();
        lease
    }

    #[test]
    fn resume_keeps_an_unreplayed_prompt_recoverable() {
        let (_dir, manager) = store();
        let id = "resume-replay";
        let lease = created(&manager, id);
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();

        let (loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert_eq!(
            pending.map(|message| message.text),
            Some("hello".to_string())
        );
        assert!(
            !loaded
                .snapshot
                .messages
                .iter()
                .any(|message| message.text == "hello"),
            "the pending view excludes the prompt it will replay"
        );
        assert_eq!(
            manager.read_resume_replay(id).unwrap(),
            Some(ResumeReplay {
                to: 1,
                turn: 1,
                rewound: None
            })
        );
        manager
            .prepare_resume_replay(&lease, &Message::user("hello"))
            .unwrap();

        let (loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert_eq!(
            pending.map(|message| message.text),
            Some("hello".to_string())
        );
        assert!(
            !loaded
                .snapshot
                .messages
                .iter()
                .any(|message| message.text == "hello"),
            "the unanswered turn is retracted once the replay is prepared"
        );
        assert_eq!(
            manager.read_resume_replay(id).unwrap(),
            Some(ResumeReplay {
                to: 1,
                turn: 1,
                rewound: Some(3),
            })
        );
        let rewinds = || {
            manager
                .load_events(id)
                .unwrap()
                .iter()
                .filter(|event| matches!(event.event, SessionEvent::Rewound { .. }))
                .count()
        };
        assert_eq!(rewinds(), 1);

        // The replayed turn never landed: a second death must hand the prompt
        // back again, not leave it hidden behind the first retraction.
        let (_loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert_eq!(
            pending.map(|message| message.text),
            Some("hello".to_string())
        );
        assert_eq!(rewinds(), 1, "a retry must not append another rewind");
        manager
            .prepare_resume_replay(&lease, &Message::user("hello"))
            .unwrap();
        assert_eq!(
            rewinds(),
            1,
            "preparing an already-prepared replay is a no-op"
        );

        // A complete turn after the rewind means the replay was accepted.
        manager
            .append_events(
                &lease,
                &[
                    logged(4, 0, SessionEvent::TurnStart { turn: 2 }),
                    logged(
                        5,
                        0,
                        SessionEvent::UserMessage {
                            turn: 2,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                    logged(
                        6,
                        0,
                        SessionEvent::AssistantMessage {
                            turn: 2,
                            round: 1,
                            text: "ok".into(),
                            reasoning: String::new(),
                            tool_calls: vec![],
                            reasoning_blocks: vec![],
                            meta: None,
                        },
                    ),
                    logged(
                        7,
                        0,
                        SessionEvent::TurnEnd {
                            turn: 2,
                            stop: StopReason::Stopped,
                            error: None,
                        },
                    ),
                ],
            )
            .unwrap();
        let (_loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert!(pending.is_none());
        assert!(manager.read_resume_replay(id).unwrap().is_none());
    }

    #[test]
    fn a_marker_without_its_rewind_is_retried() {
        let (_dir, manager) = store();
        let id = "resume-replay-ghost";
        let lease = created(&manager, id);
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();
        manager
            .write_resume_replay(
                id,
                &ResumeReplay {
                    to: 1,
                    turn: 1,
                    rewound: None,
                },
            )
            .unwrap();

        let (loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert_eq!(
            pending.map(|message| message.text),
            Some("hello".to_string())
        );
        assert!(
            !loaded
                .snapshot
                .messages
                .iter()
                .any(|message| message.text == "hello"),
            "the pending view excludes the prompt it will replay"
        );
        assert_eq!(
            manager.read_resume_replay(id).unwrap(),
            Some(ResumeReplay {
                to: 1,
                turn: 1,
                rewound: None
            })
        );
        assert!(!manager
            .load_events(id)
            .unwrap()
            .iter()
            .any(|event| matches!(event.event, SessionEvent::Rewound { .. })));

        manager
            .prepare_resume_replay(&lease, &Message::user("hello"))
            .unwrap();

        let (_loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert_eq!(
            pending.map(|message| message.text),
            Some("hello".to_string())
        );
        assert_eq!(
            manager.read_resume_replay(id).unwrap(),
            Some(ResumeReplay {
                to: 1,
                turn: 1,
                rewound: Some(3),
            })
        );
        assert_eq!(
            manager
                .load_events(id)
                .unwrap()
                .iter()
                .filter(|event| matches!(event.event, SessionEvent::Rewound { .. }))
                .count(),
            1
        );
    }

    #[cfg(feature = "tools")]
    fn assert_todo_reverted(manager: &SessionManager, id: &str) {
        use crate::session::manager::TodoCallPosition;

        let sidecar = manager
            .read_todo_sidecar(id)
            .unwrap()
            .expect("a list survives the undo");
        let shown: Vec<(&str, &str)> = sidecar
            .todos
            .iter()
            .map(|todo| (todo.content.as_str(), todo.status.as_str()))
            .collect();
        assert_eq!(shown, vec![("a", "pending"), ("b", "in_progress")]);
        assert_eq!(
            sidecar.through,
            Some(TodoCallPosition {
                turn: 1,
                round: 1,
                index: 0,
            })
        );
    }

    #[cfg(feature = "tools")]
    #[test]
    fn a_legacy_sidecar_without_a_position_does_not_double_apply_a_log_add() {
        use crate::session::manager::TodoSidecarItem;
        use atomcode_kernel::tool::ToolCall;

        let (_dir, manager) = store();
        let id = "legacy-sidecar-add";
        let lease = created(&manager, id);
        let add = ToolCall {
            id: "a".into(),
            name: "todowrite".into(),
            arguments: r#"{"action":"add","content":"new"}"#.into(),
        };
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::AssistantMessage {
                            turn: 1,
                            round: 1,
                            text: String::new(),
                            reasoning: String::new(),
                            tool_calls: vec![add],
                            reasoning_blocks: vec![],
                            meta: None,
                        },
                    ),
                    logged(
                        3,
                        0,
                        SessionEvent::ToolResultLogged {
                            turn: 1,
                            round: 1,
                            call_id: "a".into(),
                            content: "#1 [ ] new".into(),
                            is_error: false,
                            images: vec![],
                        },
                    ),
                    logged(
                        4,
                        0,
                        SessionEvent::TurnEnd {
                            turn: 1,
                            stop: StopReason::Stopped,
                            error: None,
                        },
                    ),
                    logged(5, 0, SessionEvent::TurnStart { turn: 2 }),
                    logged(
                        6,
                        0,
                        SessionEvent::UserMessage {
                            turn: 2,
                            text: "later".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();
        manager
            .write_todo_sidecar(
                id,
                &[TodoSidecarItem {
                    content: "new".into(),
                    status: "pending".into(),
                }],
                2,
                None,
            )
            .unwrap();

        // Rewinding turn 2 triggers a rebuild; the legacy sidecar cannot say
        // which calls it already reflects, so the log's add must not append a
        // second copy on top of it.
        manager
            .append_events(
                &lease,
                &[logged(
                    7,
                    0,
                    SessionEvent::Rewound {
                        turn: 2,
                        to: 5,
                        scope: RewindScope::Conversation,
                    },
                )],
            )
            .unwrap();

        let sidecar = manager.read_todo_sidecar(id).unwrap().unwrap();
        assert_eq!(sidecar.todos.len(), 1);
        assert_eq!(sidecar.todos[0].content, "new");
        assert!(sidecar.through.is_some());
    }

    #[cfg(feature = "tools")]
    #[test]
    fn a_rewind_rebuilds_the_todo_sidecar() {
        let (_dir, manager) = store();
        let lease = todo_session(&manager, "rewind-todos");
        manager
            .append_events(
                &lease,
                &[logged(
                    7,
                    0,
                    SessionEvent::Rewound {
                        turn: 2,
                        to: 4,
                        scope: RewindScope::Conversation,
                    },
                )],
            )
            .unwrap();

        assert_todo_reverted(&manager, "rewind-todos");
    }

    #[test]
    fn a_user_undo_with_the_same_target_before_prepare_is_not_replayed() {
        let (_dir, manager) = store();
        let id = "resume-user-undo-before";
        let lease = created(&manager, id);
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();
        // A load records the target but no rewind; a user's undo of that same
        // turn must not be mistaken for the resume's own retraction.
        let (_loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert!(pending.is_some());
        manager
            .append_events(
                &lease,
                &[logged(
                    3,
                    0,
                    SessionEvent::Rewound {
                        turn: 1,
                        to: 1,
                        scope: RewindScope::Conversation,
                    },
                )],
            )
            .unwrap();

        let (_loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert!(pending.is_none());
        assert!(manager.read_resume_replay(id).unwrap().is_none());
    }

    #[test]
    fn a_user_undo_with_the_same_target_after_prepare_retires_the_replay() {
        let (_dir, manager) = store();
        let id = "resume-user-undo-after";
        let lease = created(&manager, id);
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();
        let (_loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert!(pending.is_some());
        manager
            .prepare_resume_replay(&lease, &Message::user("hello"))
            .unwrap();
        // The person undoes the same turn after resume prepared its own rewind.
        manager
            .append_events(
                &lease,
                &[logged(
                    4,
                    0,
                    SessionEvent::Rewound {
                        turn: 1,
                        to: 1,
                        scope: RewindScope::Conversation,
                    },
                )],
            )
            .unwrap();

        let (_loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert!(pending.is_none());
        assert!(manager.read_resume_replay(id).unwrap().is_none());
    }

    #[test]
    fn a_user_rewind_without_a_marker_is_not_replayed() {
        let (_dir, manager) = store();
        let id = "resume-user-rewind";
        let lease = created(&manager, id);
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                    logged(
                        3,
                        0,
                        SessionEvent::Rewound {
                            turn: 1,
                            to: 1,
                            scope: RewindScope::Conversation,
                        },
                    ),
                ],
            )
            .unwrap();

        let (_loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert!(pending.is_none());
        assert!(manager.read_resume_replay(id).unwrap().is_none());
    }

    #[test]
    fn a_replay_turn_lost_before_its_answer_is_replayed_again() {
        let (_dir, manager) = store();
        let id = "resume-replay-chain";
        let lease = created(&manager, id);
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();
        let (_loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert!(pending.is_some());
        manager
            .prepare_resume_replay(&lease, &Message::user("hello"))
            .unwrap();

        // The replay's turn was accepted, then the process died before answering.
        manager
            .append_events(
                &lease,
                &[
                    logged(4, 0, SessionEvent::TurnStart { turn: 2 }),
                    logged(
                        5,
                        0,
                        SessionEvent::UserMessage {
                            turn: 2,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();

        let (_loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert_eq!(
            pending.map(|message| message.text),
            Some("hello".to_string())
        );
        manager
            .prepare_resume_replay(&lease, &Message::user("hello"))
            .unwrap();

        let (_loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert_eq!(
            pending.map(|message| message.text),
            Some("hello".to_string())
        );
        assert_eq!(
            manager.read_resume_replay(id).unwrap(),
            Some(ResumeReplay {
                to: 4,
                turn: 2,
                rewound: Some(6),
            })
        );
        assert_eq!(
            manager
                .load_events(id)
                .unwrap()
                .iter()
                .filter(|event| matches!(event.event, SessionEvent::Rewound { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn prepare_does_not_retract_a_newer_turn_the_resume_did_not_name() {
        let (_dir, manager) = store();
        let id = "resume-replay-moved";
        let lease = created(&manager, id);
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();
        let (_loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert!(pending.is_some());

        // The person typed a new message before the replay was sent.
        manager
            .append_events(
                &lease,
                &[
                    logged(3, 0, SessionEvent::TurnStart { turn: 2 }),
                    logged(
                        4,
                        0,
                        SessionEvent::UserMessage {
                            turn: 2,
                            text: "new message".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();

        assert!(
            matches!(
                manager.prepare_resume_replay(&lease, &Message::user("hello")),
                Ok(PrepareResumeReplay::Obsolete)
            ),
            "the marker names turn 1; a newer turn must not be retracted"
        );
        assert!(!manager
            .load_events(id)
            .unwrap()
            .iter()
            .any(|event| matches!(event.event, SessionEvent::Rewound { .. })));
    }

    #[test]
    fn prepare_after_the_replay_started_does_not_retract_again() {
        let (_dir, manager) = store();
        let id = "resume-replay-started";
        let lease = created(&manager, id);
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();
        let (_loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert!(pending.is_some());
        manager
            .prepare_resume_replay(&lease, &Message::user("hello"))
            .unwrap();

        // The replay's turn was accepted; a retry must not rewind it.
        manager
            .append_events(
                &lease,
                &[
                    logged(4, 0, SessionEvent::TurnStart { turn: 2 }),
                    logged(
                        5,
                        0,
                        SessionEvent::UserMessage {
                            turn: 2,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();
        assert!(matches!(
            manager.prepare_resume_replay(&lease, &Message::user("hello")),
            Ok(PrepareResumeReplay::Obsolete)
        ));
        assert_eq!(
            manager
                .load_events(id)
                .unwrap()
                .iter()
                .filter(|event| matches!(event.event, SessionEvent::Rewound { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn a_marker_pointing_at_another_turns_start_is_not_retracted() {
        let (_dir, manager) = store();
        let id = "resume-replay-retargeted";
        let lease = created(&manager, id);
        // A truncated and rewritten log can reuse the sequence a marker names.
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 2 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();
        manager
            .write_resume_replay(
                id,
                &ResumeReplay {
                    to: 1,
                    turn: 1,
                    rewound: None,
                },
            )
            .unwrap();

        assert!(
            matches!(
                manager.prepare_resume_replay(&lease, &Message::user("hello")),
                Ok(PrepareResumeReplay::Obsolete)
            ),
            "the marker no longer names this turn's TurnStart"
        );
        assert!(manager.read_resume_replay(id).unwrap().is_none());
        assert_eq!(
            manager
                .load_events(id)
                .unwrap()
                .iter()
                .filter(|event| matches!(event.event, SessionEvent::Rewound { .. }))
                .count(),
            0
        );
    }

    #[test]
    fn a_corrupt_replay_marker_does_not_hide_the_still_visible_prompt() {
        let (dir, manager) = store();
        let id = "resume-replay-corrupt";
        let lease = created(&manager, id);
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();
        // A first load writes a marker; corrupt it before the next load.
        let (_loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert!(pending.is_some());
        std::fs::write(
            dir.path().join(BUCKET).join(format!("{id}.replay")),
            b"not a marker",
        )
        .unwrap();

        let (loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert_eq!(
            pending.map(|message| message.text),
            Some("hello".to_string())
        );
        assert!(
            !loaded
                .snapshot
                .messages
                .iter()
                .any(|message| message.text == "hello"),
            "the pending view excludes the prompt it will replay"
        );
        assert_eq!(
            manager.read_resume_replay(id).unwrap(),
            Some(ResumeReplay {
                to: 1,
                turn: 1,
                rewound: None
            })
        );
        assert!(!manager
            .load_events(id)
            .unwrap()
            .iter()
            .any(|event| matches!(event.event, SessionEvent::Rewound { .. })));
    }

    #[test]
    fn prepare_without_a_marker_does_not_replay_an_answered_prompt() {
        let (_dir, manager) = store();
        let id = "resume-replay-no-marker";
        let lease = created(&manager, id);
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();
        let (_loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert!(pending.is_some());
        // The marker is a retry aid. Without it, a prompt the log no longer
        // shows unanswered must not be sent again unretracted.
        manager.clear_resume_replay(id).unwrap();
        manager
            .append_events(
                &lease,
                &[
                    logged(
                        3,
                        0,
                        SessionEvent::AssistantMessage {
                            turn: 1,
                            round: 1,
                            text: "ok".into(),
                            reasoning: String::new(),
                            tool_calls: vec![],
                            reasoning_blocks: vec![],
                            meta: None,
                        },
                    ),
                    logged(
                        4,
                        0,
                        SessionEvent::TurnEnd {
                            turn: 1,
                            stop: StopReason::Stopped,
                            error: None,
                        },
                    ),
                ],
            )
            .unwrap();

        assert!(matches!(
            manager.prepare_resume_replay(&lease, &Message::user("hello")),
            Ok(PrepareResumeReplay::Obsolete)
        ));
        assert!(!manager
            .load_events(id)
            .unwrap()
            .iter()
            .any(|event| matches!(event.event, SessionEvent::Rewound { .. })));
    }

    #[test]
    fn a_mismatched_prompt_keeps_the_committed_retraction() {
        let (_dir, manager) = store();
        let id = "resume-replay-kept";
        let lease = created(&manager, id);
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                    logged(
                        3,
                        0,
                        SessionEvent::Rewound {
                            turn: 1,
                            to: 1,
                            scope: RewindScope::Conversation,
                        },
                    ),
                ],
            )
            .unwrap();
        manager
            .write_resume_replay(
                id,
                &ResumeReplay {
                    to: 1,
                    turn: 1,
                    rewound: Some(3),
                },
            )
            .unwrap();

        // A copy that is not the retracted prompt must not be sent, and must
        // not take the marker — the only record of the retracted prompt — with
        // it.
        assert_eq!(
            manager
                .prepare_resume_replay(&lease, &Message::user("other"))
                .unwrap(),
            PrepareResumeReplay::Obsolete
        );
        assert_eq!(
            manager.read_resume_replay(id).unwrap(),
            Some(ResumeReplay {
                to: 1,
                turn: 1,
                rewound: Some(3)
            })
        );
        assert_eq!(
            manager
                .load_events(id)
                .unwrap()
                .iter()
                .filter(|event| matches!(event.event, SessionEvent::Rewound { .. }))
                .count(),
            1
        );

        // The next resume hands the log's prompt back, and preparing it is a
        // no-op on the rewind already committed.
        let (_loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert_eq!(
            pending.map(|message| message.text),
            Some("hello".to_string())
        );
        assert_eq!(
            manager
                .prepare_resume_replay(&lease, &Message::user("hello"))
                .unwrap(),
            PrepareResumeReplay::Prepared
        );
    }

    #[test]
    fn an_unreadable_marker_does_not_strand_the_visible_prompt() {
        let (dir, manager) = store();
        let id = "resume-replay-unreadable";
        let lease = created(&manager, id);
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();
        let (_loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert!(pending.is_some());
        std::fs::write(
            dir.path().join(BUCKET).join(format!("{id}.replay")),
            b"not a marker",
        )
        .unwrap();

        // A marker that cannot be read must not block a prompt the log still
        // shows unanswered: the log decides and a fresh marker replaces it.
        assert_eq!(
            manager
                .prepare_resume_replay(&lease, &Message::user("hello"))
                .unwrap(),
            PrepareResumeReplay::Prepared
        );
        assert_eq!(
            manager.read_resume_replay(id).unwrap(),
            Some(ResumeReplay {
                to: 1,
                turn: 1,
                rewound: Some(3)
            })
        );
        assert_eq!(
            manager
                .load_events(id)
                .unwrap()
                .iter()
                .filter(|event| matches!(event.event, SessionEvent::Rewound { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn an_unreadable_marker_does_not_retract_a_different_prompt() {
        let (dir, manager) = store();
        let id = "resume-replay-unreadable-moved";
        let lease = created(&manager, id);
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();
        let (_loaded, pending) = manager.load_native_session_for_resume(&lease).unwrap();
        assert!(pending.is_some());
        std::fs::write(
            dir.path().join(BUCKET).join(format!("{id}.replay")),
            b"not a marker",
        )
        .unwrap();
        // The person typed a new message before the replay was sent.
        manager
            .append_events(
                &lease,
                &[
                    logged(3, 0, SessionEvent::TurnStart { turn: 2 }),
                    logged(
                        4,
                        0,
                        SessionEvent::UserMessage {
                            turn: 2,
                            text: "new message".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();

        assert!(matches!(
            manager.prepare_resume_replay(&lease, &Message::user("hello")),
            Ok(PrepareResumeReplay::Obsolete)
        ));
        assert!(!manager
            .load_events(id)
            .unwrap()
            .iter()
            .any(|event| matches!(event.event, SessionEvent::Rewound { .. })));
    }

    #[test]
    fn a_converted_sessions_inflight_prompt_becomes_a_log_turn() {
        let (_dir, manager) = store();
        let (lease, snapshot) = snapshot_session(&manager, "s1");
        let mut inflight = snapshot.clone();
        inflight
            .messages
            .push(Message::user("continue after crash"));
        manager
            .save_inflight_snapshot("s1", &inflight, true)
            .unwrap();

        let (loaded, pending) = manager.open_for_resume(&lease).unwrap();
        assert_eq!(
            pending.map(|message| message.text),
            Some("continue after crash".to_string())
        );
        assert!(
            !loaded
                .snapshot
                .messages
                .iter()
                .any(|message| message.text == "continue after crash"),
            "the pending view excludes the prompt it will replay"
        );

        let events = manager.load_events("s1").unwrap();
        assert!(
            events.iter().any(|logged| matches!(
                &logged.event,
                SessionEvent::UserMessage { text, .. } if text == "continue after crash"
            )),
            "the accepted prompt is a fact of the log, not only of the checkpoint"
        );
        assert!(
            events.iter().any(|logged| matches!(
                &logged.event,
                SessionEvent::TurnStart { turn } if *turn == 4
            )),
            "the prompt continues the session's turn numbering"
        );

        assert_eq!(
            manager
                .prepare_resume_replay(&lease, &Message::user("continue after crash"))
                .unwrap(),
            PrepareResumeReplay::Prepared
        );
        // The retraction is a fact now: a second resume hands the same prompt
        // back without depending on the checkpoint, which conversion moved aside.
        let (_loaded, pending) = manager.open_for_resume(&lease).unwrap();
        assert_eq!(
            pending.map(|message| message.text),
            Some("continue after crash".to_string())
        );
    }

    #[test]
    fn a_converted_sessions_tail_that_is_not_a_prompt_is_not_a_log_turn() {
        let (_dir, manager) = store();
        let (lease, _snapshot) = snapshot_session(&manager, "s1");
        let loaded = manager.load_native_session("s1").unwrap();
        // A synthetic note is a `User`-role message, so only the prompt test
        // keeps it from becoming a turn the model would read as the person's.
        let note = Message::synthetic_user("=== RESUME ===");
        manager.convert(&lease, loaded, Some(&note)).unwrap();

        assert!(!manager.load_events("s1").unwrap().iter().any(|logged| {
            matches!(
                &logged.event,
                SessionEvent::UserMessage { text, .. } if text == "=== RESUME ==="
            )
        }));
    }

    #[cfg(feature = "tools")]
    #[test]
    fn a_rewind_before_the_plan_clears_the_todo_sidecar() {
        let (_dir, manager) = store();
        let lease = todo_session(&manager, "rewind-clear-todos");
        manager
            .append_events(
                &lease,
                &[logged(
                    7,
                    0,
                    SessionEvent::Rewound {
                        turn: 2,
                        to: 1,
                        scope: RewindScope::Conversation,
                    },
                )],
            )
            .unwrap();

        let sidecar = manager
            .read_todo_sidecar("rewind-clear-todos")
            .unwrap()
            .expect("the sidecar is rewritten, not left stale");
        assert!(sidecar.todos.is_empty());
        assert_eq!(sidecar.through, None);
    }

    #[cfg(feature = "tools")]
    #[test]
    fn an_undo_that_does_not_touch_the_plan_keeps_a_sidecar_only_list() {
        use crate::session::manager::{TodoCallPosition, TodoSidecarItem};

        let (_dir, manager) = store();
        let id = "sidecar-only-plan";
        let lease = created(&manager, id);
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "first".into(),
                            images: vec![],
                        },
                    ),
                    logged(
                        3,
                        0,
                        SessionEvent::AssistantMessage {
                            turn: 1,
                            round: 1,
                            text: "ok".into(),
                            reasoning: String::new(),
                            tool_calls: vec![],
                            reasoning_blocks: vec![],
                            meta: None,
                        },
                    ),
                    logged(
                        4,
                        0,
                        SessionEvent::TurnEnd {
                            turn: 1,
                            stop: StopReason::Stopped,
                            error: None,
                        },
                    ),
                    logged(5, 0, SessionEvent::TurnStart { turn: 2 }),
                    logged(
                        6,
                        0,
                        SessionEvent::UserMessage {
                            turn: 2,
                            text: "second".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();
        let through = Some(TodoCallPosition {
            turn: 1,
            round: 1,
            index: 0,
        });
        manager
            .write_todo_sidecar(
                id,
                &[TodoSidecarItem {
                    content: "keep me".into(),
                    status: "in_progress".into(),
                }],
                4,
                through,
            )
            .unwrap();

        manager
            .append_events(
                &lease,
                &[logged(
                    7,
                    0,
                    SessionEvent::Rewound {
                        turn: 2,
                        to: 5,
                        scope: RewindScope::Conversation,
                    },
                )],
            )
            .unwrap();

        let sidecar = manager
            .read_todo_sidecar(id)
            .unwrap()
            .expect("a sidecar-only plan is not evidence of a taken-back plan");
        assert_eq!(sidecar.todos.len(), 1);
        assert_eq!(sidecar.todos[0].content, "keep me");
        assert_eq!(sidecar.through, through);
    }

    #[cfg(feature = "tools")]
    #[test]
    fn a_cancel_boundary_does_not_erase_a_sidecar_only_plan() {
        use crate::session::manager::{TodoCallPosition, TodoSidecarItem};

        let (_dir, manager) = store();
        let id = "sidecar-only-cancel";
        let lease = created(&manager, id);
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "first".into(),
                            images: vec![],
                        },
                    ),
                    logged(
                        3,
                        0,
                        SessionEvent::AssistantMessage {
                            turn: 1,
                            round: 1,
                            text: "ok".into(),
                            reasoning: String::new(),
                            tool_calls: vec![],
                            reasoning_blocks: vec![],
                            meta: None,
                        },
                    ),
                    logged(
                        4,
                        0,
                        SessionEvent::TurnEnd {
                            turn: 1,
                            stop: StopReason::Stopped,
                            error: None,
                        },
                    ),
                    logged(5, 0, SessionEvent::TurnStart { turn: 2 }),
                    logged(
                        6,
                        0,
                        SessionEvent::UserMessage {
                            turn: 2,
                            text: "second".into(),
                            images: vec![],
                        },
                    ),
                    logged(
                        7,
                        0,
                        SessionEvent::Interrupted {
                            turn: 2,
                            undone: false,
                        },
                    ),
                    logged(8, 0, SessionEvent::TurnStart { turn: 3 }),
                    logged(
                        9,
                        0,
                        SessionEvent::UserMessage {
                            turn: 3,
                            text: "third".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();
        let through = Some(TodoCallPosition {
            turn: 1,
            round: 1,
            index: 0,
        });
        manager
            .write_todo_sidecar(
                id,
                &[TodoSidecarItem {
                    content: "keep me".into(),
                    status: "in_progress".into(),
                }],
                4,
                through,
            )
            .unwrap();

        // The undo takes back turn 3. The cancel at turn 2 must not retire a
        // plan the log never held: the sidecar is its only copy.
        manager
            .append_events(
                &lease,
                &[logged(
                    10,
                    0,
                    SessionEvent::Rewound {
                        turn: 3,
                        to: 8,
                        scope: RewindScope::Conversation,
                    },
                )],
            )
            .unwrap();

        let sidecar = manager
            .read_todo_sidecar(id)
            .unwrap()
            .expect("the sidecar is the only copy of the plan");
        assert_eq!(sidecar.todos.len(), 1, "{sidecar:?}");
        assert_eq!(sidecar.todos[0].content, "keep me");
        assert_eq!(sidecar.through, through);
    }

    #[cfg(feature = "tools")]
    #[test]
    fn a_cancel_boundary_does_not_truncate_a_sidecar_only_plan() {
        use crate::session::manager::{TodoCallPosition, TodoSidecarItem};
        use atomcode_kernel::tool::ToolCall;

        let (_dir, manager) = store();
        let id = "sidecar-only-cancel-call";
        let lease = created(&manager, id);
        let update = ToolCall {
            id: "u".into(),
            name: "todowrite".into(),
            arguments: r#"{"action":"update","id":1,"status":"completed"}"#.into(),
        };
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "first".into(),
                            images: vec![],
                        },
                    ),
                    logged(
                        3,
                        0,
                        SessionEvent::AssistantMessage {
                            turn: 1,
                            round: 1,
                            text: "ok".into(),
                            reasoning: String::new(),
                            tool_calls: vec![],
                            reasoning_blocks: vec![],
                            meta: None,
                        },
                    ),
                    logged(
                        4,
                        0,
                        SessionEvent::TurnEnd {
                            turn: 1,
                            stop: StopReason::Stopped,
                            error: None,
                        },
                    ),
                    logged(5, 0, SessionEvent::TurnStart { turn: 2 }),
                    logged(
                        6,
                        0,
                        SessionEvent::UserMessage {
                            turn: 2,
                            text: "second".into(),
                            images: vec![],
                        },
                    ),
                    logged(
                        7,
                        0,
                        SessionEvent::Interrupted {
                            turn: 2,
                            undone: false,
                        },
                    ),
                    logged(8, 0, SessionEvent::TurnStart { turn: 3 }),
                    logged(
                        9,
                        0,
                        SessionEvent::UserMessage {
                            turn: 3,
                            text: "third".into(),
                            images: vec![],
                        },
                    ),
                    logged(
                        10,
                        0,
                        SessionEvent::AssistantMessage {
                            turn: 3,
                            round: 1,
                            text: String::new(),
                            reasoning: String::new(),
                            tool_calls: vec![update],
                            reasoning_blocks: vec![],
                            meta: None,
                        },
                    ),
                    logged(
                        11,
                        0,
                        SessionEvent::ToolResultLogged {
                            turn: 3,
                            round: 1,
                            call_id: "u".into(),
                            content: "#1 [x] keep me".into(),
                            is_error: false,
                            images: vec![],
                        },
                    ),
                    logged(
                        12,
                        0,
                        SessionEvent::TurnEnd {
                            turn: 3,
                            stop: StopReason::Stopped,
                            error: None,
                        },
                    ),
                    logged(13, 0, SessionEvent::TurnStart { turn: 4 }),
                    logged(
                        14,
                        0,
                        SessionEvent::UserMessage {
                            turn: 4,
                            text: "fourth".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();
        manager
            .write_todo_sidecar(
                id,
                &[TodoSidecarItem {
                    content: "keep me".into(),
                    status: "pending".into(),
                }],
                4,
                Some(TodoCallPosition {
                    turn: 1,
                    round: 1,
                    index: 0,
                }),
            )
            .unwrap();

        // The update after the cancel still sees the sidecar's list; the undo of
        // turn 4 must fold it over that baseline, not from an empty one.
        manager
            .append_events(
                &lease,
                &[logged(
                    15,
                    0,
                    SessionEvent::Rewound {
                        turn: 4,
                        to: 13,
                        scope: RewindScope::Conversation,
                    },
                )],
            )
            .unwrap();

        let sidecar = manager
            .read_todo_sidecar(id)
            .unwrap()
            .expect("the list survives the rebuild");
        assert_eq!(sidecar.todos.len(), 1, "{sidecar:?}");
        assert_eq!(sidecar.todos[0].content, "keep me");
        assert_eq!(sidecar.todos[0].status, "completed");
    }

    #[cfg(feature = "tools")]
    #[test]
    fn a_legacy_sidecar_keeps_its_list_when_the_log_only_updates() {
        use crate::session::manager::TodoSidecarItem;
        use atomcode_kernel::tool::ToolCall;

        let (_dir, manager) = store();
        let id = "legacy-sidecar-update";
        let lease = created(&manager, id);
        let update = ToolCall {
            id: "u".into(),
            name: "todowrite".into(),
            arguments: r#"{"action":"update","id":2,"status":"completed"}"#.into(),
        };
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                    logged(
                        3,
                        0,
                        SessionEvent::AssistantMessage {
                            turn: 1,
                            round: 1,
                            text: String::new(),
                            reasoning: String::new(),
                            tool_calls: vec![update],
                            reasoning_blocks: vec![],
                            meta: None,
                        },
                    ),
                    logged(
                        4,
                        0,
                        SessionEvent::ToolResultLogged {
                            turn: 1,
                            round: 1,
                            call_id: "u".into(),
                            content: "#2 [x] b".into(),
                            is_error: false,
                            images: vec![],
                        },
                    ),
                    logged(
                        5,
                        0,
                        SessionEvent::TurnEnd {
                            turn: 1,
                            stop: StopReason::Stopped,
                            error: None,
                        },
                    ),
                    logged(6, 0, SessionEvent::TurnStart { turn: 2 }),
                    logged(
                        7,
                        0,
                        SessionEvent::UserMessage {
                            turn: 2,
                            text: "later".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();
        manager
            .write_todo_sidecar(
                id,
                &[
                    TodoSidecarItem {
                        content: "a".into(),
                        status: "pending".into(),
                    },
                    TodoSidecarItem {
                        content: "b".into(),
                        status: "in_progress".into(),
                    },
                ],
                2,
                None,
            )
            .unwrap();
        manager
            .append_events(
                &lease,
                &[logged(
                    8,
                    0,
                    SessionEvent::Rewound {
                        turn: 2,
                        to: 6,
                        scope: RewindScope::Conversation,
                    },
                )],
            )
            .unwrap();

        // A sidecar from before positions existed cannot say whether the log's
        // update is already reflected; folding from empty would erase the plan.
        let sidecar = manager.read_todo_sidecar(id).unwrap().unwrap();
        assert_eq!(sidecar.todos.len(), 2, "{sidecar:?}");
        assert_eq!(sidecar.todos[0].content, "a");
        assert_eq!(sidecar.todos[1].content, "b");
        assert_eq!(sidecar.todos[1].status, "completed");
        assert!(sidecar.through.is_some());
    }

    #[cfg(feature = "tools")]
    #[test]
    fn the_sidecar_fold_keeps_a_later_call_with_a_reused_id() {
        use atomcode_kernel::tool::ToolCall;

        let (_dir, manager) = store();
        let id = "failed-call-id";
        let lease = created(&manager, id);
        // A provider that numbers calls per response can reuse an id.
        let failed = ToolCall {
            id: "call_0".into(),
            name: "todowrite".into(),
            arguments: r#"{"todos":[{"content":"failed","status":"pending"}]}"#.into(),
        };
        let later = ToolCall {
            id: "call_0".into(),
            name: "todowrite".into(),
            arguments: r#"{"action":"add","content":"later"}"#.into(),
        };
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "hello".into(),
                            images: vec![],
                        },
                    ),
                    logged(
                        3,
                        0,
                        SessionEvent::AssistantMessage {
                            turn: 1,
                            round: 1,
                            text: String::new(),
                            reasoning: String::new(),
                            tool_calls: vec![failed],
                            reasoning_blocks: vec![],
                            meta: None,
                        },
                    ),
                    logged(
                        4,
                        0,
                        SessionEvent::ToolResultLogged {
                            turn: 1,
                            round: 1,
                            call_id: "call_0".into(),
                            content: "denied".into(),
                            is_error: true,
                            images: vec![],
                        },
                    ),
                    logged(
                        5,
                        0,
                        SessionEvent::TurnEnd {
                            turn: 1,
                            stop: StopReason::Stopped,
                            error: None,
                        },
                    ),
                    logged(6, 0, SessionEvent::TurnStart { turn: 2 }),
                    logged(
                        7,
                        0,
                        SessionEvent::UserMessage {
                            turn: 2,
                            text: "retry".into(),
                            images: vec![],
                        },
                    ),
                    logged(
                        8,
                        0,
                        SessionEvent::AssistantMessage {
                            turn: 2,
                            round: 1,
                            text: String::new(),
                            reasoning: String::new(),
                            tool_calls: vec![later],
                            reasoning_blocks: vec![],
                            meta: None,
                        },
                    ),
                    logged(
                        9,
                        0,
                        SessionEvent::ToolResultLogged {
                            turn: 2,
                            round: 1,
                            call_id: "call_0".into(),
                            content: "#1 [ ] later".into(),
                            is_error: false,
                            images: vec![],
                        },
                    ),
                    logged(
                        10,
                        0,
                        SessionEvent::TurnEnd {
                            turn: 2,
                            stop: StopReason::Stopped,
                            error: None,
                        },
                    ),
                    logged(11, 0, SessionEvent::TurnStart { turn: 3 }),
                    logged(
                        12,
                        0,
                        SessionEvent::UserMessage {
                            turn: 3,
                            text: "undo".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();
        manager
            .append_events(
                &lease,
                &[logged(
                    13,
                    0,
                    SessionEvent::Rewound {
                        turn: 3,
                        to: 11,
                        scope: RewindScope::Conversation,
                    },
                )],
            )
            .unwrap();

        let sidecar = manager
            .read_todo_sidecar(id)
            .unwrap()
            .expect("the later call shapes the list");
        assert_eq!(sidecar.todos.len(), 1, "{sidecar:?}");
        assert_eq!(sidecar.todos[0].content, "later");
    }

    #[cfg(feature = "tools")]
    #[test]
    fn a_cancelled_plan_does_not_return_through_a_later_rebuild() {
        use atomcode_kernel::tool::ToolCall;

        let (_dir, manager) = store();
        let id = "cancelled-plan";
        let lease = created(&manager, id);
        let plan = ToolCall {
            id: "p".into(),
            name: "todowrite".into(),
            arguments: r#"{"todos":[{"content":"cancelled","status":"pending"}]}"#.into(),
        };
        let update = ToolCall {
            id: "u".into(),
            name: "todowrite".into(),
            arguments: r#"{"action":"update","id":1,"status":"completed"}"#.into(),
        };
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::AssistantMessage {
                            turn: 1,
                            round: 1,
                            text: String::new(),
                            reasoning: String::new(),
                            tool_calls: vec![plan],
                            reasoning_blocks: vec![],
                            meta: None,
                        },
                    ),
                    logged(
                        3,
                        0,
                        SessionEvent::ToolResultLogged {
                            turn: 1,
                            round: 1,
                            call_id: "p".into(),
                            content: "#1 [ ] cancelled".into(),
                            is_error: false,
                            images: vec![],
                        },
                    ),
                    logged(
                        4,
                        0,
                        SessionEvent::TurnEnd {
                            turn: 1,
                            stop: StopReason::Cancelled,
                            error: None,
                        },
                    ),
                    logged(
                        5,
                        0,
                        SessionEvent::Interrupted {
                            turn: 1,
                            undone: false,
                        },
                    ),
                    logged(6, 0, SessionEvent::TurnStart { turn: 2 }),
                    logged(
                        7,
                        0,
                        SessionEvent::AssistantMessage {
                            turn: 2,
                            round: 1,
                            text: String::new(),
                            reasoning: String::new(),
                            tool_calls: vec![update],
                            reasoning_blocks: vec![],
                            meta: None,
                        },
                    ),
                    logged(
                        8,
                        0,
                        SessionEvent::ToolResultLogged {
                            turn: 2,
                            round: 1,
                            call_id: "u".into(),
                            content: "#1 already completed".into(),
                            is_error: false,
                            images: vec![],
                        },
                    ),
                    logged(
                        9,
                        0,
                        SessionEvent::TurnEnd {
                            turn: 2,
                            stop: StopReason::Stopped,
                            error: None,
                        },
                    ),
                    logged(10, 0, SessionEvent::TurnStart { turn: 3 }),
                    logged(
                        11,
                        0,
                        SessionEvent::UserMessage {
                            turn: 3,
                            text: "undo this".into(),
                            images: vec![],
                        },
                    ),
                    logged(
                        12,
                        0,
                        SessionEvent::Rewound {
                            turn: 3,
                            to: 10,
                            scope: RewindScope::Conversation,
                        },
                    ),
                ],
            )
            .unwrap();

        let sidecar = manager
            .read_todo_sidecar(id)
            .unwrap()
            .expect("the update after the cancel writes the current list");
        assert!(
            sidecar.todos.is_empty(),
            "the plan before the cancel must not come back: {:?}",
            sidecar.todos
        );
    }

    #[cfg(feature = "tools")]
    #[test]
    fn truncation_rebuilds_only_when_the_cut_tail_has_an_undo() {
        let (_dir, manager) = store();
        let id = "truncate-tail-plain";
        let lease = created(&manager, id);
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "first".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();
        let mark = manager.events_mark(id).unwrap();
        manager
            .append_events(
                &lease,
                &[
                    logged(3, 0, SessionEvent::TurnStart { turn: 2 }),
                    logged(
                        4,
                        0,
                        SessionEvent::UserMessage {
                            turn: 2,
                            text: "second".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();
        assert!(!manager.truncated_tail_has_undo(id, mark));

        let id = "truncate-tail-undo";
        let lease = created(&manager, id);
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::UserMessage {
                            turn: 1,
                            text: "first".into(),
                            images: vec![],
                        },
                    ),
                ],
            )
            .unwrap();
        let mark = manager.events_mark(id).unwrap();
        manager
            .append_events(
                &lease,
                &[logged(
                    3,
                    0,
                    SessionEvent::Rewound {
                        turn: 1,
                        to: 1,
                        scope: RewindScope::Conversation,
                    },
                )],
            )
            .unwrap();
        assert!(manager.truncated_tail_has_undo(id, mark));
    }

    #[cfg(feature = "tools")]
    #[test]
    fn truncating_an_append_rolls_the_todo_sidecar_back() {
        use crate::session::manager::{TodoCallPosition, TodoSidecarItem};
        use atomcode_kernel::tool::ToolCall;

        let (_dir, manager) = store();
        let id = "truncate-todos";
        let lease = created(&manager, id);
        let plan = ToolCall {
            id: "p".into(),
            name: "todowrite".into(),
            arguments: r#"{"todos":[{"content":"task","status":"in_progress"}]}"#.into(),
        };
        manager
            .append_events(
                &lease,
                &[
                    logged(1, 0, SessionEvent::TurnStart { turn: 1 }),
                    logged(
                        2,
                        0,
                        SessionEvent::AssistantMessage {
                            turn: 1,
                            round: 1,
                            text: String::new(),
                            reasoning: String::new(),
                            tool_calls: vec![plan],
                            reasoning_blocks: vec![],
                            meta: None,
                        },
                    ),
                    logged(
                        3,
                        0,
                        SessionEvent::ToolResultLogged {
                            turn: 1,
                            round: 1,
                            call_id: "p".into(),
                            content: "#1 [~] task".into(),
                            is_error: false,
                            images: vec![],
                        },
                    ),
                    logged(
                        4,
                        0,
                        SessionEvent::TurnEnd {
                            turn: 1,
                            stop: StopReason::Stopped,
                            error: None,
                        },
                    ),
                ],
            )
            .unwrap();
        let through = Some(TodoCallPosition {
            turn: 1,
            round: 1,
            index: 0,
        });
        manager
            .write_todo_sidecar(
                id,
                &[TodoSidecarItem {
                    content: "task".into(),
                    status: "in_progress".into(),
                }],
                4,
                through,
            )
            .unwrap();
        let mark = manager.events_mark(id).unwrap();

        manager
            .append_events(
                &lease,
                &[logged(
                    5,
                    0,
                    SessionEvent::Rewound {
                        turn: 1,
                        to: 1,
                        scope: RewindScope::Conversation,
                    },
                )],
            )
            .unwrap();
        assert!(manager
            .read_todo_sidecar(id)
            .unwrap()
            .expect("the rewind writes the current list")
            .todos
            .is_empty());

        manager.truncate_events(&lease, mark).unwrap();
        let sidecar = manager
            .read_todo_sidecar(id)
            .unwrap()
            .expect("the truncated log's list is rebuilt");
        assert_eq!(sidecar.todos.len(), 1);
        assert_eq!(sidecar.todos[0].content, "task");
        assert_eq!(sidecar.through, through);
    }

    #[cfg(feature = "tools")]
    #[test]
    fn forking_an_event_session_copies_its_todo_sidecar() {
        use crate::session::manager::{TodoCallPosition, TodoSidecarItem};

        let (_dir, manager) = store();
        let lease = created(&manager, "s1");
        manager.append_events(&lease, &a_turn()).unwrap();
        let items = vec![TodoSidecarItem {
            content: "a".into(),
            status: "in_progress".into(),
        }];
        let through = Some(TodoCallPosition {
            turn: 1,
            round: 1,
            index: 0,
        });
        manager
            .write_todo_sidecar("s1", &items, 4, through)
            .unwrap();

        let (_forked, _fork_lease) = manager.fork_native_session("s1", "s2", 2_000).unwrap();

        let sidecar = manager
            .read_todo_sidecar("s2")
            .unwrap()
            .expect("the fork carries the plan");
        assert_eq!(sidecar.todos.len(), 1);
        assert_eq!(sidecar.todos[0].content, "a");
        assert_eq!(sidecar.todos[0].status, "in_progress");
        assert_eq!(sidecar.through, through);
    }

    #[cfg(feature = "memory")]
    #[test]
    fn memory_injection_header_matches_the_real_block() {
        use crate::memory::context::MemoryContext;
        use crate::memory::store::MemoryStore;

        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new(dir.path().join("memory.md"));
        store.append("prefers tabs").unwrap();
        let context = MemoryContext::load(
            &store,
            &store,
            &store,
            "project",
            &dir.path().join("knowledge/INDEX.md"),
        );
        assert!(context.prompt.starts_with(MEMORY_INJECTION_HEADER));
    }
    #[test]
    fn discarded_reasoning_is_never_written_to_native_sessions() {
        let (_dir, manager) = store();
        let lease = created(&manager, "discarded");
        manager
            .append_events(
                &lease,
                &[
                    logged(
                        1,
                        10,
                        SessionEvent::AssistantChunk {
                            turn: 1,
                            round: 1,
                            delta: "bad prefix".into(),
                            reasoning: true,
                        },
                    ),
                    logged(
                        2,
                        11,
                        SessionEvent::ReasoningDiscarded { turn: 1, round: 1 },
                    ),
                ],
            )
            .unwrap();
        let text = std::fs::read_to_string(manager.events_path("discarded").unwrap()).unwrap();
        assert!(!text.contains("bad prefix"));
        assert!(!text.contains("reasoning_discarded"));
        assert_eq!(text.lines().count(), 1, "only the header remains");
    }
}
