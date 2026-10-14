# Single MCP server reconnect

Baseline: release/v5.2.4, 9bbfdd1de. Work is isolated in feat/mcp-server-reconnect; existing user edits are preserved.

The existing CodingRuntime owns this idle-only operation. Extend McpAction across UI/host/runtime, without a new lifecycle owner. Re-read the chosen server through runtime storage, enforce disabled/project trust gates, withdraw only its exact tool aliases and instructions, replace only its registry client, list and publish its new tools through the existing publisher. On failure its old tools remain withdrawn and the error is shown. Other servers, session and approval switches remain unchanged. Refuse while initial catalog publication or a turn is in progress.

Tasks: registry scoped replacement; shared scoped tool publication; runtime action and guards; UI bilingual action and mapping; transport/runtime/menu tests; relevant crate tests and compile/fmt gates. No release push requested.

## Validation and audit notes

Implemented Reconnect through the existing MCP action protocol and UI menu. No config toggles or credentials are written by reconnect. Tool removal uses exact alias ownership, and new publication reuses mcp-host. Config-derived auto-approval entries are tracked by declaring server so removal can revoke stale entries without revoking a neighbor's declaration. Discovery jobs are counted with a drop guard, including cancellation; a reconnect waits for an idle runtime and refuses while startup or remount discovery is active.

Tests completed: capabilities default features plus MCP, 1190 passed; final MCP transport suite, 12 passed, including scoped restart, failure/retry and scoped grant refresh. Four focused runtime/UI tests passed after correcting the UI test's panel setup, including disabled/project trust guards and failure detail refresh. Initial aggregate CLI/TUI/coding/host-api run: 3206 run, 3198 passed, 8 failed. Six failures were reproduced on unmodified baseline 9bbfdd1de with matching atomgit feature: two differential outside-workspace approval cases, permission_grants::always_allow_grants_survive_reassembly, product_identity::the_agent_says_it_is_the_product_that_started_it, runtime_criteria::an_always_allow_survives_an_undo, and tool_args_repair::approval_sees_the_repaired_arguments_that_execute. The newly added UI test setup was fixed; the unrelated timeout-display e2e passed when rerun. Historical goldens unchanged.

The no-default-features MCP-only test command cannot compile baseline code due to a missing tracing feature dependency; verification uses the supported default features plus MCP. Windows/Linux were not manually exercised; real stdio process tests ran on macOS. No release merge or remote push requested.


Final validation: 33 reconnect/menu/host/remount scenarios passed, including discovery cancellation, idle refusal and disabled/project trust boundaries; 12 transport tests passed. `CARGO_BUILD_JOBS=2 bash gates/compile.sh` passed using this worktree's independent target directory; fmt and diff checks passed. An earlier shared-target check loaded mismatched dependency metadata after a baseline build, and the first independent parallel build was terminated by the system. The bounded independent run is the delivery compile baseline. Cargo.lock unchanged and checked for private dependency contamination. Work remains on feat/mcp-server-reconnect, with no release merge or push.


## Audit repair: responsive cancellation

The owner formerly awaited reconnect I/O in its control arm. Reconnect is now an owned pending task with an exactly-once result, a captured generation and original phase. Read queries and correlated responses remain serviceable; new turns/reconfiguration are refused as Busy. Driver accepts/dispatch observe the reconfiguration phase and a private owner-managed MCP flag, so this operation remains cancellable without changing admission for other reconfiguration types. Cancel, shutdown, management replacement/stop, persistence failure and a competing active turn abort/join the task and withdraw only its server. Completion cannot restore a changed generation. The row still owns catalog publication; no second agent lifecycle or persistence format was added.

The MCP panel's Esc routes a captured session/server CancelReconnect action through HostControl, independently of turn cancellation. A drop guard clears the cancellation target even if the UI request is dropped.

New regression scenarios use actual stdio children that withhold initialize or tools/list. They assert queries, Busy refusal, cancellation/shutdown within one second, scoped cancellation, no neighbor restart, no old tools, retry, and safe late cancel. Real clocks are deliberate for OS subprocess readiness. Pure UI/route tests use paused clocks.

Validation so far: 3211 aggregate CLI/TUI/coding/host-api tests, 3205 passed and the same six baseline failures; 72 MCP/reconfiguration focused tests passed; four slow-server/runtime tests passed. Final stable source receives independent compile/fmt checks before commit. Cargo.lock unchanged.


Delivery checks for the audit repair: stable four slow-server/runtime scenarios passed (0.631s); 72 MCP/reconfiguration scenarios passed; the aggregate run remains 3205 passed / 6 independently confirmed baseline failures. Independent `CARGO_BUILD_JOBS=2 bash gates/compile.sh` passed, as did fmt and diff checks. The existing Reconfiguring phase is published during the job and the original phase is restored at its terminal; direct fire-and-forget driver submissions are refused while it is pending. No merge or push.

Final MCP transport regression: 12 passed. All audit repairs remain on feat/mcp-server-reconnect, without release merge or push.
