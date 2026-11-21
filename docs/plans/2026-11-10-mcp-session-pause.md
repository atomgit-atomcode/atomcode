# MCP session pause implementation plan

**Goal:** Pause/resume all MCP servers for the current runtime session without editing per-server configuration.

**Architecture:** CodingRuntime remains the lifecycle owner. Add an ephemeral preparation overlay and a generation-fenced reprepare target; reuse the existing idle-only transactional rebuild. No persistent session schema or kernel change. Same-session reload/model changes preserve the overlay; fresh/resume/cd reset it. Failure returns an error, never success. Pause/resume refuses active turns and active child/background work through existing rebuild guards. A paused candidate creates no MCP connections. Per-server enabling/reconnect is refused while paused.

**Tech stack:** Rust runtime/host/TUI, existing daemon HTTP API, TypeScript WebUI.

1. Add `PrepareOptions::mcp_paused`, runtime command through existing MCP action and reprepare seams; project paused state in MCP rows. Verify configuration is never modified and paused preparation has no tools/connections.
2. Add TUI top-row action with keyboard and mouse activation, bilingual labels and current-session scope.
3. Add session-addressed WebUI read/write endpoint through LiveViewHub; stale session/generation requests must not mutate a replacement session. Put the button atop the MCP menu.
4. Test pause/resume, busy refusal, same-session reload continuity, fresh reset, per-server disabled preservation and UI interaction. Run affected crate nextest suites, WebUI checks, compile gate and fmt. Preserve unrelated CLI edits.

Baseline: release/v5.2.7 a64d91b1e. Dirty CLI headless_json/main changes are unrelated and preserved. Recent MCP history and lifecycle seams inspected before changes.

## Verification

- CodingRuntime: 1308 passed, 16 skipped. Five known retry-timing baseline failures excluded (previously reproduced on clean baseline); remaining skips are existing. New real-subprocess criterion verifies process exit after pause, no MCP tools while paused, same-session reload continuity, failure rollback, resume and fresh-session reset, and independent disabled server preservation.
- TUI/host-api/i18n/daemon: 2450 passed. Includes session-fenced daemon calls and independent mouse hit target for the top control.
- Capabilities: 1264 passed, 1 existing skip. Disabled driver-supplied servers now follow the same filter as file-configured servers at the common registry load boundary.
- CLI MCP host/port tests: 24 passed.
- WebUI typecheck, unit tests, build and mocked-browser pause/resume/busy/stale-session interaction verified. No real session/MCP server used by browser fixture.
- Compile gate passed; fmt and diff checks passed. Current-session overlay is intentionally not persisted across explicit session reopening or process restarts. Refuses existing delegated agents (including idle members) and pending/uncollected background jobs instead of cancelling them.

Final review: guarded late WebUI mutation replies by request revision as well as session id; after menu/session context changes, re-read authoritative state instead of applying an old reply. Browser regression includes switching away and back before a held response arrives. No review-latency changes included.
