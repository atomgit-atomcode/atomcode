# Grep result memory implementation plan

**Goal:** Bound retained grep results before copying long lines, while retaining streaming search over complete files.

**Baseline:** `release/v5.2.4`, `2cf0f497cff7e806d4b65763fd38490b6b077e14`. Existing untracked artifacts are preserved.

**Architecture:** FileSystem search owns collection; GrepTool owns presentation. The existing CollectingSink is shared by LocalFs and the trait fallback. Keep the 10MiB search-buffer cap and match-count cap; add a 1MiB retained-result budget covering previews, paths and result records, and a bounded per-line preview. No runtime, persistence or driver changes.

**Failure semantics:** Budget exhaustion keeps collected results, stops the current file and directory walk, and explicitly reports incomplete search. Long-line previews carry an ellipsis. Do not turn incomplete search into a definitive no-match answer. Existing cancellation and timeout behavior remains intact. The generic filesystem fallback still obtains a whole file through read_bytes; this task bounds shared result retention, while production LocalFs remains streaming.

**Tech stack:** Rust, grep-searcher, cargo-nextest.

## Steps

1. Add regression tests for long matching and context lines, Unicode boundaries, aggregate result and path budgets, and stopping a directory scan at the budget.
2. Bound previews before allocating result text, share byte accounting across files, and propagate a byte-limit fact through ScanReport.
3. Keep grep's existing display format; report byte-limit exhaustion in its existing scan summary.
4. Run targeted tests, affected capabilities crate tests, formatting, and `bash gates/compile.sh`; inspect Cargo.lock for accidental changes.

## Validation

- Capabilities crate: 1089 tests passed. After adding the final file-tail and invalid-UTF-8 cases, all 31 relevant grep/world/search-discovery criteria passed.
- Tests inspect retained bytes directly, cover matching and context lines, UTF-8 boundaries and invalid bytes, paths and empty group records, cross-file accounting, and stopping before a second large file. A 14MB text fixture still finds its match at the end, beyond the 10MiB line-buffer ceiling.
- `bash gates/compile.sh` passed, including feature-gated targets and both negative controls. The first run collided with another gate's temporary canary; the final standalone run passed.
- `cargo fmt --all`, its check and `git diff --check` passed. Cargo.lock has no diff and no private crypto dependency entries.
- This is a retained-result data budget, not a claim that total process RSS is 1MiB: scanner buffers, allocator capacity, rendering and concurrency have additional bounded costs. No multi-GB/OOM stress run was performed.

## Pre-commit audit

Audited on `release/v5.2.4`, most recently at `2aa7c99267fc57a9dc73f6c4e01790aa878e4c8c`. Fixed an exact-capacity edge case: a result that filled the budget previously needed another candidate to mark the scan incomplete and stop. The collector now retains that last record and stops immediately for matches, context and separators. A real-file regression first failed on the missing byte-limit fact and passed after the fix.

Final validation: all 1092 capabilities tests passed, `bash gates/compile.sh` passed including feature checks and negative controls, and formatting/diff checks passed. Cargo.lock remained unchanged. No remaining blocking finding in the retained-result change; unrelated worktree artifacts are excluded from the commit.
