//! `SnapshotHook` — persists the working-set snapshot + session metadata every turn.
//!
//! Hangs off `turn_complete` (fires on EVERY terminal), so the resumable `<id>.snapshot`
//! and the `<id>.meta` are refreshed however the turn ended — NOT only on success. This
//! is the L1-hook realization of B3a: no driver round-trip (`AgentCommand::Snapshot`) is
//! needed for the per-turn save path, because `turn_complete` already hands us the live
//! `Conversation`, and `SessionSnapshot::from_conversation` is public.
//!
//! Per-turn wall-clock `duration_ms` (and the `errored` flag) live HERE in L1 — the
//! kernel is clock-free — feeding the `turn_stats` a resume uses to re-render dividers.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_kernel::checkpoint::{CompactionCheckpoint, CompactionCheckpointError};
use atomcode_kernel::event::StopReason;
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::{Conversation, Message, SessionSnapshot};

use super::{
    now_ms, PresentationFile, SessionLease, SessionManager, SessionMeta, StorageOwner, TurnStat,
};

/// Per-turn accumulation (reset each turn): duration, round/tool counts, and the final
/// model request's token/context figures.
#[derive(Default)]
struct TurnAccum {
    started_ms: i64,
    round_count: u32,
    tool_calls: u32,
    total_tokens: u32,
    used_tokens: u32,
    ctx_window: u32,
}

/// Saves `<id>.snapshot` (the compacted working set) + updates `<id>.meta` each turn.
pub struct SnapshotHook {
    mgr: Arc<SessionManager>,
    session_id: String,
    working_dir: String,
    lease: Option<SessionLease>,
    accum: Mutex<TurnAccum>,
}

impl SnapshotHook {
    pub fn new(
        mgr: Arc<SessionManager>,
        session_id: impl Into<String>,
        working_dir: impl Into<String>,
    ) -> Self {
        Self {
            mgr,
            session_id: session_id.into(),
            working_dir: working_dir.into(),
            lease: None,
            accum: Mutex::new(TurnAccum::default()),
        }
    }

    pub fn with_lease(mut self, lease: SessionLease) -> Self {
        self.lease = Some(lease);
        self
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TurnAccum> {
        self.accum.lock().unwrap_or_else(|e| e.into_inner())
    }
}

fn reindex_compacted_sidecars(
    before: &SessionSnapshot,
    after: &SessionSnapshot,
    meta: &mut SessionMeta,
    presentation: &mut PresentationFile,
) {
    if before.messages.len() != after.messages.len() {
        let prefix = before
            .messages
            .iter()
            .zip(&after.messages)
            .take_while(|(left, right)| left == right)
            .count();
        let suffix = before.messages[prefix..]
            .iter()
            .rev()
            .zip(after.messages[prefix..].iter().rev())
            .take_while(|(left, right)| left == right)
            .count();
        let old_end = before.messages.len().saturating_sub(suffix);
        let new_end = after.messages.len().saturating_sub(suffix);
        meta.turn_stats.retain_mut(|stat| {
            if stat.after_message > prefix && stat.after_message < old_end {
                false
            } else {
                if stat.after_message >= old_end {
                    stat.after_message = new_end + stat.after_message.saturating_sub(old_end);
                }
                true
            }
        });
    }
    let surviving_turn_ids: std::collections::BTreeSet<_> = meta
        .turn_stats
        .iter()
        .filter_map(|stat| (stat.turn_id != 0).then_some(stat.turn_id))
        .collect();
    presentation.retain_turns(&surviving_turn_ids);
    meta.message_count = u32::try_from(after.messages.len()).unwrap_or(u32::MAX);
    meta.turn_count = u32::try_from(meta.turn_stats.len()).unwrap_or(u32::MAX);
    meta.updated_at = now_ms();
}

impl CompactionCheckpoint for SnapshotHook {
    fn save(&self, snapshot: &SessionSnapshot) -> Result<(), CompactionCheckpointError> {
        if let Some(lease) = &self.lease {
            let old_snapshot = self
                .mgr
                .load_snapshot(&self.session_id)
                .map_err(|error| CompactionCheckpointError::new(error.to_string()))?;
            let mut meta = self
                .mgr
                .read_meta(&self.session_id)
                .map_err(|error| CompactionCheckpointError::new(error.to_string()))?;
            let mut presentation = match self.mgr.read_presentation(&self.session_id) {
                Ok(presentation) => presentation,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    PresentationFile::default()
                }
                Err(error) => return Err(CompactionCheckpointError::new(error.to_string())),
            };
            reindex_compacted_sidecars(&old_snapshot, snapshot, &mut meta, &mut presentation);
            return self
                .mgr
                .commit_native_runtime_mutation(lease, snapshot, &presentation, &meta)
                .map_err(|error| CompactionCheckpointError::new(error.to_string()));
        }
        self.mgr
            .save_snapshot(&self.session_id, snapshot)
            .map_err(|error| CompactionCheckpointError::new(error.to_string()))
    }
}

