# Goal overnight resilience implementation plan

Baseline: release/v5.2.4, 45d23493a, with the approved phase-1 goal changes still uncommitted. Work in /tmp/atomcode-goal-overnight; preserve the initial bytes when integrating this increment.

Owner: CodingRuntime owns goal policy, holds, retry budgets and cancellable timers. Provider timing is neutral error metadata; kernel and harness must preserve it. No new lifecycle owner or persistence format.

1. Preserve provider retryability/status/code/Retry-After through the production harness's existing TurnFinishing seam and live handle projection. Add a defaulted optional retry_after_secs to neutral AgentEvent::Error, updating producers and consumers. Replay of old facts has no recovery classification.
2. Permit exhausted 429 recovery only when explicitly retryable with authoritative Retry-After and no quota/auth/policy code. Apply the same rule to main and evaluator requests. Never shorten a server delay to fit a budget.
3. Add optional [goal.recovery.long_wait], default disabled: 32 additional main attempts at 900-second intervals, for at most 8 additional hours. One existing RetryBudget owns short and long stages, caps and cancellation. Evaluator keeps its separate short budget; global request timeouts and goal round/time caps remain unchanged.
4. Inject repeated failures, long server delays, truncated streams, stale work and cancellation with virtual clocks. Test the actual harness-to-handle boundary, not only hand-made runtime events. Keep existing tool and partial-output persistence paths.
5. Run scoped suites, the compile gate and fmt/diff/lock checks. Integrate without committing or overwriting unrelated work. Process-restart recovery remains out of scope.

Production rate limiting can end cleanly as RateLimited, with no Error fact. TurnComplete therefore also carries defaulted optional neutral ProviderFailure metadata. The existing TurnFinishing seam supplies it; no stored event changes. Explicit host policy pauses and partial-output pauses are classified non-retryable at the rate-limit owner, so goal cannot override account policy. Tests cover both ordinary errors and clean pauses through actual handle assembly.

Verification in the isolated increment:
- 565 coding library/recovery tests passed; 8 feature-gated tests skipped.
- Full coding suite: 1043 run, 1034 passed, the same 9 baseline failures in approvals/compaction, 9 skipped.
- Kernel/harness/config: 928 run, 927 passed; the existing tool-mount source scanner fails on unchanged mcp_tool_search.rs:532. Running that scanner standalone against the untouched baseline reproduces it.
- Compile gate, fmt and diff checks passed. No Cargo.lock changes.
- All long waits use virtual clocks; no real overnight model request or Windows manual test was performed.

Integrated into release/v5.2.4 without committing. Concurrent DeepSeek and TUI generation work was preserved; the combined main workspace passed gates/compile.sh, cargo fmt --all -- --check and git diff --check. Cargo.lock is unchanged and has no private overlay entries.
