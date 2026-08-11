# Tasks Long-Line Rendering Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Keep the pinned Tasks panel bounded while making the current task readable across up to three wrapped terminal rows.

**Architecture:** Preserve `TodoProgress` and the existing frontier-window selection. Compute the current task's visual line count before selecting logical rows, reduce the logical row budget by the additional wrapped rows, and render continuation rows with a hanging indent. `/todo` remains the full-list view; no runtime, persistence, protocol, or keyboard-focus state changes.

**Tech Stack:** Rust, atomcode-tuix retained terminal renderer, existing display-width wrapping helpers.

---

### Task 1: Add visual-row regression coverage

**Files:**
- Modify: `crates/atomcode-tuix/src/render/retained.rs`

1. Add a test with a long pending frontier task and a fixed-width terminal.
2. Assert that the task uses continuation rows, stays within `MAX_TODO_PANEL_ROWS`, and retains a folded `+N more` indicator.
3. Add a short-task parity assertion proving the existing one-row layout is unchanged.

### Task 2: Allocate Tasks by visual rows

**Files:**
- Modify: `crates/atomcode-tuix/src/render/retained.rs`

1. Add a helper that wraps one current task into at most three display-width-safe lines.
2. Determine the current frontier as the in-progress task, otherwise the first pending task.
3. Subtract continuation rows from the logical task-window budget before calling `todo_panel_rows`.
4. Render continuation rows with a hanging indent aligned to the task text.
5. Make footer row measurement use the same width-aware builder so measurement and painting cannot diverge.

### Task 3: Verify the retained footer

**Files:**
- Test: `crates/atomcode-tuix/src/render/retained.rs`

1. Run the focused todo renderer tests.
2. Run `cargo test -p atomcode-tuix --lib`.
3. Run `git diff --check` and inspect the final diff, preserving the pre-existing text-caret change.
