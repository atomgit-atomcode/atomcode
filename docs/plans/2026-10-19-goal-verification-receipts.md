# Goal Verification Receipts Plan

Baseline: release/v5.2.4 c4a7e3ac4; isolated fix/goal-verification-receipts. Preserve unrelated WebUI changes. Existing c4a7 already reserves two shell receipts; verify rather than recreate that implementation.

CodingRuntime remains sole owner; this change is a bounded native snapshot projection only. No lifecycle hooks, persisted formats, provider requests or verdict weakening. Unknown stays Inconclusive.

1. Reproduce the real phase-2 receipt (19 tests and exit_code=0) followed by documentation/todo and harmless shell calls. Fixture is a small path-anonymized extract of actual canonical tool facts.
2. Reserve a bounded quota for verification-like execution candidates independently of arbitrary shell/housekeeping receipts. Heuristics affect selection only, never certify success. Retain current failures.
3. Annotate message/call provenance and order relative to observed file-write attempts. Later writes may be docs; shell side effects remain unknown. Never claim old tests certify new source.
4. Run regression red/green, replay the real canonical session through native projection, crate tests and compile/fmt/lock checks.
5. Integrate only this change; build using the user-required ./scripts/build-official.sh release/v5.2.4. Preserve the old 19-minute run and start a separate documented long-run segment using the built binary. No commit/push unless requested.

Validation: both new regressions failed on the prior c4a7 policy; all 10 evidence tests passed after the change. Full coding: 1067 passed, the same 9 previously recorded failures, 9 skipped (1076 executed). Compile gate and fmt/diff checks passed. User-required ./scripts/build-official.sh release/v5.2.4 built successfully and restored public stub/lock. Binary version c4a7e3ac4+dirty, SHA256 45466839d48d9bc9b0a6ea14b14e5671e865f7d4417ae8097da128638646397f.
Real follow-up: resumed session 68124106-f4b7-417d-8c4f-9728eb475293; evaluator-only recheck of phase 2 returned Met with unchanged 37 tool calls, then the external controller explicitly started phase 3. New test segment /Users/theo/Documents/workspace/atomcode-longrun-tests/20261019-225116-lumen-resume, started 2026-10-19 22:53 local, at most two hours. Statistics subtract the old native event/goal totals via resume.json; old evidence and project-at-stop remain preserved. No commit/push in this task.
