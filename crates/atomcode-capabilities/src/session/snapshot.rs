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

use super::rewind::{
    RewindLedger, RewindPoint, RewindTransactionJournal, WorkspaceRestorePlan, LEDGER_VERSION,
    TRANSACTION_VERSION,
};
use super::{
    now_ms, ModelUsageStat, PresentationFile, SessionLease, SessionManager, SessionMeta,
    SessionStoreError, TokenBreakdown, TurnStat, WorkspaceCheckpoint, WorkspaceCheckpointError,
    WorkspaceRestoreReceipt,
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
    auxiliary_warning: Arc<Mutex<Option<String>>>,
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

    pub fn report_auxiliary_warning(&self, message: impl Into<String>) {
        *self
            .auxiliary_warning
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(message.into());
    }

    pub fn take_auxiliary_warning(&self) -> Option<String> {
        self.auxiliary_warning
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
    rewind: Mutex<RewindState>,
}

#[derive(Default)]
struct RewindState {
    checkpoint: Option<Arc<WorkspaceCheckpoint>>,
    unavailable: Option<String>,
    transaction_unavailable: Option<String>,
    pending: Option<PendingRewindPoint>,
    points: Vec<RewindPoint>,
}

struct PendingRewindPoint {
    prompt_number: usize,
    prompt_preview: String,
    before_tree: Option<String>,
}

const CODE_REWIND_DISABLED_REASON: &str =
    "Code Rewind (workspace file restore) is off by default to protect disk space; \
     set ATOMCODE_CODE_REWIND=1 to opt in. Conversation Rewind remains available.";

#[derive(Clone, Debug)]
pub struct RewindTransactionReceipt {
    session_id: String,
    workspace: Option<WorkspaceRestoreReceipt>,
    previous_points: Vec<RewindPoint>,
    retained_points: Vec<RewindPoint>,
    target_snapshot: Option<SessionSnapshot>,
}

impl RewindTransactionReceipt {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn restored_files(&self) -> &[String] {
        self.workspace
            .as_ref()
            .map(|receipt| receipt.restored_files.as_slice())
            .unwrap_or_default()
    }
}

#[derive(Clone)]
struct ModelAttribution {
    provider_id: String,
    model_id: String,
}

impl SnapshotHook {
    pub fn new(
        mgr: Arc<SessionManager>,
        session_id: impl Into<String>,
        working_dir: impl Into<String>,
    ) -> Self {
        let session_id = session_id.into();
        let working_dir = working_dir.into();
        // Code Rewind is off by default.  When the user opts in via
        // ATOMCODE_CODE_REWIND=1 we construct the bounded workspace checkpoint
        // whose store lives under $ATOMCODE_HOME/rewind/<bucket>/<session_id> —
        // NOT inside the worktree or on a hard-coded C: path.
        let (checkpoint, unavailable) = if crate::session::rewind::code_rewind_opt_in() {
            match WorkspaceCheckpoint::for_session(
                std::path::Path::new(&working_dir),
                &session_id,
            ) {
                Ok(cp) => (Some(Arc::new(cp)), None),
                Err(e) => (None, Some(format!(
                    "Code Rewind unavailable (ATOMCODE_CODE_REWIND=1 is set but setup failed): {e}"
                ))),
            }
        } else {
            (None, Some(CODE_REWIND_DISABLED_REASON.to_string()))
        };
        let points = mgr
            .load_rewind_ledger(&session_id)
            .map(|ledger| ledger.points)
            .unwrap_or_default();
        Self {
            mgr,
            session_id,
            working_dir,
            lease: None,
            accum: Mutex::new(TurnAccum::default()),
            persistence_status: SnapshotPersistenceStatus::default(),
            attribution: Mutex::new(None),
            rewind: Mutex::new(RewindState {
                checkpoint,
                unavailable,
                points,
                ..RewindState::default()
            }),
        }
    }

    pub fn with_lease(mut self, lease: SessionLease) -> Self {
        self.lease = Some(lease);
        self.recover_pending_rewind();
        self
    }

    pub fn with_model_attribution(
        mut self,
        provider_id: impl Into<String>,
        model_id: impl Into<String>,
    ) -> Self {
        self.attribution = Mutex::new(Some(ModelAttribution {
            provider_id: provider_id.into(),
            model_id: model_id.into(),
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
    ) {
        *self
            .attribution
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(ModelAttribution {
            provider_id: provider_id.into(),
            model_id: model_id.into(),
        });
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TurnAccum> {
        self.accum.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn persistence_status(&self) -> SnapshotPersistenceStatus {
        self.persistence_status.clone()
    }

    pub fn rewind_points(&self) -> Vec<RewindPoint> {
        self.rewind
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .points
            .clone()
    }

    pub fn code_rewind_unavailable(&self) -> Option<String> {
        self.rewind
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .unavailable
            .clone()
    }

    /// A failed durable-transaction recovery disables every Rewind scope, not
    /// merely code restoration. Continuing could overwrite the journal needed
    /// to recover the conversation/worktree pair.
    pub fn rewind_transaction_unavailable(&self) -> Option<String> {
        self.rewind
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .transaction_unavailable
            .clone()
    }

    pub fn restore_workspace(
        &self,
        point: &RewindPoint,
    ) -> Result<WorkspaceRestoreReceipt, WorkspaceCheckpointError> {
        let rewind = self
            .rewind
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let checkpoint = rewind.checkpoint.as_ref().ok_or_else(|| {
            WorkspaceCheckpointError::Unsupported(
                rewind
                    .unavailable
                    .clone()
                    .unwrap_or_else(|| "code rewind is unavailable".into()),
            )
        })?;
        let before_tree = point.before_tree.as_deref().ok_or_else(|| {
            WorkspaceCheckpointError::Unsupported("rewind point has no code snapshot".into())
        })?;
        let expected_current = rewind
            .points
            .last()
            .and_then(|latest| latest.after_tree.as_deref())
            .or(point.after_tree.as_deref())
            .ok_or_else(|| {
                WorkspaceCheckpointError::Unsupported("rewind point has no code snapshot".into())
            })?;
        checkpoint.restore(before_tree, expected_current)
    }

    pub fn compensate_workspace(
        &self,
        receipt: &WorkspaceRestoreReceipt,
    ) -> Result<(), WorkspaceCheckpointError> {
        let rewind = self
            .rewind
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let checkpoint = rewind.checkpoint.as_ref().ok_or_else(|| {
            WorkspaceCheckpointError::Unsupported("code rewind is unavailable".into())
        })?;
        checkpoint.compensate(&receipt.recovery_tree, &receipt.restored_files)
    }

    /// Start a durable Rewind transaction. Recovery state is persisted before
    /// either the worktree or point ledger is changed.
    pub fn begin_rewind(
        &self,
        point: &RewindPoint,
        restore_code: bool,
        target_snapshot: Option<SessionSnapshot>,
    ) -> Result<RewindTransactionReceipt, WorkspaceCheckpointError> {
        let mut rewind = self
            .rewind
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(reason) = rewind.transaction_unavailable.as_ref() {
            return Err(WorkspaceCheckpointError::Persistence(reason.clone()));
        }
        let previous = rewind.points.clone();
        if !previous.iter().any(|candidate| candidate == point) {
            return Err(WorkspaceCheckpointError::Persistence(format!(
                "rewind point {} is no longer available",
                point.turn_id
            )));
        }
        let workspace_plan = if restore_code {
            let checkpoint = rewind.checkpoint.as_ref().ok_or_else(|| {
                WorkspaceCheckpointError::Unsupported(
                    rewind
                        .unavailable
                        .clone()
                        .unwrap_or_else(|| "code rewind is unavailable".into()),
                )
            })?;
            let before_tree = point.before_tree.as_deref().ok_or_else(|| {
                WorkspaceCheckpointError::Unsupported("rewind point has no code snapshot".into())
            })?;
            let expected_current = previous
                .last()
                .and_then(|latest| latest.after_tree.as_deref())
                .or(point.after_tree.as_deref())
                .ok_or_else(|| {
                    WorkspaceCheckpointError::Unsupported(
                        "rewind point has no code snapshot".into(),
                    )
                })?;
            Some(checkpoint.prepare_restore(before_tree, expected_current)?)
        } else {
            None
        };
        let retained = previous
            .iter()
            .filter(|candidate| candidate.turn_id < point.turn_id)
            .cloned()
            .collect::<Vec<_>>();
        let journal = RewindTransactionJournal {
            version: TRANSACTION_VERSION,
            previous_points: previous.clone(),
            retained_points: retained.clone(),
            recovery_tree: workspace_plan
                .as_ref()
                .map(|plan| plan.recovery_tree.clone()),
            restored_files: workspace_plan
                .as_ref()
                .map(|plan| plan.files.clone())
                .unwrap_or_default(),
            target_snapshot: target_snapshot.clone(),
            committed: false,
        };
        self.save_rewind_transaction(&journal)?;
        let workspace = match self.apply_workspace_plan(&rewind, workspace_plan.as_ref()) {
            Ok(receipt) => receipt,
            Err(error) => {
                if let Err(clear) = self.clear_rewind_transaction() {
                    return Err(WorkspaceCheckpointError::Compensation {
                        operation: error.to_string(),
                        compensation: format!("could not clear recovery journal: {clear}"),
                    });
                }
                // prepare_restore pinned refs/atomcode/recovery/current; unpin it
                // best-effort so the ref does not strand a dead object on disk.
                if let Some(cp) = rewind.checkpoint.as_ref() {
                    let _ = cp.unpin_recovery_ref();
                }
                return Err(error);
            }
        };
        if let Err(error) = self.replace_rewind_points_locked(&mut rewind, retained) {
            let compensation = workspace
                .as_ref()
                .map(|receipt| {
                    rewind
                        .checkpoint
                        .as_ref()
                        .ok_or_else(|| {
                            WorkspaceCheckpointError::Unsupported(
                                "code rewind compensation lost its checkpoint".into(),
                            )
                        })?
                        .compensate(&receipt.recovery_tree, &receipt.restored_files)
                })
                .transpose();
            if let Err(compensation) = compensation {
                // Unpin best-effort before returning the double-failure error.
                if let Some(cp) = rewind.checkpoint.as_ref() {
                    let _ = cp.unpin_recovery_ref();
                }
                return Err(WorkspaceCheckpointError::Compensation {
                    operation: error.to_string(),
                    compensation: compensation.to_string(),
                });
            }
            if let Err(clear) = self.clear_rewind_transaction() {
                // Unpin best-effort before returning the clear-failure error.
                if let Some(cp) = rewind.checkpoint.as_ref() {
                    let _ = cp.unpin_recovery_ref();
                }
                return Err(WorkspaceCheckpointError::Compensation {
                    operation: error.to_string(),
                    compensation: format!("could not clear recovery journal: {clear}"),
                });
            }
            // Unpin best-effort: transaction is being unwound cleanly.
            if let Some(cp) = rewind.checkpoint.as_ref() {
                let _ = cp.unpin_recovery_ref();
            }
            return Err(error);
        }
        Ok(RewindTransactionReceipt {
            session_id: self.session_id.clone(),
            workspace,
            previous_points: previous,
            retained_points: journal.retained_points,
            target_snapshot,
        })
    }

    pub fn commit_rewind(
        &self,
        receipt: RewindTransactionReceipt,
    ) -> Result<(), WorkspaceCheckpointError> {
        self.validate_rewind_receipt(&receipt)?;
        let journal = RewindTransactionJournal {
            version: TRANSACTION_VERSION,
            previous_points: receipt.previous_points,
            retained_points: receipt.retained_points,
            recovery_tree: receipt
                .workspace
                .as_ref()
                .map(|workspace| workspace.recovery_tree.clone()),
            restored_files: receipt
                .workspace
                .as_ref()
                .map(|workspace| workspace.restored_files.clone())
                .unwrap_or_default(),
            target_snapshot: receipt.target_snapshot,
            committed: true,
        };
        self.save_rewind_transaction(&journal)?;
        // Drop the temporary recovery ref that prepare_restore pinned: the
        // transaction is committed so gc can reclaim the object if needed.
        {
            let rewind = self.rewind.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(checkpoint) = rewind.checkpoint.as_ref() {
                let _ = checkpoint.unpin_recovery_ref();
            }
        }
        self.clear_rewind_transaction()
    }

    pub fn compensate_rewind(
        &self,
        receipt: RewindTransactionReceipt,
    ) -> Result<(), WorkspaceCheckpointError> {
        self.validate_rewind_receipt(&receipt)?;
        let mut rewind = self
            .rewind
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.replace_rewind_points_locked(&mut rewind, receipt.previous_points)?;
        if let Some(workspace) = receipt.workspace.as_ref() {
            let checkpoint = rewind.checkpoint.as_ref().ok_or_else(|| {
                WorkspaceCheckpointError::Unsupported(
                    "rewind compensation lost its workspace checkpoint".into(),
                )
            })?;
            checkpoint.compensate(&workspace.recovery_tree, &workspace.restored_files)?;
            // Drop the temporary recovery ref: workspace is restored so the object
            // is no longer needed for crash recovery.
            let _ = checkpoint.unpin_recovery_ref();
        }
        self.clear_rewind_transaction()
    }

    /// Resolve an abandoned handle-side transaction from durable state. If the
    /// canonical conversation already equals the target, the operation crossed
    /// its commit point and must be finalized; otherwise restore its preimage.
    pub fn recover_rewind(
        &self,
        receipt: RewindTransactionReceipt,
    ) -> Result<(), WorkspaceCheckpointError> {
        self.validate_rewind_receipt(&receipt)?;
        let conversation_committed = receipt.target_snapshot.as_ref().is_some_and(|target| {
            self.mgr
                .load_snapshot(&self.session_id)
                .is_ok_and(|snapshot| snapshot == *target)
        });
        if conversation_committed {
            self.commit_rewind(receipt)
        } else {
            self.compensate_rewind(receipt)
        }
    }

    fn validate_rewind_receipt(
        &self,
        receipt: &RewindTransactionReceipt,
    ) -> Result<(), WorkspaceCheckpointError> {
        if receipt.session_id == self.session_id {
            Ok(())
        } else {
            Err(WorkspaceCheckpointError::Persistence(format!(
                "rewind receipt belongs to session {}, not {}",
                receipt.session_id, self.session_id
            )))
        }
    }

    fn replace_rewind_points_locked(
        &self,
        rewind: &mut RewindState,
        points: Vec<RewindPoint>,
    ) -> Result<(), WorkspaceCheckpointError> {
        let previous = rewind.points.clone();
        if let Some(checkpoint) = rewind.checkpoint.as_ref() {
            checkpoint.retain_points(&points)?;
        }
        let ledger = RewindLedger {
            version: LEDGER_VERSION,
            points: points.clone(),
        };
        let saved = match &self.lease {
            Some(lease) => self.mgr.save_rewind_ledger_with_lease(lease, &ledger),
            None => self.mgr.save_rewind_ledger(&self.session_id, &ledger),
        };
        if let Err(error) = saved {
            if let Some(checkpoint) = rewind.checkpoint.as_ref() {
                if let Err(compensation) = checkpoint.retain_points(&previous) {
                    return Err(WorkspaceCheckpointError::Compensation {
                        operation: error.to_string(),
                        compensation: compensation.to_string(),
                    });
                }
            }
            return Err(WorkspaceCheckpointError::Persistence(error.to_string()));
        }
        rewind.points = points;
        Ok(())
    }

    fn apply_workspace_plan(
        &self,
        rewind: &RewindState,
        plan: Option<&WorkspaceRestorePlan>,
    ) -> Result<Option<WorkspaceRestoreReceipt>, WorkspaceCheckpointError> {
        let Some(plan) = plan else {
            return Ok(None);
        };
        let checkpoint = rewind.checkpoint.as_ref().ok_or_else(|| {
            WorkspaceCheckpointError::Unsupported(
                "code rewind lost its workspace checkpoint".into(),
            )
        })?;
        checkpoint.apply_prepared_restore(plan).map(Some)
    }

    fn save_rewind_transaction(
        &self,
        journal: &RewindTransactionJournal,
    ) -> Result<(), WorkspaceCheckpointError> {
        let lease = self.lease.as_ref().ok_or_else(|| {
            WorkspaceCheckpointError::Persistence(
                "rewind transaction requires an active session lease".into(),
            )
        })?;
        self.mgr
            .save_rewind_transaction_with_lease(lease, journal)
            .map_err(|error| WorkspaceCheckpointError::Persistence(error.to_string()))
    }

    fn clear_rewind_transaction(&self) -> Result<(), WorkspaceCheckpointError> {
        let lease = self.lease.as_ref().ok_or_else(|| {
            WorkspaceCheckpointError::Persistence(
                "rewind transaction requires an active session lease".into(),
            )
        })?;
        self.mgr
            .clear_rewind_transaction_with_lease(lease)
            .map_err(|error| WorkspaceCheckpointError::Persistence(error.to_string()))
    }

    fn recover_pending_rewind(&self) {
        let journal = match self.mgr.load_rewind_transaction(&self.session_id) {
            Ok(Some(journal)) => journal,
            Ok(None) => return,
            Err(error) => {
                self.mark_rewind_unavailable(format!(
                    "pending rewind transaction is unreadable: {error}"
                ));
                return;
            }
        };
        let result = (|| {
            let mut rewind = self
                .rewind
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let conversation_committed = journal.target_snapshot.as_ref().is_some_and(|target| {
                self.mgr
                    .load_snapshot(&self.session_id)
                    .is_ok_and(|snapshot| snapshot == *target)
            });
            if journal.committed || conversation_committed {
                self.replace_rewind_points_locked(&mut rewind, journal.retained_points)?;
                // Transaction was committed — drop the recovery pin so gc can
                // reclaim the object.
                if let Some(checkpoint) = rewind.checkpoint.as_ref() {
                    let _ = checkpoint.unpin_recovery_ref();
                }
            } else {
                self.replace_rewind_points_locked(&mut rewind, journal.previous_points)?;
                if let Some(tree) = journal.recovery_tree.as_deref() {
                    // Code Rewind is opt-in (default off), but an interrupted
                    // prior transaction may have already changed files. Open its
                    // existing store only long enough to compensate; never retain
                    // it for future turn capture.
                    if let Some(checkpoint) = rewind.checkpoint.as_ref() {
                        checkpoint.compensate(tree, &journal.restored_files)?;
                        let _ = checkpoint.unpin_recovery_ref();
                    } else {
                        let checkpoint = WorkspaceCheckpoint::for_session_recovery(
                            std::path::Path::new(&self.working_dir),
                            &self.session_id,
                        )?;
                        checkpoint.compensate(tree, &journal.restored_files)?;
                        let _ = checkpoint.unpin_recovery_ref();
                    }
                }
            }
            self.clear_rewind_transaction()
        })();
        if let Err(error) = result {
            self.mark_rewind_unavailable(format!("pending rewind recovery failed: {error}"));
        }
    }

    fn mark_rewind_unavailable(&self, reason: String) {
        self.rewind
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .transaction_unavailable = Some(reason);
    }

    fn record_persistence_error(&self, error: &SessionStoreError) {
        if error.is_uncertain_commit() {
            self.persistence_status
                .report_uncertain_commit(error.to_string());
        }
    }

    /// Surface a RECOVERABLE persistence failure (the last committed state is
    /// intact but THIS turn was not saved). Unlike an uncertain commit — which
    /// fail-stops loudly — a recoverable failure keeps the runtime running, so
    /// without this the unsaved turn would be lost silently on the next
    /// restart/resume. The wording stays cause-agnostic: `{error}` already
    /// carries the specifics (e.g. "磁盘空间不足"/no space, permission denied).
    fn warn_turn_not_persisted(&self, error: &SessionStoreError) {
        self.persistence_status.report_auxiliary_warning(format!(
            "this turn could not be saved ({error}); the last saved state is intact — resolve the error and retry"
        ));
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

    /// Save the accepted user prompt after it has entered the conversation.
    ///
    /// Model responses are intentionally excluded: this hook runs before
    /// requested tools execute, so persisting them could restore dangling tool
    /// calls without their results.
    async fn turn_start(&self, convo: &mut Conversation) {
        let prompt_number = convo
            .messages
            .iter()
            .filter(|message| {
                message.role == atomcode_kernel::message::Role::User && !message.synthetic
            })
            .count();
        let prompt_preview = convo
            .messages
            .iter()
            .rev()
            .find(|message| {
                message.role == atomcode_kernel::message::Role::User && !message.synthetic
            })
            .map(|message| message.text.chars().take(120).collect())
            .unwrap_or_default();
        let checkpoint = self
            .rewind
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .checkpoint
            .clone();
        let before_tree = match checkpoint {
            Some(checkpoint) => tokio::task::spawn_blocking(move || checkpoint.capture())
                .await
                .ok()
                .and_then(Result::ok)
                .flatten(),
            None => None,
        };
        {
            let mut rewind = self
                .rewind
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            rewind.pending = Some(PendingRewindPoint {
                prompt_number,
                prompt_preview,
                before_tree,
            });
        }
        let snapshot = SessionSnapshot::from_conversation(convo);
        let result = match &self.lease {
            Some(lease) => self
                .mgr
                .save_inflight_snapshot_with_lease(lease, &snapshot, true),
            None => self
                .mgr
                .save_inflight_snapshot(&self.session_id, &snapshot, true),
        };
        if let Err(error) = result {
            eprintln!("[SnapshotHook] inflight save at turn_start failed: {error}");
        }
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
            // support. Store mutually-exclusive buckets so token aggregation
            // never counts cached input twice.
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
        drop(a);
        if let Err(error) = self.mgr.mark_inflight_not_replayable(&self.session_id) {
            eprintln!("[SnapshotHook] inflight phase update failed: {error}");
        }
    }

    /// The turn TERMINATED: persist the working-set snapshot, then read-modify-write the
    /// session meta (bump turn/message counts, append this turn's stat, stamp updated_at).
    /// Both are best-effort — an IO failure must never panic or break the turn.
    async fn turn_complete(&self, convo: &Conversation, reason: &StopReason, ctx: &TurnCtx) {
        let (pending, checkpoint) = {
            let mut rewind = self
                .rewind
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            (rewind.pending.take(), rewind.checkpoint.clone())
        };
        let turn_id = ctx.turn_id;
        let completed_rewind = match pending {
            Some(pending) => {
                let workspace = match (pending.before_tree, checkpoint) {
                    (Some(before_tree), Some(checkpoint)) => {
                        tokio::task::spawn_blocking(move || {
                            let after_tree = checkpoint.capture().ok()??;
                            let files = checkpoint.diff(&before_tree, &after_tree).ok()?;
                            Some((before_tree, after_tree, files))
                        })
                        .await
                        .ok()
                        .flatten()
                    }
                    _ => None,
                };
                let (before_tree, after_tree, files) = workspace
                    .map(|(before, after, files)| (Some(before), Some(after), files))
                    .unwrap_or_else(|| (None, None, Vec::new()));
                Some(RewindPoint {
                    turn_id,
                    prompt_number: pending.prompt_number,
                    prompt_preview: pending.prompt_preview,
                    before_tree,
                    after_tree,
                    files,
                })
            }
            None => None,
        };
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
                // Same recoverable-loss guarantee as the leased path below: warn
                // instead of dropping the unsaved turn silently. `save_snapshot`
                // is a single atomic write, so it never yields an uncertain commit.
                self.warn_turn_not_persisted(&error);
                eprintln!("[SnapshotHook] save_snapshot failed: {error}");
                return;
            }
            let fresh = SessionMeta::new(&self.session_id, &self.working_dir, now);
            self.mgr
                .update_meta_or_insert(&self.session_id, fresh, update_meta)
        };
        if let Err(error) = result {
            self.record_persistence_error(&error);
            // An uncertain commit already fail-stops loudly; only a recoverable
            // failure needs a user-visible warning so the unsaved turn isn't lost
            // silently.
            if !error.is_uncertain_commit() {
                self.warn_turn_not_persisted(&error);
            }
            eprintln!("[SnapshotHook] update_meta failed: {error}");
            // Preserve the accepted-prompt checkpoint until a later successful
            // aggregate commit supersedes it.
            return;
        }
        if let Some(point) = completed_rewind {
            let mut rewind = self
                .rewind
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let previous_points = rewind.points.clone();
            rewind.points.push(point);
            const MAX_REWIND_POINTS: usize = 100;
            if rewind.points.len() > MAX_REWIND_POINTS {
                let excess = rewind.points.len() - MAX_REWIND_POINTS;
                rewind.points.drain(..excess);
            }
            let ledger = RewindLedger {
                version: LEDGER_VERSION,
                points: rewind.points.clone(),
            };
            if let Some(checkpoint) = rewind.checkpoint.as_ref() {
                if let Err(error) = checkpoint.retain_points(&ledger.points) {
                    eprintln!("[SnapshotHook] rewind refs update failed: {error}");
                    rewind.points = previous_points;
                    return;
                }
            }
            let saved = match &self.lease {
                Some(lease) => self.mgr.save_rewind_ledger_with_lease(lease, &ledger),
                None => self.mgr.save_rewind_ledger(&self.session_id, &ledger),
            };
            if let Err(error) = saved {
                eprintln!("[SnapshotHook] rewind ledger save failed: {error}");
                if let Some(checkpoint) = rewind.checkpoint.as_ref() {
                    let _ = checkpoint.retain_points(&previous_points);
                }
                rewind.points = previous_points;
            }
        }
        self.mgr.clear_inflight_snapshot(&self.session_id);
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

    fn rewind_point(turn_id: u64, prompt_number: usize) -> RewindPoint {
        RewindPoint {
            turn_id,
            prompt_number,
            prompt_preview: format!("prompt {prompt_number}"),
            before_tree: Some("a".repeat(40)),
            after_tree: Some("b".repeat(40)),
            files: Vec::new(),
        }
    }

    fn git(dir: &std::path::Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn pending_rewind_is_rolled_back_when_conversation_was_not_committed() {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(SessionManager::with_root(dir.path()));
        let id = "rewind-crash-rollback";
        let points = vec![rewind_point(1, 1), rewind_point(2, 2)];
        manager
            .save_rewind_ledger(
                id,
                &RewindLedger {
                    version: LEDGER_VERSION,
                    points: points.clone(),
                },
            )
            .unwrap();
        let original = SessionSnapshot::from_conversation(&convo_with(2));
        manager.save_snapshot(id, &original).unwrap();

        let lease = manager.acquire_lease(id).unwrap();
        let hook = SnapshotHook::new(manager.clone(), id, "/not-a-git-worktree").with_lease(lease);
        let target = SessionSnapshot::from_conversation(&convo_with(0));
        let _receipt = hook.begin_rewind(&points[0], false, Some(target)).unwrap();
        assert!(hook.rewind_points().is_empty());
        drop(hook); // Simulate process death before conversation persistence.

        let lease = manager.acquire_lease(id).unwrap();
        let recovered =
            SnapshotHook::new(manager.clone(), id, "/not-a-git-worktree").with_lease(lease);
        assert_eq!(recovered.rewind_points(), points);
        assert!(manager.load_rewind_transaction(id).unwrap().is_none());
    }

    #[test]
    fn pending_rewind_is_committed_when_canonical_conversation_matches_target() {
        let dir = tempfile::tempdir().unwrap();
        let manager = Arc::new(SessionManager::with_root(dir.path()));
        let id = "rewind-crash-commit";
        let points = vec![rewind_point(1, 1), rewind_point(2, 2)];
        manager
            .save_rewind_ledger(
                id,
                &RewindLedger {
                    version: LEDGER_VERSION,
                    points: points.clone(),
                },
            )
            .unwrap();
        manager
            .save_snapshot(id, &SessionSnapshot::from_conversation(&convo_with(2)))
            .unwrap();

        let lease = manager.acquire_lease(id).unwrap();
        let hook = SnapshotHook::new(manager.clone(), id, "/not-a-git-worktree").with_lease(lease);
        let target = SessionSnapshot::from_conversation(&convo_with(0));
        let _receipt = hook
            .begin_rewind(&points[0], false, Some(target.clone()))
            .unwrap();
        manager.save_snapshot(id, &target).unwrap();
        drop(hook); // Simulate death after conversation commit, before finalization.

        let lease = manager.acquire_lease(id).unwrap();
        let recovered =
            SnapshotHook::new(manager.clone(), id, "/not-a-git-worktree").with_lease(lease);
        assert!(recovered.rewind_points().is_empty());
        assert!(manager.load_rewind_transaction(id).unwrap().is_none());
    }

    #[test]
    #[serial_test::serial(atomcode_code_rewind_env)]
    fn pending_code_rewind_restores_workspace_after_interrupted_transaction() {
        // Ensure Code Rewind is off so the recovered hook reports unavailable.
        let prev = std::env::var("ATOMCODE_CODE_REWIND").ok();
        std::env::remove_var("ATOMCODE_CODE_REWIND");
        let worktree = tempfile::tempdir().unwrap();
        git(worktree.path(), &["init", "--quiet"]);
        std::fs::write(worktree.path().join("tracked.txt"), "before\n").unwrap();
        git(worktree.path(), &["add", "tracked.txt"]);
        git(
            worktree.path(),
            &[
                "-c",
                "user.name=AtomCode",
                "-c",
                "user.email=atomcode@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "initial",
            ],
        );
        let checkpoint = Arc::new(
            WorkspaceCheckpoint::for_session(worktree.path(), "rewind-code-crash").unwrap(),
        );
        let before = checkpoint.capture().unwrap().unwrap();
        std::fs::write(worktree.path().join("tracked.txt"), "after\n").unwrap();
        let after = checkpoint.capture().unwrap().unwrap();
        let point = RewindPoint {
            before_tree: Some(before),
            after_tree: Some(after),
            files: vec![super::super::FileChangeSummary {
                path: "tracked.txt".into(),
                additions: 1,
                deletions: 1,
                binary: false,
            }],
            ..rewind_point(1, 1)
        };

        let session_store = tempfile::tempdir().unwrap();
        let manager = Arc::new(SessionManager::with_root(session_store.path()));
        manager
            .save_rewind_ledger(
                "rewind-code-crash",
                &RewindLedger {
                    version: LEDGER_VERSION,
                    points: vec![point.clone()],
                },
            )
            .unwrap();
        let lease = manager.acquire_lease("rewind-code-crash").unwrap();
        let hook = SnapshotHook::new(
            manager.clone(),
            "rewind-code-crash",
            worktree.path().to_string_lossy(),
        );
        hook.rewind
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .checkpoint = Some(checkpoint);
        let hook = hook.with_lease(lease);
        let _receipt = hook.begin_rewind(&point, true, None).unwrap();
        assert_eq!(
            std::fs::read_to_string(worktree.path().join("tracked.txt")).unwrap(),
            "before\n"
        );
        drop(hook);

        let lease = manager.acquire_lease("rewind-code-crash").unwrap();
        let recovered = SnapshotHook::new(
            manager.clone(),
            "rewind-code-crash",
            worktree.path().to_string_lossy(),
        )
        .with_lease(lease);

        assert_eq!(
            std::fs::read_to_string(worktree.path().join("tracked.txt")).unwrap(),
            "after\n"
        );
        assert_eq!(recovered.rewind_points(), vec![point]);
        assert!(recovered
            .code_rewind_unavailable()
            // "off by default" is UNIQUE to CODE_REWIND_DISABLED_REASON; the
            // opted-in-setup-failed message also contains "ATOMCODE_CODE_REWIND",
            // so that substring cannot distinguish the disabled state.
            .is_some_and(|reason| reason.contains("off by default")));
        assert!(recovered
            .rewind
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .checkpoint
            .is_none());
        match prev {
            Some(v) => std::env::set_var("ATOMCODE_CODE_REWIND", v),
            None => std::env::remove_var("ATOMCODE_CODE_REWIND"),
        }
    }

    /// Assert that the recovery ref (`refs/atomcode/recovery/current`) is pinned
    /// by `prepare_restore` and released on every normal transaction-end path
    /// (`commit_rewind`, `compensate_rewind`).
    ///
    /// Failure-arm unpin (the new fix): the two arms of `begin_rewind` that
    /// return an error AFTER `prepare_restore` has already pinned the ref
    /// (`apply_workspace_plan` failure and `replace_rewind_points_locked` failure)
    /// are not exercised here because there is no injection point between
    /// `prepare_restore` (which runs inside `begin_rewind`) and the two failure
    /// arms — the only observable failure conditions require corrupting the store
    /// mid-call, which is not safe in a unit test.  The fix is guarded by code
    /// inspection and by the fact that the same `unpin_recovery_ref` call-site
    /// pattern is used everywhere.  The common-path assertions below provide a
    /// smoke-test that the ref lifecycle is correct end-to-end.
    #[test]
    fn recovery_ref_is_released_after_commit_and_compensate_rewind() {
        // ── helpers ──────────────────────────────────────────────────────────
        fn setup_worktree(name: &str) -> (tempfile::TempDir, Arc<WorkspaceCheckpoint>) {
            let worktree = tempfile::tempdir().unwrap();
            git(worktree.path(), &["init", "--quiet"]);
            std::fs::write(worktree.path().join("f.txt"), "before\n").unwrap();
            git(worktree.path(), &["add", "f.txt"]);
            git(
                worktree.path(),
                &[
                    "-c", "user.name=AtomCode",
                    "-c", "user.email=atomcode@example.invalid",
                    "commit", "--quiet", "-m", "initial",
                ],
            );
            let cp = Arc::new(WorkspaceCheckpoint::for_session(worktree.path(), name).unwrap());
            (worktree, cp)
        }

        fn ref_exists(git_dir: &std::path::Path, refname: &str) -> bool {
            std::process::Command::new("git")
                .arg("--git-dir").arg(git_dir)
                .args(["for-each-ref", "--format=%(refname)", refname])
                .output()
                .map(|o| !o.stdout.is_empty())
                .unwrap_or(false)
        }

        fn make_hook(
            name: &str,
            manager: &Arc<SessionManager>,
            worktree: &std::path::Path,
            cp: Arc<WorkspaceCheckpoint>,
            points: &[RewindPoint],
        ) -> SnapshotHook {
            manager
                .save_rewind_ledger(
                    name,
                    &RewindLedger {
                        version: LEDGER_VERSION,
                        points: points.to_vec(),
                    },
                )
                .unwrap();
            let lease = manager.acquire_lease(name).unwrap();
            let hook =
                SnapshotHook::new(manager.clone(), name, worktree.to_string_lossy().as_ref());
            hook.rewind
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .checkpoint = Some(cp);
            hook.with_lease(lease)
        }

        // ── Part 1: commit_rewind releases the recovery ref ──────────────────
        {
            let (worktree, cp) = setup_worktree("ref-lifecycle-commit");
            let git_dir = cp.git_dir_path().to_path_buf();
            let before = cp.capture().unwrap().unwrap();
            std::fs::write(worktree.path().join("f.txt"), "after\n").unwrap();
            let after = cp.capture().unwrap().unwrap();
            let file_change = super::super::FileChangeSummary {
                path: "f.txt".into(),
                additions: 1,
                deletions: 1,
                binary: false,
            };
            let point = RewindPoint {
                before_tree: Some(before),
                after_tree: Some(after),
                files: vec![file_change],
                ..rewind_point(1, 1)
            };
            let session_store = tempfile::tempdir().unwrap();
            let manager = Arc::new(SessionManager::with_root(session_store.path()));
            let hook =
                make_hook("ref-lifecycle-commit", &manager, worktree.path(), cp, &[point.clone()]);

            let receipt = hook.begin_rewind(&point, true, None).unwrap();
            // After begin_rewind the ref must be pinned.
            assert!(
                ref_exists(&git_dir, "refs/atomcode/recovery/current"),
                "recovery ref must be present after prepare_restore"
            );
            hook.commit_rewind(receipt).unwrap();
            // After commit_rewind the ref must be gone.
            assert!(
                !ref_exists(&git_dir, "refs/atomcode/recovery/current"),
                "recovery ref must be absent after commit_rewind"
            );
        }

        // ── Part 2: compensate_rewind releases the recovery ref ───────────────
        {
            let (worktree, cp) = setup_worktree("ref-lifecycle-compensate");
            let git_dir = cp.git_dir_path().to_path_buf();
            let before = cp.capture().unwrap().unwrap();
            std::fs::write(worktree.path().join("f.txt"), "after\n").unwrap();
            let after = cp.capture().unwrap().unwrap();
            let file_change = super::super::FileChangeSummary {
                path: "f.txt".into(),
                additions: 1,
                deletions: 1,
                binary: false,
            };
            let point = RewindPoint {
                before_tree: Some(before),
                after_tree: Some(after),
                files: vec![file_change],
                ..rewind_point(1, 1)
            };
            let session_store = tempfile::tempdir().unwrap();
            let manager = Arc::new(SessionManager::with_root(session_store.path()));
            let hook = make_hook(
                "ref-lifecycle-compensate",
                &manager,
                worktree.path(),
                cp,
                &[point.clone()],
            );

            let receipt = hook.begin_rewind(&point, true, None).unwrap();
            assert!(
                ref_exists(&git_dir, "refs/atomcode/recovery/current"),
                "recovery ref must be present after prepare_restore"
            );
            hook.compensate_rewind(receipt).unwrap();
            assert!(
                !ref_exists(&git_dir, "refs/atomcode/recovery/current"),
                "recovery ref must be absent after compensate_rewind"
            );
        }
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
        let h = base.with_model_attribution("provider-a", "model-a");
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
        let h = base.with_model_attribution("provider-a", "model-a");

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

        h.set_model_attribution("provider-b", "model-b");
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

    #[tokio::test]
    async fn turn_start_checkpoints_the_accepted_user_prompt() {
        let (hook, manager, _dir) = hook("inflight-prompt");
        let mut conversation = convo_with(2);

        hook.turn_start(&mut conversation).await;

        assert_eq!(
            manager
                .load_inflight_snapshot("inflight-prompt")
                .unwrap()
                .unwrap()
                .snapshot,
            SessionSnapshot::from_conversation(&conversation)
        );
    }

    #[tokio::test]
    async fn successful_turn_commit_clears_the_inflight_prompt() {
        let (hook, manager, _dir) = hook("inflight-clear");
        let mut conversation = convo_with(1);
        hook.turn_start(&mut conversation).await;

        hook.turn_complete(&conversation, &StopReason::Stopped, &TurnCtx::default())
            .await;

        assert!(!manager.has_inflight_snapshot("inflight-clear"));
        assert_eq!(
            manager.load_snapshot("inflight-clear").unwrap(),
            SessionSnapshot::from_conversation(&conversation)
        );
    }

    #[tokio::test]
    async fn conversation_rewind_point_does_not_require_a_workspace_snapshot() {
        let (hook, manager, _dir) = hook("conversation-rewind");
        let mut conversation = convo_with(1);
        hook.turn_start(&mut conversation).await;

        hook.turn_complete(
            &conversation,
            &StopReason::Stopped,
            &TurnCtx {
                turn_id: 1,
                request_id: 1,
                ..TurnCtx::default()
            },
        )
        .await;

        let points = hook.rewind_points();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].prompt_number, 1);
        assert_eq!(points[0].prompt_preview, "m0");
        assert!(points[0].before_tree.is_none());
        assert!(points[0].after_tree.is_none());
        assert!(points[0].files.is_empty());
        assert!(hook
            .code_rewind_unavailable()
            // "off by default" is UNIQUE to the disabled reason (the
            // opted-in-setup-failed error also mentions ATOMCODE_CODE_REWIND).
            .is_some_and(|reason| reason.contains("off by default")));
        assert_eq!(
            manager
                .load_rewind_ledger("conversation-rewind")
                .unwrap()
                .points,
            points
        );
    }

    #[tokio::test]
    async fn failed_unleased_snapshot_save_preserves_the_inflight_prompt() {
        let (hook, manager, _dir) = hook("inflight-save-failure");
        let mut conversation = convo_with(1);
        hook.turn_start(&mut conversation).await;
        std::fs::create_dir(manager.snapshot_path("inflight-save-failure").unwrap()).unwrap();

        hook.turn_complete(&conversation, &StopReason::Stopped, &TurnCtx::default())
            .await;

        assert!(
            manager.has_inflight_snapshot("inflight-save-failure"),
            "the recovery checkpoint must survive a failed canonical save"
        );
    }

    /// Build a SnapshotHook over a real git worktree so the Code Rewind
    /// opt-in path (which calls `WorkspaceCheckpoint::for_session`) succeeds.
    fn make_test_hook_with_worktree(
        id: &str,
    ) -> (SnapshotHook, Arc<SessionManager>, tempfile::TempDir, tempfile::TempDir) {
        let worktree = tempfile::tempdir().unwrap();
        git(worktree.path(), &["init", "--quiet"]);
        // A bare init has no commits; for_session only needs a valid git dir.
        let store_dir = tempfile::tempdir().unwrap();
        let mgr = Arc::new(SessionManager::with_root(store_dir.path()));
        let hook = SnapshotHook::new(
            mgr.clone(),
            id,
            worktree.path().to_string_lossy(),
        );
        (hook, mgr, worktree, store_dir)
    }

    #[test]
    #[serial_test::serial(atomcode_code_rewind_env)]
    fn snapshot_hook_keeps_code_rewind_off_by_default() {
        let prev = std::env::var("ATOMCODE_CODE_REWIND").ok();
        std::env::remove_var("ATOMCODE_CODE_REWIND");
        let (hook, _mgr, _worktree, _store) =
            make_test_hook_with_worktree("hook-default-off");
        assert!(
            hook.code_rewind_unavailable().is_some(),
            "Code Rewind must be unavailable when ATOMCODE_CODE_REWIND is unset"
        );
        match prev {
            Some(v) => std::env::set_var("ATOMCODE_CODE_REWIND", v),
            None => std::env::remove_var("ATOMCODE_CODE_REWIND"),
        }
    }

    #[test]
    #[serial_test::serial(atomcode_code_rewind_env)]
    fn snapshot_hook_enables_code_rewind_when_opted_in_and_bounded_store_builds() {
        let prev = std::env::var("ATOMCODE_CODE_REWIND").ok();
        std::env::set_var("ATOMCODE_CODE_REWIND", "1");
        let (hook, _mgr, _worktree, _store) =
            make_test_hook_with_worktree("hook-opted-in");
        assert!(
            hook.code_rewind_unavailable().is_none(),
            "Code Rewind must be available when ATOMCODE_CODE_REWIND=1 and worktree is valid"
        );
        match prev {
            Some(v) => std::env::set_var("ATOMCODE_CODE_REWIND", v),
            None => std::env::remove_var("ATOMCODE_CODE_REWIND"),
        }
    }

    #[test]
    fn disabled_reason_does_not_pin_a_stale_version() {
        assert!(
            !CODE_REWIND_DISABLED_REASON.contains("v5.0.5"),
            "CODE_REWIND_DISABLED_REASON must not contain a stale version string"
        );
    }
}
