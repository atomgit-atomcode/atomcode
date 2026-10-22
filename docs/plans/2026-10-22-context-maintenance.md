# Context maintenance implementation plan

**Goal:** Reduce repeated coding context without losing edit presentation or session replay.

**Architecture:** Coding owns a request row which proposes bounded result rewrites and commits existing `MessagesRewritten` facts before sending them. Original tool facts remain available to presentation readers. Datalog remains observation-only. No new session schema, runtime owner, summarizer, or threshold.

**Baseline:** release/v5.2.5 at dfa998f49383445f934680eabeb4e0e8d2ea0e86. Existing provider/proxy/i18n changes belong to other work and are preserved.

**Tech stack:** Rust, coding harness rows, native event projection, opt-in CAS datalog.

1. Add a coding request row. Shorten only exact successful `edit_file` results. Preserve fuzzy/error results, original events and UI diff. Commit before replacing request text; persistence retains its existing fail-closed boundary.
2. In that same bounded pass, let a new complete numbered `read_file` supersede an older complete read with the same literal path. Exclude partial/range/truncated/error/image reads. Restrict edits to the last 12 messages and 8k estimated tokens, keeping deep cached prefixes unchanged. Rewrites are idempotent and replayable.
3. Add datalog byte categories and estimated-token categories, plus common-prefix/request-size changes and measured response usage. Labels distinguish byte estimates, changes between requests, and provider measurements; request deltas are not presented as compaction savings.
4. Verify projection/replay and presentation against literal logs, conservative read exclusions, idempotence and tail protection. Run focused nextest, affected crate tests, formatting and the compile gate. Review lock and diff; do not change existing differential golden files.

Absolute summary budgets and controller pacing remain separate follow-up experiments. No threshold changes in this implementation.

## Result

Implemented the coding row and opt-in datalog diagnostics. Opaque signed reasoning disables cleanup; new input and non-read/edit tool results reset read supersession candidates so relative paths are not merged across possible directory changes. Original result facts remain the presentation source, including after event serialization/replay. Rewrite commits use the native store's existing cancellation on uncertain writes; a cancelled owner prevents model dispatch.

Validation: coding library and mount wiring (617 passed after audit), capabilities library with session (1344 passed), runtime criteria and differential (176 passed). The final focused pass, including directory boundaries and presentation replay, passed all 6 tests after audit. `bash gates/compile.sh` passed both compilation checks and its canary self-tests. No differential golden files, session formats, dependencies, versions, summary thresholds, or user sessions were changed. This is a bounded cleanup, not a measured provider-cost benchmark.

## Pre-commit audit

Require matching cleanup decisions from both the sent request and the authoritative logged projection: request-only tool content cannot authorize persisted supersession. Unknown/unpaired tool results also reset read candidates. Added a regression covering both cases. Only the six context-maintenance implementation, wiring, test, diagnostics and plan files belong to this change; unrelated provider/UI edits remain outside the commit.
