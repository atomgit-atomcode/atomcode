# WebUI live provider synchronization implementation plan

Goal: saved provider configuration is reconciled with the live runtime, and removed selections cannot silently serve subsequent messages.

Baseline: release/v5.2.4 at eac6caa71. Preserve unrelated dirty files, including capabilities, CLI, coding tests and TUI.

Architecture: the live hub and CodingRuntime remain the sole runtime owners. Config reload checks the existing binding against current selection and fingerprint, then uses the established provider reload operation. Busy reloads are reported as pending for the next idle operation; no background owner or second transition state machine is introduced. Deleted selections require explicit user choice. Rebuild failures are visible and do not pretend that settings were applied.

1. Add one backend reconciliation helper for config reload, exposing an additive status in its response and live command notices for peers.
2. Have all successful ModelProviders mutations call config reload and use its returned settings snapshot. Surface pending, invalid and failed application states.
3. Check the current selection and fingerprint on live message submission even when the caller omitted provider. Keep the existing busy-steer and runtime generation guards.
4. Test reconciliation decisions (missing, unchanged and changed selections), message selection validation, busy/error reporting and existing provider reload lifecycle tests. Typecheck/test the web UI; run daemon tests, compile and format gates.

Failure semantics: config persistence is distinct from runtime application. Never auto-map a deleted account to an unrelated provider; never cancel an active turn to apply settings. Existing explicit switches remain the recovery path.

Validation completed: daemon 351 tests passed (343 unit + 8 integration), WebUI 271 tests passed, TypeScript typecheck and production build passed, compile/feature gates and formatting/diff checks passed. New tests cover implicit selection fingerprint changes, deleted/recreated selection refusal, unchanged configuration, and returned live application status. Windows/browser interaction has not been manually exercised. The implementation intentionally applies busy changes on the next message/model switch, not automatically at turn terminal; no deferred lifecycle queue is claimed.

Commit audit: the selected six source/test files were applied alone to an isolated worktree at eac6caa71. All 351 daemon tests, 271 frontend tests, typecheck, production build, workspace/feature compile gates, formatting and diff checks passed. The patch was compared byte-for-byte with the main worktree before committing. Concurrent unrelated commits and working changes are preserved.
