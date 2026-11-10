# Bounded background review latency

**Goal:** Return useful, explicitly partial review outcomes instead of spending repeated 15-minute attempts without a conclusion.
**Architecture:** Reuse the reviewer budget hook, cancellation seam, harness request/tool waterfalls and native session log. Each CodingRuntime remains its agent's lifecycle owner. CLI background slots only identify host-owned review assignments and route their terminal outcomes.
**Baseline:** a64d91b1e; isolated `fix/review-latency` worktree. Main checkout contains unrelated changes.

## Evidence and scope

The grape session's deep review exhausted 900 seconds in all four dimensions. Its outer coding agent then launched another single review, exhausted another 900 seconds, and continued manual inspection without returning to the parent. Background pytest jobs completed in seconds. There was no team tool call in this incident: this establishes a nested-review reporting problem, not a general team scheduler bottleneck.

The implementation retains the existing embedded defaults of **200 rounds / 900 seconds**. The reviewer gets soft landing guidance after the earlier of five minutes and 70% of its remaining allowance; the last 15% strengthens the reminder. Guidance runs at existing request boundaries, so it cannot interrupt an in-flight model request. The hard deadline still bounds execution. Environment overrides, including explicit zero/unbounded settings, retain their meaning. Standalone CLI reviews remain opt-in bounded.

Deep dimensions and verification share one hard deadline. Failed dimensions include their stop/error reasons; partial findings survive. An unfinished verification pass explicitly labels its retained candidates as unverified.

## Completion ownership and failure semantics

- `ReviewDelegate::report_only` defaults to false. CLI reports true only for a background review assignment still owned by its host. Opening or telling that session takes it over and removes the restriction. Ordinary foreground sessions and team members are unaffected.
- A coding row reads the current turn's authoritative event log. After the first requested `code_review` result, the next ordinary request returns that result without another provider call. Tool failure ends as local policy refusal, not a fabricated provider failure. Normal success uses the existing text-only completion path. No second lifecycle or turn-completion hook is introduced.
- The same row narrows the wrapper's tool catalog and denies unrelated calls or another review. The first requested review is identified in the committed assistant batch before parallel execution, avoiding a race between ToolStarted events. The specialist reviewer itself retains its own read-only tools.
- CLI routes every terminal background outcome to its original parent using the existing session-bound note/retry mechanism. Failed/cancelled outcomes are explicitly incomplete; an earlier turn's response is never reused. Explicitly dropped sessions are not resurrected. Generic team scheduling and shell-job budgets are unchanged.
- The report-only flag is live slot metadata, not a new session persistence format. The native log remains the only conversation authority. Taking an archived session up as a human conversation does not reapply the restriction.

## Validation

- 111 review tests: paused-clock landing pressure at five minutes, whole-review deadlines, remaining verification allowance, retained findings, dimension diagnostics and delegated instructions.
- Four coding runtime integration tests: success and failure return after one provider call, duplicate-review/unrelated-write batches cannot expand the assignment, and a human-owned conversation continues normally.
- 18 CLI background tests, including real runtime/UI failure notification without a closing assistant message and existing review result, navigation and recovery coverage.
- Three existing coding review-row/runtime criteria, including explicit review availability across reload.
- Workspace and feature-gated compile gate; formatting, diff whitespace and Cargo.lock checks. Cargo.lock unchanged; hkdf/hmac/zeroize_derive absent.

No live provider request was used for validation. No running user process or session log was modified. These checks establish deterministic budget and handoff behavior; they do not establish a real-model quality benchmark or promise a five-minute completion time.
