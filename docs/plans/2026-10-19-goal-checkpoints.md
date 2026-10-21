# Goal checkpoints implementation plan

Goal: retain a goal and its run report across native session resume; explicitly retry only the evaluator without replaying main work.
Baseline: release/v5.2.4 c49616dc1 with approved, uncommitted resilience work. Work in /tmp/atomcode-goal-checkpoints; preserve all pre-existing changes.

Owner and architecture: CodingRuntime remains the sole controller owner. Conversation and tool results come only from the native event log. A versioned, bounded .controller sidecar carries non-authoritative controller progress and report data under the existing session lease; it cannot restore approvals, pending requests, a provider, conversation or tools. SessionManager only stores opaque typed data and deletes it with the session. Goal progress is an immutable projection used for checkpoints/reports, not another live state machine.

Decisions: prefer an independent control sidecar to adding coding goal facts to neutral kernel session vocabulary. Restored unfinished goals are paused; a mismatched log marker invalidates evaluator readiness. No automatic tool execution on startup. /goal recheck obtains a runtime snapshot receipt, validates generation/revision and idle state, then performs tool-free evaluation only; an unmet verdict pauses rather than continuing main work. /goal report works during and after a goal, including after resume.

Steps:
1. Add lease-checked, bounded, atomic controller-sidecar storage and deletion; test corrupt/symlink/future/absent cases at the appropriate boundary.
2. Add serialized goal progress/report fields and restore-as-paused behavior. Count evaluator attempts, provider recoveries, tool calls, tokens and observed waiting time.
3. Save significant progress at the runtime owner boundary, before acknowledging a new goal and on shutdown. Handle storage failure explicitly, avoid stale state across session/provider changes, and keep terminal reports.
4. Add runtime-owned recheck/report controls and shared slash commands for CLI/TUI/daemon. No main replay during recheck; late evaluations remain fenced by generation/controller/epoch.
5. Test restart, terminal reports, log changes, evaluation-only outcomes, cancellation and storage failures. Run scoped tests, full compile gate, fmt/diff and lock checks. Integrate without committing.

Failure semantics: checkpoints never authorize actions. Unknown versions/corruption produce an explicit warning and block overwriting the unresolved checkpoint. A checkpoint write failure stops autonomous continuation and reports the loss of durability. Native session write failures retain their existing fail-closed behavior. Recheck refuses busy/changed sessions or goals without a completed work boundary. A process interrupted mid-tool requires human review; this phase does not claim external-operation idempotence or unattended process relaunch.

Verification (2026-10-19):
- Coding full nextest: 1053 run, 1044 passed, 9 failed, 9 skipped. The failures match the previously confirmed baseline: two approval differential cases, grant reassembly, approval after undo, rolling compaction criteria, three compaction/session-reference cases, and repaired-argument approval. No golden files changed.
- Final checkpoint/command tests after the storage hardening: 9 passed. They cover a real goal and restart, evaluator-only NotMet/Met, stale snapshot refusal, invalid log marker, future/corrupt/oversized data, symlink refusal, unwritable storage, shutdown lease release, deletion, command routing, and observed wait accounting.
- Capabilities native-session and fixture tests: 11 passed.
- Isolated worktree compile gate (workspace all targets, gated features, both negative canaries): passed.
- Integrated the increment into release/v5.2.4 without committing or pushing. All 11 implementation files match the validated worktree. Final main-worktree compile gate also passed, including gated features and both negative canaries.
- rustfmt check, diff whitespace check and lock audit passed. Cargo.lock unchanged and no hkdf/hmac/zeroize_derive contamination.

Limits: checkpoints cover native persistent sessions. Nonpersistent sessions retain reports only while the runtime lives. Reports count observed, saved progress, not an exactly-once external-operation ledger. Restoring a session never runs the goal automatically; inspect uncertain tool effects before explicitly continuing. This change does not add a process supervisor or unattended crash relaunch. Windows crash/restart has not been exercised on a real Windows host in this phase.

Audit follow-up (2026-10-19):
- Reviewed runtime ownership, generation/epoch fencing, recovery terminal paths, native checkpoint storage, report restoration, provider error metadata and CLI/TUI/daemon/WebUI consumers. Baseline at audit start: c49616dc1; concurrent unrelated WebUI work was committed as 118baaa11 while the audit ran. Goal changes remain uncommitted.
- Fixed a blocked-recovery branch that emitted TurnFinished without leaving InTurn. It now reports Ready when the agent remains available (the pending policy still blocks submits), or Failed when unavailable.
- Fixed restoration after a person grants a new time window: cumulative elapsed time cannot represent the current window's remaining budget. Checkpoints now retain remaining_time_secs, subtract offline time on resume, and fall back to the earlier calculation when the field is absent. Conversation authority and automatic-start behavior are unchanged.
- Added two regression tests. Both failed before their fixes and passed afterward: policy_blocking_a_recovery_closes_the_foreground_and_clears_busy; checkpoint_restores_the_current_renewed_time_window.
- Combination regression (coding/config/kernel/harness/host-api/TUI): 3710 run, 3705 passed, 5 failed, 9 skipped. Four existing compaction/session-reference failures and the known row-contribution scanner failure remain.
- Coding independently with its default features: 1055 run, 1051 passed, 4 failed, 9 skipped. The remaining failures are the same four existing compaction/session-reference cases; no new failure was observed.
- WebUI: typecheck passed, 284 tests passed, production build passed.
- Full compile gate including gated features and both negative controls passed. Formatting, whitespace and Cargo.lock checks passed. No goal commit or push was performed. Windows real-host overnight verification remains outstanding.
