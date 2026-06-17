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

use std::io;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use atomcode_kernel::event::StopReason;
use atomcode_kernel::hook::{LifecycleHooks, TurnCtx};
use atomcode_kernel::message::{Conversation, Message, SessionSnapshot};

use super::{now_ms, SessionManager, SessionMeta, TurnStat};

/// Per-turn accumulation (reset each turn): start time for duration, and the turn's
/// tool-call count / token total gathered across rounds.
#[derive(Default)]
struct TurnAccum {
    started_ms: i64,
    tool_calls: u32,
    total_tokens: u32,
}

/// Saves `<id>.snapshot` (the compacted working set) + updates `<id>.meta` each turn.
pub struct SnapshotHook {
    mgr: Arc<SessionManager>,
    session_id: String,
    working_dir: String,
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
            accum: Mutex::new(TurnAccum::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TurnAccum> {
        self.accum.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[async_trait]
impl LifecycleHooks for SnapshotHook {
    /// Mark the turn's wall-clock start (for `duration_ms`) and reset per-turn counters.
    async fn user_prompt_submit(&self, _text: &mut String) -> Result<(), String> {
        let mut a = self.lock();
        *a = TurnAccum { started_ms: now_ms(), tool_calls: 0, total_tokens: 0 };
        Ok(())
    }

    /// Accumulate this round's tool-call count + the cumulative context-token count.
    async fn on_model_response(&self, response: &mut Message) {
        let mut a = self.lock();
        a.tool_calls = a.tool_calls.saturating_add(response.tool_calls.len() as u32);
        if let Some(meta) = &response.meta {
            // `used_tokens` is the round's cumulative context size; the last round's is
            // the turn's footprint — what a `✓ … tokens` divider shows.
            a.total_tokens = meta.used_tokens;
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
        if let Err(e) = self.mgr.save_snapshot(&self.session_id, &snap) {
            eprintln!("[SnapshotHook] save_snapshot failed: {e}");
        }

        let now = now_ms();
        let (duration_ms, tool_call_count, total_tokens) = {
            let a = self.lock();
            (
                (now - a.started_ms).max(0) as u64,
                a.tool_calls,
                a.total_tokens,
            )
        };

        // Read-modify-write so accumulated turn_stats (persisted on disk) survive, and a
        // user `/rename` is preserved. ONLY a genuinely-absent meta (NotFound) starts
        // fresh; any OTHER read error (a transient IO failure, or a corrupt/partially
        // written file) must NOT clobber the on-disk meta — skip this turn's update and
        // leave the existing data intact (the snapshot above already persisted).
        let mut meta = match self.mgr.read_meta(&self.session_id) {
            Ok(m) => m,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                SessionMeta::new(&self.session_id, &self.working_dir, now)
            }
            Err(_) => return,
        };
        let msg_count = convo.messages.len();
        meta.updated_at = now;
        meta.turn_count = meta.turn_count.saturating_add(1);
        meta.message_count = msg_count as u32;
        // Drop turn_stats whose divider position no longer exists in the (possibly
        // compacted, hence shorter) snapshot, so the stats stay consistent with what a
        // resume will load — mirrors production's `retain(|s| s.after_message <= new_len)`.
        meta.turn_stats.retain(|s| s.after_message <= msg_count);
        meta.turn_stats.push(TurnStat {
            after_message: msg_count,
            tool_call_count,
            duration_ms,
            total_tokens,
            errored: *reason != StopReason::Stopped,
        });
        let _ = self.mgr.write_meta(&meta);
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

    fn resp(tool_calls: usize, used: u32) -> Message {
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
            tokens: TokenUsage { prompt: used, completion: 5, cached: 0 },
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
        h.turn_complete(&convo, &StopReason::Stopped, &TurnCtx::default()).await;

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
        assert_eq!(st.total_tokens, 1234);
        assert_eq!(st.after_message, 3);
        assert!(!st.errored);
    }

    #[tokio::test]
    async fn stamps_live_turn_ids_even_when_turn_stored_no_meta() {
        let (h, mgr, _d) = hook("s1");
        h.user_prompt_submit(&mut "go".to_string()).await.unwrap();
        // The turn died before ANY assistant message was stored — the convo carries
        // no metas, so derive_counters alone would say 0. The live TurnCtx is
        // authoritative: a resume must seed PAST this turn.
        let ctx = TurnCtx { turn_id: 7, request_id: 12, ..Default::default() };
        h.turn_complete(&convo_with(1), &StopReason::ProviderError, &ctx).await;
        let snap = mgr.load_snapshot("s1").unwrap();
        assert_eq!(snap.turn_counter, 7, "ctx.turn_id stamps the high-water mark");
        assert_eq!(snap.request_counter, 12, "ctx.request_id too");
    }

    #[tokio::test]
    async fn marks_errored_for_non_stopped_terminal() {
        let (h, mgr, _d) = hook("s1");
        h.user_prompt_submit(&mut "go".to_string()).await.unwrap();
        h.on_model_response(&mut resp(0, 10)).await;
        h.turn_complete(&convo_with(2), &StopReason::ProviderError, &TurnCtx::default()).await;
        let meta = mgr.read_meta("s1").unwrap();
        assert!(meta.turn_stats[0].errored, "a ProviderError terminal is errored");
    }

    #[tokio::test]
    async fn accumulates_turn_stats_across_turns() {
        let (h, mgr, _d) = hook("s1");
        for t in 1..=3u32 {
            h.user_prompt_submit(&mut "go".to_string()).await.unwrap();
            h.on_model_response(&mut resp(1, 100 * t)).await;
            h.turn_complete(&convo_with(t as usize), &StopReason::Stopped, &TurnCtx::default()).await;
        }
        let meta = mgr.read_meta("s1").unwrap();
        assert_eq!(meta.turn_count, 3);
        assert_eq!(meta.turn_stats.len(), 3);
        assert_eq!(meta.turn_stats[2].total_tokens, 300);
    }

    #[tokio::test]
    async fn corrupt_meta_read_does_not_clobber_existing_file() {
        let (h, mgr, _d) = hook("s1");
        // Turn 1 writes a good meta.
        h.user_prompt_submit(&mut "go".to_string()).await.unwrap();
        h.on_model_response(&mut resp(0, 1)).await;
        h.turn_complete(&convo_with(1), &StopReason::Stopped, &TurnCtx::default()).await;
        // Corrupt the meta on disk → read_meta now returns InvalidData (NOT NotFound).
        std::fs::write(mgr.meta_path("s1"), b"not valid json {{{").unwrap();
        // Turn 2 must NOT overwrite the file with a fresh (reset) meta.
        h.user_prompt_submit(&mut "more".to_string()).await.unwrap();
        h.on_model_response(&mut resp(0, 1)).await;
        h.turn_complete(&convo_with(2), &StopReason::Stopped, &TurnCtx::default()).await;
        let raw = std::fs::read_to_string(mgr.meta_path("s1")).unwrap();
        assert_eq!(raw, "not valid json {{{", "a non-NotFound read error must not clobber the meta");
        // The snapshot still saved fine — resume is unaffected by the meta read failure.
        assert_eq!(mgr.load_snapshot("s1").unwrap().messages.len(), 2);
    }

    #[tokio::test]
    async fn prunes_stale_turn_stats_when_snapshot_shrinks() {
        let (h, mgr, _d) = hook("s1");
        // Turn 1: a 5-message snapshot → stat at after_message=5.
        h.user_prompt_submit(&mut "go".to_string()).await.unwrap();
        h.on_model_response(&mut resp(0, 1)).await;
        h.turn_complete(&convo_with(5), &StopReason::Stopped, &TurnCtx::default()).await;
        // Turn 2: a compaction shrank the snapshot to 2 messages.
        h.user_prompt_submit(&mut "more".to_string()).await.unwrap();
        h.on_model_response(&mut resp(0, 1)).await;
        h.turn_complete(&convo_with(2), &StopReason::Stopped, &TurnCtx::default()).await;
        let meta = mgr.read_meta("s1").unwrap();
        assert!(
            meta.turn_stats.iter().all(|s| s.after_message <= 2),
            "stale stats (after_message > new len) must be pruned: {:?}",
            meta.turn_stats
        );
        assert_eq!(meta.turn_stats.len(), 1, "only the in-range stat remains");
    }

    #[tokio::test]
    async fn preserves_user_rename_across_turns() {
        let (h, mgr, _d) = hook("s1");
        // First turn creates the meta.
        h.user_prompt_submit(&mut "go".to_string()).await.unwrap();
        h.on_model_response(&mut resp(0, 1)).await;
        h.turn_complete(&convo_with(1), &StopReason::Stopped, &TurnCtx::default()).await;
        // User renames; a later turn must NOT clobber it.
        mgr.rename("s1", "My Work").unwrap();
        h.user_prompt_submit(&mut "more".to_string()).await.unwrap();
        h.on_model_response(&mut resp(0, 1)).await;
        h.turn_complete(&convo_with(2), &StopReason::Stopped, &TurnCtx::default()).await;
        let meta = mgr.read_meta("s1").unwrap();
        assert_eq!(meta.name, "My Work");
        assert!(meta.user_renamed);
        assert_eq!(meta.turn_count, 2);
    }
}
