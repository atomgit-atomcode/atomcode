# Execution evidence and transport hardening

Baseline: release/v5.2.4 at 612110719. Main checkout contains unrelated relay/daemon changes and a private crypto overlay; preserve them. Work in an isolated checkout with the public stub.

Owners: capabilities MCP transport owns protocol correlation; coding runtime owns goal/evaluation/usage; the harness owns request projections. No new lifecycle owner, persistence format or credential-bearing configuration.

Plan:
1. MCP: dispatch notifications separately, correlate responses by id under one deadline, reject unsupported server requests, classify malformed frames as uncertain transport failures. Preserve no automatic replay of side-effecting tools. Add real-process fixture regressions for notifications, stale replies and malformed-frame recovery.
2. Goal: provide bounded whole real-user instructions separately from untrusted assistant/tool evidence; omit oversized instructions whole with an explicit notice. Preserve execution receipts alongside later navigation/housekeeping, record call ids and position, retain success/failure budgets and mark earlier evidence. Add inspection-provider tests against the actual evaluator request and evidence selection.
3. Accounting: run existing interrupted/restart/uncertainty regressions; avoid duplicating recent fixes.
4. Context: reuse existing image/output rows; ensure a 413 retry actually reduces carried images even below the configured ceiling; make skill listing bounded and searchable. Preserve the canonical log and cache stability.
5. Run affected crate tests, feature-gated MCP tests, formatting and compile gate. Transfer only owned changes back; no commit/push without request. No production model calls or replay of the user's project.

Implementation notes:
- MCP uses one request deadline across notifications, stale replies and unsupported server requests. Malformed protocol frames retire the uncertain connection; tools/call is never replayed automatically.
- Goal evaluation receives up to eight whole real-user messages within 8192 characters, separately from assistant/tool evidence. Missing or compacted instructions remain explicitly unavailable. Receipts retain call IDs and snapshot message positions; two slots preserve execution evidence.
- Existing pause/accounting behavior is retained. Existing observed-work, checkpoint/resume and inconclusive-verdict regressions passed.
- Skills: query/offset/limit, default 20, maximum 50, abbreviated descriptions; full descriptions remain searchable. Images: one 413 retry halves actual carried bytes; canonical session messages remain intact.

Validation:
- capabilities with mcp+skills: 1266 passed.
- coding+harness full run: 1316 passed, 8 failed, 9 skipped. All new regressions passed. The same eight failures reproduce on the unchanged 612110719 baseline in the same environment: three approval fixtures under the system temp tree, four compaction expectations, and one existing tool-mount source audit. They are outside this change.
- The two differential approval fixtures require non-temp paths; the isolated target/diff-scratch was mapped to a non-temp scratch directory for the full run. No approval implementation or golden was changed.
- Compile gate (including feature gates and negative controls), cargo fmt --all -- --check and git diff --check passed. Cargo.lock is unchanged and contains zero private overlay dependency markers.
- No live model or production-service calls; goal judgment still depends on the evaluator and bounded available context.

Logs: /tmp/atomcode-hardening-capabilities.log, /tmp/atomcode-hardening-runtime-final.log, /tmp/atomcode-hardening-baseline.log, /tmp/atomcode-hardening-compile.log.

Pre-commit audit (release/v5.2.4 at 830812450):
- Fixed an additional malformed-frame boundary: NDJSON read_line can fail on invalid UTF-8 before JSON decoding. InvalidData now uses the same uncertain-connection recovery, and the real-process no-replay regression exercises both malformed JSON and invalid UTF-8.
- Added coverage excluding internally marked retained historical user messages from current goal directions.
- Revalidated in the main checkout: capabilities (mcp,skills) 1266 passed; coding+harness library 622 passed, 8 skipped; compile gate and formatting passed. The eight pre-existing full-integration failures remain documented above.
