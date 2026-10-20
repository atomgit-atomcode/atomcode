# Tool generation throughput

Baseline: release/v5.2.4 at 148fc9fa0 (advanced from 5fce39a61 during worktree creation); unrelated recovery, skill and TUI work is dirty in the primary checkout and must be preserved.

Keep throughput ephemeral and owned by the TUI Generation projection. Forward provider tool fragments through a harness emit event and the existing AgentEvent::ToolCallStreaming observer; never log tool arguments as assistant prose or add a persisted session event. The handle scopes tool fragments to its own session. The TUI only applies these unscoped legacy observations when showing that handle's root session, so a member view cannot inherit the lead's rate.

Start generation timing at the first non-empty text/reasoning/tool content, count streamed tool name/argument characters for estimates, and calibrate with provider completion usage. Complete tool calls following streamed fragments must not be counted twice. A provider that only returns a completed tool call has no observable generation duration; retain an unavailable reading instead of including TTFT in the denominator. Preserve the weighted average across tools and subsequent request waits; retry and turn resets keep their current semantics.

Render unavailable throughput as `— tok/s`; narrow layouts may still omit whole trailing figures. Test pure tool streams, mixed content, retries, final-call deduplication, stream/session isolation and the unavailable live line. Verify affected harness/TUI tests, fmt and the compile gate before integrating only this patch.

Short responses: keep the one-second minimum for live character estimates, but accept a completed request with positive provider usage and a nonzero observed generation interval even below one second. Otherwise fast tool-only calls would remain unavailable forever during the next request wait. Zero-duration/replayed observations still never invent a rate.

Validation: TUI library 1,549 tests, harness library 72 tests and the handle session-isolation integration test passed. `bash gates/compile.sh` passed, including feature-gated targets and both negative controls, in the isolated checkout. Applied the patch onto primary checkout 2dc0bc7cc without replacing concurrent recovery changes; owned source files match the verified checkout. Cargo.lock is byte-identical to HEAD and contains no private dependency markers.
