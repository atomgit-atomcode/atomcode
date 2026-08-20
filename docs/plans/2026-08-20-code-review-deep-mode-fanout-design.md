# Built-in code-review deep mode (dimension fan-out) — design

Date: 2026-08-20
Status: Approved (design), pending implementation plan
Scope crate: `atomcode-review` (L2), with a small touch in `atomcode-tuix`
command wiring.

## Problem

The built-in `code_review` tool (`atomcode-review/src/review_tool.rs`) runs a
**single** read-only reviewer sub-agent: it computes the scoped diff, builds one
task (annotated diff + per-language rules + deterministic impact plan), spins up
ONE agent via `build_review_agent_with`, runs it to completion, collects
findings from a single `report_finding` sink, then filters/sorts/renders.

It is already sophisticated (deterministic impact plan, round-budget pressure
hook, scope preflight/confirmation, tool-pinning to changed files, careful
persona), but it has **one lens and one pass**. A single reviewer trades recall
for cost: distinct concerns (correctness vs security vs performance vs
tests/contracts) compete for the same round budget, and there is no adversarial
second look.

atomcode already has the orchestration primitives to fan out
(`atomcode-capabilities` task/team: `JoinSet` + semaphore, `reviewer`/
`security`/`performance` roles, explore/worker permissions), and
`atomcode-review` already exposes everything a fan-out needs on its own:
`build_review_agent_with` returns `(agent, report_sink)`, and
`ReviewAgentConfig::with_persona_append` lets each agent carry a specialized
lens without touching the base persona.

## Goals

- Add an **opt-in** deep review mode that fans out one reviewer per concern
  dimension, runs them concurrently, and merges/dedups their findings — raising
  recall over the single-agent default.
- Keep the current single-agent path as the **default** (cost-sensitive: the
  headless engineering service drives review at scale on cheap models).
- Reuse existing review machinery per dimension (impact plan, round budget,
  scope preflight, rules, render) — no parallel re-implementation.

## Non-goals (v1)

- **No adversarial verify pass** in v1. It is reserved for phase 2 (see below);
  the orchestration leaves a hook for it.
- No change to the single-agent default behavior or its output.
- No new dependency on the `task`/team tool: `atomcode-review` fans out on its
  own via `build_review_agent_with`, which is simpler and keeps the review
  crate self-contained.
- No config-file default knob in v1 (invocation-scoped only). Can be added
  later without redesign.

## User-facing interface

- Slash command: `/review deep [scope]`. The leading `deep` keyword is parsed
  in the `/review` arg mapping (`atomcode-tuix/src/event_loop/commands.rs`) and
  becomes the tool's `depth` argument. `/review [scope]` (no `deep`) is
  unchanged.
- Tool argument: `code_review` `Args` gains `depth: Option<String>` with values
  `"single"` (default when absent) and `"deep"`. An unknown/empty value falls
  back to `"single"`.
- The `code_review` tool description and the reviewer persona note the `depth`
  option so the model (and the headless `gitcode-assist-service`, which can pass
  the arg directly) can request it.

## Architecture

`ReviewTool::execute()` keeps its front half unchanged:

1. Parse args (now including `depth`).
2. Compute the scoped diff in the live working dir.
3. Empty diff → early "nothing to review".
4. **Scope preflight/confirmation runs BEFORE any fan-out.** A large diff still
   requires the confirmation token; fan-out never multiplies an unconfirmed
   large scope.
5. Build the shared task inputs: annotated diff, changed-file list, per-language
   rules, deterministic impact plan.

Then dispatch on `depth`:

- `single` (default): today's exact path — one `build_review_agent_with`, one
  `run_to_completion`, one sink. **Zero behavior change.**
- `deep`: hand the shared task inputs to a new `fanout` orchestrator.

### Fan-out orchestrator (`atomcode-review/src/fanout.rs`)

New module, three responsibilities, each independently testable:

1. **Dimension table** — a fixed list of lenses, each `{ id, display, lens }`
   where `lens` is the `persona_append` string that biases an otherwise-standard
   reviewer:
   - `correctness` — logic, edge cases, concurrency, error handling, regressions
   - `security` — injection, authz, secrets, supply-chain (deps, CI, config)
   - `performance` — hot paths, allocations, blocking calls, N+1
   - `tests_contracts` — test coverage of the change, API/contract consistency,
     one-sided cross-file divergences
   Each reviewer sees the **full** diff through its lens; overlap between
   dimensions is expected and resolved by dedup (favor recall).

