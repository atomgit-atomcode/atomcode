# Awaited MCP shutdown barrier

Baseline: release/v5.2.7 6f8c7aebc. CodingRuntime remains the sole lifecycle owner. Existing finalization stops the agent and drops resources before publishing RuntimeExit, but client Drop only schedules asynchronous cleanup. Executor destruction can interrupt that cleanup.

Add a transport-owned async shutdown seam and await it from the existing runtime finalization before releasing resources/publishing exit. Registry shutdown fences dispatch and publication, cancels startup/reconnect work, waits for in-flight connection attempts to dispose unpublished candidates, and closes published clients in parallel with bounded cleanup. Repeated shutdown is harmless. No new agent, turn-end hook, protocol, or persistence authority is added. Drop remains an emergency fallback.

A dedicated closing token is required: cancel_pending_work is also used when withdrawing tools during reload, and must not silently acquire permanent shutdown semantics. A shared connection barrier covers both background startup and explicit add_server; publishing after shutdown is forbidden under the existing server-map lock. Stdio shutdown cancels its transport operations/recovery, then closes its owned handles with the established EOF/grace/kill budget. HTTP shuts down its owned server-issued session once, retaining pinned-session exclusion and bounded best effort.

Test actual runtime shutdown, concurrent initialization, stale calls after shutdown, idempotence and neighboring registry independence. Preserve session leases and existing runtime terminal semantics. Shutdown acknowledges that local cleanup was awaited; it cannot prove remote rollback or universal descendant containment.

## Verified behavior

The actual runtime EOF fixture passed: its disposal marker exists before runtime.handle.shutdown returns. Existing MCP runtime criteria also passed (15 total). Both background and explicit initializing candidates are cancelled, cleaned, and prevented from late publication. Shutdown rejects stale calls and tool listings. HTTP shutdown takes ownership of its session once under a shared shutdown lock; concurrent shutdown callers wait for the same bounded DELETE and Drop cannot repeat it. Custom client cleanup has an eight-second parallel budget; runtime reports a warning for published clients that exceed it instead of implying confirmed cleanup.

Real installed Playwright MCP/Chrome acceptance passed four cases: ordinary form/screenshot, result-lost click without replay, Drop disposal, and awaited registry shutdown. The awaited case verifies all exact captured backend/browser PIDs are already gone when shutdown returns, and a retained adapter cannot dispatch a stale click. This is macOS evidence, not a Windows/Linux or arbitrary escaped-descendant claim.

## Cancellation and replacement ownership

Initial readiness remains a prompt cancellation signal and is separate from resource disposal. An unpublished client carries an owned connection read lease; dropping its initialization future transfers both client and lease into bounded cleanup. Replaced/removed clients use the same leased disposal. Aborting a reconnect caller detaches its cleanup task rather than aborting it, so registry shutdown still waits for that task through the write barrier. This also covers failure before publication and avoids relying on a future being polled again to clean up. No lease is reconstructed from a call ID or process name.

## Regression results

Capabilities/MCP: 1481 passed, two skipped. Runtime MCP criteria: 15 passed. Real backend browser acceptance: four passed. The focused lifecycle set covers prompt startup cancellation, forced initialization drop, old transport disposal after aborted add/reconnect, parallel bounded client shutdown, and HTTP exactly-once session cleanup. No Cargo dependency or lockfile change is required.

An intermediate regression exposed that initial readiness had accidentally waited for cleanup. The final leased-drop implementation preserves prompt cancellation readiness while the owner shutdown barrier still waits for disposal. Tests also explicitly poll shutdown before asserting its fence, rather than mistaking an earlier cancel_pending_work notification for shutdown having started.

Final focused lifecycle rerun: five passed. Full compile gate passed on final source, including all-targets, feature variants and both negative canaries. Format and diff checks passed. Cargo.lock has zero hkdf/hmac/zeroize_derive package names and index flag H; no new dependency was added.
