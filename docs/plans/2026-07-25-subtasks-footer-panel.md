# Subtasks Footer Panel Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use executing-plans to implement this plan task-by-task.

**Goal:** Render concurrent `task` progress as one fixed, in-place footer panel instead of permanent transcript output or a rapidly changing spinner label.

**Architecture:** The TUI remains the sole owner of the presentation projection. It seeds a structured subtask list from `ToolCallStarted.arguments`, folds the existing task progress messages into that list by stable `explore#N` / `worker#N` labels, exposes the projection through `StatusLine`, and clears it at the matching tool terminal. Kernel and coding-runtime protocols remain unchanged.

**Tech Stack:** Rust, atomcode-tuix retained renderer, existing `AgentEvent::ToolOutputChunk` and footer layout.

---

### Task 1: Model and parse subtask progress

**Files:**
- Modify: `crates/atomcode-tuix/src/render/mod.rs`
- Modify: `crates/atomcode-tuix/src/state.rs`
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs`

1. Add failing tests for seeding task descriptions and folding start/activity/completion updates.
2. Add a TUI-owned `SubtaskProgress` view keyed by call id and child label.
3. Keep generic tool progress unchanged; consume only progress belonging to `task`/`code_review`.
4. Clear the matching projection on success, failure, cancellation, session reset, or runtime replacement.

### Task 2: Render the fixed footer panel

**Files:**
- Modify: `crates/atomcode-tuix/src/render/retained.rs`

1. Add failing row-layout tests for header, running/completed/failed items, truncation, and row cap.
2. Add the panel above the input box using the existing top-panel/footer-height machinery.
3. Keep each child to one row; show the common model once in the header and per-row models only when mixed.
4. Hide the panel behind approval/user-input/round-cap panels and collapse TodoWrite while subtasks are active.

### Task 3: Remove transient transcript noise

**Files:**
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs`
- Modify: `crates/atomcode-tuix/src/render/retained.rs`

1. Stop rendering `dispatching`, child start, child activity, and child completion messages as `CommandOutput`.
2. Prevent subagent activity from replacing the ordinary spinner label.
3. On the matching task terminal, remove the footer projection and retain only the existing compact committed `Task(...)` tool row.

### Task 4: Verify lifecycle and rendering invariants

**Files:**
- Test: `crates/atomcode-tuix/src/event_loop/mod.rs`
- Test: `crates/atomcode-tuix/src/render/retained.rs`
- Test: `crates/atomcode-tuix/src/state.rs`

1. Run focused event-loop and footer tests.
2. Run `cargo test -p atomcode-tuix`.
3. Run `git diff --check` and inspect the final diff without modifying unrelated worktree changes.
