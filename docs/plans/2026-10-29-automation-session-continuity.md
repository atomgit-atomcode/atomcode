# One conversation per automation task

Goal: each task resumes its most recently bound native conversation; TUI and
WebUI show the conversation with an automation marker rather than hiding it.

Baseline: release/v5.2.6, bfcfee411e75a76e41c385f5a390a988d66eca10, clean.
Owner: the schedule run lease serializes executions; existing Session run facts
link a task to its conversation. CodingRuntime alone owns live conversation state.
Native session events remain the sole source of conversation content.

1. Add a bounded-record backwards reader for the latest committed Session fact
   and its matching Started cwd. Failed runs without a session do not lose binding.
   Reuse old tasks' most recent session; do not merge or delete earlier sessions.
2. The CLI validates the saved cwd, resumes that exact id without busy-session
   forking, records the same id each run, and requires Scheduled metadata to save.
   Missing, busy, incompatible or corrupted sessions fail the run explicitly;
   metadata failure must not permit model/tool work. Unattended policy stays intact.
3. Surface Scheduled as a typed display flag in host/API listings and render a
   localized marker in the TUI resume panel and WebUI sidebar. Ordinary history
   readers also include scheduled conversations; delegated work stays excluded.
4. Test repeated runs, task independence, interruption, failed starts, corrupted
   binding/path traversal, cwd changes, marker projection and native resume.
   Run affected crate nextest suites, WebUI tests/typecheck/build, compile gate,
   formatting and lock checks. Real OS schedulers and devices are not exercised.

Changing a bound task's cwd must not silently create a replacement conversation.
If the bound session was deleted, report the missing session and require an
explicit new task. Existing task deletion semantics and historical logs stay intact.

Verification evidence (2026-10-29): 44 schedule tests, 6 CLI scheduler integration
cases, 267 capabilities session tests, and 2247 TUI/daemon/host-api/tree-host
cases passed. WebUI typecheck/build and 344 tests passed. A Chromium fixture
verified the sidebar marker in five loading states. The offline Unix CLI probe
`python3 evals/automation/session_continuity_probe.py --binary target/debug/atomcode`
verified two runs share one log and prior provider context, and that busy,
changed-project and missing-session cases make no model request or fresh fork.
The probe uses only temporary directories and loopback, never a real OS task.
