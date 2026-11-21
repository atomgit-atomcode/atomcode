> Status: the uncommitted OpenAI `qwen_prompt_cache` experiment, diagnostic module
> and live fixture were withdrawn on 2026-11-16. The following is a historical
> experiment record, not instructions for an available configuration option.
> The separate Anthropic stable-cache-boundary fix remains committed.

# Qwen cache live experiment — 2026-11-16

This was an explicitly authorized live test of the local `AtomGit-qwen3.8-27b`
selection through `llm-api.atomgit.com`, using the current OpenAI Chat adapter
and its opt-in `qwen_prompt_cache` markers. No default model/config was changed.
The existing AtomGit authentication/signing path was used. A private signing
component was built in a disposable worktree with its own lockfile; the public
checkout retained its stub and clean dependency lock.

## Method

- Six interleaved requests: cache off/on, three calls per arm. Each arm used an
  isolated synthetic system prefix (512 inert records), a stable session ID,
  and a changing short probe. First calls were cold-prefix controls.
- Two additional requests with markers enabled and a synthetic assistant tool
  call, tool result and ephemeral reminder. No real tools were executed.
- No real project files or conversation content were sent. Output was capped
  at 96 tokens per call and adapter retries were disabled.
- Metadata-only result capture: no tokens/credentials, signed headers, response
  content, user identity or proprietary signing implementation are retained.

## Results

All eight requests completed without truncation or provider errors.

| Scenario | Input/request | Cache read | First content |
|---|---:|---|---|
| Off, cold | 19,831 | not reported | 3.473s |
| On, cold | 19,831 | not reported | 3.278s |
| Off, warm 1 | 19,831 | not reported | 4.237s |
| On, warm 1 | 19,831 | not reported | 3.087s |
| Off, warm 2 | 19,831 | not reported | 3.129s |
| On, warm 2 | 19,831 | 16,896 (85.20%) | 0.991s |
| On, tool history cold | 20,160 | not reported | 2.586s |
| On, tool history warm | 20,160 | not reported | 2.526s |

The normalized `cached_tokens: 0` in raw results is NOT a verified cache miss
when `cache_read_reported` is false. All eight write counters were absent.
A quicker first-content time is observational, not proof of a cache effect.

## Conclusion

The request format, including tool-result text blocks and a breakpoint before
an ephemeral reminder, is accepted by this gateway. At least one request
reported real cache reads. This small experiment does not isolate whether the
marker caused that hit or guarantee stable reuse: implicit cache, backend
routing, warm-up behavior and scheduling remain possible contributors. It does
not validate cache-write billing, Qwen4 Preview or Qoder behavior. Keep the
feature opt-in until a controlled comparison with complete counters is available.

## Reproduction

`crates/atomcode-capabilities/tests/qwen_cache_live.rs` is ignored by default.
It additionally requires `ATOMCODE_QWEN_CACHE_LIVE=1`, a report path in
`ATOMCODE_QWEN_CACHE_REPORT`, the configured test selection and an official
signer-enabled build. `ATOMCODE_QWEN_CACHE_TOOLS=1` selects the two-request tool
compatibility scenario; otherwise it makes at most six interleaved calls.
Run only in an isolated official-signing worktree using `cargo nextest run -p
atomcode-capabilities --test qwen_cache_live --features
atomcode-auth/codingplan-crypto -E 'test(qwen_cache_live_ab)' --run-ignored only --no-capture`. It consumes real
model quota and may refresh the existing login through the standard auth layer.
The public signing stub cannot run this test. Never overlay private crypto into
a shared checkout or commit its dependency changes.


## Expanded follow-up: multi-prefix, crossover and rolling history

The user authorized more real tests. This follow-up stayed within 32 inference
calls: 31 streams completed with usage; one additional request may have been in
flight when nextest killed the first batch at its default 120-second whole-test
limit. This was a test-runner limit, not an upstream timeout. Completed requests
were not replayed. The unfinished independent-prefix phase has unequal warm
sample counts, so it is not a complete balanced comparison.

The subsequent crossover/tool batch used a temporary `live` nextest profile with
`slow-timeout = { period = "60s", terminate-after = 10 }`, without modifying
ordinary project validation settings. It finished all 14 requests. Two final
long-prefix requests used the remaining inference-call allowance.

### Results, excluding cold prefixes

| Scenario | Default | Explicit markers |
|---|---|---|
| Independent prefixes | 4 warm requests; no read counter returned | 5 warm requests; one reported 16,896/19,834 (85.19%) |
| Same-prefix mode crossover | 3 warm requests; no read counter returned | 3 warm requests; no read counter returned |
| Growing synthetic tool history | 2 warm requests; both reported 18,432 reads | 2 warm requests; both reported 18,432 reads |
| 61,478-token prefix | Not tested | 1 warm request; no read counter returned |

