# Interrupted session recovery implementation plan

Goal: preserve bounded in-progress reply data and make an abnormal runtime exit explicit on resume, without replaying tools automatically.

Baseline: release/v5.2.6 at ca887e19a. Shared worktree contains unrelated follow-up/schedule edits; preserve them.

Owner: harness commits reply checkpoint deltas through the existing session write path; native SessionManager appends recovery facts under a newly acquired runtime lease. The event log remains the sole authority. A runtime rebuild reusing its lease must not be mistaken for a crashed process. Checkpoint data is not model-visible until explicitly recovered as PartialReply. Request retries and discarded reasoning reset checkpoint state. Completed messages supersede it.

1. Add a versioned neutral ReplyCheckpoint event (bounded incremental text/reasoning, reset boundary), ignored by ordinary projections.
2. Aggregate harness streaming checkpoints by time/bytes. Keep final messages, cancellation and retry behavior authoritative.
3. On cold resume append missing-result markers, the latest incomplete reply, a recovery reminder/notice and a terminal fact. Recovery is idempotent; no tools execute.
4. Cover completed and interrupted rounds, discarded/retried output, tool outcome uncertainty, repeat recovery and runtime lease reuse.
5. Run affected crate nextest suites, workspace feature compile gate and format checks. Preserve unrelated changes and audit Cargo.lock.

Limits: already pending OS writes and the aggregation window may still be lost; this is process-crash recovery, not power-loss durability or exact tool/process continuation. Direct non-coding kernel agents do not use the native product session store.

## Implemented and validated

- Format 15 adds neutral ReplyCheckpoint and SessionInterrupted facts. Original
  raw chunks remain transient. One second / 16 KiB aggregation bounds the normal
  uncommitted window; checkpoints are deltas, avoiding quadratic snapshots.
- Cold runtime acquisitions recover only retained, session-owned facts. Reloads
  reusing the same session lease skip recovery. Rewinds and inherited prefixes
  cannot revive discarded output. Team members recover under their own leases.
- Missing tool results are explicitly unknown, not successful; recovery appends
  an InternalError terminal and does not dispatch tools or a new model request.
- Capabilities default 1220 passed; session-feature suite 1524 passed, followed
  by focused coverage for inherited and rewound records. Harness 301 passed,
  kernel 380 passed, coding 1222 passed (9 skipped), and cold-runtime/member
  integration regressions passed again after final changes.
- Recovery tests include a subprocess exiting without destructors or turn-end
  hooks; the next process must acquire the lease, restore once, and keep the
  unsigned reasoning out of model context.
- This does not salvage corrupt/truncated event files, guarantee power-loss
  durability, or resume an external process. No real model calls or user-session
  mutations are used by the tests. Windows-specific UI/terminal closure still
  needs native verification.
