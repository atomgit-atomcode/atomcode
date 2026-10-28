# High vs max effort — fixed-input comparison

Eight direct model calls: two synthetic fixed histories × two repetitions × requested efforthigh/max. Same frozen rendered persona, loaded-skill replay, messages, endpoint, model and max_tokens32768. Only requested effort differs within a case. Input hashes and randomized schedule preserved under ignored results/effort-controls-20261026. Gateway does not attest actual compute/weights or that the setting has identical semantics over time. No native runtime or config modification.

All8requests returned stop and the expected DeepSeek model. Targeted assistant review is not independent blind adjudication or a full SQL audit.

| Case | high seconds (rep1/2) | max seconds (rep1/2) | Targeted result |
|---|---|---|---|
| New BootId evidence |25.8/19.2 |23.6/33.9 | All retain four-column identity; unverified clock restrictions/inferences still occur in both arms |
| Rename after rejected proposal |10.5/10.2 |9.6/7.2 | high replies preserve five-column detail; both max replies add UpdatedAt |

The rename error is a concrete contract violation, not a keyword-only flag: approved detail inventory contains DeviceNo, PipingNo, TubeSeq, Photo1Ms, Photo2Ms; user accepts that inventory and later requests only the two time-column renames. max replies include a sixth physical SQL column (`updated_at` or `UpdatedAt`), and one says it only renamed fields. Snake_case conversion is not the error; adding an unrequested column is. Both high replies keep the corresponding five columns. This supplies a reproducible observed scope-creep counterexample, not proof that high always preserves scope or max is generally worse.

High is not a fact-drift fix: one high answer says DeviceMs is measured within the current boot because it probably resets. Input only establishes TubeSeq reset. Other statements choose conservative policies; those must not be conflated with asserted protocol facts. Likewise, introduced aggregation algorithms and chosen SQL dialects are not automatically validated by correct field identities.

No uniform latency winner, no significance estimate from two samples per cell, and no pooling with earlier cohorts. Request timing includes provider service conditions/cache effects. Model reasoning counts in usage are metadata, not saved reasoning text. No default setting or user config change is justified from this pilot.

The concrete rename violation is frozen separately in rename-scope-regression.json as test data with explicit source history and allowed column inventories, for a future narrowly targeted intervention. This is a known failing model-output fixture, not a passing semantic test.26offline tool tests, Python compilation and diff checks passed; credentials absent from saved JSON. Production unchanged; no commit/push.
