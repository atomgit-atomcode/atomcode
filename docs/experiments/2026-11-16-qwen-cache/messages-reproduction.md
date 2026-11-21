# Qwen4 Messages: transient-tail breakpoint failure reproduced

## Scope and method

After the user configured `alitoken/qwen4-max-preview` with provider
`anthropic-compatible`, the selected endpoint was verified as
`https://token-plan.cn-beijing.maas.aliyuncs.com/apps/anthropic`. Tests used the
user-specified `target/release/atomcode`, version `5.2.7 (eab22d60d+dirty)`.
The problematic Anthropic marker algorithm is also present in dd97065f.

Five bounded experiments made 50 completed main-model streams, plus five
auxiliary title calls. There were no real project files, actual shell tools or
child agents: only synthetic reference text and `read_file` on disposable linked
files. Output was capped at 512 tokens per request; automatic error retries were
disabled. Private config copies and isolated product/session directories kept
real user configuration and history unchanged. Temporary credential-containing
config copies were deleted after each run.

A loopback forwarding proxy relayed requests to the same TLS-protected official
Messages endpoint. It never persisted keys, headers, response text or request
bodies. It recorded numeric usage events, content-block counts, cache marker
locations/types, and in-memory normalized prefix comparisons. No content hashes
were persisted. Main and auxiliary calls were compared separately.

The first resumed-turn scenario captured marker positions but not raw usage
because the metadata collector initially required a space after `data:`; the
upstream omits that space. The collector was corrected before all single-turn
and controlled-comparison experiments. CLI usage was available in every run.
For every main stream with raw buckets, fresh input + cache creation + cache read
was checked against CLI prompt tokens, and raw read against CLI cached tokens.
All matched. The cold `message_start.input_tokens` estimate was superseded by the
final authoritative `message_delta` buckets as the current adapter intends.

## Reproduction sequence

1. Ten separate user turns, each executing one actual read: warm request cache
   reads advanced and each resumed turn was about 99.6% cached. Not a reproduction.
2. One user turn following ten linked files (eleven model calls): cache reads
   stayed fixed at 39,235. Each new call wrote a growing conversation suffix
   again, despite the historical blocks remaining stable up to the temporary tail.
   Because the large synthetic reference was in the SYSTEM prompt, that surviving
   base cache dominated the input and hid the severity in percentage terms.
3. Same one-turn shape with a wire-only experimental change: on a final mixed
   user message containing tool results plus the transient reminder, remove the
   reminder's marker and mark the last tool-result block instead. Reads advanced
   39,239 -> 39,454 -> 39,581 -> ... -> 40,497. Subsequent writes stayed around
   108–134 tokens rather than growing with the entire conversation suffix.
4. Put a large inert reference in the USER conversation rather than the system;
   follow three linked files. Existing strategy reproduces a low aggregate rate.
5. Repeat the same generated-workload recipe with independent synthetic prefixes,
   changing only the tool-result/reminder marker location at the proxy boundary.
   This is a matched-recipe comparison, not byte-identical requests: nonces, file
   IDs and generated model output differ. Account, endpoint, model, executable,
   request options and workload structure remain the same.

## Large-user-history results

| Model request | Existing strategy: read / input | Stable-tool-result strategy: read / input |
|---|---:|---:|
| 1, cold | 0 / 150,056 | 0 / 150,063 |
| 2, first tool result | 39,234 / 150,098 (26.14%) | 39,238 / 150,106 (26.14%) |
| 3 | 39,234 / 150,231 (26.12%) | 150,065 / 150,239 (99.88%) |
| 4 | 39,234 / 150,336 (26.10%) | 150,198 / 150,347 (99.90%) |
| All requests, including cold | 117,702 / 600,721 (19.59%) | 339,501 / 600,755 (56.51%) |

Both strategies incur the initial cold request and another substantial write
when the first stable tool-result boundary is established. Do not describe the
full experiment as a jump from 19.59% to 99.9%; the latter is the later hot-request
rate, while the fixed arm's aggregate is 56.51%.

Raw final usage for requests 3/4:

