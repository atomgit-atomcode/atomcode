# Single MCP server reconnect

Baseline: release/v5.2.4, 9bbfdd1de. Work is isolated in feat/mcp-server-reconnect; existing user edits are preserved.

The existing CodingRuntime owns this idle-only operation. Extend McpAction across UI/host/runtime, without a new lifecycle owner. Re-read the chosen server through runtime storage, enforce disabled/project trust gates, withdraw only its exact tool aliases and instructions, replace only its registry client, list and publish its new tools through the existing publisher. On failure its old tools remain withdrawn and the error is shown. Other servers, session and approval switches remain unchanged. Refuse while initial catalog publication or a turn is in progress.

Tasks: registry scoped replacement; shared scoped tool publication; runtime action and guards; UI bilingual action and mapping; transport/runtime/menu tests; relevant crate tests and compile/fmt gates. No release push requested.

## Validation and audit notes

Implemented Reconnect through the existing MCP action protocol and UI menu. No config toggles or credentials are written by reconnect. Tool removal uses exact alias ownership, and new publication reuses mcp-host. Config-derived auto-approval entries are tracked by declaring server so removal can revoke stale entries without revoking a neighbor's declaration. Discovery jobs are counted with a drop guard, including cancellation; a reconnect waits for an idle runtime and refuses while startup or remount discovery is active.

Tests completed: capabilities default features plus MCP, 1190 passed; final MCP transport suite, 12 passed, including scoped restart, failure/retry and scoped grant refresh. Four focused runtime/UI tests passed after correcting the UI test's panel setup, including disabled/project trust guards and failure detail refresh. Initial aggregate CLI/TUI/coding/host-api run: 3206 run, 3198 passed, 8 failed. Six failures were reproduced on unmodified baseline 9bbfdd1de with matching atomgit feature: two differential outside-workspace approval cases, permission_grants::always_allow_grants_survive_reassembly, product_identity::the_agent_says_it_is_the_product_that_started_it, runtime_criteria::an_always_allow_survives_an_undo, and tool_args_repair::approval_sees_the_repaired_arguments_that_execute. The newly added UI test setup was fixed; the unrelated timeout-display e2e passed when rerun. Historical goldens unchanged.

The no-default-features MCP-only test command cannot compile baseline code due to a missing tracing feature dependency; verification uses the supported default features plus MCP. Windows/Linux were not manually exercised; real stdio process tests ran on macOS. No release merge or remote push requested.


Final validation: 33 reconnect/menu/host/remount scenarios passed, including discovery cancellation, idle refusal and disabled/project trust boundaries; 12 transport tests passed. `CARGO_BUILD_JOBS=2 bash gates/compile.sh` passed using this worktree's independent target directory; fmt and diff checks passed. An earlier shared-target check loaded mismatched dependency metadata after a baseline build, and the first independent parallel build was terminated by the system. The bounded independent run is the delivery compile baseline. Cargo.lock unchanged and checked for private dependency contamination. Work remains on feat/mcp-server-reconnect, with no release merge or push.
