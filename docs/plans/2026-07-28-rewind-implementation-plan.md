# Rewind Implementation Plan

> **v5.0.4 status override:** Tasks below describe the original implementation.
> Workspace/code Rewind is disabled in v5.0.4 after the per-session Git object
> stores were found able to exhaust the system disk. Current production behavior
> records conversation-only points with optional/absent workspace trees, keeps
> conversation Rewind available, and rejects code-only or combined scopes with an
> explicit reason. Re-enabling code restoration requires a separate plan covering
> a project-shared object store, source-object reuse, quotas, free-space checks,
> killable timeouts, GC, and orphan cleanup.

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Original goal (superseded for v5.0.4):** Add a safe Claude-style Rewind picker
with conversation-only, code-only, and combined restoration.

**Original architecture (workspace portion disabled for v5.0.4):** Extend the
existing native session `SnapshotHook` with a separate-Git-dir workspace
checkpoint service, persist compact per-turn rewind metadata, and expose one
runtime-owned rewind operation. The TUI is a pure selector/projection and
defaults to `(current)`.

**Tech Stack:** Rust, Tokio, serde/serde_json, Git plumbing commands, crossterm, AtomCode native session/runtime APIs.

---

### Task 1: Workspace checkpoint service

**Files:**
- Create: `crates/atomcode-capabilities/src/session/rewind.rs`
- Modify: `crates/atomcode-capabilities/src/session/mod.rs`
- Test: inline tests in `rewind.rs`

**Steps:**

1. Write failing tests for Git worktree detection, capture, changed-file summary,
   untracked-file capture, ignored-file exclusion, restore, and conflict detection.
2. Run `cargo test -p atomcode-capabilities --features session session::rewind`.
3. Implement a separate-Git-dir store using explicit `--git-dir` and `--work-tree`.
4. Make restore file-scoped and capture a recovery tree before mutation.
5. Re-run the focused tests.

### Task 2: Persist per-turn rewind metadata

**Files:**
- Modify: `crates/atomcode-capabilities/src/session/manager.rs`
- Modify: `crates/atomcode-capabilities/src/session/snapshot.rs`
- Modify: `crates/atomcode-capabilities/src/session/mod.rs`
- Test: manager and snapshot inline tests

**Steps:**

1. Write failing tests for `<id>.rewind.json`, bounded deserialization, deletion,
   and turn-start/turn-complete point creation.
2. Add versioned `RewindLedger` and `RewindPoint` types.
3. Capture the before-tree in `turn_start` and the after-tree/diff in
   `turn_complete`.
4. Keep checkpoint failure best-effort for ordinary turns but record an explicit
   unavailable reason for the UI.
5. Run the session tests.

### Task 3: Runtime-owned rewind operation

**Files:**
- Modify: `crates/atomcode-coding/src/runtime.rs`
- Modify: `crates/atomcode-coding/src/parts.rs`
- Test: runtime inline tests

**Steps:**

1. Write failing tests for listing targets and the three rewind scopes.
2. Add neutral `RewindScope`, `RewindTarget`, `RewindResult`, and typed errors.
3. Add `CodingRuntimeHandle::rewind_points()` and `rewind(...)`.
4. Reuse the existing undo computation and native aggregate commit.
5. Add workspace compensation when combined conversation persistence fails.
6. Reject busy, stale-generation, stale-revision, conflicting-file, and
   unavailable-checkpoint requests.
7. Run `cargo test -p atomcode-coding --lib`.

### Task 4: Rewind modal and Esc routing

**Files:**
- Create: `crates/atomcode-tuix/src/modals/rewind.rs`
- Modify: `crates/atomcode-tuix/src/modals/mod.rs`
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs`
- Modify: `crates/atomcode-tuix/src/state.rs`
- Modify: `crates/atomcode-config/src/i18n/messages.rs`
- Modify: `crates/atomcode-config/src/i18n/en.rs`
- Modify: `crates/atomcode-config/src/i18n/zh_cn.rs`
- Test: modal and event-loop inline tests

**Steps:**

1. Write failing tests proving `(current)` is initially selected and Enter is a
   no-op.
2. Add prompt-list and scope-selection modal states.
3. Replace direct double-Esc `dispatch_undo` with async target loading and modal
   installation.
4. Clear/suppress rewind arming on streaming cancellation.
5. Route submitted rewind through the runtime and repaint only after its terminal.
6. Run `cargo test -p atomcode-tuix --lib`.

### Task 5: Cross-layer audit and verification

**Files:**
- Update: `docs/plans/2026-07-28-rewind-design.md` if implementation differs

**Steps:**

1. Run `cargo fmt --all -- --check`.
2. Run `cargo test -p atomcode-capabilities --features session`.
3. Run `cargo test -p atomcode-coding --lib`.
4. Run `cargo test -p atomcode-tuix --lib`.
5. Run `cargo check --workspace` only if dependency/features changed beyond what
   the preceding tests compiled.
6. Audit CLI, daemon, background, ACP and clix impact; confirm unchanged drivers
   keep the existing explicit `/undo` behavior.
7. Produce a manual checklist for Git/non-Git projects, cancelled turns,
   conflicts, resume, and combined recovery.
