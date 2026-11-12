# Session Fork Implementation Plan

**Goal:** Add `/fork` and discoverable terminal/web entry points for copying the current conversation and continuing independently.

**Architecture:** CodingRuntime owns the idle-only fork/reprepare transaction. SessionManager copies the authoritative event aggregate under a new lease; intentional branches remain independent catalog entries, unlike automatic busy-session forks. Drivers issue a typed host command or daemon request, observe the ordinary session transition, and never construct another live owner.

**Tech Stack:** Rust runtime and native event store; Host API/TUI; daemon HTTP; Preact/TypeScript WebUI.

## Baseline and semantics

- Baseline: release/v5.2.7, bb762c163; clean worktree.
- Copy current committed conversation, presentation and supported sidecars; preserve provider, working directory and paused MCP selection. Switch only on successful assembly. No automatic compaction or working-tree copy.
- Reject running turns, pending approvals/questions and active goal/loop operations before copying. Delete an uncommitted candidate on failure. Keep the original visible and resumable.
- Reference local Codex, Claude Code, OpenCode, oh-my-pi and DeepSeek Harness; missing or feature-gated source does not establish available product behavior.

## Steps

1. Add intentional native branching without automatic lineage collapse/GC; test both copies stay listed.
2. Add a runtime-owned fork target through existing reprepare/lease transfer; test isolation, resume, busy rejection and failed assembly rollback.
3. Expose typed Host API/TUI `/fork`, bilingual help, completion and runtime self-knowledge.
4. Add daemon command handling and web slash/current-session menu entry, including standalone persisted conversations; use existing session navigation projection.
5. Run affected crate tests, web tests/typecheck/build, formatting, compile/layer/product-dir gates; review all affected dispatchers. Do not publish a release.

## Local reference findings

- Codex: TUI advertises fork-current-chat and defers saved-history forks through its normal session navigation.
- Claude Code: this checkout references a feature-gated fork command, but does not contain its implementation module. Its complete user-facing behavior was not inferred from that stub.
- OpenCode: `packages/core/src/session.ts::fork` copies an explicit message prefix into a distinct session and rejects an empty source.
- oh-my-pi: `packages/coding-agent/src/session/agent-session.ts::fork` supports whole-session or entry-boundary forks, with idle admission and session-transition ownership.
- DeepSeek Harness: `packages/core/session/src/fork.ts` copies an exact event prefix and repairs an open tail; AtomCode instead refuses active turns/pending interactions for this first whole-conversation entry.

Intentional copies retain `fork_info = None`, so existing/older catalog and auto-fork GC rules treat them as independent sessions. Their source must be lease-owned; an automatic-fork source is also detached from automatic lineage bookkeeping, preserving the chosen return point without adding a persistence format or writer. Provenance remains in the copied event header.

## Verification

- Affected capabilities/coding/daemon/TUI/host-api/i18n crates: 6,424 passed, 13 skipped; includes fork isolation, explicit compaction, source resumption, busy/pending refusal, failed candidate cleanup and preservation of an automatic-fork return point.
- CLI host/background suites: 116 passed before the final source-promotion refinement; final fork-focused host/background regressions: 5 passed, including shared WebUI-to-terminal switching and return to the source.
- WebUI: 453 unit tests, typecheck and production build passed. Fixture-only browser checks passed for desktop title-menu entry, mobile `/fork`, inherited context and refusal retaining the original.
- `cargo fmt --all -- --check`, `gates/compile.sh`, `gates/layers.sh`, and `gates/product-dir.sh` passed. No prohibited lockfile packages or lockfile changes.
- `gates/tui-layers.sh` remains red on both the worktree and an isolated unmodified HEAD tree, with identical failures: existing environment read, literal glyphs 528 vs baseline 16, one raw color and two OS probes. No baseline or gate relaxation was made.
