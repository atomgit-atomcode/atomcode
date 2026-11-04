# Session switch feedback implementation plan

Baseline release/v5.2.6 8a69534a5. Preserve concurrent cd-recent cleanup work.

Current native runtime is the sole owner and already offers resume-or-fork on session leases. No runtime or lease changes. WebUI carried notices were inserted into transcript state before history loading replaced that state, losing rejection/fork feedback. Keep ephemeral switching/terminal notices in Chat presentation state outside transcript/history, retain generation guards and existing rollback/fork behavior, and preserve structured HTTP failure messages. Verify with real-app mock SSE browser scenarios for occupied/rejected sessions and fork feedback surviving history load; run WebUI tests, typecheck/build and Rust format check.

Browser regression also reproduced a loading dead end after same-project forks: the history effect waited for matching activeSession.id but depended only on sessionId/project_hash, so resolving copy metadata did not retry the load. Add activeSession.id to that effect dependency. Carry notices with their destination id so unrelated later navigation cannot inherit an old fork/rejection notice.

Validation: 392 WebUI tests passed; typecheck and production build passed. Browser regressions passed at 1200px and 390px for lease rejection/rollback, same-project fork history loading and structured HTTP failure diagnostics, including dismissible visible feedback after history replacement. Existing session-browse regression also passed for both widths (running-history browsing, approvals/questions, completion and return without rebinding). Screenshots inspected. Rust implementation unchanged; formatting and whitespace checks passed. Actual two-terminal user environment remains unverified.

Pre-submit audit: shared JSON rejection diagnostics, destination-scoped notices, generation guards, and delayed same-project metadata loading reviewed. 392 WebUI tests, typecheck/build, switch feedback desktop/mobile regression and existing browse regression all passed again. Full workspace compile in the shared checkout hit unrelated in-progress background.rs:1738 `kill(&id).await` (synchronous Result is not a future). Validation continues in a detached worktree at 8a69534a5 with only the five tracked WebUI changes applied; concurrent Bash/harness/cd changes are excluded from the commit. Cargo.lock is unchanged and contains none of hkdf/hmac/zeroize_derive.

The isolated full compile gate passed, including both negative controls. No changes to runtime/session ownership, lease acquisition, or persistent format were needed.
