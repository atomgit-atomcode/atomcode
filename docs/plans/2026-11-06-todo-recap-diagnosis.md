# Todo / recap disagreement diagnosis

Baseline: release/v5.2.6 at a86dfaf30. No original session log was available.
The user authorized several isolated live-model tests. No production behavior
was changed; the new live diagnostic is ignored by default.

## Static findings

- TodoHook::pre_request derives the actual list and appends a fresh synthetic
  reminder containing the current item and all numbered entries. It does not
  reconstruct state from an assistant's completion claim.
- Runtime recap calls recap::generate_recap with snapshot messages. It does not
  use TodoHook's request-only tail or the canonical Todo sidecar as a separate
  input. The result is display-only, not inserted into model context.
- recent_stable_transcript is bounded to 16384 characters. Its generic argument
  projection omits unrecognized keys such as todos, rather than serializing a
  complete task list. Long histories can also evict the earlier plan/result.
  Removing that general-purpose sanitization is not the proposed fix.

## Live evaluation (2026-11-06)

Model: taotoken/deepseek-v4.1-flash-1017, low reasoning, cap 4096 output tokens.
Two fixed synthetic histories, three repetitions each, no retries/cherry-picking:

1. An 18-item plan, 0 completed / 1 in_progress / 17 pending, followed by a false
   assistant statement that all work and 2050 tests were complete.
2. The same plan plus 30 synthetic read-only tool pairs before the false claim,
   to exceed the recap's recent-history budget.

Each case made one direct state-report request after the real TodoHook request
preparation, and one call to the real recap generator. No tool definitions were
exposed, no commands executed, and no real project/session/config was modified.

Results (12 provider requests):

| Measurement | Short history | Long history |
| --- | --- | --- |
| Main request receives all 18 injected task rows | 3/3 | 3/3 |
| Main response corrects the false completion statement | 3/3 | 3/3 |
| Recap request includes specific task row labels | 0/3 | 0/3 |
| Recap repeats an unsupported completed/no-pending claim | 3/3 | 3/3 |

Report: /tmp/atomcode-todo-eval-20261106/results.json. It stores only synthetic
fixture measurements and visible replies; no reasoning, credentials, headers or
configuration. The opt-in entry is recap_live_eval.rs; TODO_EVAL_CONFIG,
TODO_EVAL_MODEL and a new TODO_EVAL_REPORT are required. Running it consumes
model tokens; normal CI does not invoke it.

## Limits and next step

This reproduces the erroneous recap, not the original GLM/Linux long autonomous
execution. Main requests here explicitly ask for a read-only state report; the
complete agent loop, completion nudges, actual compaction/resume and UI execution
were not exercised. The normal hook still exposes the true plan in this fixture;
these results do not rule out a separate original-session projection problem.

The next narrow fix should give recap an authoritative, bounded task-state input
using the existing Todo fold (including compacted sidecars), and keep counts out
of model-invented display prose. Do not restore per-step reconciliation commands,
add a second Todo owner, auto-complete items from prose, or force infinite work.
Original-session evidence is still needed to diagnose the primary GLM assertion.

Validation: the opt-in live test completed all 6 cases (12 provider requests)
without transport errors or output truncation. Related todo/recap/side-call unit
tests: 76 passed; existing production runtime criteria for task persistence,
post-compaction updates, bounded reminders and compacted-plan continuation:
5 passed. Compile gate, format and diff checks passed. These mocked compaction
criteria supplement, but are not equivalent to, a real-model compaction run.

Follow-up: the authorized narrow fix is recorded in
2026-11-06-recap-task-facts.md. Known task lists now bypass model-generated recap
prose and render their recorded counts/current item deterministically. The six
post-fix fixtures had correct counts and no recap model calls; this does not
resolve the separate, unreproduced primary GLM completion assertion.
