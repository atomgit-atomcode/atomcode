# Session browsing feedback

Approved scope: preserve read-only history navigation during a running task, improve the banner and sidebar discoverability, and keep live binding rejection unchanged. No stop-and-switch action.

Baseline: release/v5.2.6, aad4a90c8efda130f303afaf2cbc80934ad26c87; existing working-tree edits are preserved. Current App already separates browseSession from the bound session, SessionBrowse aborts stale fetches, and Chat uses dedicated session-switch notices.

Implementation: use a labeled, wrapping banner with separate session title and background-task explanation. While running show only Return to running session; after completion offer Continue this session. Add a sidebar hint and tooltips explaining read-only navigation. Keep pending approval/input attached to the running session.

Verification: TypeScript, frontend unit tests, production build, desktop/mobile browser regressions for rapid history selections, stale history requests, retained background stream and approval/input, and post-turn continuation.

Results: TypeScript check and 400 frontend unit tests passed; production build passed. Browser regression covers 1200px and 390px, stale delayed history, retained background output and pending interactions. Full compile gate, cargo fmt --all -- --check and git diff --check passed. No runtime binding or server rejection semantics changed.
