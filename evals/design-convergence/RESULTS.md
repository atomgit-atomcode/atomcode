# Design convergence: 2026-10-25 exploratory results

## Conclusion

The small prompt patch has a concrete positive paired example and fewer
model-rated unnecessary questions in ordinary history. It does **not** establish
a stable overall correctness improvement or resolve the reported incident.
Keep the change reviewable; do not market these results as a verified fix.

## Setup and provenance

- Baseline: current AtomCode source at d7c01642a before this patch, **not** the
  reporter's 5.2.4/dfa998f4 build.
- Selection: taotoken/deepseek-v4.1-flash-1017. Every completed execution request
  reported deepseek-v4.1-flash-1017. A stable alias is not an immutable backend.
- Configured execution effort: max. Both arms used output cap 8192, request
  timeout 300s, identical six cases and six user turns, three repetitions/arm.
- Production-rendered persona was captured before/after through dump_persona.
  Source constants supplied the summarization directives. The only snapshot
  differences are the new persona section and summary-template rules.
- Random seed 20261025 shuffles trial order; it does not seed model decoding.
- Ordinary phase: 36/36 complete, 216 successful requests.
- Controlled summary phase: summarize after three turns and continue three more;
  35/36 complete. One candidate local-change trial (trial-012) returned an
  incomplete/empty response at the summary call after three successful answers.
  It is an invalid trial, never a pass. Successful requests in this phase: 248,
  including the completed calls of the failed trial. Failed-call usage is unknown.
- Total primary execution: 72 scheduled trials, 71 complete, 464 successful
  requests. Preflight and reviewer calls are separate overhead.
- The 8192 summary cap is **below production's 16000**; this is a bounded stress
  experiment, not validation of production summary budgets. The exact reason of
  the incomplete/empty response was not captured; do not assume a specific cap
  or network cause. No automatic retry changed the formal sample.
- No tools, skills, memory, project instructions, real session selection, actual
  compaction trigger/retained-prefix policy, or Windows UI were exercised.
  Historical semantic drift here cannot establish that production compaction
  caused the reporter's incident.

## Blind model-assisted screening

The reviewer received anonymous transcripts and rubrics without the arm manifest.
It used the same model family at high effort, output cap 16384. This is **not**
independent human validation. Each flag must contain an exact quote from the
claimed answer/summary; invalid or unfinished reviews are not passes.

| Phase / arm | Complete trials | Usable model reviews | Review failures | Model-flagged critical trials | Model-rated unnecessary questions | Model-rated unjustified redesigns |
|---|---:|---:|---:|---:|---:|---:|
| ordinary / baseline | 18 | 17 | 1 | 8 | 10 | 2 |
| ordinary / candidate | 18 | 17 | 1 | 8 | 3 | 3 |
| summary / baseline | 18 | 18 | 0 | 7 | 6 | 5 |
| summary / candidate | 17 | 14 | 3 | 4 | 6 | 1 |

The summary candidate also has one execution failure, so its 14 usable reviews
must not be compared as though all 18 trials passed. Two earlier preflight
review attempts also failed and are excluded from the formal ratings.

These counts are screening signals, **not adjudicated error rates**. The reviewer
sometimes introduced unstated requirements: e.g. treating NOT NULL as necessarily
violating raw capture because malformed packets might exist, or treating one
transaction as necessarily losing events without an acknowledgement/retry
contract. It also missed the concrete TubeSeq semantic error below. Do not use
these model scores alone as a release gate or claim statistical significance
from three repetitions.

## Concrete paired observation, checked against the raw outputs

Case necessary-complexity, repetition 3, summary phase:

- Baseline trial-024 summary retained that TubeSeq is local to one pipe and is
  not a device number, but omitted the positive definition “test-tube sequence”.
  The final schema said:
  `| tube_seq | INT NOT NULL | 管号，仅在同一条管道内有效，不是设备序号 |`
  Calling TubeSeq a pipe number contradicts the supplied business definition.
- Candidate trial-001 summary explicitly retained “TubeSeq 是管道内试管序号，用于关联同一根试管的光电1、光电2事件”. Its final schema retained that meaning.
- The baseline error was recorded in results/spot-checks.json before its arm was
  revealed to the root assistant. This was an **assistant spot-check**, not a
  human review. The automated reviewer did not flag that field-meaning error.

This is evidence of a possible benefit, not proof that the patch prevents drift.
There is no authoritative user session available to replay. Some tested outputs
still propose deduplicating an event log despite the requested original-event
retention. A shorter or more stable-looking answer is not necessarily correct.

## Cost observations

Ordinary phase (108 successful requests per arm):

| Arm | Prompt tokens (includes overlapping history) | Cached prompt tokens | Completion tokens (includes reasoning) | Median summed request seconds/trial |
|---|---:|---:|---:|---:|
| baseline | 828247 | 773760 | 173475 | 70.81 |
| candidate | 848200 | 795520 | 170845 | 71.41 |

There is no meaningful latency improvement in these observations. Candidate
prompt size increased. The schedule fixes six/seven requests per successful
trial, so this experiment does **not** measure fewer real-world discussion rounds.
Phases overlap under concurrency; cache and timing observations are descriptive,
not causal savings or billing estimates.

## Artifacts and verification

- results/{baseline,candidate}-prompts.json: immutable prompt snapshots and hashes.
- results/{ordinary,summary}/: anonymous transcripts, private arm manifests,
  per-request model/usage/timing, raw model ratings and aggregate summary.json.
- results/spot-checks.json: the version-blind assistant observation above.
- Results stay ignored by Git. Credentials, endpoint URLs and reasoning bodies
  were not copied into the artifacts. Reviewer costs are separate from execution.
- Python runner/aggregation/evidence validation: 7 offline tests passed.
- Affected Rust libraries: 2605 tests passed, 9 skipped.
- bash gates/compile.sh, cargo fmt --all -- --check and diff checks passed.
- No full-runtime skill interaction, actual-user replay or independent human
  annotation was performed. Final acceptance needs those checks; a larger system
  for decision persistence is not justified by this experiment.

## Follow-up: 2026-10-26

[Flag-by-flag adjudication](ADJUDICATION.md) reviewed all 41 original critical
flags: 5 confirmed issues (4 trials), 6 unsupported critical labels, and 30
ambiguous/unverified flags; it also retains one confirmed missed field-meaning
error. This assistant follow-up is not an independent human review or a complete
regrading of all outputs. The raw model scores above remain historical screening
counts, not measured error rates. Review protocol v2 adds temporal user-requirement
evidence checks for future judging; it was not retroactively applied to v1 data.

## 后续决定：分离干预后撤回生产候选

见 [ABLATION.md](ABLATION.md)。18 次分离试验未显示稳定增量收益，生产 persona 与摘要模板候选均撤回；以上历史对比保留为实验记录，不代表当前待发布实现。新旧案例、输出预算不同，不合并统计。
