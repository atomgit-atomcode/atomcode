# Dirty worktree reminder implementation plan

**Goal:** Warn the model when a real user task starts with existing Git changes; never clean the workspace automatically.
**Architecture:** The existing coding session-context row adds a PreStep reminder using a stateless capabilities Git probe. Existing injection facts persist it before model requests. Preserve the frozen system prefix, native runtime ownership and all approval policy. No kernel, protocol or persistence schema changes.
**Tech Stack:** Rust, Tokio bounded subprocess, existing harness events and recording-provider tests.

Baseline: release/v5.2.7 at 592643ea3. Main worktree has unrelated Git branch/WebUI work; use /tmp/atomcode-dirty-worktree-reminder and transfer only this feature.

1. Add read-only Git status probe with total timeout, output bound, no optional Git locks, and four outcomes: clean, dirty, not a worktree, unknown. Do not report failures as clean. Cover staged/unstaged/untracked paths without sending arbitrary filenames into the new reminder.
2. Fix the existing session-start snapshot renderer to show unknown when status failed, without changing saved Git sections on resume.
3. Attach to existing session-context PreStep: only first step of a non-rejected real-user input, only root agents. Stamp reminder with turn; no inspection on every model request, no background/team/internal turns, no permissions or cleanup side effects.
4. Validate probe outcome/failure/bounds, repeated inputs, clean-to-dirty and dirty-to-clean transitions, resumed logs and target directory changes. Verify reminder is in log before first request and system prefix remains unchanged. Real subprocess tests use real time; timeout-only tests use paused Tokio time.
5. Run affected capabilities/coding tests, compile gate, fmt and lock checks. Defer stash/drop/restore classifier changes to a separate task. Do not commit or push without instruction.

Validation and limitations:
- Capabilities with session feature: 1587 passed, 2 skipped. Latest probe/snapshot subset: 4 passed.
- New coding scenarios: 5 passed (current state transitions, multi-request dedup, persisted resume with frozen prefix, different prepared directory, unknown state without refusing the task, internal/delegated exclusions).
- The full coding run exposed one new composition-audit defect: SessionSvc is agent-local, not a registered root row. Removed that false root dependency declaration; all 5 gate_order cases and all 5 new scenarios passed afterward (one unrelated opt-in gate test skipped).
- The remaining full run passed 1251 tests but exposed nine existing failures, plus the earlier excluded rate-limit test. All ten were reproduced without this feature: rate-limit allowance on the untouched main baseline 4be315e07; the nine below on clean 592643ea3.
  - differential: a_write_outside_the_workspace_diverges_and_here_is_why; an_always_covers_the_scope_not_just_the_one_call
  - permission_grants: always_allow_grants_survive_reassembly
  - recovery: mixed_failures_share_six_attempts_and_keep_partial_progress; mixed_failures_share_the_wait_allowance_without_shortening_retry_after; ordinary_request_recovery_is_finite; ordinary_requests_survive_a_short_gateway_outage
  - runtime_criteria: an_always_allow_survives_an_undo
  - tool_args_repair: approval_sees_the_repaired_arguments_that_execute
- Deferred those unrelated fixed-delay/jitter and approval-expectation failures; no approval or retry behavior is modified here.
- Main worktree compile gate passed after rerunning sequentially: an earlier concurrent main-worktree gate collided on its temporary self-test canaries. No gate scripts were modified and no other task's files were deleted.
- Only feature files were transferred; unrelated quota/login edits are preserved. No real model or destructive Git operations were used for this implementation's tests.
