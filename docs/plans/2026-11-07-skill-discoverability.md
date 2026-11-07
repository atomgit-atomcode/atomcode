# Skill discoverability implementation plan

**Goal:** Make multi-skill invocation discoverable and show retained session usage without changing permissions.
**Architecture:** Harness skills gateway owns parsing, validation and output. TUI forwards its existing dollar shorthand. Usage is a disposable projection of retained SessionEvent::Injected facts; no schema or kernel changes. Explicit references are all validated before preparation or queuing. Existing automatic loading is unchanged.
**Tech Stack:** Rust, existing i18n, harness session log, nextest.

Baseline: release/v5.2.7 at 16f85c45a. Main worktree has unrelated crypto/lock edits; develop in isolated worktree. No dependencies or lock changes intended.

1. Extend prefix parser to accept explicit $ / ¥ skill tokens and reject unknown explicit names atomically; preserve ordinary task spacing and existing unmarked prefix syntax.
2. Add retained-history projection (canonical names, distinct turns, latest turn), exclude custom-command provenance; use it for listing and repeat notices on gateway and direct skill commands.
3. Add English/Chinese usage and status messages. Keep TUI shorthand forwarding explicit secondary references to the common parser.
4. Test parser aliases, unknown second references, ordinary prose/variables, images, same-input dedup, repeat execution, restore and rewind. Regress automatic loading/permission tests.
5. Run affected crates, compile gate, format and lock checks; transfer only feature files back without crypto/lock edits. No commit/push without a new instruction.

Implementation notes: TUI `/skills` is a picker rather than a bare command submit. Added
`/skills --list` and an explicit picker option for usage/history. TUI deliberately suppresses
successful turn-command replies and reconstructs acknowledgements from facts, so repeat notices
are also folded into that existing transcript projection. The view index contains only skill
name/turn/sequence metadata, resets on replay, and retracts on conversation rewind/undone input;
no authorization state or new persistent event kind was introduced.

Validation: harness 305/305, i18n 73/73, TUI 1936/1936, automatic-skill regressions 7/7 passed;
one opt-in real-model test remains ignored. End-to-end test covers multi-skill dollar input,
unknown-secondary rejection without a model turn, image delivery, visible repeat notices and
list/history output. Compile gate passed in the isolated worktree. Feature diff was applied
onto the same baseline in the main worktree; no crypto or lock changes were transferred.
