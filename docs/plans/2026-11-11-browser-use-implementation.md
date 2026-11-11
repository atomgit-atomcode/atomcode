# Browser Use Implementation Plan

**Goal:** Deliver the external-browser MCP integration described in the approved design, with real controlled browser acceptance and explicit limits.

**Architecture:** CLI host owns setup; existing CodingRuntime and MCP clients own execution. No embedded browser or second runtime. Keep unrelated provider changes outside this worktree.

**Tech Stack:** Rust, existing MCP transport/approval, external Node and pinned @playwright/mcp 0.0.85.

1. Add a host-local browser setup service; verify exact backend version without launching a browser. Require an explicit installed server JS path on first enable; save resolved Node/server paths and isolated/no-WebMCP options. Test ownership, conflicts, no-write failures and bounded diagnostics.
2. Add addressed Browser host commands, terminal early dispatch, bilingual TUI commands and explicit reload. Test round trips, strict arguments, host failures and busy reload.
3. Add explicit browser-use guidance and bilingual setup docs. No automatic installation, trust grants, user-profile import or origin-enforcement claims.
4. Exercise the external server with a controlled local web app, temporary profile, screenshot and submission counter; exercise the actual AtomCode adapter/approval and unknown-result behavior. Record exact tools and browser/backend versions.
5. Run affected nextest suites, compile gate, format, diff/lock checks. Measure source bytes and comparable binary sizes. Review complete requirements against evidence before delivery.

Later design stages remain in scope for separate verified work: enforceable origin policy, cancellation/cleanup, concurrency and activity display. Windows/Linux native desktop support requires available backends and platform acceptance; do not relabel untested platforms as supported.

## Implementation evidence (in progress)

- CLI/TUI setup, addressed BrowserAction, explicit browser-use skill and bilingual docs implemented. First setup requires an installed server JS path; diagnostics check Node/backend versions only.
- Real backend 0.0.85 exposes arbitrary-code/upload tools by default. Added optional exact raw-name allowedTools policy at the existing registry, covering discovery, cached listings and direct/stale-adapter execution. Absent policy preserves existing MCP behavior; empty means deny all. No approval grants are added.
- Backend action responses can return only snapshot file links; guidance and acceptance use explicit browser_snapshot to obtain inline observations. Output files are directed outside the project, with owner-only artifact-directory access on Unix and a backend 16 MiB eviction target.
- Isolated tree: 406 CLI/lib/host/setup tests passed (one opt-in live test skipped), 1,972 host API/TUI tests passed, 1,464 capabilities/MCP tests passed (two opt-in tests skipped).
- Explicit live runs: local form navigate/fill/submit/screenshot passed through AtomCode adapter; fault-injected lost click response reports unknown result and preserves submission count one after reconnect; actual runtime approval denial prevented any HTTP request.
- Isolated compile gate including feature checks and negative controls passed. Final branch integration and final-state checks still pending.

- Rebased implementation commits onto committed provider-diagnostics baseline 8966af165 without importing the ongoing title-validation edits. Existing coding runtime MCP criteria: 13 passed.
- Review found desktop/browser setup used different locks for the same user document; both now share the managed MCP setup lock. A cross-service contention test and all related setup/probe tests passed: 22 tests. Non-cooperating editors still receive optimistic precommit checks, not an atomic filesystem CAS guarantee.
- Actual final-page PNG inspected: expected confirmation, marker and submission-count=1; evidence retained outside Git at /tmp/atomcode-browser-evidence. Browser Chrome 157.0.8103.39, Node v22.22.0; MCP package 0.0.85 reports Playwright 1.66.0-alpha-2026-11-09.

### Real-model idle failure and correction

The first real-model TUI trial waited for human approval longer than the backend's default 5-second heartbeat. Both navigation and re-observation returned unknown results; the model stopped without filling/submitting or claiming cleanup. Inspection of the pinned backend startHeartbeat implementation and AtomCode's request-driven stdio reader established the compatibility issue. Managed setup now sets PLAYWRIGHT_MCP_PING_TIMEOUT_MS=0 for this optional backend heartbeat, while keeping AtomCode's bounded RPC deadline and subprocess ownership. Live tests now wait seven real seconds before the first action; a paused Tokio clock cannot advance an external Node timer. A fresh real-model run is required before claiming autonomous acceptance.

Release-size measurement reused a temporary cloned cache and exposed stale workspace artifacts across worktrees (old host API/i18n types). All workspace package artifacts in this temporary release target were removed before the corrected build; no user target or tracked files were cleaned. Failed cached builds are not counted as release verification.

### Corrected real-model acceptance

Fresh isolated TUI session 708f80db was the failed-heartbeat diagnostic; the successful replacement session is recorded under /tmp/atomcode-browser-model-acceptance/user-heartbeat-fixed/sessions and summarized in real-model-evidence.json outside Git. The actual configured taotoken/deepseek-v4.1-flash-1017 model loaded browser-use, navigated, observed, typed the marker, reobserved, submitted once, observed the saved record, requested a screenshot and closed the browser. All four mutations used Allow once; no Always grant or auto mode. The native screenshot fact contains one image, independently inspected. The local server persisted exactly {count:1,name:ATOMCODE_MODEL_BROWSER_OK}. Turn ended normally and the TUI exited 0; the controlled web server was stopped. No user profile or credentials were copied into the test configuration (the API key was child-environment-only).

Post-fix verification: both live tests passed after seven seconds real idle time; final CLI/lib/host/setup suite 407 passed with the explicit opt-in live test skipped; the final compile gate passed all default/feature checks and negative controls. Release build/size verification and integration into the release branch remain pending.
