# Prior-version difference review — 2026-10-27

## Question

Can discussion-only revision stay narrower when the user asks to compare the actual previous version, instead of a general omission audit? This is a user-workflow/output-contract comparison, not a hidden product prompt change or proof that the original user was at fault.

Baseline histories come from reference-tail-controls, without the candidate tail. Candidate changes only the relevant latest real user request; system/persona, earlier history and reminders remain identical within each case. For omission audit it asks only for truly missing name changes relative to the previous text and disallows inventing existing indexes/interfaces. For BootId it asks for a necessary-change list followed by final tables. A payment rename control likewise asks for differences then final tables, retaining original unaffected fields. Three cases ×2arms ×2repetitions =12completed direct requests, same DeepSeek endpoint/max effort/cap32768. Personas differ between cases as in the source datasets, so comparisons are within case.

## Targeted results

- Omission audit: both candidate answers directly say no missing name substitutions and stay close to the actual prior text. Both baseline answers expand into indexes, DTOs, migration/reference checklists. Not all hypothetical baseline suggestions are errors, but they are broader than the local discussion. Candidate does not need an extra reviewer call.
- Candidate did not obviously block legitimate BootId addition: both final keys include the four required columns. However one candidate **also adds an unrequested detail-table Id**, turning the approved5-column inventory plus BootId into7columns rather than6. A correct key does not make the entire revision correct. This is another concrete scope-expansion counterexample even with a difference-first answer.
- Both payment candidate final tables retain the intended8/6column inventories and accepted PaidCents rename. One says “收款事件流水六项” in its preceding summary while listing8columns later. That is an internal accounting inconsistency, not actual field deletion.

Thus difference-based wording has a useful **narrow-convergence signal**, but does not reliably preserve facts or prevent additions across the full final design. No numeric intelligence score, semantic pass rate, statistical significance or general model-quality claim. No independent blind review or native runtime integration. This was one predeclared batch; no selective reruns or hidden discarded failures.

## Decision

Keep as an optional way to ask for a scoped discussion revision, not a global automatic repair. Example: “以上一版为基准，只列本次必要差异；未改变的内容沿用原版。” It does not authorize an agent to change the user's request automatically, and is not a reason to blame short prompts. Explicitly maintained design documents plus native local edits remain the stronger tested preservation route, under user authorization.

No production changes, commit or push.26offline tests, Python compilation and diff checks passed. Credentials absent from saved JSON. Inputs, full visible replies, schedule and exact counterexample quotes are retained under ignored results/diff-review-controls-20261027.