For the rolling-tool warm requests, **both arms** reported 36,864 cache-read
tokens out of 40,583 input tokens, or **90.84%**. Thus the default adapter can
achieve high reported cache reuse without explicit markers on this gateway.
This contradicts attributing low cache reuse solely to the missing marker.
It does not establish that markers never help, nor that the gateway honors or
ignores their cache semantics in every case.

No write counters were provided. A missing read counter is not established zero
reuse; reported-read sums divided by all input cannot be treated as exact hit
rates for the incomplete-counter scenarios. Request latency is also descriptive
only. One long-prefix cold stream terminated with usage (one output token) but
no text/reasoning delta; it has no first-content timing and is not evidence of a
successful visible answer.

Total completed usage in this follow-up: **700,622 input / 894 output tokens**.
All payloads were generated data; no tools or project tasks were executed.
The independent-prefix mode orders alternated. Crossover sequences were
`off/off/on/on` and `on/on/off/off`, each with a fresh prefix. The tool histories
grew between requests and included a changing, non-persisted reminder tail.
Only metadata is retained in `expanded-results.json`.

### Updated conclusion

Do not enable explicit markers by default or call them a verified fix for the
user's Qwen4 cache report. Keep diagnostics and the experimental opt-in. Investigate
the actual user's model/endpoint, per-request prefix stability, cache reporting
and upstream reuse behavior. Routing, eviction, cache publication and input
transformation remain hypotheses rather than established causes. This is an
adapter-level synthetic benchmark, not a Qoder or full AtomCode workload comparison.

### Expanded reproduction

Select only `-E 'test(qwen_cache_live_expanded)'` and use a temporary nextest
profile with a sufficiently long whole-test limit. The normal expanded plan
makes at most 32 calls. `ATOMCODE_QWEN_CACHE_CONTINUATION=1` skips the independent
phase (14 calls); `ATOMCODE_QWEN_CACHE_LONG_PREFIX=1` replaces the plan with two
1,536-record long-prefix probes. Preserve the existing live opt-in/report-path
requirements. Do not select all ignored tests: the earlier AB test is separate
and also consumes quota. All scenarios stop on a provider failure.

## Qwen4 Max Preview through the user-specified release executable

Tested the existing `/Users/theo/Documents/workspace/atomcode/target/release/atomcode`
(`5.2.7 (eab22d60d+dirty)`) rather than rebuilding or using the adapter test.
Selection: `alitoken/qwen4-max-preview`, OpenAI-compatible endpoint host
`token-plan.cn-beijing.maas.aliyuncs.com`. This differs from the previous
AtomGit Qwen3.8 endpoint. Results are in `qwen4-release-results.json`.

Twelve ephemeral headless invocations: two scenarios, each with independent
synthetic prefixes for off/on and three alternating rounds. First round is
cold. Each run uses a fresh runtime; the system prefix is stable within each
arm. The second scenario exposes the normal tool schemas but instructs the
model not to call tools; actual tool calls were zero. No project source was
sent. The original config and default selection were not changed; private
temporary config copies were removed after testing. Max output was 96 per
request and automatic error retries were disabled (`retry_max_attempts=1`).

| Scenario | Input, first request per run | Default warm reads | Explicit-marker warm reads |
|---|---:|---:|---:|
| No tools | 17,920 | 17,664 (98.57%), twice | 16,980 (94.75%), twice |
| Normal tool schemas | 36,622 | 36,352 (99.26%), twice | 35,578 (97.15%), twice |

The table compares only the first request of each invocation. Three extra
requests occurred through output-limit continuation (one in enabled cold tool
run, two in default final tool run), bringing the total to 15 successful
streams. Those continuations are recorded separately in each run's usage list;
this is not a test of visible-answer completeness at a 96-token cap.

File diagnostics confirm all tested cache switches reached the adapter. Unlike
the older Qwen3.8 endpoint, this endpoint explicitly reported cache-read counters
including zero on cold first requests. Enabled no-tool cold request reported
17,880 cache-write tokens; enabled no-tool warm requests each reported 900 writes.
The explicit markers used here are the implemented message-level `cache_control`,
not the newer top-level `prompt_cache_options` parameter. Writes prove a measurable
server-side difference but do not establish billing cost without account pricing.

Conclusion: default AtomCode requests can achieve high cache reuse with Qwen4 Max
Preview, including the normal tool-schema payload. These small independent-prefix
samples do not show a benefit from enabling explicit markers, and cannot establish
that markers cause lower reuse or faster responses. Keep the switch opt-in.
The original user's long-session low-hit report remains unreproduced: this does
not compare Qoder, actual growing project history, tool execution, compaction,
long idle intervals, or the original user's gateway/account.

Two local setup failures preceded the successful experiment: an invalid retry
setting was rejected before inference; passing a profile selection to `--model`
(which overrides the raw model ID) produced one model-not-found 404. Corrected
runs use the temporary config's `default_model` selection with no `--model`
override. Neither setup failure is included in the twelve completed invocations.

