# Scoped Search Feedback Implementation Plan

**Goal:** Make slow Glob/Grep searches visibly active in their tool cards and keep recovery within the path the user supplied.

**Architecture:** Capabilities owns a delayed, cancellable activity loop around the existing bounded search future. It emits existing call-correlated `ToolProgress` events; no new wire or session facts. Drivers keep only the latest activity on pending calls and discard it on terminal/session changes. TUI updates its live transcript block through the owning producer rather than the global notice bar. Search ignore/link defaults and filesystem permissions stay unchanged.

**Tech Stack:** Rust/Tokio, existing TUI producer and stream APIs, Preact/VS Code tool projections.

Baseline: `release/v5.2.6` at `c37314193`; clean worktree. Relevant history: `e3186e06e` introduced scoped discovery, `fbf71647b` wired defaults and bounded metadata/traversal, `16a2d43dd` removed routine notices to avoid global-tip spam. Preserve that intent: immediate searches are silent, slow progress stays in its own card.

1. Add a shared search activity wrapper, first update after one second and subsequent updates at one-second intervals. It owns no task or timeout, drops with the search, and reports the requested scope and elapsed time. Test fast/slow, metadata-blocked, cancelled and timed-out requests with paused time.
2. Include actual scope and effective filters in empty/timeout recovery hints; instruct retrying the same path with relevant switches and direct reads for known filenames, without automatic parent/disk expansion. Add ignored-link and exact-path regressions.
3. Project live tool progress into TUI's owning pending block, replacing its current activity and ignoring unknown/finished calls. Keep it visible in folded/grouped calls and remove it on result/stop/reset. Keep WebUI/VS Code's existing card rendering, hardening late-event rejection.
4. Run capabilities search tests, affected crate/library tests, UI regression tests/typechecks and a browser smoke covering visible activity, replacement and completion. Run workspace/feature compile gate and format/diff checks. Windows junction tests remain in the suite; this macOS host cannot provide Windows execution evidence.

No automatic process-wide scan, new search configuration, dependency, runtime owner or persistent activity log is introduced.

## Outcome and validation

Implemented delayed per-call activity without adding a worker/task, timeout or wire event. TUI amends its existing live block through its owning producer; pending activity remains visible in folded views and is not merged behind another parallel call. Results, interruption and session reset discard it. WebUI and VS Code reject late activity for finished calls. Filter recovery now includes the actual scope and same-path retry parameters. Existing ignore/link defaults and direct-path behavior remain intact.

- `cargo nextest run -p atomcode-tui -p atomcode-capabilities --lib --test search_discovery`: 3,563 passed, 1 skipped. The 12 search-discovery tests cover fast/slow requests, scope hints, metadata deadlines, cancellation, ignored links and exact lookup. Windows-only junction coverage was expanded to include Git-ignore plus exact reads/lookups; it is not executed on this macOS host.
- WebUI: 387 tests, typecheck and production build passed. `node scripts/search-activity-e2e.mjs` passed at 1200px and 390px; screenshots inspected under `/tmp/atomcode-search-activity-*.png`.
- VS Code `npm run test:webview`: passed, including replacement and late-event rejection.
- `bash gates/compile.sh`: passed workspace/all-target compilation, feature compilation and negative controls.
- `cargo fmt --all -- --check` and `git diff --check`: passed. Cargo.lock unchanged; prohibited package names absent.

Known baseline failures, reproduced against isolated unmodified HEAD source copies:

- `bash gates/tui-layers.sh`: existing ansi.rs environment access and counts `literal_glyphs=509`, `raw_colours=1`, `os_probes=2`; unchanged by this task. No baseline or gate edits were made.
- VS Code webview standalone typecheck: existing `ChatProvider.tsx:248-249` handles `runtimeInfo` although the message union omits it (TS2678/TS2339). The same three errors occur on HEAD; the changed reducer itself introduces no additional errors.

Unrelated model-card data/export files appeared during execution and are excluded from this change.

## Pre-commit audit

Reviewed the activity future's cancellation/drop behavior, TUI pending-block ownership and render caching/grouping, frontend terminal projection, same-scope retry hints, and Windows path handling. Fixed the following before committing:

- Integration-test scope assertions now use the production path display normalizer rather than native separators, including Windows verbatim-prefix handling.
- WebUI's existing completion projection settles any still-open tool rows and clears their activity when a turn terminal arrives before a tool result. It marks them incomplete, never assumes tool success, and does not alter runtime state. Reused the existing terminal boundary after checking kernel `finish_turn`/`LifecycleHooks::turn_complete` and compaction; no overlapping hook or state machine was added.
- VS Code's existing interrupted/error settlement also discards activity. Added regression coverage for a terminal without individual results and subsequent late progress in both frontends and TUI. The desktop/mobile browser case covers cancellation before any tool result.

Validation after audit: all 3,563 selected Rust tests passed (1 skipped); all 387 WebUI tests passed, as did typecheck, production build, browser regression, VS Code regression tests, workspace/feature compile gate, format check and diff check. Windows execution remains unverified on this macOS host. No task-blocking findings remain; the two separately reproduced baseline failures above remain outside this change.
