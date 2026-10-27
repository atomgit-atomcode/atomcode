# Closed-inventory single-generation A/B

Candidate: `closed_inventory_candidate.txt`, a narrow rule preserving approved field inventories and meanings during local revisions, forbidding convenience columns on renames, and explicitly allowing required structure changes from new evidence. No UpdatedAt/TubeSeq/payment identifiers appear in the candidate text. Added only to the frozen system persona; all remaining messages, skill replay, model, endpoint, effort=max and cap32768 match baseline. Input equality checked programmatically.

Fixed batch before execution: observed rename-after-rejected-proposal case, legitimate BootId addition control, and a payment-domain rename control. Two repeats per case/arm =12requests. Each is one generation, not a second reviewer. Model reported the expected DeepSeek identity; all requests completed. No production integration.

## Outcome

Targeted assistant review finds approved field inventories retained in all12replies, with the authorized rename or BootId addition. Therefore baseline and candidate both satisfy this narrow target in this batch; no demonstrated field-preservation gain. The prior two max rename answers that added UpdatedAt remain valid observed counterexamples, but the error is not deterministic: it did not recur in the fresh baseline samples. Do not mix old/new cohorts to claim an intervention effect or keep sampling until a desired result appears.

Candidate still adds unsupported factual claims. Example candidate BootId reply: “同一根试管的两次光电事件必然来自同一次启动”. Input only establishes a reset and non-reused BootId; it does not rule out a reboot between sensors. That answer then asserts adding BootId cannot split a physical tube into two rows. This is outside the narrow field-count target and prevents treating inventory preservation as overall correctness. Other scope, clock and duplicate-aggregation assumptions remain unvalidated; no SQL execution or independent blind judging.

Some replies still add follow-up questions or an offer to create a file despite a final-design request. The candidate is not a proven convergence fix. Observed differences in verbosity/timing are not a controlled quality advantage with two samples per cell.

## Decision

Do not promote this prompt to production. Retain it only as an evaluated candidate. No runtime hook/state, session persistence or product configuration changes; no commit/push.26offline tool tests, script compilation and diff checks passed. Configured credential absent from saved JSON. Raw visible responses, input hashes, seeded schedule and targeted review are in ignored results/inventory-controls-20261026.

Current practical evidence favors the existing **explicitly authorized approved-document + minimal-edit workflow**, which already works without a new mandatory model reviewer. That is a mitigation for a scoped workflow, not a fix for discussion-only factual drift. Further general prompt tuning should not be represented as a validated repair without a reliable baseline failure and measured benefit.
