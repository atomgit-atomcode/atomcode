# Session Persistence Recovery Design

## Goal

Make disk-exhaustion failures visible and provide an explicit, conservative way
to restore a native session whose committed snapshot is intact but whose
presentation sidecar is missing. This complements the v5.0.4 safety stop for
workspace/code Rewind; it does not re-enable workspace snapshots.

## Ownership and recovery boundary

The native aggregate remains `SessionMeta + SessionSnapshot + PresentationFile`.
`SessionSnapshot` is the runtime conversation authority, `PresentationFile`
contains display-only entries, and the append-only JSONL transcript remains a
recall artifact. JSONL must never overwrite or replace a native snapshot.

Normal readers stay strict. They must not manufacture an empty presentation or
silently fall back to JSONL. Recovery is an explicit daemon operation that
acquires the exact session lease and succeeds only when all of the following are
true:

- metadata exists, validates, and has `owner = native`;
- the canonical snapshot exists and validates;
- the presentation path is absent, rather than corrupt, oversized, unsafe, or
  an unsupported future version;
- no runtime currently owns the session lease.

The repair writes a versioned empty `PresentationFile` atomically, then reloads
the strict aggregate before reporting success. Existing valid or invalid bytes
are never overwritten. A valid presentation is reported as already healthy.

## Daemon API

Add an authenticated endpoint:

```text
POST /projects/:hash/sessions/:id/repair
```

The request defaults to inspection. Mutation requires an explicit `apply: true`.
The response reports the observed metadata, snapshot, presentation and transcript
states plus one of `healthy`, `repairable_missing_presentation`, `repaired`, or
`not_repairable`. Busy sessions return the existing session-in-use conflict.

The endpoint is the driver boundary: filesystem mutation stays in
`SessionManager`, while HTTP validation and response projection stay in daemon.

## Persistence failure visibility

`TranscriptHook` currently discards append errors. It will share the runtime's
persistence status channel and report an auxiliary persistence warning containing
the session operation and concrete storage error. On the turn terminal,
`CodingRuntime` emits the existing `ControllerWarning` event. Snapshot aggregate
failures retain their current stronger fail-closed semantics; a JSONL failure
does not invalidate a successfully committed native aggregate.

Warnings are one-shot and bounded. They are diagnostic, not model conversation
messages, and therefore do not change provider context or session state.

## JSONL recovery

v5.0.4 does not synthesize canonical JSONL from snapshot. A compacted snapshot
may omit old turns, and cannot reliably recover original timestamps, per-round
usage, or the exact raw reasoning/tool transcript. Generating apparently complete
records would create a second, misleading history owner.

An external support tool may export clearly labelled, lossy records from an
intact snapshot, but those records must use a separate filename and must not be
consumed as canonical recall data. The product repair endpoint reports transcript
absence so support can distinguish it from a display-sidecar failure.

## Verification

Tests cover dry-run inspection, successful missing-presentation repair, no-op on
a healthy aggregate, rejection of corrupt presentation, session-lease conflicts,
strict reload after repair, transcript append warning propagation, and unchanged
snapshot fail-close behavior.
