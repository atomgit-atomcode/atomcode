# Adaptive todowrite confirmation

Baseline: release/v5.2.8 at 2c121002fcfafb13f46d4811c524774573209919 (PR1194 merged). Existing unrelated WebUI/file viewer changes are retained.

Owner and scope: TodoTool validates full plans and formats the successful receipt in capabilities. The transcript remains the authority; coding TodoHook, sidecar persistence, compact/resume, frontend title projection and incremental updates are unchanged. Invalid plans remain errors.

Implementation: remove repeated update/1-based-id boilerplate from the summary (already documented in the tool and reminder). Preserve total/status counts and the current in-progress id. Count pending items without collecting unused ids. Choose the shorter UTF-8 payload between the concise summary and the existing normalized numbered list; ties prefer the list. Empty-list confirmation remains unchanged. Do not add runtime state, configuration or provider-specific branches.

Verification: full-plan output is no longer than either baseline numbered output or PR1194 summary across short/long, Chinese/English, status mixtures and >9 ids; a long plan omits repeated task titles; short plans retain numbered titles when smaller. Validate malformed plans and stringified arrays, incremental update ids and clear behavior. Exercise TodoHook reminder and compaction/sidecar regression with both receipt forms. Run capabilities and coding todo tests, compile gate and formatting.

Quality limitation: byte reduction is not a token/cost benchmark or proof of unchanged model behavior. Before claiming behavioral parity, run identical multi-step tasks against baseline and candidate on at least a strong and a weaker model, with repeated trials: initial/re-plan/reorder, long-context interruption, compact/resume, and final reconciliation. Measure task completion, missed tasks, wrong-id updates, false completion, redundant planning calls, and reported input/cache/output usage. No paid live inference or user code upload is part of this implementation verification.

## Verification results

Validated in /tmp/atomcode-todo-adaptive-review at the specified clean baseline plus this task's two Rust file changes. Main workspace had an unrelated transient provider/trait compile mismatch during concurrent work; it was not repaired by this task. Both task files were byte-compared against the isolated validated copies.

- capabilities: 1394 passed, 6 skipped (/tmp/todo-adaptive-isolated-crate.log).
- coding todo unit tests: 53 passed (/tmp/todo-adaptive-isolated-coding.log), including identical full-plan reminder payloads for numbered and compact receipts.
- compile gate including feature/canary checks passed (/tmp/todo-adaptive-isolated-compile.log).
- formatting and diff checks passed.
- representative all-pending plans of 1/3/6/20 items: merged PR1194 receipts 97/98/98/100 bytes; adaptive 31/36/36/38 bytes (/tmp/todo-adaptive-measure.log). These fixtures measure UTF-8 bytes, not model tokens or workload-level savings.
- No live-model A/B performed; no behavioral-parity claim.
