# Background tool presentation

Baseline: release/v5.2.6 ac11f3668. Preserve unrelated dirty TUI ansi.rs.

Tool calls and background work have separate lifetimes. Keep existing native
jobs, completion notices, model results and persistence unchanged. WebUI derives
labels from known tool semantics, not from arbitrary text containing "running".

- Show acknowledgement for background Bash/PowerShell start, stop requests and
  team dispatch/message/status calls. Poll labels report observed job state;
  nonzero process exit is distinct from successful retrieval.
- Group adjacent empty same-job polls across assistant rounds in the visible
  message window only. Text, user/system messages, other tools, progress,
  substantive output, failures and completion break the streak. Disable grouping
  while searching. Preserve every original call/argument/output in group details.
- Live and reloaded history use the same pure projection. No mutable job registry
  or synthetic tool completion is introduced. Reuse existing completion support;
  this display change does not itself reduce model polling or change wait limits.
- Verify status/ordering/immutability with unit tests and a browser fixture that
  covers streaming, reload, group details and meaningful intervening output.

Validation completed:
- 354 frontend tests passed, including 10 background presentation/grouping tests.
- TypeScript checking and production build passed.
- Browser fixtures at 1280px and 390px confirmed history grouping, original
  arguments/results on expand, nonzero exit styling and team dispatch labels.
  Live pending-to-result transitions are covered by projection unit tests;
  a real provider/background-process end-to-end run was not performed.
- Workspace/feature compilation gate and its negative controls passed.
