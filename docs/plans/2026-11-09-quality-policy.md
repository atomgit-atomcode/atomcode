# Quality Policy Implementation Plan

**Goal:** Add opt-in `/quality on|off|status|evidence` for ordinary coding tasks, independent of approval mode.

**Architecture:** The coding runtime owns session policy through `CodingParts` and ordered runtime controls. A coding harness row requests configured checks through the existing bash tool pipeline before accepting a normal handoff. Kernel lifecycle and persistence formats remain unchanged. Terminal listeners only finalize evidence; they do not restart execution.

**Tech Stack:** Rust, existing plexus rows, native session log, atomic JSON sidecars, Git read-only snapshots.

## Baseline and boundaries

- Branch `release/v5.2.7`, commit `a32f38bee`; initial worktree clean.
- Reuse `agent/request`, normal tool execution and `turn/finishing`; do not add a second runtime or execute shell commands outside approval.
- Existing verify cadence is advisory, not a configured gate. `turn_complete` is observation-only.
- No automatic rollback, commits, inferred CI commands or remote evidence storage.
- MVP: Git working tree and ordinary lead turns only. Goal/loop integration is explicitly deferred and refused while quality is enabled.
- Gate denial, failure, timeout, cancellation, missing execution receipt, snapshot failure and evidence write failure never certify a pass.
- Evidence describes tracked differences and nonignored untracked files; ignored build artifacts and external services are outside its guarantee.

## Implementation steps

1. Define validated `[quality]` configuration: disabled by default, named commands, bounded foreground timeout. Preserve through config load/save and coding configuration.
2. Add quality policy/evidence state with before/after Git fingerprints and atomic private evidence files under host-supplied ProductDirs. Keep this policy through same-session runtime rebuilds, reset for a new session.
3. Mount one coding row. Capture baseline before model work. At a normal handoff request checks sequentially through ordinary tool calls, require matching execution facts, and stop on failure. Recheck the fingerprint after checks. Never ask the model to certify its own checks.
4. Add ordered runtime control and a shared capability command for all drivers that expose the command catalog. Mutations are idle-only. Status revalidates evidence against current files. Register describe_self guidance on the owning row.
5. Cover pass/fail/cancel, stale evidence, approval denial, unchanged tasks, dirty baselines and replacement continuity with meaningful tests. Document configuration and MVP limits in both languages.
6. Run affected-crate nextest suites, `bash gates/compile.sh`, formatting and lock/diff audits. Do not commit or push without the user's delivery instruction.

## Verification notes

- Baseline comparison uses a detached worktree at `a32f38bee` and a separate target directory. Five existing recovery timing tests fail unchanged there: `coding_rate_limits_share_the_attempt_and_wait_allowance`, `mixed_failures_share_the_wait_allowance_without_shortening_retry_after`, `mixed_failures_share_six_attempts_and_keep_partial_progress`, `ordinary_requests_survive_a_short_gateway_outage`, and `ordinary_request_recovery_is_finite`. They assert fixed waits while the current recovery path applies jitter. Do not change recovery behavior or its tests as part of quality policy.
- The initial composition audit caught missing service declarations on the quality row; fixed by declaring both provided services. The final targeted audit must pass.
- Shell exit 7 is intentionally not ToolResult.is_error in the existing shell contract. Quality uses the new typed process receipt rather than parsing or trusting that flag.

## Completed verification

- `bash gates/compile.sh`: passed, including all-targets, gated features and negative controls.
- Quality plus composition regression tests after final review: 21 passed (16 quality scenarios and 5 composition cases; 1 pre-existing skip).
- Coding suite excluding the five baseline failures identified above: 1306 passed, 16 skipped including those five exclusions.
- Config: 362 passed. Harness: 313 passed. Capabilities: 1254 passed, 1 skipped. Daemon: 414 passed, plus the added cross-session quality routing regression passed separately.
- WebUI: TypeScript check and 427 tests passed. Chinese/English command documentation, README entries and scoped search-index updates were checked.
- Formatting, diff whitespace and Cargo.lock checks passed. No version bump.
- Builds used an isolated temporary target directory because another worktree was sharing the checkout's normal Cargo artifacts. The detached baseline comparison worktree and its build directory were removed after confirming the pre-existing failures.
- Windows shell behavior and manual TUI/browser interaction were not exercised; CI/all-target compilation does not substitute for those checks.

## Final review (2026-11-10)

- A verified handoff may be reopened by queued input in the same harness turn. Start a new quality attempt when the prior attempt is finished, even if the session/turn ids are unchanged; otherwise terminal receipts from the reopened work are ignored. A synchronous assistant-message observer exercises this boundary deterministically.
- On/off are policy controls and do not need historical audit files. Corrupt old evidence must not block these switches. A resumed-session regression verifies both switches remain usable.
- Inspecting an active attempt must not run crash recovery against its live evidence file. Restore only while idle, and never restore over an existing in-memory attempt; the cancellation scenario also asserts inspection leaves the on-disk status verifying.
- Remote release gained ATI commits `5730add1b` and `297ffe090` during implementation. Preserve these commits and revalidate integration before pushing; never force-push over them.
