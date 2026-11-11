# Bounded owned MCP process cleanup

Baseline: release/v5.2.7 700f2d39a, clean isolated worktree.

StdioClient remains the subprocess owner. Current clear_transport and Drop immediately kill its child. The installed Playwright backend's tools/utils/mcp/server implementation registers a server-close listener that invokes backend.dispose. Allowing EOF to reach that listener gives the backend a chance to close its owned isolated browser; direct SIGKILL cannot run it.

Close this client's stdin, keep stdout draining, and give its child a two-second grace period. If it does not exit, kill and reap the direct owned child with a bounded fallback. Drop may schedule only owned-handle cleanup on the existing Tokio executor; outside an executor the existing kill_on_drop remains the fallback. Recovery cancellation retains its existing transport ownership. No browser business tool, arbitrary process-name kill, extra runtime owner or claim of universal descendant cleanup is introduced.

Immediate kill is simpler but prevents backend disposal. Unbounded graceful shutdown can hang reconnect or shutdown. Process-group killing is insufficient for independently detached Chromium processes and risks incorrect ownership if inferred from names; do not claim it solves that boundary. This change only improves cooperative cleanup and bounds uncooperative direct-child cleanup.

Verify a real child that records EOF before exit, an uncooperative child killed within the budget, Drop cleanup, existing reconnect and cancellation tests, and real isolated Playwright browser cleanup. Remaining escaped-descendant behavior must be explicitly documented if not proven.

## Acceptance evidence

The focused EOF and bounded-kill tests both passed. Real installed @playwright/mcp 0.0.85 with Chrome on this macOS host passed all three acceptance cases: normal form/screenshot, unknown click result without replay, and registry-drop disposal. The disposal test records the exact launched backend PID, captures its live descendant tree (including Chrome), releases all registry-owning adapters, and verifies every captured PID exits within five seconds. It never kills processes by name and does not emit unrelated process arguments.

This verifies cooperative cleanup for that backend and platform, not universal descendant containment. Hard process termination or executor shutdown can still fall back to direct-child kill_on_drop. HTTP remote sessions retain their independent best-effort deletion semantics.

Capabilities/MCP regression: 1477 passed, two skipped. Runtime MCP criteria: 14 passed. Full compile gate including feature variants and negative canaries passed. cargo fmt check and git diff check passed. Cargo.lock has no forbidden crypto overlay packages; no dependency was added. An awaited runtime shutdown barrier remains a follow-up: this bounded Drop task cannot promise completion after its executor is destroyed.
