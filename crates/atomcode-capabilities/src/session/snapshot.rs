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
    now_ms, ModelPricing, ModelUsageStat, PresentationFile, SessionLease, SessionManager,
    SessionMeta, SessionStoreError, TokenBreakdown, TurnStat,
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
    tokens: TokenBreakdown,
}

/// One-shot signal from the persistence hook to its owning runtime. A normal
/// I/O failure whose rollback completed stays best-effort; an uncertain commit
/// means the on-disk aggregate can no longer be trusted and must fail-close.
#[derive(Clone, Default)]
pub struct SnapshotPersistenceStatus {
    uncertain_commit: Arc<Mutex<Option<String>>>,
    cost_warning: Arc<Mutex<Option<String>>>,
}

impl SnapshotPersistenceStatus {
    #[doc(hidden)]
    pub fn report_uncertain_commit(&self, message: impl Into<String>) {
        *self
            .uncertain_commit
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(message.into());
    }

    pub fn take_uncertain_commit(&self) -> Option<String> {
        self.uncertain_commit
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }

    pub fn report_cost_warning(&self, message: impl Into<String>) {
        *self
            .cost_warning
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(message.into());
    }

    pub fn take_cost_warning(&self) -> Option<String> {
        self.cost_warning
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }
}

/// Saves `<id>.snapshot` (the compacted working set) + updates `<id>.meta` each turn.
pub struct SnapshotHook {
    mgr: Arc<SessionManager>,
    session_id: String,
    working_dir: String,
    lease: Option<SessionLease>,
    accum: Mutex<TurnAccum>,
    persistence_status: SnapshotPersistenceStatus,
    attribution: Mutex<Option<ModelAttribution>>,
}

