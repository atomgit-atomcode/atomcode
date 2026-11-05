# Recovery progress summary

Baseline: release/v5.2.6 at b4c8669d9, clean worktree after delivering crash recovery.

Keep recovery informational: no new automatic turn, tool execution, modal approval,
UI protocol or persisted state owner. Build a bounded summary from retained facts:
recorded assistant rounds, recorded tool results, last successful tool name and
unanswered calls. Display tool names only, never command arguments or full output.
The coding host augments its startup warning from the existing current_todos fold
(transcript plus existing sidecar), so task status agrees with ordinary runtime
presentation. Limit previews and strip control characters. A read error must say
that task state is unavailable rather than claiming there are no tasks.

Validate summary counts, bounds, tool failure vs success, task progression,
compacted sidecar fallback and cold/member recovery. Run affected tests, compile
and format gates. Windows/Linux terminal closure remains a manual validation item.

Validation: capabilities with session 1528 passed (1 skipped), coding 1224 passed
(9 skipped, 2 slow). Cold runtime resume verifies the warning contains tool counts
and canonical task status together, without triggering automatic work. Workspace
and feature compile gates, rustfmt check and diff check passed. Cargo.lock is
unchanged. The reviewed tool pairing now keys on (round, call id), covering IDs
reused across rounds without hiding an unresolved newer invocation.

Phase one was delivered separately as b4c8669d9 on release/v5.2.6. This follow-up
changes informational summaries only and introduces no additional event format.

Final review on 2026-11-05: include the recovered partial reply in the saved-reply
count and assert it in the cold-resume test. Capabilities (skills,session): 1612
passed, 1 skipped; coding: 1242 passed, 10 skipped. An existing open_file test hit
its one-second process-start deadline during concurrent suites; the complete
capabilities suite passed with two test threads. Compile gate, format and diff
checks passed. Native Windows/Linux terminal closure remains unverified here.
