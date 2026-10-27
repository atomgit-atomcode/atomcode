# AtomCode automation: current implementation and DSH comparison

Status: proposal for review, not an implementation or accepted ADR.
Baseline: AtomCode release/v5.2.5 c97c78656; local deepseek-harness 687f282e86.
Read-only code review; unrelated dirty worktree changes preserved.

## Current behavior and evidence

- `crates/atomcode-cli/src/tui_schedule.rs`: `/schedule` lists tasks only; its
  arguments are ignored. Add/remove/enable/disable/sync live in `schedule_cmd.rs`.
- `crates/atomcode-config/src/schedule.rs`: JSON task definitions include cwd,
  prompt, permission mode, enabled, last_run_at and last_status. There is no
  per-run ledger or explicit time zone. Writes replace the file directly;
  listing silently skips unreadable/corrupt entries.
- `schedule_os.rs`: launchd/systemd/Windows adapters register calls to
  `atomcode schedule run <id>`. Cron is accepted by the CLI schema but refused
  by these adapters. Registration failure can leave a saved enabled task that
  is not actually scheduled; show this separately from enabled/disabled.
- `schedule_cmd.rs::run_task`: uses the normal native runtime/headless path,
  always fresh session, marks origin Scheduled, caps Auto at AcceptEdits,
  rejects unattended approval escalation. Uses the default configuration,
  without explicit per-task provider/model pinning. No common per-task run
  lease appears in this executor; do not rely on every OS adapter to prevent
  overlaps or simultaneous manual invocations.
- The executor loads the whole definition before running and saves it again
  afterwards to update last status. An intervening edit can be overwritten;
  deletion can be undone by that final save. Some failures return before
  recording terminal status, and status saves are best effort.
- `schedule::next_run` computes daily time with timezone-free UTC arithmetic,
  treats weekly like daily, calculates intervals from the query time and
  returns None for cron. OS triggers have their own behavior. It is unsuitable
  as an authoritative next-run timestamp for a management page.
- No scheduler management routes found in daemon or corresponding frontend
  API/UI. Browser visibility and relay connectivity must not own scheduling.
- Schedule files are definitions; native session event logs remain the only
  conversation authority. Runtime ownership must remain CodingRuntime.

## What DSH actually provides

Read `docs/user/guide/schedule.zh.md`, `packages/schedule/schedule/src/{index,
runtime,storage,update}.ts`, `tool-schedule/src/index.ts`, and ui-schedule README.

DSH owns task storage at Host scope independently of loaded Sessions. Its
schedule_create/list/update/delete tools bind to the calling Session; delegated
agents cannot create/edit reminders. The UI has a global searchable catalog,
per-session entries, editable rule details and paginated saved delivery history.
Updates carry observed state to detect conflicts; shared change events refresh
readers and stale UI responses are discarded.

Its timer resolves/restores the original Session, enqueues a followup and flushes
its inbox before acknowledging delivery. Recurring targets advance; one-shots
become inactive and remain inspectable. Reboot catches overdue one-shots and the
latest missed recurring occurrence. Host shutdown stops dispatch. Saved history
means delivery acknowledgement, not successful model execution. A crash between
inbox persistence and task acknowledgement can duplicate delivery. DSH does not
promise exactly-once execution, pause, or a new Session per occurrence.

## Proposed product direction

Reuse `/schedule` and the label "自动化任务" instead of introducing a competing
`/automation` subsystem. Offer natural-language creation plus explicit forms.
One task catalog feeds CLI, TUI, WebUI and model tools.

Two execution meanings must be explicit:

| Meaning | Execution | Lifetime |
| --- | --- | --- |
| Independent job (existing default) | Fresh native session per occurrence | OS trigger; does not require an open TUI/browser |
| Session reminder (later) | Persistently queue followup to original runtime/session | Requires a single elected resident host |

Recommend shipping independent jobs first. Keep existing definitions and OS
registrations compatible; do not secretly switch all jobs to a host-only timer.
An OS trigger is not a guarantee that a powered-off/unavailable machine runs;
platform registration and catch-up behavior must be reported honestly. Never
let OS and resident timers both own the same task occurrence.

## Shared boundaries

