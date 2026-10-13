# Overlap workspace checkpoints with model streaming

Windows logs identify native-snapshot turn_start at 23.960 seconds; model streaming is 2.464 seconds. Keep before-tool workspace fidelity while allowing model text to arrive during capture.

SnapshotHook owns one shared pending capture future per pending rewind point. turn_start starts spawn_blocking capture without awaiting it, but still synchronously checkpoints the accepted prompt. Consumers clone the shared future under the rewind mutex and release the mutex before awaiting; cancellation cannot consume the join handle or attach the result to a replacement pending point. Prior capture is drained before a new turn. Capture never writes runtime/session state from the blocking thread.

The existing on_model_response hook joins the before snapshot after streaming, before tool execution/continuation. The existing native session plugin adds a ToolsExecuteBatch barrier as defense in depth (including delegated tools) using that same SnapshotHook. Completed capture facts are appended after on_model_response, once per turn. turn_complete drains capture before computing the after tree and committing the ledger. Text appears early; tool execution, final turn completion and next-turn admission still wait. This preserves ownership, ordering and failure semantics without a new lifecycle hook or kernel command.

Capture failures preserve the existing best-effort conversation-only rewind and explicitly report a warning, never advertise a fake code checkpoint. No capture deadline is added: dropping spawn_blocking does not kill git, so a guessed timeout could orphan ongoing work. Retain capture-process cleanup for later targeted optimization.

Preparation phase start/end, lifecycle context spans and Provider request-start/raw-chunk milestones become DEBUG. Request parameters, headers, first reasoning/text and final Provider summary stay INFO. Slow workspace captures (>=1s) retain INFO summaries; failures WARN. Snapshot job/process/session identity and model request IDs remain available; no request/response contents or credentials are added.

Verification covers canceled/parallel awaiters, delayed checkpoint publication, before-tool barrier, accepted-prompt persistence, rewind fidelity and existing runtime submit/cancel tests. Use current-thread virtual clocks for future delays and controlled futures; real Git capture tests retain wall-clock behavior. Do not re-record differential goldens. Local macOS first-text verification is not Windows performance evidence.
