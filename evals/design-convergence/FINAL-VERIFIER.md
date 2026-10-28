# Second-pass final-design verification experiment

## Intervention

A separate same-model call reviews a saved draft against full numbered history and returns grounded findings plus a minimally corrected complete answer. It must respect user corrections and scope, distinguish proposals/assumptions from asserted protocol facts, preserve correct portions, and avoid reopening nonblocking questions. The reviewer is not given private model reasoning, tools, or authority to act on the workspace.

Dataset fixed before calling the reviewer: all6saved DeepSeek drafts from model-controls-20261026, not an error-selected subset. Add one clearly valid synthetic answer and one deliberately bad answer (ID merger, TubeSeq misnaming, ignoring BootId, unproven clock reset, stored Duration). Controls are diagnostic, not samples of natural model failure rates. Same DeepSeek model/gateway, max effort, cap32768, one extra request per draft; jobs shuffled with a fixed seed. Not native integration.

Validator checks quote existence, history bounds/types, allowed finding categories, and requires exact draft preservation when no finding is reported. Missing evidence may be labeled an unsupported-fact suspicion, never a grounded contradiction. These checks do not establish semantic correctness or whether an assistant source was actually approved; external review still required.

## Result: do not promote

| Group | Valid complete reviews | Outcome |
|---|---|---|
|6recorded drafts |2/6 | Both returned no findings and preserved their drafts exactly |
| Remaining4recorded drafts |0/4 | `incomplete_or_empty_response`; no valid correction to assess |
| Correct synthetic control |1/1 | Preserved unchanged |
| Injected-bad synthetic control |1/1 | Five injected problems identified with existing draft excerpts; corrected fields/key and marked clock reset unknown |

Thus this run demonstrates correction of a conspicuous injected example, **not successful correction of the problematic recorded drafts**. Do not claim a real-error correction rate from synthetic controls. Failure reason comes from the existing Client's combined incomplete-or-empty response check; it does not retain enough metadata to separate truncation from empty replies, so no assertion about which budget/finish reason caused each failure.

Successful additional calls took approximately35–155seconds, including controls; failed calls have no successful completion metadata. These are observed latencies for this configured max-effort setup, not general product performance benchmarks. This is too unreliable and costly to enable as an unconditional pre-final step. It also cannot justify fail-closing user answers or silently replacing them with a verifier failure.

## Delivery

No runtime hook, controller state, session store, production persona or mandatory final-answer gate added. No commit/push. Script `verify_final_design.py`, validation tests and evidence retained under ignored results/final-verifier-20261026, including the full predeclared8-job input cohort.21offline tests passed; Python compilation/diff checks passed; configured credential absent from saved JSON. No independent blind quality judge or held-out fixture validation. Only consider another intervention if it can meet both correction and overhead/availability criteria; this full-rewrite max-effort reviewer does not.
