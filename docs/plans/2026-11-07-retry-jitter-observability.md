# Bounded retry jitter and local observations

**Goal:** Desynchronize local retries and measure their outcomes without learning new policies or changing retry eligibility.

**Owner:** The existing `llm-retry` row in `loop_policy.rs` owns this change. Rate limits and context-overflow recovery retain their existing handlers; provider attempts and waits remain charged to the shared recovery budget. No runtime, session-log schema, telemetry upload schema or dependency changes.

1. Apply an 80–100% bounded jitter to local exponential backoff, including at its cap. Preserve server hints byte-for-byte after existing validation/flooring.
2. Emit structured local tracing records for scheduled retries and terminal retry-pass outcomes. Use a process-local id, turn/round, fixed error categories, reserved waits and monotonic elapsed time. Never add raw errors, prompts, URLs, models or credentials to these records.
3. An interrupted/dropped retry future reports interruption; a pass re-entered by outer recovery has its own id. This is not a whole logical-request success metric, nor an uploaded telemetry event.
4. Test jitter bounds, unchanged server hints, and existing budgets/cancellation/replay refusal using the harness suite. Run the coding differential suite and compile gate, then formatting checks.

Historical adaptation, learned timeouts, retry-count changes and remote reporting remain deferred.
