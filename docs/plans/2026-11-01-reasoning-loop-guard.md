# Reasoning-loop guard implementation plan

Goal: interrupt high-confidence exact reasoning cycles and retry at most once,
without replaying visible answers, tool fragments or signed reasoning.

Architecture: kernel owns a bounded Unicode detector and request-local options;
harness and kernel own their respective stream/retry lifecycles. A transient
ReasoningDiscarded event marks already streamed reasoning as discarded. It is
not serialized into session files, does not rewrite history, and does not enter
the model projection. Existing notices remain the visible explanation.

1. Add detector/config in kernel: bounded 4096 Unicode scalars, periods 32..1024,
   at least four repeats and 1024 repeated scalars, incremental checks independent
   of provider chunk boundaries. Validate positive and negative/CJK cases.
2. Add transient discard event and native/handle projection; mark the TUI's
   current reasoning block discarded and settle it. Keep persistence unchanged.
3. Detect only before visible text, tool fragments or signed-block boundaries.
   Abort by dropping the provider stream. Explicit recovery owns one retry,
   preserves existing cancellation and shared provider-attempt limits, and never
   presents arbitrary partial failures as content-free retryable errors.
4. Wire ChatOptions config through the existing chat-options row; document the
   customization seam. No provider-specific heuristics or正文检测.
5. Mock-provider tests cover recovery, exhaustion, cancellation, successful
   history, signed/tool boundaries and storage omission. Run affected nextest
   suites, compile gate and formatting checks. Do not commit unless requested.

## Verification

- Full affected-crate run: 5,859 passed, 10 skipped (two pre-existing slow runtime
  tests took ~75 seconds). Kernel/harness plus targeted configuration/storage/UI
  regressions were revalidated after final adjustments: 672 passed.
- Final stream-deadline/loop/cancellation regressions: 13 passed. In particular,
  cancellation cannot revive discarded chunks as a durable PartialReply.
- `bash gates/compile.sh`, `cargo fmt --all -- --check`, and diff checks passed.
  No dependency or Cargo.lock change. Existing unused-mut warnings remain.
- The standalone harness JSONL writer now skips stream chunks as well as discard
  boundaries, matching native storage; its timestamp regression compares durable
  records only. Readers still accept existing logs containing chunks.
- No live model or Windows terminal test was performed; deterministic mock
  providers cover detection, exhaustion, cancellation, no-replay boundaries,
  final snapshots, request budgets, and provider reload configuration.
- Pre-commit audit found no additional blocking issues; live-model and Windows
  validation remain outstanding.
