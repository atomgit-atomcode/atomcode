# Compaction diagnostics implementation plan

**Goal:** distinguish failed/timed-out summaries, non-shrinking proposals and genuinely empty compaction; include request sizing in overflow errors without losing history.

**Architecture:** Keep CodingRuntime as lifecycle owner, extend existing read-only result metadata and adapt old compaction services by default. No session schema or compression state machine changes.

**Tech Stack:** Rust async traits, native harness/session events, crossterm TUI, Preact WebUI.

Baseline: release/v5.2.7 / 7aa3885ea, clean worktree. Reviewed recent compaction/recovery history and manual/automatic/overflow producers, runtime normalization and TUI/daemon consumers. CodingRuntime remains lifecycle owner; kernel types carry neutral read-only diagnostics, no session schema or new lifecycle hook.

1. Add neutral optional diagnostics to compaction plans and terminal events. Keep metadata out of conversation application and session persistence. Harness exposes a detailed attempt with a default adapter for existing providers; existing compact API stays compatible.
2. Capabilities report failed/timed-out summary without authorizing a drain; harness reports net-loss rejection. Overflow includes model/window/reserved-output/estimate and actual failure category.
3. Runtime forwards diagnostics through live and stateless compaction. TUI and daemon use localized reason messages, including degraded-but-committed results; WebUI receives warnings or an optional diagnostic on the existing context_compacting event, plus stateless command results.
4. Reproduce 512K→1M failed and healthy summaries with virtual time; verify no-op history/epoch, timeout, net-loss, wire defaults and UI projections. Run affected crate nextest, compile gate and fmt; record limitations.

Failure semantics: summary failure never drains dialogue; checkpoint failure remains its own terminal; diagnostic metadata does not add session event variants or bypass net-loss checks; automatic no-op degradation reuses the existing CompactionDegraded notice. Missing summary provider retains explicitly model-free behavior.

Preliminary validation: 205 compaction/summary regressions passed; WebUI 413 tests, typecheck and production build passed. Final run additionally includes all kernel/capabilities/harness/i18n tests and the latest TUI/no-op/size diagnostics cases. Real DS/lumen API failure reproduction remains unverified.

Completed behavior: summary failure/timeout diagnostics survive no-op plans, harness projection, runtime normalization, TUI and both daemon modes. Net-loss refusal has its own reason; genuinely empty compaction retains its old acknowledgement. Overflow errors include current model, total/usable token window, configured output budget and message estimates before/after preflight. A failed summary reason survives partial tool-only savings and an exhausted ladder; a later committed summary clears the old reason. Retry limits and reservations are unchanged.

Validation: expanded core/compaction run passed 3294 tests; its one new TUI test initially failed in a real-time frame-settle helper under a paused clock. Switching the fixture to the established event-driven paused-clock startup made that test pass. After the final ladder change, 60 related coding/harness regressions passed, including partial savings at the attempt limit. WebUI 413 tests, typecheck and build passed. One attempted concurrent gate/test build observed stale i18n artifacts; serial rebuild after refreshing source timestamps succeeded without build/profile changes. Real DS/lumen services were not called; this fixes the reproduced diagnostic loss, not an unverified provider failure.

Final bash gates/compile.sh passed on the completed source, including feature-gated tests and both gate negative controls.

Final review: no blocking findings in diagnostic propagation, no-op/net-loss retention, partial-fold retry limits, checkpoint/cancel terminals or additive wire compatibility. Before committing, fast-forwarded to a157f412b; the only upstream diff is the independent test-name correction in gates/tui-negative.sh. Previously completed compile/nextest/WebUI validation still covers the identical product source; fresh fmt and diff checks passed, and Cargo.lock remains unchanged with all three prohibited package names absent.
