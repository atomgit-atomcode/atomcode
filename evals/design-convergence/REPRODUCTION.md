# Fixed-history reproduction probes (2026-10-26)

Purpose: attempt to reproduce unwanted workflow restart or loss of confirmed field meanings before proposing another production patch. Original user session is unavailable.

Current source exports the full AtomCode persona using `dump_persona`; `reproduce_fixed_history.py` replays the original brainstorming skill result and synthetic history through the configured same-model endpoint. This is not native runtime execution, actual skill activation, compaction, a real conversation recording, or a full system/tool-schema comparison. Only the persona fragment is production-rendered; the catalog and histories are synthetic.

Three cases, two independent calls each, effort=max, output cap32768:

- brief finalization: earlier complete 7/5-column inventory and user approval; final request is short, without repeating requirements.
- rename after rejected proposal: deliberately insert an assistant suggestion combining two IDs and misnaming TubeSeq; user explicitly rejects it, accepts only renaming time columns; final request does not repeat these facts. This tests rejected-proposal contamination, not spontaneous generation of the initial bad suggestion.
- necessary revision control: new protocol fact includes non-reused BootId, requiring a four-column association despite prior approval.

Execution completion is not semantic approval. Inspect original visible text against fixture constraints. Do not use keyword occurrence to assert correct semantics, and do not treat selected SQL types or an unrequested algorithm as user-confirmed facts. No independent blind review. Inputs, hashes, response model, usage and visible answers are stored in ignored `results/fixed-history-reproduction-20261026`. Model reasoning and credentials are not saved.

## Results

All6 requests completed with stop and actual model `deepseek-v4.1-flash-1017`. Assistant review (not an independent blind assessment):

| Case | Repetitions | Observation |
|---|---|---|
| Brief finalization | 2 | Exact inventories and key meanings retained; no restart of interview |
| Rename after rejected suggestion | 2 | Rejected merge/misnaming did not reappear; only requested time-column names changed |
| Necessary new-evidence revision | 2 | Correct four-column identity and BootId in both tables; both explanations falsely describe the earlier scope as device-wide unique |

The last finding is a repeated explanatory factual drift: prior history only defines TubeSeq within a pipe, but the answers say its earlier scope was “设备内唯一”. Both still retain PipingNo in the key and describe pipe-local meaning in final fields. Do not equate this with the original user's column merge or device-sequence misnaming, and do not label the whole solution incorrect. Two repeats of one fixture are not a population failure rate.

Other unconfirmed additions remain: chosen database types/dialects and aggregation rules. One brief-finalization answer combines AUTO_INCREMENT-style table declarations with ON CONFLICT prose; these outputs have not been executed as SQL. That is outside the main schema-meaning reproduction criterion and is not scored as a tested SQL failure.

In this probe, the approved definition is present in the fixed input sent by the client. There is no compaction or session-history restoration in this path. Thus missing client-side history is not necessary for this particular explanatory drift. This does not establish the root cause of the original user session, or prove runtime context handling has no other problems.

No new production change: generic stage-guidance candidate remains withdrawn. The new fixture and visible quoted scope error provide a narrower target for a subsequent controlled intervention; do not alter runtime ownership or add persistence based on this probe. Offline evaluation tests14 passed; script py_compile and diff checks passed. No Rust code changed this round.

## Scope/skill controls (second batch)

The runner now supports `--suite scope --skill-modes original none --repetitions 2`: three fixtures, two skill conditions, two independent responses per cell (12 completed requests). The same frozen full persona, model, effort and output cap are used. Original condition includes the skill catalog and replayed loaded skill; none omits both. This measures the combined catalog/loaded-skill exposure, not the skill body alone, and does not test actual activation or compaction. All requests returned stop and the expected model.

Fixtures: original uniqueness wording; explicit user confirmation that different pipes may have the same sequence; the explicit case with DeviceNo/PipingNo/TubeSeq/BootId renamed to StationId/LaneId/ItemSeq/EpochId. Identifier replacement preserves history roles and all business sentences. No production intervention is included. Offline tests17 passed, including history preservation, absence of future BootId evidence in prior messages, and exact renamed-control equivalence.

Targeted assistant review:

- In these samples, explicit pipe-local scope and the renamed explicit scope retain the four-column final identity and do not recharacterize the old scope as device-wide unique. This is not evidence that users must repeat constraints or that all explanatory statements are correct.
- The original wording still elicited a historical overstatement: “TubeSeq 不再全局唯一，只在一台设备的一次启动内唯一”. Final tables still carry pipe identity; explanation and structure must be scored separately.
- Unsupported clock claims appear with and without skill exposure. Example loaded-skill answer: “重启后毫秒归零，跨启动不能比大小。” Example no-skill answer: “设备毫秒从启动起算”. Input establishes TubeSeq reset, not DeviceMs reset or its epoch. These are asserted protocol facts, distinct from proposing a conservative same-boot calculation policy.
- The no-skill explicit sample also says the old event table has6columns, though the approved inventory has7. This is an old/new accounting error, not deletion of a final field.

Consequences: remove-skill is not supported as a complete remedy. The test suite now has separate targets for historical factual drift, unsupported new facts, final structure, and necessary revision. No aggregate semantic pass rate or skill-effect claim is assigned by this targeted review. No SQL execution or independent blind review. Inputs and quoted evidence are preserved under ignored results/scope-controls-20261026. Production prompts remain unchanged.

## Narrow factual-boundary A/B

Tested `fact-boundary-candidate.txt` as a system-tail addition to the same frozen persona, against unchanged baseline. The addition asks for a distinction among confirmed facts, deductions and assumptions, forbids inferring another measurement's behavior from one field's change, and advises proceeding without a confirmation round when unknowns do not block the answer. This is an evaluation-only candidate, not production code.

Three scope fixtures × two repetitions × two arms =12completed requests, same original loaded-skill replay, model/max effort/cap32768. Seed20261026 shuffles scheduled jobs; each repetition starts from the same synthetic fixed history, not a continuation of another sampled answer. Input prompts and hashes, job schedule, model metadata and visible responses preserved in ignored results/fact-boundary-ab-20261026. The source commit recorded in manifest identifies the orchestration run, not the older frozen persona's revision; the actual persona content/hash is preserved.

Decision: do not promote. Candidate produced both a useful explicit caveat (“DeviceMs 是否也随重启归零，尚未证实”) and a direct target failure (“重启后它归零或跳变，跨启动比较无意义”). Another candidate reply states as fact that device clocks cannot compare across boots. These go beyond choosing a conservative same-boot pairing policy. Necessary four-column final identities were retained in the reviewed outputs, but neither this nor one good caveat establishes overall correctness. Candidate replies still append confirmation questions; not all are necessarily unnecessary, so no blanket question-failure count is assigned.

Baseline also invents unrelated deployment facts (“上一版尚未落库，无需迁移”), showing this is broader than a single TubeSeq/DeviceMs association. With only two samples per fixture, related fixtures, no independent review and no native runtime/compaction, no statistical quality improvement or regression rate is claimed. Do not pool these A/B responses with earlier cohorts to report a pass rate.

17 offline tests pass, py_compile and diff checks pass. Production persona and skill-first are unchanged; no commit/push. Candidate remains solely as a reproducible rejected experimental input.
