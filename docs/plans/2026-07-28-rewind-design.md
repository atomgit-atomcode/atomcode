# AtomCode Rewind Design

## Goal

Replace the unsafe “double Esc immediately runs `/undo`” gesture with an explicit
Rewind workflow that can restore conversation state, workspace files, or both.
The picker opens on `(current)`, so an accidental double Esc plus Enter is a no-op.

## Ownership and boundaries

`CodingRuntime` remains the only owner of the live coding lifecycle. Rewind is a
runtime operation, not a TUI-side combination of filesystem writes and `/undo`.
The TUI lists targets, selects a scope, submits one request, and waits for one
success or failure terminal.

Workspace capture belongs to `atomcode-capabilities::session`. It uses the
existing `SnapshotHook::turn_start` and `turn_complete` seams, so no second
per-turn state machine is introduced. The kernel remains neutral and unchanged.

The workspace backend uses a separate Git directory under AtomCode's data root:

```text
~/.atomcode/rewind/<project-hash>/
```

Commands always pass both `--git-dir` and `--work-tree`; they never change the
user repository's branch, HEAD, index, or stash. The first implementation is
available only in Git worktrees. Ignored and oversized untracked files are not
claimed as recoverable.

## Per-turn data

Each accepted user turn records a rewind point:

```text
prompt ordinal and preview
conversation revision/boundary
workspace tree before the turn
workspace tree after the turn
changed-file summaries
```

The before-tree is captured at `turn_start`, before tools execute. The after-tree
and diff summary are captured at unconditional `turn_complete`, including
cancelled and failed turns.

The session stores only compact metadata and Git tree identifiers. Blob content
lives in the separate Git object database. Session deletion removes the metadata;
project snapshot garbage collection is independent.

## Rewind transaction

Rewind is accepted only while the runtime is idle. It validates generation,
session binding, conversation revision, and selected target. Before modifying
anything it captures the current workspace as a recovery tree.

For code restoration, AtomCode first compares the current state of every affected
file with the recorded post-turn state. A mismatch means the file was modified
after the checkpoint; the operation fails closed and reports conflicts. It never
silently overwrites those files.

For a combined rewind:

1. capture the recovery workspace tree;
2. restore affected files to the selected before-tree;
3. commit the existing native conversation undo;
4. if conversation persistence fails, restore the recovery workspace tree;
5. emit exactly one success or failure terminal.

Conversation-only rewind uses the existing native undo path. Code-only rewind
does not truncate conversation history.

## TUI interaction

While an agent is running, Esc only cancels. Cancellation clears and suppresses
the idle Rewind gesture so repeated Esc key events cannot spill into a rewind.

While idle:

1. first bare Esc shows the existing hint;
2. second bare Esc opens `Rewind`;
3. `(current)` is selected initially;
4. Up/Down selects a prior prompt;
5. Enter on `(current)` closes as a no-op;
6. Enter on a prompt opens the scope step;
7. scope defaults to “conversation only”;
8. Enter submits; Esc returns or cancels.

Targets display prompt previews and per-file `+N/-N` summaries. If workspace
checkpointing is unavailable, code scopes are disabled with an explicit reason.

## Failure semantics

All failures are visible. Unsupported worktrees, capture failures, stale
generations, busy runtime, revision changes, file conflicts, partial filesystem
restore, and persistence rollback failures must not be reported as success.
Pending approval/request state remains fail-closed because rewind is idle-only.

## Verification

Tests cover picker default/current behavior, Esc cancellation isolation, target
selection, snapshot capture/diff/restore, ignored and untracked files, conflict
detection, combined rollback compensation, stale generation/revision rejection,
session resume, session deletion, and TUI transcript repaint.
