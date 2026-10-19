# Bounded stream continuation

Baseline: release/v5.2.4 0faa4f508. Other runtime/controller/relay changes are in progress and are preserved.

Goal: recover transient mid-stream transport failures with saved progress up to three times, explain final refusal, and keep cancellation and tool execution safe.

Owner: the shipping harness `llm-stream-recovery` row, with per-session/per-turn counters and tool receipts in its existing Progress service. Kernel Agent has an independent conservative recovery policy; it is not the shipping coding assembly and is unchanged. No persistence or public event schema changes.

Implementation:
1. Change harness default and bundle budget to three. Loop through the existing waterfall instead of only issuing one continuation. Preserve each partial once and re-project from the authoritative session log.
2. Use the existing turn counter for numbered notices and capped exponential delays (1/2/4s for defaults). The agent loop races the entire request against cancellation, so dropping a backoff prevents another provider request.
3. Localize notices and terminal explanations for exhausted/disabled budget, incomplete tool output, and absent safe progress. Keep original error details and codes. Do not retry HTTP errors or unknown failures.
4. Test repeated recovery, shared timeout budget, exhaustion, progress retention, cancellation during backoff, and confirmed-tool deduplication using paused Tokio time. Run relevant crate tests, formatting and compile gate.

No automatic model/credential switching or unbounded retry. Runtime lifecycle and session ownership remain unchanged.

Validation: 42 recovery criteria passed; affected crate run 1369 passed, 10 failed, 9 skipped. The same 10 approval/compaction/tool-mount failures reproduced on unchanged baseline 0faa4f508 (1364 passed, 10 failed, 9 skipped). No new failing tests. Format check, diff check and the full compile gate passed. No live gateway fault injection was performed.
