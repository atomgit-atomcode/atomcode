# Preserve conversation when overflow summarization fails

Goal: prevent a configured summary model's failure from silently replacing design
history with an incomplete model-free checkpoint during emergency overflow.

Owner and boundary: capabilities decides the existing compaction plan; harness
owns the event session and request recovery ladder. CodingRuntime and drivers
keep their existing lifecycle and terminal events. No new hook, state machine,
session format, or runtime owner is introduced.

1. Extend strategy and native projection regressions to overflow, including
   failed, empty, interrupted, truncated, oversized and timed-out summaries.
   A configured summary failure must not authorize a history drain. Existing
   tool-result rewrites remain available; explicit model-free mode is unchanged.
2. After bounded preflight recovery, if estimated input still exceeds its usable
   limit, return a non-retryable context-overflow error through the existing
   request error seam instead of sending the unchanged oversized request.
   The terminal must explain that history was retained; a later retry can work
   after the provider recovers or the person changes model/context.
3. Add a preflight refusal regression. Run capabilities compaction, harness
   projection, coding recovery/overflow/runtime tests, then native CLI fault
   injection plus restart. Update the earlier probe's emergency expectation from
   lossy continuation to explicit error with retained history. Keep the old
   behavior as a failing negative control.
4. Run the compile gate, formatting and dependency-lock checks. Do not include
   unrelated concurrent work or change version/default model settings.

Baseline: release/v5.2.5 at 8795785bbedef35e231005721b1e0f5ef048b191;
existing dirty probe and verification document are this investigation's work.


Implemented: configured-model overflow failures now preserve dialogue, including
oversized user/assistant messages at the preceding truncation tier. Exhausted
preflight recovery returns the existing typed context-overflow terminal. The
explicit model-free strategy retains its previous behavior.

Verification: 1,322 Rust tests and 11 native CLI cases passed; the old binary
fails the new refusal contract. Native emergency cases stop before any history
cut, then resume after summary-provider recovery. The scope-only patch passes
the full compile gate in both the isolated and final shared worktree. Details and reproducible probe
commands are in `evals/design-convergence/OFFLINE-PROBE.md`.
