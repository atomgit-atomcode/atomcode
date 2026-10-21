# Goal resilience phase 1 Implementation Plan

> Execute in this session using the existing runtime owner and scoped verification.

**Goal:** Recover unattended goals from transient provider failures and transient or unusable evaluator responses, with bounded, cancelable waits and visible status.

**Architecture:** CodingRuntime remains the only owner. Reuse the kernel's existing turn-complete funnel, partial-output persistence and harness stream recovery. Hold the already committed turn while a generation/controller/attempt-tagged timer waits; do not replay tools or add hooks. User input invalidates pending work; pause, cancel, reconfigure and shutdown invalidate it through the controller token and epochs. No durable goal state or automatic model fallback in this phase.

**Tech Stack:** Rust, Tokio virtual clocks, existing config catalog and runtime/host events.

---

Baseline: release/v5.2.4 at 82b6f5a3b, clean working tree; isolated /tmp/atomcode-goal-resilience.

## Steps

1. Add [goal.recovery] policy: bounded retries and elapsed outage budget, capped exponential jitter, independent evaluator retry cap and attempt timeout. Default provider outage recovery: 12 retries / 1800s, 30s base / 300s max. 0 attempts opts out. Preserve global model timeout defaults.
2. Add goal-owned recovery bookkeeping and evaluation epochs, with virtual-clock unit tests for budgets, reset, stale work and cancellation.
3. Make evaluator setup and collection cancelable and bounded. Retry only transient provider errors and invalid/empty verdicts, preserving the original held snapshot. Permanent configuration/auth/permission/quota errors end or pause explicitly. Never treat an unknown verdict as success.
4. Add a runtime-owned wakeup channel that only schedules work for the matching generation, goal and epoch. Delay transient main-turn recovery instead of immediately injecting continuations. Do not infer retryability from error prose. Rate-limit retries remain owned by the existing provider layer; exhausted 429/quota errors are not retried by the goal without authoritative timing.
5. Expose recovery metadata through existing goal progress, CLI/TUI and WebUI; show reason, attempt and next wait. Keep accepted foreground operations paired with exactly one terminal.
6. Inject transient failures, empty/invalid evaluator results, permanent failures, budget exhaustion, cancel/pause/user input and late wakeups. Use paused Tokio clocks where timeouts/backoff are involved.
7. Run affected crate tests, fmt/diff checks and gates/compile.sh before integration. Keep Cargo.lock clean; inspect current overlay before changing any local Git flags.

## Ownership and failure semantics

Session content remains the native event log; persistence failures remain fail-closed. Recovery does not grant approvals, reset overall goal round limits, restart completed tools or replace the primary model. Existing max-round, time, tool-loop and no-progress limits still apply. User cancellation always wins; delayed events cannot act on a new goal or replacement runtime. Evaluation retries accrue usage and have their own cap; they do not increment main work rounds.

## UI cancellation seam

The kernel is idle while the runtime holds a completed work turn for evaluation or cooldown. Goal progress therefore includes an in-flight evaluation state (attempt 0), and the neutral host Running contract carries a defaulted recovering flag. TUI stop gestures count that retained work only when viewing the lead; WebUI keeps its stop action available. Both still invoke the same runtime cancellation command. A recovery attempt with retry_in_secs = 0 is running, not another timer.

## Verification

- Scoped coding suite: 1026 passed, 9 failed, 9 skipped. All 9 failures reproduce on unmodified baseline 82b6f5a3b: outside-workspace approval/grant checks and compaction history/reference expectations. No golden files were changed.
- Final coding library suite after cleanup: 521 passed, 8 feature-gated tests skipped.
- Config, host API, TUI, CLI and daemon: 2974 passed.
- WebUI typecheck and all 279 tests passed.
- gates/compile.sh passed including feature-gated targets and negative controls; fmt and diff checks passed. Cargo.lock unchanged.
- Fault injection uses virtual time; no real overnight lumen-alpha run or Windows manual UI test was performed. Goal state remains process-local in this phase.