## Real tool execution, resume, and the reported dd97065f build

The user confirmed the original endpoint host matches this test and identified
`atomcode 5.2.7 (dd97065f)` at `/usr/local/bin/atomcode`. That executable is present
and was tested directly, with no rebuild. The earlier release executable is newer;
its success alone would not exclude a defect in the reported build.

All runs here use the default implicit cache, the same configured Qwen4 endpoint,
normal coding persona plus an appended synthetic reference block, actual
`read_file` execution, native persisted sessions, and a separate headless process
resuming the same session each turn. Model output cap is 512 tokens/request and
error retries are disabled. Each scenario has its own temporary product directory
and synthetic workspace; the real configuration is copied privately, not modified,
and temporary credential-containing copies are removed. No real source or user
conversation is sent. Individual large tool outputs are subject to the product's
normal truncation, adding about 20k input tokens per new file, not an unlimited
file injection. These are sequential runs, not a multi-agent or long-idle test.

| Scenario | Completed main streams | Largest request input | Overall reported cache read / input |
|---|---:|---:|---:|
| Newer release, four small-file turns | 8 | 40,123 | 282,880 / 318,124 = 88.92% |
| Newer release, four large-file turns | 8 | 121,376 | 541,952 / 643,258 = 84.25% |
| Reported dd97065f, thirteen large-file turns | 26 | 306,769 | 4,210,176 / 4,517,903 = 93.19% |

The original-version fourteenth turn was refused by the upstream rate limiter
(`RateLimited`, no usage, no tool call). Testing stopped there; it is not a completed
inference and is excluded from the ratios. No retry bypass or additional paid
probe was attempted. There may also be background title calls; these are excluded
from main-turn usage totals, not falsely counted as nonexistent.

For dd97065f, turn 13 had two requests: 286,208 reads / 286,382 input (99.94%),
then 286,208 / 306,769 (93.30%) after reading the new file. The combined turn was
96.50%. This illustrates normal denominator growth from fresh tool content;
a reduced percentage immediately after a large read is not itself a cache loss.

The existing `ATOMCODE_DUMP_REQUEST` metadata instrumentation was enabled for
large-history scenarios. All 27 assembled main requests in the old run (including
the rejected final one) have the same system-text digest and tool-definition
count. Every prior assembled message projection remains an identical prefix of
the next. This dump is before request-local reminder hooks and does not prove
full wire-body identity or tokenizer-level identity; current-release wire logs
separately confirm stable system/tools on normal successive chat requests. The
committed report stores lengths and comparison booleans, not content hashes,
session IDs, credentials or message contents. See `qwen4-multiturn-results.json`.

### Diagnostic pitfall and remaining fault boundary

A title request can share the chat provider instance. In the small-tool test,
an interleaved 95-token title call changed the comparison's system/tools/options,
then the next main chat call changed them back and still achieved 99.16% reads.
Thus a provider-instance `system_changed=true` or `tools_changed=true` log alone
is NOT proof that the chat history was rewritten. Correlate provider request IDs,
process, main-turn usage IDs and caller purpose; do not mix title/summary/child
calls into a single conversation comparison.

Code review found bounded, intentional ways a real task can differ from these
stable-schema runs: deferred MCP/builtin search replaces the selected schemas;
MCP connection instructions can replace a system fragment; task/team children
have their own system instructions and restricted tool catalogs; compaction can
rewrite old messages when triggered. These are candidate boundaries to correlate,
not established causes of the original low-hit incident. Prompt fragments and the
normal tool registry are explicitly ordered; the date reminder is a request-local
tail in both builds. The tested histories did not trigger compaction (configured
1M context, under 31% utilization), whereas the original screenshot used a 512k
window. At the shown 304k/512k utilization it is also below the default 70% automatic
threshold, but earlier/manual compaction in that user's session remains unknown.

**Conclusion:** the same model endpoint and reported executable can sustain high
implicit-cache reuse through real tool calls, resume, and 306k-token context.
Neither a universal dd97065f cache defect nor a 300k-token cache cliff was
reproduced. This does not prove the original incident was upstream, nor reproduce
its runtime duration, concurrency, dynamic tools, reasoning volume or full task.
The next discriminating evidence is per-request usage plus prefix/schema-change
metadata from the actual low-hit session, with auxiliary calls separated. No
production fix is claimed from this experiment.

## Actual user log and Messages-path reproduction

The supplied user log revealed the failing Qwen4 calls used Anthropic Messages,
not the OpenAI Chat path tested above. See [user log findings](user-log-findings.md)
and [controlled Messages reproduction](messages-reproduction.md). A transient
reminder cache-boundary defect was reproduced; a proxy-only stable-tool-result
boundary restored incremental reads. The production adapter is now corrected and separately verified with an
unmodified forwarding proxy; see the final section of that report.
