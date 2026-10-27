# Current-version native retest and conclusion audit — 2026-10-27

## Provenance

Branch release/v5.2.5, build source HEAD `60ef0b6520e6b9007901d75024424426a7a0e823`. Ran `cargo build --release --locked -p atomcode --bin atomcode` successfully (6m12s including waiting for another build). Default features, not the other concurrently built codingplan-crypto feature variant. Existing private crypto-overlay/Cargo.lock changes were preserved, not reset or committed. Runtime/product files were clean at build/freeze; dirty-file list saved in provenance.

Copied executable immediately to `/tmp/atomcode-current-regression-60ef0b652/atomcode` and verified source/copy hashes match. Reported version `atomcode 5.2.4 (60ef0b652)`; SHA256 `d65ee400a2335ab81d460a9cb9a72838486e1981f180be5d3276b65fdca05521`. All3test manifests match this hash. The package version string remains5.2.4; no version/release policy change was made. Build source HEAD stayed unchanged through compilation. Tests characterize this frozen default-feature build, not later working-tree changes.

Previously most native results used `6085861aa+dirty`; those results do not automatically cover this build. Old artifacts/binary were kept. No randomized simultaneous old/new comparison or performance-improvement claim.

## Predeclared native regressions

| Path | Execution | Targeted semantic/artifact review |
|---|---|---|
| Two native compactions, then same-session daemon restart/resume |9user turns and2compacted facts complete | Updated8/6columns, four-column key, zero-valid rule, distinct IDs, accepted rename and rejected proposals retained in second anchor and final replies |
| Payment document rename + explicit reversal |2turns complete | Both final files exactly match requested substitution/deletion, including all unaffected text |
| Native six-turn rename-only conversation |6turns complete | Final answer retains key meanings, names and query-only Duration, **but turn3contains factual drift** |

Native compaction uses `/live/compact` to CodingRuntime, not a summary simulation. Second summary request contains `<previous-summary>`, and both native summary requests retain max_tokens16000. After second compact, original BootId/zero user message is absent as a standalone message while the updated anchor is present. Two snapshots identify the same session across daemon restart. On resume a recent final answer is also retained, so it is not an anchor-only recall test.

These are1sample per path/task, synthetic inputs, and assistant targeted review—not broad correctness, independent blind review, SQL execution, real-user-session reproduction, overflow/fallback/cancel coverage, or all-model validation.

## Concrete current-version failure

Native rename-only first user request explicitly says both tables independently retain DeviceNo, PipingNo and TubeSeq. Turn3assistant instead says:

> 事件流水没有解析列（只有原始报文）

That contradicts the accepted two-table identifier fields. Same turn adds confirmation about preserving Duration naming and compatibility aliases. Some implications may be reasonable to discuss, but the identifier assertion is an actual factual drift. Later user scope restrictions lead back to a final answer listing those identifier fields correctly. Final-only scoring would hide the intermediate defect and the user's additional correction effort.

The rename run records native events, all user inputs/visible answers and request metadata, but not assembled request bodies; do not claim this run alone proves which prior messages reached the provider at turn3. The compaction run does capture sanitized assembled requests. Earlier other-path captures show facts can be present despite generated drift, but that is not byte-level proof for every current conversation.

## Audit of previous conclusions

- **Keep:** unsupported facts and unrequested columns were observed in saved model outputs. These are real output errors, even if not deterministic on a fresh run. Current native intermediate drift confirms the broad problem still exists.
- **Narrow:** successful native compaction samples show fact preservation on those scenarios; they do not prove all compaction/history handling correct or identify the original missing session's cause.
- **Narrow:** approved-document/minimal-edit tests have checkable benefits for opt-in local revisions, with explicit unknowns and edit authorization; not equivalent to discussion-only design tasks.
- **Narrow:** previous four-agent comparisons mix different native tools/protocols/catalog behavior and installed versions; no intelligence ranking or universal quality advantage follows.
- **Keep rejection:** evaluated prompt additions and same-model reviewers did not establish reliable benefit; failures and false edits remain recorded. Tool validation passing is not semantic correctness.
- **Correct scoring practice:** review intermediate turns as well as the final answer. All17user turns finished, but the suite is not17semantic passes.

## Delivery

Only evaluation entrypoints changed to accept an explicit binary; production prompts/runtime/storage/approval logic were not changed.26offline tool tests, Python compilation and diff checks passed. Build succeeded; this is not a new workspace-all-targets or full Rust-test-suite claim. Configured credential absent from saved JSON. No commit/push.

Provenance/plan/verification are in ignored `results/current-60ef0b652-*.json`; native artifacts in corresponding compact/documents/rename directories. The original user session remains unavailable. Further repair needs a narrowly demonstrated intervention against intermediate-turn drift; a successful final answer must not be reported as a fix.
