# Goal Evidence and Accounting Implementation Plan

**Goal:** Correct observed goal statistics across pause/resume and keep current validation receipts in the evaluator's bounded input.
**Architecture:** CodingRuntime remains the sole owner. Reuse its generation-fenced Usage/ToolStarted event processing to count observed work once, before pause can make the goal inactive; remove the second aggregation at snapshot completion. Evaluation continues to read only the native snapshot, with a bounded receipt projection rather than a second store or lifecycle hook. No persistence format or permission changes.
**Tech Stack:** Rust, existing coding/kernel neutral events, Tokio virtual-time tests, nextest.

Baseline: dedicated worktree feat/goal-verdict-evidence at 0faa4f508 (main advanced concurrently from initial inspection 18ee021c4). Runtime/controller code is still the f89201f33 implementation. Preserve all unrelated IDE/relay/config changes in main. No commit/push in this task.

## 1. Reproduce
- Add runtime regression tests in runtime/goal_recovery_tests.rs for live observed counts, explicit pause including final in-flight usage, cancel, and no double counting on a normal terminal. Reuse controller_test_runtime and its existing commands/events.
- Add controller tests recreating the real session's four historic failures + latest no-tasks receipt + validation output near the end + final report beyond 200 chars. Include an error in the current submission and enforce bounded Unicode-safe output.
- Run the focused tests first and record the expected failures.

## 2. Runtime accounting
- Update the existing Usage and ToolStarted owner branches. Count only an active goal's work, plus its cancelling turn while explicitly paused; unrelated work while an idle goal is paused must not count.
- Keep evaluator usage accounting separate. Remove snapshot-time main aggregation so a terminal cannot count the same event twice.
- Existing GoalChanged pause/terminal events persist the already observed totals. A final usage observed while cancellation drains must republish paused progress. Never reconstruct or fabricate previously lost historical totals from an ambiguous old checkpoint.
- Verify the native checkpoint restart tests still retain new totals and release the lease.

## 3. Evaluator receipts
- Select evidence from the latest submitted conversation segment (including synthetic goal continuation), not every historic error. Keep the genuine compaction anchor separately and bound it.
- Reserve space for recent successes and current failures; keep tool identity/invocation context and head/tail of long outputs. Do not fabricate exit codes or file existence from assistant claims.
- Retain a much larger head/tail of the latest assistant report, clearly identified as a claim. Keep recent edited paths rather than the oldest first 20.
- Preserve the overall 6000-character recap cap and existing sentinel/anchor trust checks. No new model/tool request for summary generation.

## 4. Verify and integrate
- Run focused failures to green, then scoped coding tests and bash gates/compile.sh, rustfmt/diff and lock audits.
- Integrate only this worktree's increment into main without overwriting concurrent changes.
- Build an isolated local test binary with public stub; if AtomGit requires the existing private build overlay, use the already authorized local build path without committing secrets or private dependencies.
- Reuse session 9139eee3-fd0b-4da6-b3e2-8c2e3a6dea18: do not continue product development. Recheck its existing artifacts with a short bounded validation receipt, pause/resume observed new statistics, then exercise tool-free evaluator-only recheck. Record real outcomes and unverified outage conditions.

Failure semantics: current errors remain visible and can block satisfaction; no evidence selection grants tools or claims successful validation. If live retest is unavailable, report that explicitly. Old checkpoint counters that were already lost cannot be reliably repaired automatically.
