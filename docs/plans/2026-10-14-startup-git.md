# Startup Git optimization implementation plan

Goal: Reduce synchronous Git process launches and overlap independent read-only queries without changing session authority or rewind availability.

Baseline: release/v5.2.4 at 1a2dfce87; existing permission-settings edits are preserved. CodingRuntime remains the lifecycle owner. Event headers retain frozen session context; leases and persistence failures retain existing behavior.

1. Cache a combined worktree probe within SessionContextHook, scoped to one preparation. Reuse its validated root when constructing the workspace checkpoint. Do not cache across sessions or directory changes.
2. Run branch, log, and status queries concurrently using scoped threads, joining all before returning the frozen context. Keep command failure rendering unchanged.
3. Keep bare repository creation synchronous; append the three core settings to its generated config, preserving existing fields and avoiding three git config processes.
4. Test nested directories, linked worktrees, non-repositories, frozen resumed context, config values, and workspace restore. Run capabilities session tests, coding tests, fmt and gates/compile.sh. Review Cargo.lock and retain unrelated edits.

No delayed initialization, new lifecycle owner, persistence format changes, or optional-context timeout policy in this change.

Validation: capabilities session tests: 237 passed, 1 ignored; coding crate tests and doctests passed, including 98 runtime criteria. Compile gate (workspace, feature coverage, and negative controls), fmt check and diff check passed. nextest is unavailable in this environment, so tests used Cargo's test runner. A subprocess-only Git wrapper added 700 ms per invocation: the nested-worktree context + checkpoint test issued five preparation commands and completed in 3.05 seconds, including filesystem overhead; no Windows latency claim is inferred from this simulation. The tracked lockfile remains skipped locally and its committed version contains none of the private overlay dependency names.

Mac TUI smoke test: the rebuilt debug executable launched in ppt-master with the same 700 ms Git-delay wrapper and rendered its banner after 3.74 seconds. The preceding run recorded prepare=2655 ms, runtime_start=2881 ms, tui_enter=3696 ms. This is a controlled simulation, not a measurement on the affected Windows machine.

Audit: validation is repeated in an isolated worktree containing only these changes, excluding the pending permission settings work. Differential approval fixtures require that worktree to reside outside the OS temporary directory, because writes under the temporary directory are intentionally auto-approved. No golden files are changed.
