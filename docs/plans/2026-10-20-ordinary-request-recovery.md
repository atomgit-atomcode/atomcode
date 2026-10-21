# Ordinary request recovery implementation plan

**Goal:** Give ordinary requests a longer, finite transient-error recovery window without replaying partial tool output or cutting off healthy streams.

**Architecture:** The existing harness `llm-retry` row owns short transient retries; coding re-applies its policy on model switches. Keep native stream continuation and tool receipt safety unchanged. Preserve explicit provider/upstream budgets and the current adapter ownership coupling; a shared budget across adapters, overflow and continuation requires a separate protocol change and is not part of this patch.

**Tech Stack:** Rust, Tokio virtual time, plexus harness, nextest.

Baseline: release/v5.2.4 at 5fce39a61. Other skill-permission edits in the working tree belong to concurrent work and must be preserved.

1. Add behavioral tests in `crates/atomcode-coding/tests/recovery.rs`: default policy survives four transient failures, stops after six attempts, rejects an excessive Retry-After rather than retrying early; fatal errors do not retry.
2. Increase the existing default from three to six total attempts in harness bundle/default and coding model patch; backoff remains 3/6/12/24/30 seconds (75 seconds scheduled waiting at exhaustion). Explicit settings still win.
3. Stop automatic retries when the server asks for more than the existing 60-second single-wait allowance. Preserve the provider's original error and log the refusal. Never shorten the server's requested delay.
4. Update the model-switch criterion, run focused and affected crate tests, formatting and `bash gates/compile.sh`. No official build or live model soak is implied by deterministic tests.

## Validation

- Harness/coding: 1366 tests passed in the final run; the two excluded 75-second tests passed in the preceding run. Existing nextest exclusions remain in place.
- Model-switch recovery criterion also passed independently with a scenario-specific virtual-clock deadline covering the 75-second backoff schedule.
- `cargo fmt --all -- --check` and `git diff --check` passed.
- Clean rerun of `bash gates/compile.sh` passed all four checks. The first run collided with another process deleting a temporary feature canary; it was not treated as success.
- No official binary rebuild, real Lumen soak, commit or push performed for this change.
