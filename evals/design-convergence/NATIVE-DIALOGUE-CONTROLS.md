# True native multi-turn profile comparison — 2026-10-27

## Purpose and execution

Previous model-profile comparison quoted history into one prompt. This follow-up uses genuinely persistent native conversations: one5-turn dialogue per configured profile, DeepSeek and GLM, same installed original CLI `5.2.4(dfa998f4)` and frozen executable hash as NATIVE-MODEL-CONTROLS.md. Existing native provider auth, no-tools, no telemetry/update; different gateways/model-conditioned persona/defaults remain confounders. Not current HEAD or intrinsic-model isolation.

Flow: explicitly approved7/5-column schema and unknown protocol facts → T1/T2 rename → omission audit → new BootId/reset/zero-valid evidence → another time-column rename and final tables. Actual prior assistant replies, not a fixed synthesized transcript, are resumed each turn with --continue. Both profiles see the same scripted user messages but their generated history naturally differs.

Two unique temporary projects were pinned to exclusively created16hex test buckets in the native session root. Owner markers prevent deleting pre-existing buckets. One.index/session ID was maintained per profile; event metadata contains5user_message and5turn_end facts each. Both5-turn runs completed; no120sper-turn test timeout. After recording only safe metadata and visible answers, both exact owned test buckets were removed. No existing sessions/config were reset or copied; native authentication may do normal token maintenance.

## Targeted observations

| Stage | DeepSeek | GLM |
|---|---|---|
| Approved inventory + first rename |7/5columns retained |7/5columns retained |
| Omission audit | Keeps fields; adds discussion suggestions/follow-up | Directly closes audit within discussion |
| BootId evidence |8/6columns, four-column key,0valid; DeviceMs reset stays unknown | Same target preservation |
| Final rename | EnterMs/ExitMs retained with BootId and existing rules | Same target preservation |

No original identifier-merger or device-sequence misdefinition reproduced in these dialogues. This is not10semantic passes: prose has caveats/imprecision and implementation details were not executed as SQL.

DeepSeek first reply says separate device/pipe fields “不能合并成一个主键”. That is imprecise: separate columns can form a composite key. Later association consistently uses multiple columns, so do not score this as a proven physical-column merger. It also adds optional questions/checklists. GLM is more concise in this one dialogue; no general superiority or lower error-rate estimate from one run.

DeepSeek turn3says the Duration formula was not explicitly confirmed. The fixture indeed only says query-computed/no column, not an exact subtraction formula; this is **not evidence of forgetting an explicitly supplied formula**. Reviewer must not import requirements from earlier different fixtures. Later EnterMs/ExitMs still refer to device milliseconds; explicit arithmetic correctness is not fully tested here.

Observed per-turn timings are recorded in verification.json; GLM's first turn was slower, later turns completed. Times include startup/resume/auth/provider conditions. Do not pool with previous quoted-history timeouts to claim a new reliability rate or attribute differences only to model family.

## Decision

Native authenticated GLM is now a real available comparison option, and its completed dialogue gives a small positive convergence signal. This does not justify changing defaults or promise that a model swap fixes the unavailable original session. Both profiles preserve the scripted facts here. Strong explicit inventories and no-tools condition narrow the task; broad natural design discussions remain untested.

No production changes, commit/push.26offline tests, Python compilation and diff checks pass. Static configured api_key values>=16characters absent from saved JSON; native stderr withheld, provider reasoning removed. Artifacts under ignored results/native-dialogue-controls-20261027. Existing opt-in approved-document/minimal-edit workflow remains the more directly verifiable preservation route.
