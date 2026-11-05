# Recap uses recorded task facts

Baseline: release/v5.2.6 at a86dfaf30, with the uncommitted opt-in live diagnostic
and diagnosis report. Six before-fix samples reproduced false completed/no-pending
recaps despite a canonical 0 completed / 1 active / 17 pending list. The primary
GLM/Linux failure remains unconfirmed; recap is display-only, never model input.

The existing Todo fold remains the sole task authority. Expose a read-only
projection returning known list / no list / read failure, including the existing
sidecar fallback after compaction. Reuse the existing recap control seam: capture
its task projection and provider only if the snapshot revision, generation and
idle state still match. Fail stale capture explicitly rather than mixing sessions.
No public wire/schema or kernel behavior changes, and no new persistence owner.

For a known list, return a bounded localized deterministic recap: recorded status
counts, a reminder to verify them against actual work/tests, and the current task
(or first pending task). Even a cleared or all-completed list is only a record,
not independent evidence that code is accepted. Do not make a model side call:
merely supplying facts to the model cannot guarantee contradictory prose stops.
A read failure is an unavailable-state message, never zero pending. With no list,
keep the original model-generated recap and its bounded/sanitized history.

No task updates, automatic completions, per-step reconciliation commands or
continuation-budget changes are introduced. Task labels are bounded and control
characters removed; deterministic text does not pass through an LLM-output
sanitizer that might discard a legitimate task named <none>.

Validation plan: deterministic conflicting-prose/count/empty/unavailable tests;
production runtime after compaction verifies sidecar use, no extra model request,
no conversation writes and explicit corrupt-sidecar handling. Repeat the same six
live fixtures into a separate report, then coding/i18n tests and compile/fmt gates.

Validation (2026-11-06): coding full suite 1246 passed, 11 skipped; final recap
runtime criteria 3 passed (including the added no-list model fallback); i18n 73
passed. The final focused criteria preserve the original conversation-model recap
selection and verify no extra model sampling for compacted task lists. Compile,
format and diff gates passed; Cargo.lock/latest.json are unchanged.

The same fixed 6 live fixtures were rerun once into
/tmp/atomcode-todo-eval-20261106/results-after.json. All 6 deterministic recaps
matched 0 completed / 1 active / 17 pending; recap model requests were zero in
all 6, while the paired primary requests still used the real configured provider.
Before-fix results remain in results.json. No claim is made about fixing the
original GLM primary-response hallucination, native Linux rendering, or arbitrary
future model behavior.

Final pre-commit review: carried onto release/v5.2.7 at 77a034f28. The target
branch full coding suite passed 1247 tests (11 skipped), i18n passed 73 tests;
workspace/feature compile gate, rustfmt and staged diff checks passed. The
original GLM primary-response failure remains outside this confirmed fix.
