# PR 1127 Git authentication hints

Baseline: release/v5.2.4 c4a7e3ac4; merge pr_1127 f7b82b104 in an isolated worktree. Preserve main-checkout dirty changes. The conflict is between existing Windows shell tests and new Git diagnostic tests; retain both.

Goal: diagnose Git authentication failures without refreshing credentials, replaying commands or changing in-band exit status semantics. This partially addresses #1645, not credential refresh on session resume.

Ownership: capabilities bash owns result formatting; background jobs own bounded diagnostic state alongside their existing output pump. Runtime/session ownership and persistence are untouched.

1. Resolve the test insertion conflict retaining both suites.
2. Use one hint formatter for foreground/background. Require a known nonzero exit and Git-specific fatal diagnostics rather than a generic Authentication failed substring. The hint refers to Git credentials, not AtomCode login.
3. Preserve bounded stderr diagnostic state across background polls and chunk splits. Emit a hint only at final failed completion; suppress on stdout-only matches, success, cancellation, signals and unknown exit status.
4. Cover parser false positives, status preservation, split stderr, earlier polls and bounded output with mock-world tests. Use paused Tokio time for asynchronous fixtures.
5. Run capabilities tests, formatter and compile gate; merge the validated result into release/v5.2.4 while retaining unrelated dirty files. No push requested in this turn.

Validation: the new foreground regression failed against the original PR (signal termination received a hint), then passed after the fix. Seven focused regressions passed; capabilities with mcp+skills: 1273 passed. Compile gates (including feature gates and negative controls), formatting and diff checks passed. The Windows shell tests were preserved. No real credentials or external repository were used; hints remain heuristic English Git HTTP diagnostics and do not refresh credentials.

Implementation: one shared hint formatter; background stderr is scanned with a bounded overlapping byte window using the job world's decoder. Only the matched diagnostic and known failed-exit facts survive polling. A hint is emitted on final failed completion; success, stdout-only matches, cancellation, signals and unknown exits do not emit it. Original output, statuses and in-band error semantics are retained.
