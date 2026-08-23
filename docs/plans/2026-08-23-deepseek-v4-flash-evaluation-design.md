# DeepSeek V4 Flash Paired Evaluation Design

## Goal

Compare `AtomGit-deepseek-v4-flash` (wire model `deepseek-v4-flash`) and
`volcengine/deepseek-v4-flash` (wire model `ep-20260822184526-dbhzk`) at both the
raw-model and AtomCode coding-agent layers. Runs are paired and concurrent; Codex
blind-judges qualitative results and writes the final report.

## Architectural boundary

The evaluator is an external harness over AtomCode's existing headless CLI. Every
candidate run owns an independent process, `ATOMCODE_HOME`, session, and writable
fixture. It selects a configured model through `--provider`; it does not add a
second live-agent owner, reload providers inside a live runtime, or change kernel,
coding-runtime, provider, session, or persistence contracts.

```text
case + immutable fixture
    |-- paired launch --> official DS --> isolated AtomCode runtime --> artifacts
    `-- paired launch --> Volcano DS  --> isolated AtomCode runtime --> artifacts
                                                              |
                                              verify + anonymize
                                                              |
                                              Codex blind judge
                                                              |
                                              statistics + report
```

## Pairing and concurrency

- A pair always submits the same case to both candidates at nearly the same time.
- The quick suite uses two pairs concurrently (four AtomCode processes maximum).
- Stress stages use 1, 4, then 8 pairs (2, 8, and 16 requests).
- Candidate launch order is randomized per pair. The harness records monotonic
  start/end timestamps; a start skew above 500 ms marks a pair non-strict.
- Each repetition receives a fresh session and writable fixture. Agent cases use
  two independent copies or worktrees rooted at the same commit.
- One candidate's failure never cancels its peer. Each accepted run records a
  terminal outcome: success, timeout, cancelled, provider error, rate limited,
  empty output, launch error, or verification failure.

## Quick suite

The raw-model tier has 20 cases: four code-understanding/debugging, four logic or
algorithm, four code-generation, three instruction-following, three long-context,
and two tool-schema cases. The AtomCode tier has eight repository-backed cases:
two local bug fixes, two cross-file features, one diagnosis-only task, one
behavior-preserving refactor, one long-context task, and one misleading-legacy-path
task. Each case runs three times per candidate.

Every case contains immutable instructions and an explicit verifier. Machine
checks are authoritative for compilation, tests, required/forbidden files, output
schema, and exact answers. Human-like quality dimensions are judged only after
candidate identities are replaced with randomized A/B labels.

## Measurements

Capability is scored on correctness (45%), code quality (20%), instruction
following (15%), agent execution quality (10%), and Codex blind assessment (10%).
Stability remains a separate result: success and first-attempt success rates,
P50/P90/P95 latency, retries, 429/5xx/transport/stream/empty-response failures,
truncation, invalid or repeated tool calls, token usage, and score variance.
Cache efficiency is reported from AtomCode's provider usage as cached prompt tokens
divided by prompt tokens, including mean/P50/P95 and cold-versus-repeat cohorts.

AtomCode's existing retries are part of the end-to-end result, but the harness
distinguishes first-request success from eventual success and records retry cost.
A capability lead requires at least five points and a paired bootstrap 95%
confidence interval excluding zero. A stability difference is material at three
percentage points of success rate or 20% P95 latency. Otherwise the report says
the candidates are comparable under the tested workload.

## Codex evaluation

Codex receives the case, rubric, verifier output, and anonymized A/B artifacts. It
does not receive endpoint or provider identity. A/B order is randomized. Each
judgment is emitted as validated JSON with a winner, per-dimension scores,
evidence, critical failures, and confidence. Important or conflicting cases are
rejudged in a fresh context; disagreement becomes `needs_review`, not a forced
winner.

A separate fresh Codex invocation reads aggregate statistics and traceable sample
evidence, then writes the Markdown report. The report includes capability,
latency, stability, task-specific strengths, limitations, and a deployment
recommendation. Machine results cannot be overridden by qualitative judgment.
Any explanation of gateway behavior is labeled as inference.

## Reproducibility and security

The run manifest records AtomCode commit, binary version, config fingerprint,
selection IDs, expected wire-model identifiers, Codex version/model, case hashes,
concurrency, timeouts, retry policy, timestamps, and random seed. Startup must
confirm that each selection resolves to the expected account/model; fallback to a
default model is an infrastructure failure.

API keys, OAuth tokens, authorization headers, and full private configuration are
never copied into result artifacts. Raw output is scrubbed before aggregation.
The report keeps links to local evidence without embedding secrets.
