# Host wiring for negotiated desktop turn scopes

Baseline: release/v5.2.7 99fd18f2e, clean isolated worktree. The backend protocol prototype is separately verified; App/socket isolation and real GUI acceptance remain incomplete.

Use the existing agent-scoped harness session log to derive a bounded opaque scope from the real session and TurnStart event sequence. Tool arguments cannot select it. Adapters resolve the scope at execution, not publication; child agents therefore cannot inherit the lead's scope. Register addressed release on the existing awaited TurnFinishing seam. Keep CodingRuntime as owner and its existing shutdown barrier as the final fallback. No kernel fields, second runtime, persisted scope state or overlapping completion hook.

Only negotiated stdio desktop-lease protocol v1 uses scope controls. Legacy servers and ordinary HTTP MCP retain their current behavior. Validate the exact acquire/release acknowledgement; a JSON-RPC result containing isError is not success. Serialize acquire+tool dispatch against end-of-scope. Known busy failures do not poison a connection or release another owner. Dropping a dispatched acquire or release must fence the stream just like a dropped tool call; never replay an unknown resource mutation. Cancelled-before-dispatch work must not poison its neighbor.

Track attempted/current scope only as transient transport state. A successful replacement handshake clears it. Scope cleanup has a short budget; uncertainty retires only this client's connection and preserves the explicit reconnect/reobserve boundary. The backend connection retains its lease until its accepted request processing ends, even when its proxy disappears. Do not broadcast turn-ended or terminate a shared App.

Verification: scoped/unscoped compatibility, same-connection agent exclusion, wrong/late scope cleanup, strict acknowledgements and protocol versions, cancellation during acquire, normal runtime end/cancel, child identity, and real native protocol calls with no GUI action. Existing MCP/runtime, image and no-replay tests plus compile/format/Clippy/lock gates remain required. No GUI exclusivity or other-platform acceptance is claimed by this unit.

## Implemented behavior and review

- Added bounded opaque `McpScope` metadata and optional scope provider on the existing MCP adapter. Generic harness and coding replacement rows both supply execution-time context and register the existing awaited TurnFinishing listener.
- The authoritative session log supplies TurnStart sequence; no persisted scope state, kernel command, driver owner or new lifecycle hook was added.
- Negotiated stdio acquire/tool dispatch and addressed release serialize through the existing operation lock. A child's refused/queued scope cannot release or wait behind the lead's ongoing work; late scope cleanup cannot release a newer owner.
- Strict acknowledgements distinguish known busy from unknown ownership. Cancelled dispatched controls fence the stream. Failed scoped transport operations never replay across connections; explicit reconnect/re-observation is required. A negotiated capability disappearing during recovery fails closed.
- Review found and fixed two additional ordering gaps: end-of-scope now waits for an already accepted acquire rather than forgetting it, and registry cleanup checks captured client identity before publishing failure so a replacement cannot inherit an old error.
- No new Rust dependencies, bundled native executable, browser or default activation. HTTP and nonparticipating servers keep their original contract. The backend prototype is still not safe to activate against the default installed App socket.

## Evidence recorded before final gates

Focused tests passed for actual CodingRuntime two-turn release-before-terminal and dispatched cancel/disposal-before-terminal; a real delegated child resolves a different scope and returning restores the lead. A native no-GUI test with the separately reproduced patched OpenComputerUse binary passed two-production-registry exclusion and addressed handover. Release timeout uses a paused Tokio clock and opens the external fixture gate only after retirement is observed, avoiding an exact-deadline/late-response race in the test itself.

Final regression/gate results are recorded below after completion. Remaining product delivery work: isolated App/socket packaging and activation, actual GUI in-flight cancellation/fresh observations, application/origin enforcement at the execution boundary, activity grouping, other-platform real acceptance and final overall size/audit.

## Final verification

- Complete capabilities+mcp: 1543 passed, 3 skipped, with `--test-threads 2` (68.121s). An earlier unconstrained run during concurrent builds hit four 10-second fake-Codex subprocess timeouts; those adapters/tests were not changed. All six fake-Codex run tests subsequently passed together at normal test concurrency (1.199s); the transient full-run timeout remains recorded rather than hidden. The earlier complete pre-final scope test run passed 1541 tests. The bounded final run includes all 35 nonignored MCP integration tests, including the two final ordering/downgrade cases.
- Complete harness: 345 passed, no skips. Includes actual delegated child scope resolution and restoration of the lead.
- Complete coding: 1329 passed, 11 skipped (75.054s). Includes normal two-turn release-before-terminal, dispatched cancel cleanup, ordinary MCP pause/reload/reconnect/resume, approvals, persistence and differential cases.
- Five unrelated approval fixture failures reproduced at clean baseline 99fd18f2e in a separate worktree/target. Their `target/` directories were still beneath `/tmp`, inside the deliberate temp-write approval exception. Commit 836d9369c gives these tests validated non-temporary, independently owned scratch directories; production approval rules and frozen golden files are unchanged.
- The existing shared-deadline MCP test also exposed wall-clock/write scheduling dependence. It now holds external reads/responses behind file gates, advances a paused Tokio clock across the write and response phases, asserts actual dispatch and verifies the original deadline/no replay without waiting for background reconnect. No production deadline policy changed for that test fix.
- Opt-in native patched backend with two production registries: passed, no GUI actions. Executable: `/tmp/atomcode-ocu-lease-delivery-build/arm64-apple-macosx/debug/OpenComputerUse`; private lease directory and direct mode avoid the installed App socket.
- Four real browser regressions passed: local form + inline screenshot, lost click response/no replay, owned process disposal on registry Drop, awaited shutdown and stale adapter fencing.
- Production capabilities Clippy passed (existing warnings remain). Final `gates/compile.sh` passed default/all-targets, feature-gated compilation and both negative controls. Formatting check and diff check passed. Cargo.lock is unchanged, index flag H; hkdf/hmac/zeroize_derive counts are all zero. No new dependencies or bundled binary.

GUI in-flight cancellation and cross-platform acceptance remain unverified. The next concrete phase is `2026-11-11-isolated-desktop-app.md`; no installed App/default socket or OS permission was changed by this work.
