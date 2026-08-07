# RequestUserInput Review Page Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use `executing-plans` to implement this plan task-by-task.

**Goal:** Replace the full-width reverse-video text field with a readable boxed input and make the final batch stop review every answer before submission.

**Architecture:** Keep `UserInputBatch` as the sole owner of in-progress answers. Project immutable answer summaries into the renderer-facing batch metadata; the renderer wraps and scrolls the review page without changing the tool wire response. Existing partial-submit semantics remain: unanswered questions are shown explicitly and serialize as declined responses.

**Tech Stack:** Rust, AtomCode TUI retained renderer, crossterm key events, existing virtual-terminal tests.

---

### Task 1: Model review summaries and scrolling

**Files:**
- Modify: `crates/atomcode-tuix/src/state.rs`
- Modify: `crates/atomcode-tuix/src/render/mod.rs`
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs`

1. Add a renderer-neutral summary record containing header, question, and optional formatted answer.
2. Add batch-owned submit-page scroll state.
3. Project summaries and submit scrolling through `UserInputBatchMeta`/`UserInputPanelView`.
4. Handle PageUp/PageDown while the batch cursor is on the submit stop; preserve Tab/Shift+Tab and Enter behavior.
5. Add state/key tests for selected, custom, text, and unanswered summaries.

### Task 2: Render a real text input field

**Files:**
- Modify: `crates/atomcode-tuix/src/render/retained.rs`

1. Add a failing renderer test that rejects reverse-video cells in text input mode.
2. Render a theme-aware three-row box with a prompt marker, answer text, and insertion caret.
3. Wrap or truncate within the field width without emitting control characters.
4. Update row-count assertions and verify light/dark-compatible styles.

### Task 3: Render the final review page

**Files:**
- Modify: `crates/atomcode-tuix/src/render/retained.rs`

1. Add a failing test requiring every question and answer on the submit stop.
2. Render answered values and an explicit `未回答` marker.
3. Keep the confirm action as the active row so small terminals retain an actionable viewport.
4. Support PageUp/PageDown indicators for summaries taller than the terminal.
5. Run focused tests, then `cargo test -p atomcode-tuix --lib` and `git diff --check`.
