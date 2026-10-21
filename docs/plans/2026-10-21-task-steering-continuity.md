# Task steering continuity implementation plan

**Goal:** Answer questions inserted during active task execution and continue the task without extra UI notices.

**Baseline:** `release/v5.2.4`, `67a7b3435fbb578bc49e0032e8d28602f7d4e508`; existing dirty files are preserved.

**Architecture:** CodingRuntime remains the lifecycle owner. Reuse TodoHook's existing offer_continuation through the mounted kernel-hooks row; do not add a second loop or turn hook. The harness log remains authoritative. Assistant MessageMeta supplies the actual execution turn, whereas the latest user message can be steering within that turn.

**Finding:** Contrary to the initial comparison, coding already has bounded completion nudges. Both eligibility and the nudge budget incorrectly restart at each user message. The harness todo-reminder row is disabled in coding and must remain disabled.

**Failure semantics:** Preserve cancellation, provider errors, round/token limits, and questions waiting for an answer. Do not reactivate stale plans from previous turns. Retain the three-nudge limit and no-progress guard across steering. User instructions to pause, cancel, or replace work take priority in model guidance. No additional UI status messages.

## Steps

1. Add regression criteria in `crates/atomcode-coding/src/todo.rs` for steering inside one execution, old plans in another execution, and budget/no-progress guards across steering.
2. Use the response's execution-turn metadata for continuation scope, retaining the existing fallback for conversations without metadata. In `crates/atomcode-coding/src/host_rows.rs`, supply stop checks the same log-backed metadata view already supplied to pre_request, without changing provider messages. Add concise task-continuity guidance to the existing ephemeral todo tail and completion nudge, including explicit pause/cancel/replace exceptions and no narration of internal reminders.
3. Add a mounted integration criterion using a scripted provider and an actual mid-turn message. Assert one turn, resumed tools, a completed list, and no intermediate TurnEnd.
4. Run relevant nextest criteria, affected coding crate tests, `cargo fmt --all` and its check, and `bash gates/compile.sh`. Inspect Cargo.lock without overwriting pre-existing changes.

**Tech stack:** Rust, existing LifecycleHooks, native harness events, cargo-nextest.

## Validation

- Isolated checkout: `/private/tmp/atomcode-task-steering-20261021`, the recorded baseline plus this change only. Primary working-tree changes were preserved.
- All 50 todo/steering criteria passed, including actual runtime steering, one start and one runtime terminal, completed tasks, cancellation, waiting for a person, and terminal provider failure.
- Negative control: restoring the original todo/host code makes the new steering criterion fail with two requests instead of four: it plans and answers, but never completes the task.
- The full crate run completed its unit tests; a subsequent non-fail-fast integration run completed all 540 integration criteria (535 passed, 5 failed). All five failures were reproduced with the original todo/host code: `a_write_outside_the_workspace_diverges_and_here_is_why`, `an_always_covers_the_scope_not_just_the_one_call`, `always_allow_grants_survive_reassembly`, `criteria::an_always_allow_survives_an_undo`, and `approval_sees_the_repaired_arguments_that_execute`. All concern approval prompts, not task continuation. No golden was changed.
- `bash gates/compile.sh` passed in the isolated checkout, including feature checks and negative controls. The primary working-tree gate failed on memory/i18n variants in concurrent pre-existing changes; those files were not repaired or reverted by this task.
- `cargo fmt --all`, its check, and `git diff --check` passed. Cargo.lock's pre-existing addition of the memory feature's i18n dependency was preserved; private crypto dependency names remained absent.
- No live provider/manual TUI trial was performed; the behavioral evidence is the scripted runtime test.

## Pre-commit audit

Reviewed against `release/v5.2.4` at `fe3321ead487e7ab95dc42b32b9c0fd17ac19bda`, including the intervening skills changes. No blocking finding: hook-only metadata restores the execution boundary without mutating provider input, and the existing runtime still owns every terminal. The 53 relevant criteria and `bash gates/compile.sh` passed on this isolated baseline, including compaction and quiet-list regressions as well as the new steering tests. Formatting and diff checks passed in the primary checkout. The five approval-related failures above remain baseline findings, not newly introduced changes.
