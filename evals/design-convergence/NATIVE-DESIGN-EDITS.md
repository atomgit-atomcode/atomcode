# Native minimal edits to an approved design

## Intervention and boundary

Instead of generating another entire free-form answer or adding a second model reviewer, ask the native agent to read an already approved `design.md` and use existing `edit_file` to make an explicitly authorized local revision. Test project and product home are temporary; no repository product code or real design file is changed. Agent has auto approval only in this isolated test, with shell/write tools denied; edits are applied to a synthetic file.

Approved artifact:7/5-column inventories, field meanings, separate device/pipe IDs, pipe-local sequence, append-only repeated/out-of-order events, query-only Duration. Unlike the earliest user complaint, it also explicitly marks DeviceMs cross-boot behavior unknown. This is a clearer supplied contract, not proof that the user's original prompt was insufficient. File-edit authorization, artifact presentation, explicit unknowns and requested minimal-diff output all differ from earlier discussion-only fixtures. This is a workflow pilot, not a one-variable causal benchmark.

Frozen native binary:5.2.4(6085861aa+dirty), exact hash/version in manifests. Same configured DeepSeek model, requested max effort, output cap32768. Original4trials: two rename tasks and two BootId tasks, max_rounds6. No extra verifier model call. Existing native read/edit tools are exercised.

## Results

- Rename2/2: native turns completed, file text equals exact global Photo1Ms→EnterMs and Photo2Ms→ExitMs substitution, every other byte unchanged. Around11–14seconds.
- BootId2/2 artifacts: add exactly one BootId line in each table, change the association to four columns and add the zero-valid/reset rule. All other original text remains byte-identical after normalizing only these four requested line changes. DeviceMs-unknown statement remains intact.
- BootId execution: one native turn completed; one reached MaxRounds and exited1after editing the file correctly. The failed trial used extra directory/glob discovery before the edits. It is retained as a failed execution, not counted as task success just because the artifact was correct.
- Separate budget control: rerun one fresh BootId trial with max_rounds12. Completed in about26seconds with the same minimal artifact changes. This is a different sample/budget, not a retroactive replacement of the original failure or proof a larger limit guarantees completion.

Total evidence: initial cohort3/4native completions with4/4targeted artifact checks; separate control1/1native completion and artifact check. Do not pool these into a single-budget success rate. Original successful/failing artifacts and per-trial diffs are preserved under ignored results/native-design-edits-20261026 and results/native-design-edits-round-control-20261026. No claim that all prose or future edits are correct.

## Practical implication

This is the first tested route here with a directly checkable preservation advantage: an existing approved artifact can remain literally unchanged outside the requested revision, without a second reviewer rewriting the answer. It is already supported by native tools; no new runtime owner, state store or mandatory reviewer is needed. For a user who explicitly wants a design artifact, it is a promising existing workflow: save the approved version, then review local diffs.

It is **not** a fix for all conversational fact drift. The original complaint was discussion-only, and the agent must not silently create files when the user says not to. No automatic design-phase detection or compulsory file workflow is justified by these5small trials. It must remain opt-in, and larger/held-out real designs are untested.

26offline tests passed, script compilation/diff checks passed; configured credential absent from saved JSON. No production changes, commit or push. The tooling preserves a true distinction between artifact correctness, terminal success, provider failures and sampled assistant explanations.

## New-domain and legitimate-reversal controls

Added `payment-edit-fixture.json`, prepared before testing. This is a synthetic store/payment event-and-receipt design, still a related two-table structure; not a real held-out production project or broad domain benchmark. It has explicit approved fields and an unresolved EventTimeMs semantic, plus an unrelated maintainer section.

Four native trials (two repeats each), max_rounds12:

- Rename only ChargedCents→PaidCents:2/2native completions and byte-exact requested substitution. All unrelated content preserved.
- Explicitly revoke an old approved choice: remove LegacyNote and its preservation rule:2/2native completions and byte-exact expected two-line deletion. The old approval did not prevent following the later user correction.

Then two additional native trials (one per task) simulate another writer after a read: the relay observer waits until a `read_file` tool result containing the original document reaches the provider, appends an integer-cents maintenance constraint to the actual test file, and lets the model continue from its stale read. In both trials the final file exactly matches the requested edit plus the injected maintenance text, with no unrelated changes; both native turns complete. This tests one controlled interleaving, not arbitrary concurrency, same-fragment conflicting writes, atomic multi-file edits, or thread-safety in general.

All6new trials complete natively, around10–17seconds; all6target artifact comparisons pass. Initial four runs and interleaving runs remain separate cohorts, with frozen binary/model identity in manifests. Exact diffs and injection flags saved under ignored results/native-payment-edits-20261026 and results/native-payment-concurrent-edits-20261026. No extra verifier call.26offline tests pass, Python compilation/diff checks pass, configured credentials absent from saved JSON.

Practical boundary remains unchanged: explicitly maintaining an approved artifact and reviewing small diffs is a usable existing workflow. These fixtures also make unknowns explicit and authorize editing. Do not claim equivalent behavior in discussion-only sessions, force document creation, or call this a production fix for factual hallucinations. No new production code, commit or push.

## Stale edits when another writer already completed the task

New four-trial cohort: two rename and two explicit-note-removal tasks, max_rounds12. Immediately after the first read_file result reaches the provider, the test writer applies the entire desired edit to disk and appends the maintenance note. The model still has its original stale read. This is a controlled already-completed-task race, not arbitrary conflicting user intent or genuinely simultaneous byte writes.

All4native turns complete. All4final files equal the external writer's desired result plus maintenance note. Trace verification finds5edit_file calls total, all rejected with old_string-not-found and no file modification; each run re-reads after its first rejection and recognizes the current target state. Zero successful model edit calls in this cohort: **the external writer, not the model, performed the desired modifications**. The result demonstrates recovery and non-overwrite, not four successful AI-generated patches.

Observed durations about22–31seconds. One final explanation (`revoke-note-2`) adds “需要你确认” and an optional offer to remove the external maintenance section, although the user's edit is complete. This is a residual convergence/communication issue; do not mark all conversational behavior correct because the file survived. Some attempted git-status shell calls were denied by the test permission rules; no claim of perfect tool discipline.

Artifacts and exact event checks retained under ignored results/native-payment-stale-edits-20261026. Every targeted artifact and terminal check passes;26offline tests, Python compilation and diff checks pass. Configured credential absent from saved JSON. Production code unchanged, no commit/push.

The basic opt-in document-edit workflow now has evidence for unchanged-content preservation, legitimate reversal, unrelated external additions and already-completed-task recovery. This is enough to close this narrow pilot rather than claim a general conversational-memory fix. Real user-session reproduction, arbitrary same-fragment conflicts and broad open-ended design tasks remain unverified.
