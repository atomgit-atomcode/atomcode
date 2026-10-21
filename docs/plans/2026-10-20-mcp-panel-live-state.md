# MCP panel live state implementation plan

**Goal:** Restore a server disabled in the current runtime without rebuilding unrelated connections, and show live, accurate connection state in the MCP panel.

**Architecture:** Registry owns actual in-flight connection attempts (drop guards clear cancellation paths). Runtime owns reusable disable records, scoped to its generation, registry and unchanged server configuration. It returns whether an action needs capability reload. TUI owns bounded, serialized reads scoped to the open panel/session; it never infers runtime success or runs transport work.

**Tech Stack:** Rust, tokio, native MCP registry, CodingRuntime control channel, TUI injected clock, nextest.

Baseline: `release/v5.2.4` at `7b20c1a36`, rebased onto `8d5fbf803` after concurrent MCP commits landed. Main checkout has unrelated recovery/TUI changes; implementation uses an isolated worktree.

1. Add explicit pending-attempt accounting to `capabilities/src/mcp/registry.rs`. Configured servers without a client or actual attempt report existing `Disconnected`. Cover pending, completion, cancellation and absent-client states.
2. Give `runtime.rs` a typed internal action result and generation/config-scoped disable records. Re-enable in place only for a valid still-published connection; preserve manual tool switches and reproject model-visible MCP instructions. Otherwise keep existing idle-only reload. Empty-registry skills-only reload must also check newly enabled config before skipping preparation. Cover neighbor connections, initial-disabled servers, stale generations/config and trust withdrawal.
3. Add panel/session/request guards and one-in-flight refresh ownership to TUI. Refresh on detail-to-list navigation; poll unresolved rows at one-second intervals, stopping when settled/closed or superseded by an action. Preserve selection, detail and notes; ignore late results from closed/reopened/switched views.
4. Run targeted registry and runtime integration criteria and full TUI tests. Run affected capability tests with MCP enabled, formatting and `gates/compile.sh`. Integrate only this patch into the main checkout, preserving unrelated changes. No protocol enum addition, no dependency or persistence change.

Validation: 1,747 TUI tests, 1,165 capability unit tests with MCP enabled, 35 MCP coding criteria, 32 final MCP panel regressions (including obsolete-scope slot release), and 8 final hosted-storage scenarios (including manual switch exceptions) passed. Formatting is clean in the integrated main checkout. The newer connection identity and scope checks replace the earlier mounted-tools-only decision and wall-clock polling path; the deleted boolean helper test is superseded by runtime integration scenarios.

Final integration: `bash gates/compile.sh` passed both in isolation and in the main checkout, including feature-gated targets and negative controls; temporary canaries were removed. `cargo fmt --all -- --check` and `git diff --check` passed. No Windows terminal/manual server test was performed.
