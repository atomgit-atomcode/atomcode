# Layered memory and injection visibility implementation plan

**Goal:** Expose disk selection versus session-logged memory and add an optional bounded project knowledge index.

**Architecture:** Capabilities owns a derived memory prompt/report, file loading, and shared rendering. The harness memory row injects that prompt through the existing logged `Injected` fact and serves `/memory` diagnostics from the session log. TUI renders the same report alongside its existing source tabs. No second session owner or new persisted format.

**Baseline:** release/v5.2.4, 67a7b3435. Existing unrelated dirty edits are preserved. No automatic maintenance, migration, TTL deletion, or rewriting historical injections.

## Tasks

1. Add `memory/context.rs`: bounded file reads and shared-budget selection with per-scope selected, omitted, truncated, read-failure and tail-limit diagnostics. Preserve newest/specific-scope priority. Optional `knowledge/INDEX.md` contributes only a bounded routing index; linked files remain on-demand through existing read/search tools. Remove elevated authority wording from generated memory context.
2. Extend the existing memory row with optional project-relative `knowledge_index`; register guidance on the owning row. `/memory` reports latest disk preview and session-logged payload separately; JSON source sections get an optional additive review field. No mutation of current history.
3. Add a diagnostics tab in the TUI; keep all source text intact. Expose a visible overflow/read-failure heading on injected memory cards.
4. Add selection, Unicode/size bounds, index-only loading, empty/missing/error, first-turn and resume/unchanged-log criteria. Run affected crates with nextest when available, otherwise cargo test; run rustfmt and `gates/compile.sh`. Check Cargo.lock and diff.

## Failure semantics

Missing optional files are empty. Unreadable/invalid files are reported distinctly and never treated as successful loading. An oversized source has explicit partial-read diagnostics; the index is head-bounded, entries retain tail preference. Memory failure does not fail the coding task. Diagnostics never claim newly edited disk contents have reached an existing session. Knowledge is historical context, not elevated instructions. All product paths derive from ProductDirs.