- Existing strategy: reads 39,234 each; writes 110,991 then 111,096; fresh input 6 each.
- Stable boundary: reads 150,065 then 150,198; writes 133 then 108; fresh input 41 each.

The larger fresh count in the stable arm is expected: the reminder is after the
cache breakpoint and is processed fresh, while the large history is reused.
See `qwen4-messages-reproduction-results.json` for exact counters and marker paths.

## Identified client-side defect and limits

The current `anthropic.rs::apply_prompt_cache` selects the last eligible block of
the last two WIRE messages, after merging adjacent user messages. A tool-result
user message followed by a request-only date reminder becomes one content array;
the last eligible block is the reminder text, not the stable tool result. That
cached prefix includes a reminder that disappears from this position on the
next call, when the history is extended and a new reminder is appended at the
new tail. The experiment's normalized comparisons show exactly this mismatch.
Moving only that breakpoint to the preceding stable tool result restores
incremental reuse on this service. This establishes a concrete AtomCode-side
cache-boundary defect for the tested compatibility path; a longer TTL does not
make an unstable prefix stable.

The same user-log symptom—fixed base reads and missing conversation reuse—is now
reproduced at low rates, with a successful targeted counterfactual. This is strong
mechanistic evidence connecting the client defect to the report, not proof that
every one of the user's original misses had that sole cause. Their complete wire
bodies and cache-write counters were not supplied. The earlier 20-block lookback
hypothesis is no longer necessary to explain the reproduced failure: dynamic
reminder inclusion breaks the written prefix itself. The service's handling of
multiple marked boundaries can still affect which earlier boundary survives.

No production code was modified in these experiments. The proxy-only variant is
not a shipped fix. A production correction should choose cache boundaries using
neutral-message provenance before merging, preserve strict user/assistant wire
alternation, exclude request-only reminder tails without matching their text,
retain stable tool-result boundaries, stay within four markers, and never mark
signed thinking. It must also cover first-round adjacent text user/reminder
messages, parallel tool results, thinking-only messages, and the DeepSeek Messages
path that intentionally does not use these Anthropic cache markers.


## Production adapter correction and verification

The actual Anthropic formatter now selects at most two stable, non-synthetic
conversation boundaries before merging user messages. A tool-result run uses its
last eligible stable result. System and tool-definition markers remain, for a
maximum of four. Text content is rendered in stable blocks regardless of whether
its moving marker is currently selected; this prevents old adjacent user context
from later rejoining into a different string. Thinking blocks are never selected,
and the DeepSeek Messages path retains its previous encoding and no markers.
No persistent session data, driver lifecycle, timeout, TTL or settings changed.

The updated `target/debug/atomcode` was then tested with the same synthetic
large-user-history workload against the configured Qwen4 Messages endpoint. The
proxy only captured metadata and did NOT mutate this run's requests. Raw final
usage buckets match CLI totals. Results:

| Request | Input | Cache read | Cache creation | Read rate |
|---|---:|---:|---:|---:|
| Cold | 150,056 | 0 | 149,928 | 0% |
| Hot 1 | 150,100 | 149,928 | 131 | 99.89% |
| Hot 2 | 150,234 | 150,059 | 134 | 99.88% |
| Hot 3 | 150,341 | 150,193 | 107 | 99.90% |

Overall read/input including cold = 450,180 / 600,731 = 74.94%. Unlike the
proxy-only minimal variant, the production formatter also preserves the initial
user text boundary, so the first hot request already reuses the large reference.
Exact metadata is in `qwen4-production-fix-results.json`. This validates the
corrected debug build; the existing release executable was not overwritten.

The final capabilities test run passed 1,385 tests (8 skipped). Regressions include
real-user/reminder adjacency, stable prefixes through the next actual tool round,
parallel and error tool results, marker movement past persisted user context,
synthetic-only requests and unchanged DeepSeek Messages encoding. Existing signed
thinking and four-marker tests remain green. The final workspace compile gate passed, including feature-gated targets and
negative controls. Format and diff checks passed. One earlier gate run used
stale config metadata; refreshing that source timestamp without changing its
contents forced recompilation and the full gate passed.
