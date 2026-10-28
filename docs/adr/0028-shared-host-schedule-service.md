# 0028: Shared host schedule service

Status: accepted implementation, 2026-10-25.

CLI, TUI and daemon/WebUI need the same automation management operations. Keeping
OS registration in the CLI would force the daemon to depend on the CLI or copy
its behavior. `atomcode-schedule` therefore owns definitions, registration, run
metadata and request receipts. It depends on config and optionally implements a
capabilities-owned injected tool interface. Capabilities never depend back on
this host service; no kernel command or second runtime is introduced.

Hosts supply the schedules directory. OS jobs receive that directory explicitly
so their configuration, authentication and native sessions use the same product
home even when the scheduler does not inherit terminal environment variables.

Definitions retain the published JSON schema. Raw-byte revisions detect stale
edits, writes replace atomically under a cross-process catalog lock. Run facts
are separate, append-only and hold the admitted instruction/revision. Completion
never writes a definition. A process lease excludes overlapping runs. Native
session events remain the conversation authority; schedule events only link them.

Management operations serialize OS changes and persist correlated receipts.
Manual launches persist uncertainty before spawning; acknowledgement loss never
justifies silently executing again. Per-run admission rechecks revision/enabled
state. Crash recovery records interrupted runs after their lease becomes free.
This does not promise exactly-once execution across arbitrary disk/power failures.

System scheduling remains the trigger owner. Browser SSE/polling only refreshes
reads. All rules are explicitly execution-host local; next runs are estimates.
Unsupported once/cron/arbitrary-zone behavior is rejected rather than guessed.
Original-session reminders require a separately designed durable admission seam
and are deferred.

Model writes own mandatory per-change consent. The approval payload marks
`requires_explicit_approval`; CodingRuntime preserves this request for the driver
even in Auto mode. Generic risk approval must not create a duplicate prompt or
substitute a session grant for consent to persistent work. Unattended execution
has no such management capability by default. Coding plan policy explicitly blocks automation writes before their
internal consent; the read-only list remains available.

See [automation user guide](../automation.md) and the implementation plan for
validation and platform limits.
