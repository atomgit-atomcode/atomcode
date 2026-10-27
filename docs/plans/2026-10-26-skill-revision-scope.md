# Skill revision scope

Goal: prevent a loaded process skill from restarting completed design steps during a local revision, while allowing new evidence to invalidate earlier decisions.

Baseline: release/v5.2.5, 16a2d43ddd064bb75a847e288145a486e9b95f3d. Provider watchdog and installation-update changes already present in the worktree belong to other work and must be preserved.

Owner/boundary: coding persona owns product guidance; skill-first remains an opening-request reminder. No runtime state, session events, persistence, approval or driver lifecycle changes. Failure behavior is unchanged; this is model guidance, not enforcement of semantic correctness.

Source comparison:
- Codex ext/skills/src/catalog_prompt.rs: turn-specific matching, minimal applicable set, progressive disclosure.
- OpenCode packages/core/src/tool/plugin/skill.ts: matches the task at hand; loading is a tool operation.
- OMP src/prompts/system/custom-system-prompt.md and skillful-notice.md: matching skills must be read; no general exemption for design tasks.
- DSH packages/skill/tool-skill/src: explicit names or clear description matches require loading before actions; provider registry embeds loaded instructions.

These are current source observations, not a claim that all installed binaries have identical behavior. None of these inspected seams provides a semantic guarantee that confirmed database fields are preserved.

Implementation: qualify matching as the current request; add stage-specific guidance to the existing SKILLS paragraph only. Reuse confirmed decisions for local revisions, do not expand requested scope, and explicitly permit necessary changes when new evidence invalidates a decision. Keep loading and catalog-name validation intact. Do not add a generic design paragraph, fact store, phase classifier or reminder hook.

Validation: existing coding tests; workspace compile gate; format check. Model probes must cover local rename and new contradictory evidence, and must be reported separately from deterministic test results. No string-presence test can prove model behavior or original-user-case resolution.

## Outcome: candidate withdrawn

Tried stage-specific wording, then withdrew both production-file edits after probes did not establish benefit. Two fixed-history cases (rename / new BootId evidence), each baseline and candidate, were executed twice: first with incomplete table inventory, then with exact column inventory (8 completed requests total). These are isolated SKILLS-fragment probes with a replayed skill result, not native runtime or full-persona tests. Neither arm restarted a full interview in these samples. Both preserved key meanings and added BootId when necessary. Incomplete inventories elicited invented columns from both arms. With exact inventories, both rename answers retained the exact columns, but still supplied unspecified SQL types; no overall semantic correctness claim. New-evidence replies still added assumptions or questions. These samples do not justify more production prompt text.

Candidate validation before withdrawal: coding nextest 1178 passed, 9 skipped; workspace compile gate passed; format check passed; evaluation-tool tests 14 passed. This establishes compatibility only. Prior todo rearm fix is retained. No commit or push performed for this experiment. Next useful evidence is a captured real failure or a fixed-history fixture that reliably reproduces unwanted workflow restart in the baseline before proposing another fix.
