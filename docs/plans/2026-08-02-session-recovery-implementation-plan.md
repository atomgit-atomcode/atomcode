# Session Persistence Recovery Implementation Plan

## Step 1: Add a lease-protected repair seam

- Add native session inspection and missing-presentation repair types to
  `atomcode-capabilities::session`.
- Validate native metadata and snapshot before mutation.
- Create only an absent presentation sidecar, atomically, under the active lease.
- Reload the strict native aggregate after repair.
- Test healthy, repairable, corrupt, missing-snapshot, and busy-session cases.

## Step 2: Expose an explicit daemon endpoint

- Add `POST /projects/:hash/sessions/:id/repair`.
- Default to dry-run; require `apply: true` for mutation.
- Preserve structured storage errors and session-in-use conflicts.
- Test response status and that inspection performs no writes.

## Step 3: Surface transcript persistence failures

- Extend the shared persistence status with a bounded auxiliary warning.
- Pass it to `TranscriptHook` during runtime assembly.
- Report JSONL append failures and project them as `ControllerWarning` at the
  authoritative turn terminal.
- Keep snapshot uncertainty fail-closed and transcript failure non-authoritative.

## Step 4: Verify and document operational recovery

- Run affected capabilities, coding, and daemon tests.
- Run cross-crate CLI/daemon compilation.
- Confirm current readers remain strict and no JSONL-to-snapshot fallback exists.
- Recover an interrupted legacy code-Rewind transaction from its existing store
  before allowing operators to remove historical objects; never retain that
  checkpoint for ordinary turn capture.
- Document that historical Rewind objects require explicit operator cleanup and
  that synthesized JSONL is intentionally outside the v5.0.4 repair path.
