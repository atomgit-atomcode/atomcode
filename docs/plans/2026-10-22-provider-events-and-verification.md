# Provider Events and Verification Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Preserve provider response facts through the active harness and recognize completed verification consistently for foreground and background commands.

**Architecture:** CodingRuntime remains the lifecycle owner. The existing ModelResponse carries provider facts into the existing AssistantMessage fields; session events remain authoritative and the persisted schema does not change. Verification remains a bounded judgement of logged tool receipts, not another completion controller.

**Tech Stack:** Rust, Tokio, atomcode-kernel/harness/coding, cargo nextest.

---

### Task 1: Preserve provider stream facts

**Files:** `crates/atomcode-harness/src/events.rs`, `crates/atomcode-harness/src/plugins/agent_loop.rs`, `crates/atomcode-harness/src/plugins/recovery.rs`; existing harness integration tests.

1. Add failing cases for split usage, response identity, signed/redacted blocks and two-request replay.
2. Carry identities and completed reasoning blocks in ModelResponse. Fold usage with the existing TokenUsage contract. Populate existing metadata and reasoning_blocks at commit.
3. Preserve completed blocks on partial recovery, without executing incomplete tool calls or attributing the failed attempt to the successful retry.
4. Run affected harness tests with `cargo nextest run -p atomcode-harness`.

### Task 2: Align verification receipts

**Files:** `crates/atomcode-coding/src/discipline/verify.rs`, `crates/atomcode-coding/src/discipline/mod.rs`, `crates/atomcode-coding/src/controllers.rs`.

1. Add failing cases: preparation is not verification; foreground checks still count; known background check exit 0 counts; running/failed/killed/unknown jobs do not; a job started before a later edit cannot validate it.
2. Associate successful bash_start receipts with their command, job id and edit generation. Reuse a single strict terminal-status parser with goal evidence selection.
3. Exclude known preparation operations while retaining custom verification commands. Keep explicit user execution limits and the existing once-per-edit reminder bound.
4. Run coding library tests with `cargo nextest run -p atomcode-coding --lib`.

### Task 3: Verify consumers

Run affected crate suites, `bash gates/compile.sh`, `cargo fmt --all`, `cargo fmt --all -- --check`, and inspect the final diff and Cargo.lock. Existing differential golden files must not be re-recorded. Do not stage or commit unrelated work.

## Execution results

- Task 1 complete: response identity/model and completed signed blocks reach existing session events; split/cumulative usage uses `merge_max`. Resume and interrupted-attempt replay are covered by mocked-provider integration tests; unfinished thinking receives no invented signature.
- Task 2 complete: common preparation commands no longer discharge verification; background verification requires a known successful start, exit 0 and the matching latest edit. Explicit execution limits and the once-per-edit reminder remain in place.
- Validation: harness suite 270 passed; the subsequently added recovery integration target passed both tests. Coding library 614 passed (8 skipped); verify_cadence/runtime_criteria/differential/mount_wiring 191 passed. `bash gates/compile.sh` passed, including its negative controls. Formatting and diff checks passed; Cargo.lock is unchanged and contains none of the three prohibited packages.
- Limits: command classification remains heuristic, and exit 0 is evidence of successful execution rather than proof of task correctness. Tests use scripted providers; no live-provider network validation was performed.
