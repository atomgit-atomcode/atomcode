# Busy Continue Fork Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Let a second interactive `atomcode -c` start from the latest committed context without sharing or corrupting the session already owned by another runtime.

**Architecture:** Keep `SessionLease` exclusive and fail-closed. Add a native aggregate fork operation to `SessionManager`; the CLI invokes it only when interactive `-c` receives `SessionInUse`, starts the runtime against the fork's new ID, replays the forked presentation, and passes a visible startup notice into the TUI. Headless continuation and every non-contention error remain unchanged.

**Tech Stack:** Rust, native `SessionManager` aggregate persistence, `CodingRuntime`, Clap CLI, AtomCode TUI/i18n.

---

### Task 1: Add a native session aggregate fork

**Files:**
- Modify: `crates/atomcode-capabilities/src/session/manager.rs`

1. Add a failing test that holds the source lease, forks its complete native aggregate to a caller-provided UUID, and verifies the source and destination have independent IDs and artifacts.
2. Add tests that a missing/corrupt source fails without publishing a destination and that a pre-existing destination remains protected.
3. Implement `SessionManager::fork_native_session` by loading one strict native aggregate, creating fresh destination metadata, acquiring the destination lease, and committing snapshot, presentation, and metadata atomically.
4. Preserve messages, presentation, turn statistics, working directory, and user-visible title while resetting destination identity, timestamps, and legacy-import provenance.
5. Run the focused session-manager tests.

### Task 2: Fall back from interactive continue to a fork

**Files:**
- Modify: `crates/atomcode-cli/src/main.rs`

1. Add a focused helper test proving only `SessionStoreError::SessionInUse` plus interactive mode selects the fork path.
2. Change CLI runtime preparation to return the actual continued session ID and optional source ID.
3. On normal lease acquisition, resume the requested ID exactly as before.
4. On interactive contention only, generate a new UUID, fork the source aggregate, and transfer the destination lease into `CodingRuntime`.
5. Keep headless `-c`, corrupt data, missing artifacts, permission failures, and destination commit failures explicit.
6. Load replay/telemetry state from the actual destination ID.

### Task 3: Show the fork decision in the TUI

**Files:**
- Modify: `crates/atomcode-config/src/i18n/messages.rs`
- Modify: `crates/atomcode-config/src/i18n/en.rs`
- Modify: `crates/atomcode-config/src/i18n/zh_cn.rs`
- Modify: `crates/atomcode-tuix/src/lib.rs`
- Modify: `crates/atomcode-tuix/src/event_loop/mod.rs`

1. Add English and Chinese text explaining that the latest session is active elsewhere and an independent fork was created from its last committed state.
2. Pass an optional startup notice into the TUI context.
3. Render the notice once before replaying the forked session.
4. Add a focused rendering/state test if the existing event-loop harness exposes startup output; otherwise cover message formatting and argument plumbing at compile time.

### Task 4: Verify and audit

**Files:**
- Verify only the files above and preserve all pre-existing dirty persona/site files.

1. Run the focused capabilities, CLI, and TUI tests.
2. Run `cargo test -p atomcode-capabilities --features session`.
3. Run `cargo test -p atomcode-cli`.
4. Run `cargo test -p atomcode-tuix`.
5. Run `git diff --check`.
6. Audit session owner, lease transfer, source/destination IDs, replay binding, telemetry binding, error propagation, headless behavior, and dirty-worktree isolation.
