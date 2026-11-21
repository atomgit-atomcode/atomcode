# Stable Anthropic conversation cache boundaries

The controlled Qwen4 Messages experiment reproduced repeated history-cache writes
when the rolling marker included a request-only reminder. Moving the marker to
the stable tool result restored incremental reads.

The provider adapter owns wire formatting and cache metadata; the harness session
log remains authoritative and is unchanged. No runtime hooks, persistence changes,
new configuration or TTL change are needed. Select at most two recent eligible
non-synthetic message boundaries during formatting, before adjacent user messages
are merged. Keep text messages in stable content blocks even when a moving marker leaves
them, so appending a
synthetic reminder cannot join it into the cached text. For parallel tool results,
select the last stable result of the run. Preserve system/tool markers (at most
four total), signed thinking, error flags and strict user/assistant alternation.
The DeepSeek Messages dialect retains its existing encoding and no cache markers.

Validation: focused adapter regressions for initial user/reminder adjacency,
request-only tails, successive real tool rounds, parallel results and thinking;
capabilities crate tests; workspace compile gate and format check. Then test the
actual corrected provider against Qwen4 through the metadata-only forwarding
proxy with no request mutation. Preserve all unrelated dirty-worktree changes.

Implemented in `provider/anthropic.rs`. Final capabilities tests: 1,385 passed,
8 skipped. Four actual corrected-adapter streams verified against Qwen4: hot
reads 99.88–99.90%, without proxy mutation. See the experiment report for counters.
