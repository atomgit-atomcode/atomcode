# Missing rewind objects implementation plan

Goal: keep a missing old workspace snapshot from preventing new rewind checkpoints.

Baseline: release/v5.2.4, 46ad81368163f69cb2226f3b33b2058630ef1e03. Existing unrelated TUI edits remain untouched.

Ownership: SnapshotHook owns the rewind ledger and user-facing auxiliary warning; WorkspaceCheckpoint owns the shadow Git store and process lock. Session event persistence remains authoritative and unchanged.

Boundary and failure semantics:
- Probe root tree objects in one Git batch under the checkpoint lock. Only explicit missing-object results are quarantined. Git/IO/parse failures remain errors.
- Retain available roots and valid points; preserve the original hashes and point records in the existing ledger for diagnostics. Do not change the persisted schema, GC or alternates policy.
- Project quarantined points as conversation-only to consumers. File restoration of a quarantined point must be rejected before modifying the worktree.
- Recheck the existing ledger on load and retention. Warn once per missing (turn, object) in a running session; after reopening, report the condition once again. This avoids a new persistence format solely for notification deduplication.
- Failed ledger writes roll back to the previous point list and report failure, without manufacturing checkpoint success.

Steps:
1. Add missing-root detection and retention reports in session/rewind.rs; test missing old trees alongside new valid trees, pin survival through GC, and ordinary Git failures.
2. Add runtime quarantine and deduplicated diagnostics to session/snapshot.rs, preserving stored IDs. Cover resume, repeated turn completion, explicit restore rejection, and ledger-write failure.
3. Run capabilities tests with the session feature (nextest when available), relevant coding/driver checks, workspace compile gate, formatting and diff checks.

Deferred: recursive tree/blob integrity validation, capture-to-commit temporary pinning, and shared-object retention changes. This is fault isolation, not proof of the user's underlying deletion cause.


## Follow-up: protect active captures and preflight restores

Baseline rechecked: release/v5.2.4, 17af32515d1e4edac76a80194c5900f813d21c29.
The two existing dirty source files and this plan belong to the first batch.

- WorkspaceCheckpoint owns RAII temporary capture references. Capture writes the
  reference while still holding the store lock. SnapshotHook holds the baseline
  pin through completion/replacement and the after pin through ledger persistence;
  diff readers hold a pin until they finish. Cancellation of a waiting listener
  does not release the baseline. Ordinary retention and purge preserve live pins.
- Dropping the last pin removes only its own reference. A crashed process may
  leave a conservative pin; no automatic stale-pin deletion is attempted without
  a proven ownership protocol. Store budget guards still apply.
- Before prepare/apply/direct restore or compensation writes files, recursively
  enumerate trees and batch-check descendant tree/blob availability and types.
  Recheck at apply because objects may disappear after prepare. Gitlinks belong
  to separate repositories and are excluded. This is availability validation,
  not a full blob-content hash/fsck scan. Failure leaves the worktree untouched.
- No session schema, lifecycle owner, source-repository refs, or alternates policy
  change. External cleanup of borrowed alternate objects remains an open cause.
- Test cross-handle retention/purge + GC while pinned, reference release and
  adoption by durable turn refs, and missing nested tree/blob at restore/apply.

Validation (both batches, 2026-10-16): capabilities with `session` feature:
1326 passed, 1 ignored; added hook-level pin/GC lifecycle regression: 1 passed;
coding runtime rewind criteria: 3 passed. Total 1330 passed. nextest is unavailable
on this machine, so these used cargo test. `bash gates/compile.sh`, rustfmt and
diff whitespace checks passed. No Windows host or original user store was tested.

## Pre-commit audit: retain rollback roots until durable commit

Found an existing ordering gap in both ledger replacement and the 100-point
retention boundary: old refs were deleted and eagerly collected before the new
ledger was saved. A failed save could restore the old ledger but not its objects.
A pending rewind also needs its previous points until runtime rebuild commits.

Retention now first pins the union of previous and target points. A successful
turn-ledger write can then collect old refs. Rewind ledger replacement preserves
its preimage until commit_rewind (or a later completed turn after recovery);
optional post-commit reference cleanup failures are logged rather than reporting
that a durable operation failed. On ledger write failure, the previous roots
remain available for rollback. Added regressions for a failed 101st-point save
and a pending workspace rewind followed by GC and compensation.

Final audit validation: capabilities/session full suite 1328 passed, 1 ignored;
coding runtime unit rewind filter 2 passed, 7 pre-existing ignored; runtime
criteria rewind filter 3 passed. Total 1333 passed. Compile gate and rustfmt
passed after the audit fix. Repository Cargo.lock has no private dependency
entries and is excluded from this commit. Windows execution remains unverified.
