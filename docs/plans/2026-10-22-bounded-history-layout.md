# Bounded history layout implementation plan

**Goal:** Restore bounded initial history layout and progressive access to older history.

**Architecture:** Session events remain the sole authority and all facts still reach UI modules. Only the derived row index is measured from the tail, extending as the viewport approaches its older boundary. Settings are resolved by the launcher through the existing Settings port. No persistence or subscription changes.

**Tech Stack:** Rust, TUI row index, launcher settings port.

1. Add a presentation-only row budget to Settings, preserving the disable/custom configuration semantics.
2. Measure a bounded suffix of the row index, cache loaded history, and extend by a page before scrolling reaches its boundary. Resize rebuilds only the needed suffix.
3. Add tests for bounded measurements, progressive access, disabling, custom configuration, resize and session replacement. Check that existing rendering invariants still pass.
4. Run affected crate nextest, formatting and compile gate. Keep earlier first-frame timing changes separate from this work.

Baseline: main a124deb61b0b330fe0989eaf750cc0faa9087af1. Existing dirty first-frame timing changes preserved. Failures do not fall back to fresh sessions; session restoration is unchanged.

## Implemented behavior and limits

- The launcher resolves Windows/VS Code/JediTerm to 1,000 rows, WezTerm to 3,000 and other terminals to 2,500. Explicit budgets, zero and the disable toggle keep their configured meaning.
- The row index formats whole blocks from the tail. It measures more as scrolling approaches the loaded boundary and retains visited history while the stream is stable or the user is reading above the bottom. Batched replay at the bottom keeps a moving bounded suffix.
- A presentation-only header explains that earlier history loads on upward scrolling. Session switching clears the derived index.
- Export and selection explicitly measure full history. Selection retains full measurements while held so prepending a lazily loaded prefix cannot change its absolute coordinates. The initial selection uses the geometry of the clicked frame.
- This is a layout budget, not a hard memory/message limit. Full event reading, subscription catch-up, module projection and the lightweight run/index metadata remain proportional to session size. A single large block is still formatted whole; this change does not claim to solve all long-session memory costs.
- Windows terminal behavior needs real Windows verification. Automated checks run on macOS.

## Verification

CLI: 591 tests passed. i18n: 73 tests passed. Compile gate passed all four phases. TUI: 1,802 tests passed on the final implementation. Formatting and diff checks passed. New coverage includes initial tail measurement, reaching the oldest block, resize, disable, switching, exports, selection, batched replay and incoming blocks while reading older history.
