# Read-only ATI v1 Implementation Plan

**Goal:** Add opt-in, bounded repository architecture evidence and file-placement candidate retrieval, with no automatic edits, memory writes or startup indexing.
**Architecture:** A `repo_architecture` tool in capabilities/codeintel reuses the existing walker and optional shared CodeIndex. A default-disabled `architecture-topology` harness row owns mounting and guidance; both generic and coding defaults include it disabled. The standard `[coding] architecture_topology = true` configuration enables it through the common runtime; SDK hosts may also opt in with a row layer. Evidence is derived per invocation, not a second session/runtime authority.
**Tech Stack:** Rust, ignore walker, existing TOML/JSON parsing and CodeIndex, harness rows, nextest.

Baseline: clean release/v5.2.7 a32f38bee. Worktree /tmp/atomcode-ati-v1, branch feat/ati. Implementation was initially prepared without publishing; review and publishing were subsequently requested.

1. Add capability tests for an overview with manifest dependency evidence and scoped instruction/ADR excerpts; candidate ranking, abstention and optional graph unavailable; dirty edits reflected immediately; read-only disk invariance; scope/symlink/oversized input/ignored trees/cancellation bounds.
2. Implement overview/placement actions. Parse only Cargo.toml and package.json for v1. Source-directory candidates are lexical retrieval signals, not architectural truths. Output fact/heuristic labels, referenced files/line numbers, coverage and limitations. Do not auto-generate constraints from patterns. Graph calls are explicit and bounded; use the existing index if mounted, otherwise report unavailable.
3. Mount through a separate disabled row, expose no tool or guidance by default, and verify enable/disable across generic and coding assembly. Enabling alone performs no repository read/index build. No new config tree for credentials, no kernel/runtime/session changes.
4. Document enablement and explicit tool requests, covered languages/manifests, untrusted evidence, no multi-repo discovery, no automatic dependency enforcement/refactoring/memory, no Git co-change mining, and bounded scan/partial-result semantics.
5. Run capability/harness/coding tests for the touched seams, compile gate, formatter and Cargo.lock audit. Preserve unrelated work and report tested vs inferred behavior.

Failure semantics: unreadable/invalid evidence is reported; incomplete scans never mean absent dependencies. Candidate misses abstain. Paths resolve inside the invocation working directory; symlink evidence is not followed. Repository text is data, not higher-priority instructions. No execution of build scripts, Git hooks or network requests. Optional call graph edges are explicitly heuristic and do not replace declared package dependencies.

## Completed validation

Implemented the default-off standard config switch in the common coding runtime and daemon config constructor, plus the optional harness row and bounded read-only tool. Added usage documentation. No kernel/session/protocol or persistent architecture state was introduced.

- `cargo nextest run -p atomcode-capabilities --features codeintel -E 'test(codeintel::)'`: 63 passed.
- Harness capabilities integration tests: 33 passed.
- Coding mount_wiring + host_plugins tests: 23 passed.
- Daemon live-runtime config propagation test: 1 passed.
- `bash gates/compile.sh`: passed, including gated features and negative controls.
- `cargo fmt --all` and `cargo fmt --all -- --check`: passed.
- Diff whitespace check passed; this worktree's Cargo.lock unchanged, forbidden package names absent.

120 related tests total after the review regression test. No live-model evaluation of architectural recommendation quality or manual TUI/WebUI interaction was performed. Placement remains lexical retrieval, explicitly labeled, rather than a semantic architectural verdict. Review additionally aligned ATOMCODE.md discovery with the standard project instruction entry point and stabilized source-evidence ordering. Concurrent edits in the primary checkout were left untouched; publishing uses the isolated worktree.
