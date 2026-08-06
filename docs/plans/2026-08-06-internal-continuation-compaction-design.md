# Internal continuation compaction

## Problem

Automatic compaction currently runs when `handle_prompt` accepts a user or host-provided synthetic prompt. This already covers Goal-controller continuations dispatched through `AgentCommand::SendSyntheticMessage`. Kernel-internal continuations created by output-truncation recovery or `LifecycleHooks::offer_typed_continuation` bypass that entry point: they append a synthetic message directly inside `run_turn` and start another model round. A long hook-driven verification or recovery loop can therefore pass the configured `compact_threshold` without giving the existing compaction policy a chance to run.

## Design

Reuse the kernel's existing `should_compact` pressure check and `run_compaction` policy at the safe boundary immediately before an internal continuation is appended. At this point the provider stream has ended, the assistant message and metadata have been stored, and no tool call is pending or executing. This keeps `CodingRuntime` as the single lifecycle owner and does not introduce another compaction strategy, command, or persistence model.

Only one internal automatic-compaction attempt is allowed per policy stage and accepted turn. The stage comes from the existing strategy's `will_summarize` verdict: a moderate-pressure rewrite/no-op cannot repeat every round, but it also cannot suppress the later high-pressure summary stage. The same boundary is shared by output-limit recovery and hook-driven verification continuations. Normal replies, tool execution, external Goal synthetic prompts, manual compaction, overflow recovery, cancellation, and provider reload retain their existing paths.

## Failure and verification

Automatic compaction remains best-effort and uses the existing `Compacted`/`CompactionFailed` events. A failed or refused plan does not discard conversation state; the internal continuation proceeds with the original conversation. Tests cover an above-threshold hook continuation producing a compaction event before continuing, a below-threshold continuation not compacting, and the existing truncation/compaction suites guard unchanged behavior.
