# Skill spelling hints implementation plan

**Goal:** Complete issue #1750's conservative spelling suggestions without changing which skills execute.
**Architecture:** A pure bounded skills helper identifies the first unresolved prefix token and suggests installed names. Harness owns validation/output; TUI uses the same helper to show a successful gateway invocation's advisory despite suppressing ordinary acknowledgements. No kernel/runtime/persistence format or permission changes.
**Tech Stack:** Rust, existing i18n and command descriptions, nextest replay tests.

Baseline: 2e7df1065, isolated worktree /tmp/atomcode-skill-spelling. Main has unrelated in-progress conflict resolution; preserve it.

1. Implement bounded single-edit/transposition and case/hyphen/underscore suggestions with deterministic ranking and at most three candidates. Qualified input stays within its namespace; only user-invocable skills are candidates. Short prose and punctuation/paths are excluded conservatively.
2. Keep explicit unknown references atomically rejected, adding candidates. Suggest for an unknown first bare skill too. For an unmarked task boundary after a valid skill, add an advisory, preserving task bytes and loaded names. Do not inspect later task text.
3. Add bilingual messages and TUI success-path advisory using shared rules and live command descriptions. Do not parse localized output strings or add protocol fields.
4. Verify parser boundary and candidate cases; gateway side-effect-free rejection, preserved task spacing, hidden skills, namespace ambiguity; TUI advisory visibility without restoring normal success acknowledgements.
5. Run affected tests, compile gate, format and lock checks. Transfer only feature files if no concurrent edits conflict. No commit or push without instruction.

Validation:
- capabilities (skills feature) + i18n: 1403 passed, 1 skipped.
- TUI complete suite: 1936 passed, including visible advisory with suppressed normal acknowledgement.
- Harness complete suite: 306 passed, including atomic explicit rejection, hidden candidate exclusion, task spacing and image preservation.
- Compile gate passed on sequential rerun (workspace all-targets, feature-gated compilation and both gate self-tests).
- Initial broad compilation saw missing i18n enum variants despite them being present in source; rebuilding i18n/config and running checks sequentially resolved it. Shared-target builds from other worktrees were active. No gate/build configuration or dependency changes were made.
- Feature hunks applied to main without replacing concurrent TUI fixture changes. No real model calls or issue comments were made.
- Formatting and diff checks passed; Cargo.lock unchanged and forbidden package names absent.