2. **Orchestration** — for each dimension, build a review agent
   (`build_review_agent_with` with the base config plus that dimension's
   `with_persona_append`, its own report sink) and run them concurrently under a
   concurrency cap (= number of dimensions, i.e. small), honoring the host
   turn's cancellation. The single "run one dimension to completion" step is
   factored behind an injectable function/trait so tests can feed canned
   findings without a live model.

3. **Merge/dedup** — a pure `merge_findings(Vec<Vec<Finding>>) -> Vec<Finding>`
   that collapses near-duplicates keyed on **(normalized file path, overlapping
   line range, similar title)**. On collision it keeps the higher
   priority/confidence finding and accumulates the set of contributing dimension
   ids as a tag.

### Data flow

```
args(depth=deep)
  → git_diff (scoped)                     [unchanged]
  → scope preflight / confirmation        [unchanged, BEFORE fan-out]
  → shared task: annotated diff + rules + impact plan   [unchanged builders]
  → fanout:
       for d in DIMENSIONS (concurrent, capped, cancellable):
         agent_d = build_review_agent_with(cfg.with_persona_append(d.lens), provider)
         run_to_completion(agent_d, task)  → sink_d.findings()
       merge_findings([sink_d.findings() ...])  (dedup + dimension tags)
  → retain changed-file paths              [unchanged sort/filter]
  → sort_findings                          [unchanged]
  → render (findings + per-dimension summary line)
```

## Error handling

- A dimension agent that errors or is cancelled **does not fail the whole
  review**: whatever it already reported still merges in. Rendering notes how
  many of N dimensions completed (mirroring the existing
  `render_incomplete_review` style) and which failed.
- The review is a hard failure only if **all** dimensions fail with zero merged
  findings.
- User cancellation (`ctx.cancel`) propagates by dropping the in-flight futures,
  which cancels the child agents (same mechanism the single path relies on).

## Reporting

Reuse `sort_findings` and `render_findings`. Add a compact per-dimension summary
line (which dimensions completed, how many findings each contributed, dedup
count) so the user can see coverage. Merged findings carry their contributing
dimension tag(s).

## Cost controls

- Deep mode is strictly opt-in (`/review deep` / `depth:"deep"`).
- Concurrency is capped at the (small) dimension count.
- Each dimension reuses the same review model and the existing round-budget and
  impact-plan machinery.
- Scope preflight gates large diffs before fan-out, so deep mode cannot silently
  multiply an unconfirmed huge scope.

## Code organization / units

- `atomcode-review/src/fanout.rs` (new): dimension table, `merge_findings` (pure),
  orchestration with an injectable per-dimension run step. Self-contained and
  unit-testable.
- `atomcode-review/src/review_tool.rs`: add `depth` to `Args`, dispatch to
  `fanout` when `deep`; front-half diff/preflight/task-building unchanged and
  shared by both paths.
- `atomcode-tuix/src/event_loop/commands.rs`: parse leading `deep` keyword in the
  `/review` arg mapping → set `depth` on the synthesized tool request/prompt.
- `atomcode-review/src/lib.rs`: export the new fan-out entry point as needed.

## Testing (TDD)

Pure / deterministic first:

- `merge_findings`: dedup by file + overlapping line range + similar title;
  collision keeps higher priority/confidence; dimension tags accumulate; distinct
  findings are preserved.
- Dimension table: ids/lenses present and stable; every lens is a non-empty
  append that does not replace the base persona.
- `/review` arg parsing: `deep [scope]` → `depth=deep` + correct scope; bare
  scope → `depth=single`; unknown depth value → single.
- Deep render: per-dimension summary line; partial-failure render (x/N
  dimensions completed).

Orchestration seam:

- Inject canned per-dimension findings (no live model) and assert the full
  merge → filter → sort → render pipeline, including partial-failure and
  all-fail paths.

The existing single-agent tests must remain green unchanged (default path
untouched).

## Phase 2 (reserved, not in v1)

Adversarial verify pass over the merged findings: one refute-biased skeptic per
finding (or per cluster), majority-refute drops it. Surface as an extended
`depth` value (e.g. `"deep+verify"`); the orchestrator leaves a post-merge hook
so this slots in without restructuring.
