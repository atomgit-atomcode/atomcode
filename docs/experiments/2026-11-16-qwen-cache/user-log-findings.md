# User log: Qwen4 cache failure evidence

Analysis of the supplied `atomcode(1).log` (6,774,706 bytes, 14,415 lines),
covering November 12–16. Only numeric request/profile metadata is retained in
`user-log-qwen-metadata.json`; no source log, conversation, credentials or real
session identifiers are copied. Sessions are locally assigned anonymous aliases.
Line numbers below refer to the supplied original log. Times are UTC.

## The screenshot is exactly accounted for

The first 50 completed Qwen4 Max Preview main requests in the affected session
sum to the original screenshot's figures exactly:

- Input: 12,161,738 tokens.
- Cache reads: 2,677,507 tokens (22.0158%).
- Output: 61,954 tokens.
- Last completion: 2026-11-16 02:27:29.750326Z (line 9848).

This connects the reported low rate to actual main-turn request usage. It is
not explained merely by comparing a different UI counter's scope. The log alone
does not expose raw upstream usage fields, so it cannot independently exclude a
provider compatibility/accounting issue; the billing screenshot corroborates the
same aggregate values.

## Crucial protocol distinction

The user's Qwen4 requests were handled by
`atomcode_capabilities::provider::anthropic`, as confirmed by HTTP response
header events following the Qwen input profiles (e.g. lines 9423–9426).
They used the Anthropic-compatible Messages adapter, not OpenAI Chat.
The prior live experiments used OpenAI Chat and thus did not exercise this path.
The log does not disclose a verified upstream URL; the endpoint host equivalence
was supplied by the user, not inferred from a URL in these events.

## The failure pattern

| Preview request | Main input | Cache reads | Rate | Completion log line |
|---|---:|---:|---:|---:|
| 1 (initial model switch) | 222,285 | 0 | 0% | 9426 |
| 2 | 223,050 | 221,569 | 99.34% | 9434 |
| 8 | 226,580 | 221,569 | 97.79% | 9487 |
| 9 | 227,521 | 26,822 | 11.79% | 9495 |
| 50 | 273,854 | 26,822 | 9.79% | 9848 |

Across all 75 completed Preview requests matched to input profiles: one initial
zero, seven reads of exactly 221,569, and 67 reads of exactly 26,822. Aggregate
reads/input = 3,348,057 / 19,716,483 = 16.98%.

For 200 completed Qwen4 Max requests, 168 read exactly 26,819 tokens; intermittent
high-read groups occur too. Aggregate reads/input = 10,819,287 / 41,752,654 = 25.91%.
This is consistent with a stable base-prefix cache surviving while conversation
reuse is lost or not advanced. Exact token attribution of the 26.8k plateau to
system/tools is a hypothesis: the log gives byte sizes, not tokenized boundaries.

The same session's matched DeepSeek requests aggregate 72,074,752 / 74,258,343
= 97.06%. They used OpenAI Chat, so this comparison differs in both model and
protocol and does not isolate either one causally.

## What the first failure does and does not correlate with

During all matched Preview requests, system JSON bytes stayed 41,123 and tool
schema JSON bytes stayed 69,331. MCP tools were absent in the profiles. Constant
size does NOT prove identical content, but there is no size-level evidence here
of tool catalog churn causing the first drop. Wire message count grew from 364
on the initial request to 378 on the last high-read request and 380 on the first
low-read request.

The first drop occurred at 02:04:41Z. The first logged compaction fact in this
file is later, at 03:06:22Z (line 10273). Therefore a logged compaction does not
explain the onset of the Preview failure. Later compactions may affect later
requests and are not ruled out globally.

The seven high-read requests all reuse the same original 221,569-token prefix;
they do not show the cache-read boundary advancing. Afterward, the large cached
prefix disappears from the usage while the base-size cache persists. The service
documents a 20-content-block lookup window. A frozen write boundary falling out
of that window is a concrete hypothesis compatible with this pattern, but wire
message counts are NOT exact content-block counts. Neither raw breakpoint
positions nor cache-write counters are present in this log; the hypothesis is
not yet proven, nor can the failure be assigned solely to client or server.

Relevant official references:
- https://help.aliyun.com/zh/model-studio/explicit-cache-qwen4
- https://help.aliyun.com/zh/model-studio/anthropic-api-messages
- https://help.aliyun.com/zh/model-studio/context-cache

## Current code and next discriminating experiment

`anthropic.rs::apply_prompt_cache` already sets up to four explicit markers:
last tool definition, system block, and the last eligible block in each of the
two most recent wire messages. Thus “no explicit cache markers” is not an
accurate diagnosis for this path. It marks after user-message merging, and the
last eligible block can be a request-local reminder. This differs from the
new opt-in OpenAI strategy, which selects a stable non-synthetic message.
The Anthropic decoder currently sums fresh input + cache read + cache creation
under standard Anthropic usage semantics. Raw response buckets are needed to
verify the gateway implements the same semantics.

The next test should reproduce Messages-format growing tool history over more
than 20 content blocks using the reported executable, capturing only marker
positions/types, stable-prefix comparisons, and all three upstream usage buckets.
Compare the existing last-two-wire-message policy with stable-history boundaries;
control model, account and endpoint. Stop on upstream limit/auth failures. Do not
change TTL or global cache defaults based solely on this log. No production fix
is claimed by this forensic analysis.

## Follow-up: causal reproduction

The configured Messages endpoint has now reproduced the low-hit pattern, and
moving the cache marker from the transient reminder to the stable tool result
restored rolling cache reads. See [Messages reproduction](messages-reproduction.md).
This supersedes the earlier section treating the 20-block window as the leading
hypothesis; the unstable cached tail itself is now a demonstrated client defect.