#[derive(Clone)]
struct ModelAttribution {
    provider_id: String,
    model_id: String,
    pricing: Option<ModelPricing>,
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
            persistence_status: SnapshotPersistenceStatus::default(),
            attribution: Mutex::new(None),
        }
    }

    pub fn with_lease(mut self, lease: SessionLease) -> Self {
        self.lease = Some(lease);
        self
    }

    pub fn with_model_attribution(
        mut self,
        provider_id: impl Into<String>,
        model_id: impl Into<String>,
        pricing: Option<ModelPricing>,
    ) -> Self {
        self.attribution = Mutex::new(Some(ModelAttribution {
            provider_id: provider_id.into(),
            model_id: model_id.into(),
            pricing,
        }));
        self
    }

    /// Atomically replace the attribution used by subsequent turns. The coding
    /// runtime calls this only after a replacement agent has assembled
    /// successfully and the previous generation has stopped.
    pub fn set_model_attribution(
        &self,
        provider_id: impl Into<String>,
        model_id: impl Into<String>,
        pricing: Option<ModelPricing>,
    ) {
        *self
            .attribution
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(ModelAttribution {
            provider_id: provider_id.into(),
            model_id: model_id.into(),
            pricing,
        });
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TurnAccum> {
        self.accum.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn persistence_status(&self) -> SnapshotPersistenceStatus {
        self.persistence_status.clone()
    }

    fn record_persistence_error(&self, error: &SessionStoreError) {
        if error.is_uncertain_commit() {
            self.persistence_status
                .report_uncertain_commit(error.to_string());
        }
    }

    fn compaction_error(&self, error: SessionStoreError) -> CompactionCheckpointError {
        self.record_persistence_error(&error);
        CompactionCheckpointError::new(error.to_string())
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
        let _ = meta.archive_turn_stats_where(|stat| {
            stat.position_valid && stat.after_message > prefix && stat.after_message < old_end
        });
        for stat in &mut meta.turn_stats {
            if !stat.position_valid {
                continue;
            }
            if stat.after_message >= old_end {
                stat.after_message = new_end + stat.after_message.saturating_sub(old_end);
            }
        }
    }
    let surviving_turn_ids: std::collections::BTreeSet<_> = meta
        .turn_stats
        .iter()
        .filter_map(|stat| (stat.position_valid && stat.turn_id != 0).then_some(stat.turn_id))
        .collect();
    presentation.retain_turns(&surviving_turn_ids);
    meta.message_count = u32::try_from(after.messages.len()).unwrap_or(u32::MAX);
    meta.turn_count = u32::try_from(meta.turn_stats.len()).unwrap_or(u32::MAX);
    meta.updated_at = now_ms();
}

impl CompactionCheckpoint for SnapshotHook {
    fn save(&self, snapshot: &SessionSnapshot) -> Result<(), CompactionCheckpointError> {
        if let Some(lease) = &self.lease {
            return self
                .mgr
                .commit_native_runtime_mutation(
                    lease,
                    snapshot,
                    |current_snapshot, meta, presentation| {
                        reindex_compacted_sidecars(current_snapshot, snapshot, meta, presentation);
                        Ok(())
                    },
                )
                .map_err(|error| self.compaction_error(error));
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
            // Provider prompt usage includes cached input for the adapters we
            // support. Store mutually-exclusive buckets so aggregation and
            // pricing never charge cached tokens twice.
            a.tokens.input = a.tokens.input.saturating_add(u64::from(
                meta.tokens.prompt.saturating_sub(meta.tokens.cached),
            ));
            a.tokens.output = a
                .tokens
                .output
                .saturating_add(u64::from(meta.tokens.completion));
            a.tokens.cached_input = a
                .tokens
                .cached_input
                .saturating_add(u64::from(meta.tokens.cached));
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
        let (
            duration_ms,
            round_count,
            tool_call_count,
            total_tokens,
            used_tokens,
            ctx_window,
            tokens,
        ) = {
            let a = self.lock();
            (
                (now - a.started_ms).max(0) as u64,
                a.round_count,
                a.tool_calls,
                a.total_tokens,
                a.used_tokens,
                a.ctx_window,
                a.tokens,
            )
        };
        let attribution = self
            .attribution
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let model_usage = attribution
            .as_ref()
            .filter(|_| tokens.total() > 0)
            .map(|attribution| {
                vec![ModelUsageStat {
                    provider_id: attribution.provider_id.clone(),
                    model_id: attribution.model_id.clone(),
                    tokens,
                    pricing: attribution.pricing,
                }]
            })
            .unwrap_or_default();

        let msg_count = convo.messages.len();
        let update_meta = |meta: &mut SessionMeta| {
            meta.auto_name_from_messages(&convo.messages);
            meta.updated_at = now;
            meta.turn_count = meta.turn_count.saturating_add(1);
            meta.message_count = msg_count as u32;
            // Only legacy position-only stats can be judged by message count.
            // Native stats have stable turn ids and are reindexed exclusively by
            // the explicit compaction/undo seams.
            let _ = meta.archive_turn_stats_where(|stat| {
                stat.turn_id == 0 && stat.position_valid && stat.after_message > msg_count
            });
            meta.turn_stats.push(TurnStat {
                after_message: msg_count,
                position_valid: true,
                turn_id: ctx.turn_id,
                round_count,
                tool_call_count,
                duration_ms,
                total_tokens,
                errored: *reason != StopReason::Stopped,
                used_tokens,
                ctx_window,
                model_usage,
            });
        };
        let result = if let Some(lease) = &self.lease {
            self.mgr.commit_native_runtime_mutation(
                lease,
                &snap,
                |_current_snapshot, meta, _presentation| {
                    update_meta(meta);
                    Ok(())
                },
            )
        } else {
            if let Err(error) = self.mgr.save_snapshot(&self.session_id, &snap) {
                eprintln!("[SnapshotHook] save_snapshot failed: {error}");
            }
            let fresh = SessionMeta::new(&self.session_id, &self.working_dir, now);
            self.mgr
                .update_meta_or_insert(&self.session_id, fresh, update_meta)
        };
        if let Err(error) = result {
            self.record_persistence_error(&error);
            eprintln!("[SnapshotHook] update_meta failed: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::StorageOwner;
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
            position_valid: true,
            turn_id,
            round_count: 1,
            tool_call_count: 0,
            duration_ms: 1,
            total_tokens: 1,
            errored: false,
            used_tokens: 1,
            ctx_window: 10,
            model_usage: Vec::new(),
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
        assert_eq!(meta.detached_unattributed_tokens, 1);
        let presentation = manager.read_presentation(id).unwrap();
        assert_eq!(presentation.entries.len(), 1);
        assert_eq!(presentation.entries[0].text, "keep");
    }

    #[test]
    fn leased_compaction_reindexes_from_the_snapshot_read_under_the_manager_lock() {
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(SessionManager::with_root(dir.path()));
        let id = "compact-current-snapshot";
        let before = SessionSnapshot::new(vec![
            Message::user("u1"),
            Message::assistant("a1", Vec::new()),
            Message::user("u2"),
            Message::assistant("a2", Vec::new()),
        ]);
        let concurrent = SessionSnapshot::new(vec![
            Message::user("u1"),
            Message::assistant("a1", Vec::new()),
            Message::user("u2"),
            Message::assistant("a2", Vec::new()),
            Message::user("u3"),
            Message::assistant("a3", Vec::new()),
        ]);
        let compacted = SessionSnapshot::new(vec![
            Message::user("summary"),
            Message::user("u3"),
            Message::assistant("a3", Vec::new()),
        ]);
        let stat = |after_message, turn_id| TurnStat {
            after_message,
            position_valid: true,
            turn_id,
            round_count: 1,
            tool_call_count: 0,
            duration_ms: 1,
            total_tokens: 1,
            errored: false,
            used_tokens: 1,
            ctx_window: 10,
            model_usage: Vec::new(),
        };
        let mut meta = SessionMeta::new(id, "/p", 1);
        meta.owner = StorageOwner::Native;
        meta.message_count = 4;
        meta.turn_count = 3;
        meta.turn_stats = vec![stat(2, 1), stat(4, 2), stat(6, 3)];
        let lease = manager.acquire_lease(id).unwrap();
        manager
            .commit_native_import(
                &lease,
                Some(&before),
                Some(&PresentationFile::default()),
                &meta,
            )
            .unwrap();

        // Hold the metadata lock after the concurrent writer has read the old
        // aggregate. The compaction can read the old snapshot outside the lock,
        // but must reindex from the writer's committed snapshot once it acquires
        // the lock itself.
        let pause = manager.pause_next_meta_read();
        let writer_manager = manager.clone();
        let writer_lease = lease.clone();
        let writer = std::thread::spawn(move || {
            writer_manager.commit_native_runtime_mutation(
                &writer_lease,
                &concurrent,
                |_current_snapshot, _meta, _presentation| Ok(()),
            )
        });
        pause.wait_until_read();

        let hook = SnapshotHook::new(manager.clone(), id, "/p").with_lease(lease);
        let (done_tx, done_rx) = mpsc::channel();
        let compact = std::thread::spawn(move || {
            let result = CompactionCheckpoint::save(&hook, &compacted);
            let _ = done_tx.send(result);
        });
        assert!(
            done_rx.recv_timeout(Duration::from_secs(1)).is_err(),
            "compaction must wait for the concurrent native mutation"
        );
        pause.resume();
        writer.join().unwrap().unwrap();
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        compact.join().unwrap();

        let stats = manager.read_meta(id).unwrap().turn_stats;
        assert_eq!(stats.len(), 2);
        assert_eq!((stats[0].turn_id, stats[0].after_message), (2, 1));
        assert_eq!((stats[1].turn_id, stats[1].after_message), (3, 3));
    }

    #[test]
    fn uncertain_compaction_commit_is_reported_once() {
        let (hook, _manager, _dir) = hook("uncertain-turn");
        let error = hook.compaction_error(crate::session::SessionStoreError::UncertainCommit {
            id: "uncertain-turn".into(),
            commit_error: "meta replacement failed".into(),
            rollback_errors: vec!["snapshot rollback failed".into()],
        });

        let status = hook.persistence_status();
        assert!(error.to_string().contains("rollback was incomplete"));
        assert!(status
            .take_uncertain_commit()
            .is_some_and(|message| message.contains("rollback was incomplete")));
        assert_eq!(status.take_uncertain_commit(), None);
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
    async fn turn_complete_auto_names_default_meta_from_first_real_user_message() {
        let (h, mgr, _d) = hook("auto-name");
        let mut convo = Conversation::new();
        convo.push(Message::synthetic_user("[System meta] ignore"));
        convo.push(Message::user("修复恢复会话名称\n补测试"));
        convo.push(Message::assistant("done", Vec::new()));

        h.turn_complete(&convo, &StopReason::Stopped, &TurnCtx::default())
            .await;

        let meta = mgr.read_meta("auto-name").unwrap();
        assert_eq!(meta.name, "修复恢复会话名称");
        assert!(!meta.user_renamed);
        assert!(!meta.ai_named);
    }

    #[tokio::test]
    async fn records_round_count_and_distinct_token_semantics() {
        let (base, mgr, _d) = hook("s1a-stats");
        let h = base.with_model_attribution(
            "provider-a",
            "model-a",
            Some(ModelPricing {
                input_per_million: 1.0,
                output_per_million: 2.0,
                cached_input_per_million: 0.1,
            }),
        );
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
        assert_eq!(stat["model_usage"][0]["provider_id"], "provider-a");
        assert_eq!(stat["model_usage"][0]["model_id"], "model-a");
        assert_eq!(stat["model_usage"][0]["tokens"]["input"], 300);
        assert_eq!(stat["model_usage"][0]["tokens"]["output"], 30);
    }

    #[tokio::test]
    async fn model_attribution_can_switch_between_completed_turns() {
        let (base, mgr, _d) = hook("s1a-model-switch");
        let h = base.with_model_attribution("provider-a", "model-a", None);

        h.user_prompt_submit(&mut "first".to_string())
            .await
            .unwrap();
        h.on_model_response(&mut resp(0, 1)).await;
        h.turn_complete(
            &convo_with(1),
            &StopReason::Stopped,
            &TurnCtx {
                turn_id: 1,
                ..TurnCtx::default()
            },
        )
        .await;

        h.set_model_attribution("provider-b", "model-b", None);
        h.user_prompt_submit(&mut "second".to_string())
            .await
            .unwrap();
        h.on_model_response(&mut resp(0, 2)).await;
        h.turn_complete(
            &convo_with(2),
            &StopReason::Stopped,
            &TurnCtx {
                turn_id: 2,
                ..TurnCtx::default()
            },
        )
        .await;

        let report =
            crate::session::aggregate_session_cost(&mgr.read_meta("s1a-model-switch").unwrap());
        assert_eq!(report.models.len(), 2);
        assert_eq!(report.models[0].provider_id, "provider-a");
        assert_eq!(report.models[0].model_id, "model-a");
        assert_eq!(report.models[1].provider_id, "provider-b");
        assert_eq!(report.models[1].model_id, "model-b");
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
    async fn snapshot_shrink_reindexes_and_keeps_unconverted_usage_stats() {
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
        assert_eq!(meta.turn_stats.len(), 1, "only the in-range stat remains");
        assert_eq!(meta.detached_unattributed_tokens, 6);
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
        assert!(stats[0].position_valid);
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

    #[tokio::test]
    async fn leased_turn_complete_preserves_prior_rename_and_presentation_append() {
        use crate::session::{DisplayAnchor, PresentationEntry, PresentationRole};

        let dir = tempfile::tempdir().unwrap();
        let mgr = Arc::new(SessionManager::with_root(dir.path()));
        let id = "leased-prior-sidecars";
        let mut meta = SessionMeta::new(id, "/proj", 1);
        meta.owner = StorageOwner::Native;
        let lease = mgr.acquire_lease(id).unwrap();
        mgr.commit_native_import(
            &lease,
            Some(&SessionSnapshot::from_conversation(&convo_with(1))),
            Some(&PresentationFile::default()),
            &meta,
        )
        .unwrap();
        let hook = SnapshotHook::new(mgr.clone(), id, "/proj").with_lease(lease);

        mgr.rename(id, "User title").unwrap();
        mgr.append_presentation(
            id,
            PresentationEntry {
                anchor: DisplayAnchor::AtStart,
                role: PresentationRole::Assistant,
                text: "prior append".into(),
            },
        )
        .unwrap();
        hook.turn_complete(&convo_with(2), &StopReason::Stopped, &TurnCtx::default())
            .await;

        let meta = mgr.read_meta(id).unwrap();
        assert_eq!(meta.name, "User title");
        assert!(meta.user_renamed);
        assert_eq!(meta.turn_count, 1);
        let presentation = mgr.read_presentation(id).unwrap();
        assert_eq!(presentation.entries.len(), 1);
        assert_eq!(presentation.entries[0].text, "prior append");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_rename_and_turn_complete_preserve_both_updates() {
        use crate::session::{DisplayAnchor, PresentationEntry, PresentationRole};
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let mgr = Arc::new(SessionManager::with_root(dir.path()));
        let id = "concurrent-meta";
        let mut meta = SessionMeta::new(id, "/proj", 1);
        meta.owner = StorageOwner::Native;
        let lease = mgr.acquire_lease(id).unwrap();
        mgr.commit_native_import(
            &lease,
            Some(&SessionSnapshot::from_conversation(&convo_with(1))),
            Some(&PresentationFile::default()),
            &meta,
        )
        .unwrap();
        let hook = SnapshotHook::new(mgr.clone(), id, "/proj").with_lease(lease);
        hook.user_prompt_submit(&mut "go".to_string())
            .await
            .unwrap();

        // Pause turn-complete after its fresh locked read. Both sidecar RMWs must
        // wait, then apply to the committed turn instead of being overwritten by it.
        let pause = mgr.pause_next_meta_read();
        let turn = tokio::spawn(async move {
            hook.turn_complete(&convo_with(2), &StopReason::Stopped, &TurnCtx::default())
                .await;
        });
        pause.wait_until_read();

        let rename_mgr = mgr.clone();
        let (renamed_tx, renamed_rx) = mpsc::channel();
        let rename = std::thread::spawn(move || {
            let result = rename_mgr.rename(id, "User title");
            let _ = renamed_tx.send(());
            result
        });
        let append_mgr = mgr.clone();
        let (appended_tx, appended_rx) = mpsc::channel();
        let append = std::thread::spawn(move || {
            let result = append_mgr.append_presentation(
                id,
                PresentationEntry {
                    anchor: DisplayAnchor::AtStart,
                    role: PresentationRole::Assistant,
                    text: "concurrent append".into(),
                },
            );
            let _ = appended_tx.send(());
            result
        });
        let rename_waited = renamed_rx.recv_timeout(Duration::from_secs(1)).is_err();
        let append_waited = appended_rx.recv_timeout(Duration::from_secs(1)).is_err();
        pause.resume();

        turn.await.unwrap();
        rename.join().unwrap().unwrap();
        append.join().unwrap().unwrap();
        assert!(
            rename_waited,
            "rename must wait for the turn meta update lock"
        );
        assert!(
            append_waited,
            "presentation append must wait for the turn sidecar update lock"
        );
        let meta = mgr.read_meta(id).unwrap();
        assert_eq!(meta.name, "User title");
        assert!(meta.user_renamed);
        assert_eq!(meta.turn_count, 1);
        assert_eq!(meta.turn_stats.len(), 1);
        let presentation = mgr.read_presentation(id).unwrap();
        assert_eq!(presentation.entries.len(), 1);
        assert_eq!(presentation.entries[0].text, "concurrent append");
    }
}