#[async_trait]
impl LifecycleHooks for SnapshotHook {
    /// Mark the turn's wall-clock start (for `duration_ms`) and reset per-turn counters.
    async fn user_prompt_submit(&self, _text: &mut String) -> Result<(), String> {
        let mut a = self.lock();
        *a = TurnAccum {
            started_ms: now_ms(),
            ..Default::default()
        };
        Ok(())
    }

    /// Count this model round and retain the final request's usage/context figures.
    async fn on_model_response(&self, response: &mut Message) {
        let mut a = self.lock();
        a.round_count = a.round_count.saturating_add(1);
        a.tool_calls = a
            .tool_calls
            .saturating_add(response.tool_calls.len() as u32);
        if let Some(meta) = &response.meta {
            // Match the live runtime projection: the turn divider shows prompt +
            // completion from the FINAL request, while context restore needs the
            // distinct used/window pair. Do not conflate these three values.
            a.total_tokens = meta.tokens.prompt.saturating_add(meta.tokens.completion);
            a.used_tokens = meta.used_tokens;
            a.ctx_window = meta.ctx_window;
        }
    }

    /// The turn TERMINATED: persist the working-set snapshot, then read-modify-write the
    /// session meta (bump turn/message counts, append this turn's stat, stamp updated_at).
    /// Both are best-effort — an IO failure must never panic or break the turn.
    async fn turn_complete(&self, convo: &Conversation, reason: &StopReason, ctx: &TurnCtx) {
        let mut snap = SessionSnapshot::from_conversation(convo);
        // `from_conversation` DERIVES the id high-water marks from stored metas; a
        // turn that died before any assistant message was stored is invisible to
        // that derivation. We hold the authoritative live ids — stamp them so a
        // resume seeds past THIS turn even when it stored nothing.
        snap.turn_counter = snap.turn_counter.max(ctx.turn_id);
        snap.request_counter = snap.request_counter.max(ctx.request_id);
        let now = now_ms();
        let (duration_ms, round_count, tool_call_count, total_tokens, used_tokens, ctx_window) = {
            let a = self.lock();
            (
                (now - a.started_ms).max(0) as u64,
                a.round_count,
                a.tool_calls,
                a.total_tokens,
                a.used_tokens,
                a.ctx_window,
            )
        };

        let msg_count = convo.messages.len();
        let update_meta = |meta: &mut SessionMeta| {
            meta.updated_at = now;
            meta.turn_count = meta.turn_count.saturating_add(1);
            meta.message_count = msg_count as u32;
            meta.turn_stats
                .retain(|s| s.turn_id != 0 || s.after_message <= msg_count);
            meta.turn_stats.push(TurnStat {
                after_message: msg_count,
                turn_id: ctx.turn_id,
                round_count,
                tool_call_count,
                duration_ms,
                total_tokens,
                errored: *reason != StopReason::Stopped,
                used_tokens,
                ctx_window,
            });
        };
        let result = if let Some(lease) = &self.lease {
            let mut meta = match self.mgr.read_meta(&self.session_id) {
                Ok(meta) => meta,
                Err(error) => {
                    eprintln!("[SnapshotHook] read_meta failed: {error}");
                    return;
                }
            };
            if meta.owner != StorageOwner::Native {
                eprintln!("[SnapshotHook] terminal rejected non-native owner");
                return;
            }
            update_meta(&mut meta);
            let presentation = match self.mgr.read_presentation(&self.session_id) {
                Ok(presentation) => presentation,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    PresentationFile::default()
                }
                Err(error) => {
                    eprintln!("[SnapshotHook] read_presentation failed: {error}");
                    return;
                }
            };
            self.mgr
                .commit_native_runtime_mutation(lease, &snap, &presentation, &meta)
        } else {
            if let Err(error) = self.mgr.save_snapshot(&self.session_id, &snap) {
                eprintln!("[SnapshotHook] save_snapshot failed: {error}");
            }
            let fresh = SessionMeta::new(&self.session_id, &self.working_dir, now);
            self.mgr
                .update_meta_or_insert(&self.session_id, fresh, update_meta)
        };
        if let Err(error) = result {
            eprintln!("[SnapshotHook] update_meta failed: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomcode_kernel::message::{Message, MessageMeta};
    use atomcode_kernel::stream::TokenUsage;

    fn hook(id: &str) -> (SnapshotHook, Arc<SessionManager>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let mgr = Arc::new(SessionManager::with_root(dir.path()));
        (SnapshotHook::new(mgr.clone(), id, "/proj"), mgr, dir)
    }

    fn convo_with(n_user: usize) -> Conversation {
        let mut c = Conversation::new();
        for i in 0..n_user {
            c.push(Message::user(format!("m{i}")));
        }
        c
    }

    #[test]
    fn compaction_checkpoint_is_immediately_resumable() {
        let (hook, manager, _dir) = hook("compact-now");
        let snapshot = SessionSnapshot::from_conversation(&convo_with(2));

        CompactionCheckpoint::save(&hook, &snapshot).expect("checkpoint save");

        assert_eq!(manager.load_snapshot("compact-now").unwrap(), snapshot);
    }

    #[test]
    fn leased_compaction_checkpoint_reindexes_meta_and_presentation() {
        use crate::session::{DisplayAnchor, PresentationEntry, PresentationRole};

        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(SessionManager::with_root(dir.path()));
        let id = "compact-sidecars";
        let before = SessionSnapshot::new(vec![
            Message::user("u1"),
            Message::assistant("a1", Vec::new()),
            Message::user("u2"),
            Message::assistant("a2", Vec::new()),
            Message::user("u3"),
            Message::assistant("a3", Vec::new()),
        ]);
        manager.save_snapshot(id, &before).unwrap();
        let stat = |after_message, turn_id| TurnStat {
            after_message,
            turn_id,
            round_count: 1,
            tool_call_count: 0,
            duration_ms: 1,
            total_tokens: 1,
            errored: false,
            used_tokens: 1,
            ctx_window: 10,
        };
        let mut meta = SessionMeta::new(id, "/p", 1);
        meta.owner = StorageOwner::Native;
        meta.turn_stats = vec![stat(2, 1), stat(4, 2), stat(6, 3)];
        meta.turn_count = 3;
        meta.message_count = 6;
        manager.write_meta(&meta).unwrap();
        manager
            .write_presentation(
                id,
                &PresentationFile {
                    v: crate::session::presentation::PRESENTATION_VERSION,
                    entries: vec![
                        PresentationEntry {
                            anchor: DisplayAnchor::AfterTurn { turn_id: 1 },
                            role: PresentationRole::Assistant,
                            text: "drop".into(),
                        },
                        PresentationEntry {
                            anchor: DisplayAnchor::AfterTurn { turn_id: 2 },
                            role: PresentationRole::Assistant,
                            text: "keep".into(),
                        },
                    ],
                },
            )
            .unwrap();
        let lease = manager.acquire_lease(id).unwrap();
        let hook = SnapshotHook::new(manager.clone(), id, "/p").with_lease(lease);
        let after = SessionSnapshot::new(vec![
            Message::user("summary"),
            Message::user("u3"),
            Message::assistant("a3", Vec::new()),
        ]);

        CompactionCheckpoint::save(&hook, &after).unwrap();

        let meta = manager.read_meta(id).unwrap();
        assert_eq!(meta.turn_stats.len(), 2);
        assert_eq!(meta.turn_stats[0].after_message, 1);
        assert_eq!(meta.turn_stats[1].after_message, 3);
        let presentation = manager.read_presentation(id).unwrap();
        assert_eq!(presentation.entries.len(), 1);
        assert_eq!(presentation.entries[0].text, "keep");
    }

    fn resp(tool_calls: usize, used: u32) -> Message {
        resp_with_usage(tool_calls, used, 5, used, 0)
    }

    fn resp_with_usage(
        tool_calls: usize,
        prompt: u32,
        completion: u32,
        used: u32,
        ctx_window: u32,
    ) -> Message {
        let calls = (0..tool_calls)
            .map(|i| atomcode_kernel::tool::ToolCall {
                id: format!("c{i}"),
                name: "bash".into(),
                arguments: "{}".into(),
            })
            .collect();
        let mut m = Message::assistant("ok", calls);
        m.meta = Some(MessageMeta {
            used_tokens: used,
            ctx_window,
            tokens: TokenUsage {
                prompt,
                completion,
                cached: 0,
            },
            ..Default::default()
        });
        m
    }

    #[tokio::test]
    async fn saves_loadable_snapshot_and_meta_on_turn_complete() {
        let (h, mgr, _d) = hook("s1");
        h.user_prompt_submit(&mut "go".to_string()).await.unwrap();
        h.on_model_response(&mut resp(2, 1234)).await;
        let convo = convo_with(3);
        h.turn_complete(
            &convo,
            &StopReason::Stopped,
            &TurnCtx {
                turn_id: 7,
                ..TurnCtx::default()
            },
        )
        .await;

        // Snapshot is loadable and holds the conversation.
        let snap = mgr.load_snapshot("s1").unwrap();
        assert_eq!(snap.messages.len(), 3);

        // Meta records the turn + its stat.
        let meta = mgr.read_meta("s1").unwrap();
        assert_eq!(meta.turn_count, 1);
        assert_eq!(meta.message_count, 3);
        assert_eq!(meta.turn_stats.len(), 1);
        let st = &meta.turn_stats[0];
        assert_eq!(st.tool_call_count, 2);
        assert_eq!(st.total_tokens, 1239);
        assert_eq!(st.round_count, 1);
        assert_eq!(st.used_tokens, 1234);
        assert_eq!(st.ctx_window, 0);
        assert_eq!(st.after_message, 3);
        assert_eq!(st.turn_id, 7);
        assert!(!st.errored);
    }

    #[tokio::test]
    async fn records_round_count_and_distinct_token_semantics() {
        let (h, mgr, _d) = hook("s1a-stats");
        h.user_prompt_submit(&mut "go".to_string()).await.unwrap();
        h.on_model_response(&mut resp_with_usage(1, 100, 10, 800, 1_000))
            .await;
        h.on_model_response(&mut resp_with_usage(2, 200, 20, 900, 1_000))
            .await;
        h.turn_complete(&convo_with(3), &StopReason::Stopped, &TurnCtx::default())
            .await;

        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(mgr.meta_path("s1a-stats").unwrap()).unwrap())
                .unwrap();
        let stat = &raw["turn_stats"][0];
        assert_eq!(stat["round_count"], 2);
        assert_eq!(stat["tool_call_count"], 3);
        assert_eq!(stat["total_tokens"], 220);
        assert_eq!(stat["used_tokens"], 900);
        assert_eq!(stat["ctx_window"], 1_000);
    }

    #[tokio::test]
    async fn stamps_live_turn_ids_even_when_turn_stored_no_meta() {
        let (h, mgr, _d) = hook("s1");
        h.user_prompt_submit(&mut "go".to_string()).await.unwrap();
        // The turn died before ANY assistant message was stored — the convo carries
        // no metas, so derive_counters alone would say 0. The live TurnCtx is
        // authoritative: a resume must seed PAST this turn.
        let ctx = TurnCtx {
            turn_id: 7,
            request_id: 12,
            ..Default::default()
        };
        h.turn_complete(&convo_with(1), &StopReason::ProviderError, &ctx)
            .await;
        let snap = mgr.load_snapshot("s1").unwrap();
        assert_eq!(
            snap.turn_counter, 7,
            "ctx.turn_id stamps the high-water mark"
        );
        assert_eq!(snap.request_counter, 12, "ctx.request_id too");
    }

    #[tokio::test]
    async fn marks_errored_for_non_stopped_terminal() {
        let (h, mgr, _d) = hook("s1");
        h.user_prompt_submit(&mut "go".to_string()).await.unwrap();
        h.on_model_response(&mut resp(0, 10)).await;
        h.turn_complete(
            &convo_with(2),
            &StopReason::ProviderError,
            &TurnCtx::default(),
        )
        .await;
        let meta = mgr.read_meta("s1").unwrap();
        assert!(
            meta.turn_stats[0].errored,
            "a ProviderError terminal is errored"
        );
    }

    #[tokio::test]
    async fn accumulates_turn_stats_across_turns() {
        let (h, mgr, _d) = hook("s1");
        for t in 1..=3u32 {
            h.user_prompt_submit(&mut "go".to_string()).await.unwrap();
            h.on_model_response(&mut resp(1, 100 * t)).await;
            h.turn_complete(
                &convo_with(t as usize),
                &StopReason::Stopped,
                &TurnCtx::default(),
            )
            .await;
        }
        let meta = mgr.read_meta("s1").unwrap();
        assert_eq!(meta.turn_count, 3);
        assert_eq!(meta.turn_stats.len(), 3);
        assert_eq!(meta.turn_stats[2].total_tokens, 305);
    }

    #[tokio::test]
    async fn corrupt_meta_read_does_not_clobber_existing_file() {
        let (h, mgr, _d) = hook("s1");
        // Turn 1 writes a good meta.
        h.user_prompt_submit(&mut "go".to_string()).await.unwrap();
        h.on_model_response(&mut resp(0, 1)).await;
        h.turn_complete(&convo_with(1), &StopReason::Stopped, &TurnCtx::default())
            .await;
        // Corrupt the meta on disk → read_meta now returns InvalidData (NOT NotFound).
        std::fs::write(mgr.meta_path("s1").unwrap(), b"not valid json {{{").unwrap();
        // Turn 2 must NOT overwrite the file with a fresh (reset) meta.
        h.user_prompt_submit(&mut "more".to_string()).await.unwrap();
        h.on_model_response(&mut resp(0, 1)).await;
        h.turn_complete(&convo_with(2), &StopReason::Stopped, &TurnCtx::default())
            .await;
        let raw = std::fs::read_to_string(mgr.meta_path("s1").unwrap()).unwrap();
        assert_eq!(
            raw, "not valid json {{{",
            "a non-NotFound read error must not clobber the meta"
        );
        // The snapshot still saved fine — resume is unaffected by the meta read failure.
        assert_eq!(mgr.load_snapshot("s1").unwrap().messages.len(), 2);
    }

    #[tokio::test]
    async fn prunes_unconverted_legacy_stats_when_snapshot_shrinks() {
        let (h, mgr, _d) = hook("s1");
        // Turn 1: a 5-message snapshot → stat at after_message=5.
        h.user_prompt_submit(&mut "go".to_string()).await.unwrap();
        h.on_model_response(&mut resp(0, 1)).await;
        h.turn_complete(&convo_with(5), &StopReason::Stopped, &TurnCtx::default())
            .await;
        // Turn 2: a compaction shrank the snapshot to 2 messages.
        h.user_prompt_submit(&mut "more".to_string()).await.unwrap();
        h.on_model_response(&mut resp(0, 1)).await;
        h.turn_complete(&convo_with(2), &StopReason::Stopped, &TurnCtx::default())
            .await;
        let meta = mgr.read_meta("s1").unwrap();
        assert!(
            meta.turn_stats.iter().all(|s| s.after_message <= 2),
            "stale stats (after_message > new len) must be pruned: {:?}",
            meta.turn_stats
        );
        assert_eq!(meta.turn_stats.len(), 1, "only the in-range stat remains");
    }

    #[tokio::test]
    async fn shrinking_snapshot_does_not_prune_native_stable_turn_stats_by_position() {
        let (h, mgr, _d) = hook("s1");
        h.user_prompt_submit(&mut "go".to_string()).await.unwrap();
        h.on_model_response(&mut resp(0, 1)).await;
        h.turn_complete(
            &convo_with(5),
            &StopReason::Stopped,
            &TurnCtx {
                turn_id: 1,
                ..TurnCtx::default()
            },
        )
        .await;

        h.user_prompt_submit(&mut "more".to_string()).await.unwrap();
        h.on_model_response(&mut resp(0, 1)).await;
        h.turn_complete(
            &convo_with(2),
            &StopReason::Stopped,
            &TurnCtx {
                turn_id: 2,
                ..TurnCtx::default()
            },
        )
        .await;

        let stats = mgr.read_meta("s1").unwrap().turn_stats;
        assert_eq!(stats.len(), 2);
        assert_eq!(stats[0].turn_id, 1);
        assert_eq!(stats[0].after_message, 5);
        assert_eq!(stats[1].turn_id, 2);
    }

    #[tokio::test]
    async fn preserves_user_rename_across_turns() {
        let (h, mgr, _d) = hook("s1");
        // First turn creates the meta.
        h.user_prompt_submit(&mut "go".to_string()).await.unwrap();
        h.on_model_response(&mut resp(0, 1)).await;
        h.turn_complete(&convo_with(1), &StopReason::Stopped, &TurnCtx::default())
            .await;
        // User renames; a later turn must NOT clobber it.
        mgr.rename("s1", "My Work").unwrap();
        h.user_prompt_submit(&mut "more".to_string()).await.unwrap();
        h.on_model_response(&mut resp(0, 1)).await;
        h.turn_complete(&convo_with(2), &StopReason::Stopped, &TurnCtx::default())
            .await;
        let meta = mgr.read_meta("s1").unwrap();
        assert_eq!(meta.name, "My Work");
        assert!(meta.user_renamed);
        assert_eq!(meta.turn_count, 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_rename_and_turn_complete_preserve_both_updates() {
        use std::sync::mpsc;
        use std::time::Duration;

        let (hook, mgr, _d) = hook("concurrent-meta");
        mgr.write_meta(&SessionMeta::new("concurrent-meta", "/proj", 1))
            .unwrap();
        hook.user_prompt_submit(&mut "go".to_string())
            .await
            .unwrap();

        // Pause turn-complete after it has read the old title. Without a lock, rename
        // completes next and the stale turn write then silently restores the old title.
        let pause = mgr.pause_next_meta_read();
        let turn = tokio::spawn(async move {
            hook.turn_complete(&convo_with(2), &StopReason::Stopped, &TurnCtx::default())
                .await;
        });
        pause.wait_until_read();

        let rename_mgr = mgr.clone();
        let (renamed_tx, renamed_rx) = mpsc::channel();
        let rename = std::thread::spawn(move || {
            let result = rename_mgr.rename("concurrent-meta", "User title");
            let _ = renamed_tx.send(());
            result
        });
        let rename_waited = renamed_rx.recv_timeout(Duration::from_secs(1)).is_err();
        pause.resume();

        turn.await.unwrap();
        rename.join().unwrap().unwrap();
        assert!(
            rename_waited,
            "rename must wait for the turn meta update lock"
        );
        let meta = mgr.read_meta("concurrent-meta").unwrap();
        assert_eq!(meta.name, "User title");
        assert!(meta.user_renamed);
        assert_eq!(meta.turn_count, 1);
        assert_eq!(meta.turn_stats.len(), 1);
    }
}