Scheduling management is a local host service, not a kernel command or browser
timer. CLI/TUI/daemon need shared application operations (validate, create, edit,
enable, disable, delete, reconcile registration, list and inspect runs). Existing
CLI handlers and OS adapters are the extraction source, not a daemon dependency
on the CLI. A focused schedule module/service, or a dedicated leaf crate if
needed by both binaries, is justified by these concrete consumers; avoid a
foundation crate. Pass ProductDirs from hosts.

Model tools are thin capabilities consuming a host-injected service, preventing
capabilities from depending on CLI/daemon/L2. Describe tools in their own
schemas; describe frontend setup at its owning row. Creation/editing persistent
background work needs a product approval contract separate from execution-time
permissions; do not equate the model asking with a user granting future Auto.
Do not persist API credentials in task definitions or printable row configs.

Actual runs continue through CodingRuntime/headless. A future session-reminder
adapter must use runtime admission/queue/lease semantics, with explicit busy,
approval, archived/deleted session, cancel and shutdown outcomes. Never restore
a second live owner or directly rewrite a conversation file to inject reminders.
No assumption that existing Submit provides a durable inbox; first verify its
acceptance, crash recovery and terminal semantics before adding any queue seam.

## Delivery plan

1. **Correctness and service boundary**: separate definitions from append-only
   run records (run id, occurrence, task revision, instruction snapshot, start,
   end, status, session id, error). Atomic updates and revisions; per-task lease
   and explicit skip/queue overlap policy. Completion never recreates a deleted
   definition. Record failures from configuration/provider startup as well as
   turn completion. Preserve unattended fail-closed execution. Distinguish
   saved/enabled/OS-registered/running, and registration error from success.
2. **Time semantics**: explicit zone, rule validation, weekday alignment and
   interval anchor; adapter-supported features only. Prefer OS-reported next
   trigger when available; otherwise clearly mark estimates/unsupported. Never
   present approximate UTC arithmetic as exact. Define DST and catch-up rules;
   do not silently reinterpret old tasks' wall-clock times during migration.
   Add once-at/after only when all supported adapters have declared behavior.
3. **Shared management surfaces**: authenticated daemon CRUD/run/history
   endpoints and change notifications; TUI `/schedule` task picker/details;
   WebUI automation list and mobile detail page. Same schema/validation and
   revision conflicts everywhere. Scoped project/path permission checks,
   idempotency for create/run across relay retries; deletion stops future
   triggers without claiming the current run was cancelled.
4. **Natural language**: schedule_create/list/update/delete tools over that
   service, with explicit time/zone context, lead-only creation by default and
   a reviewable rule/permission summary. No prompt-parsing heuristics in TUI.
5. **Optional original-session reminders**: only after durable admission,
   single-host ownership and missed-occurrence semantics are specified and
   verified. Keep delivery acknowledgement separate from run success. A
   running task gets a correlated cancellation operation, not an unrelated
   foreground Cancel.

## UI outline

TUI `/schedule`: title, rule/timezone, next trigger, enabled/registration status,
last run; Enter details, keyboard actions create/edit/pause/run/history/delete.
Detail shows cwd, permissions and configuration source, with a session link for
each run. WebUI sidebar adds "自动化任务", searchable list plus rule/run tabs;
mobile opens a full-width detail page. Start with direct forms (usable without
an available model); natural-language entry pre-fills a request in chat.
Neither UI may treat request errors as an empty catalog or replay a write merely
because its confirmation response was lost.

## Acceptance and limits

Test CLI/TUI/WebUI reading one catalog; definitions survive restart; edits and
deletes during runs are preserved; multiple frontend/trigger instances cannot
claim the same occurrence; denied/unavailable provider ends in visible failure;
registration errors remain repairable; hourly/weekly/timezone/DST/clock rollback
are deterministic. Relay reconnect refetches state without repeating creation
or runs. Session history links resolve real native event sessions. Time tests
use paused Tokio time where appropriate; OS adapters use injected command
runners. Real launchd/systemd/Windows registration requires platform validation.

This review changes no production code or persisted format. No tests run for
this proposal; implementation must follow the compile/format and relevant
nextest gates. Initial recommendation is phases 1–3, then model tools; a new
host timer is not needed for the first usable release.
