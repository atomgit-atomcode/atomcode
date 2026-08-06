# AtomCode Rewind Design

## Goal

Replace the unsafe “double Esc immediately runs `/undo`” gesture with an explicit
Rewind workflow that restores conversation state to a selected prompt boundary.
The picker opens on `(current)`, so an accidental double Esc plus Enter is a no-op.

> **v5.0.5 safety status:** Workspace/code restoration is disabled. The original
> per-session shadow-Git implementation had no disk quota or object collection and
> could exhaust the system disk. Rewind points now persist independently of Git
> trees, so conversation Rewind remains available without creating
> `~/.atomcode/rewind` objects. Code restoration may return only after a bounded,
> project-shared snapshot design is implemented and reviewed separately.
> The retained compatibility backend routes every Git child through Windows
> `CREATE_NO_WINDOW`; this is defense in depth and does not re-enable capture.

## Ownership and boundaries

`CodingRuntime` remains the only owner of the live coding lifecycle. Rewind is a
runtime operation, not a TUI-side combination of filesystem writes and `/undo`.
The TUI lists targets, selects a scope, submits one request, and waits for one
success or failure terminal.

Conversation checkpoint metadata belongs to `atomcode-capabilities::session`. It
uses the existing `SnapshotHook::turn_start` and `turn_complete` seams, so no
second per-turn state machine is introduced. The kernel remains neutral and
unchanged.

The following historical v1 workspace layout is retained only for compatibility
and cleanup; v5.0.5 does not initialize or write it:

```text
~/.atomcode/rewind/<project-hash>/
```

Existing code must not treat the presence of an old object store as evidence that
code restoration is available.

v5.0.5 intentionally does not delete an existing store automatically. On the
first affected-session load it uses an existing store only to finish compensation
for an interrupted v5.0.3 code-Rewind transaction, then drops the backend again.
Operators must preserve the store whenever AtomCode reports a pending-Rewind
recovery failure or any `*.rewind.txn.json` sidecar still exists under the native
sessions root. After those transaction sidecars are absent and AtomCode is
stopped, they may remove `$ATOMCODE_HOME/rewind` (or `~/.atomcode/rewind` when
`ATOMCODE_HOME` is unset). This removes only historical code checkpoints; native
conversation sessions are stored separately and remain available.

## Per-turn data

Each accepted user turn records a rewind point:

```text
prompt ordinal and preview
conversation revision/boundary
```

`turn_start` records prompt metadata without scanning the worktree.
`turn_complete`, including cancelled and failed turns, persists the conversation
point with absent workspace-tree fields. Older ledgers containing Git tree IDs
remain readable, but v5.0.5 does not offer code scopes against them.

## Rewind transaction

Rewind is accepted only while the runtime is idle. It validates generation,
session binding, conversation revision, and selected target, then uses the
existing native undo transaction. Code-only and combined requests fail before
mutation with an explicit `CodeRewindUnavailable` reason.

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

Targets display prompt previews as conversation checkpoints. Code-only and
combined scopes are disabled with an explicit disk-safety reason.

## Failure semantics

All failures are visible. Stale generations, busy runtime, revision changes and
persistence rollback failures must not be reported as success. No ordinary turn
may scan or write a workspace checkpoint while code Rewind is disabled.
Pending approval/request state remains fail-closed because rewind is idle-only.

## Verification

Tests cover picker default/current behavior, Esc cancellation isolation, target
selection, conversation-point creation without workspace trees, explicit code
scope rejection, stale generation/revision rejection, session resume, and TUI
transcript repaint. Historical workspace tests remain as compatibility coverage;
they do not imply that v5.0.5 enables the backend.
