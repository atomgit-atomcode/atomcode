> Historical: the built-in `/computer` integration described here has been removed. Current browser setup and automatic use are tracked in `docs/plans/2026-11-12-browser-default-and-computer-retirement.md`.

# Computer Use cancellation boundary

Baseline: release/v5.2.7 committed 97910188c; work runs in the isolated /tmp/atomcode-browser-delivery worktree. The previous goal turn delivered browser setup and real-model acceptance; this turn addresses the remaining lifecycle boundary, not a second agent owner.

## Decision

Runtime/harness owns turn cancellation and already supplies the original token in ToolContext (harness exec.rs) and rejects not-yet-started batch calls. MCP ToolAdapter currently ignores that token. StdioClient serializes RPCs and handles normal unknown-result failures, but dropping an executing future can abandon a partially consumed response stream without entering recovery. A later tool can otherwise use a connection whose previous mutation is still running.

Use the existing boundaries: adapter races execution against its supplied cancellation token. Pre-cancelled execution performs no RPC. A dropped dispatched stdio tool future synchronously marks its client uncertain before the operation lock releases. Calls and discovery on that client then fail closed with a reconnect/reobserve diagnostic; explicit existing MCP reconnect replaces the client and tears down its owned process. Guard does not arm while waiting for the serialization lock or before a write can start. It disarms on a normal terminal result, including the existing managed unknown-result/recovery path. No notification claims that remote GUI effects were undone.

Alternatives: merely returning early from the adapter leaves an unknown stream usable (rejected); automatically closing browser tools from a new turn-end hook risks acting under cancelled authorization or closing another agent's context (rejected); synchronously blocking cancel until the remote call finishes defeats cancellation (rejected). The explicit reconnect barrier trades convenience for honest external-state reconciliation. HTTP cancellation also marks the client uncertain until explicit reconnect. Concurrent normal success and late responses cannot clear that marker. Best-effort HTTP session deletion has a two-second budget and skips user-pinned session IDs; it does not claim process teardown for remote servers.

## Ownership and failure semantics

No kernel or driver protocol changes. StdioClient is the existing transport owner; its uncertain marker is connection state, shared only with its own recovery clones. Manual reconnect creates a new client. Old replies cannot unpoison a cancelled client, and no interrupted call is replayed. Existing normal transport reconnect behavior remains unchanged. Browser/desktop policy, concurrency leases and origin enforcement remain separate follow-ups; this change does not pretend to implement them.

## Verification

Add pre-cancel and active-cancel adapter tests, a dispatched-future-drop stdio test with an external side-effect counter, a queued-future-drop case that must not poison an unrelated active call, and manual reconnect that restores subsequent explicitly requested operations without replay. Preserve existing malformed-frame, timeout, EOF and unknown-result tests. Use a paused current-thread clock for in-memory cancellation tests; external subprocess acceptance must account for real OS I/O and must not mistake a virtual-clock timeout for a server failure. Run relevant MCP tests, affected coding cancellation criteria, compile/format/lock gates before delivery.

## Results

Capabilities library and MCP tests: 1475 passed, two skipped. Runtime MCP criteria: 14 passed. The final parallel-HTTP late-response regression also passed independently. Decision and remaining boundaries are recorded in ADR 0031. Real installed-browser acceptance: both the ordinary form/screenshot flow and the successful-click/lost-response no-replay flow passed. The full compile gate passed, including feature-gated tests and both negative canaries. Format and diff checks passed. Cargo.lock has no hkdf, hmac or zeroize_derive packages and its index flag is H. No new dependency was added.
