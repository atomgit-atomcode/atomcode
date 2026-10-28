# Native Compact + resume probe — 2026-10-26

## Executed path

An isolated daemon invoked `/live/compact`, which dispatches `DriverCommand::Compact(None)` to its bound CodingRuntime, the same runtime operation used by the TUI command. This is a real native compaction, not a model instruction to summarize, not the stateless `/command` importer path, and not an emulated summary prompt. It does not test TUI keystrokes/rendering.

Frozen binary: `atomcode5.2.4(6085861aa+dirty)` (exact reported string/hash in manifest). Therefore the result characterizes that binary, not every current uncommitted change. DeepSeek model via the configured static-key gateway; real key remains in the relay. Test daemon listens on loopback with an isolated product directory and temporary project. No real user session/config is modified.

Scenario:

1. Open a native session; explicitly confirm the7/5-column inventory and meanings.
2. Record a rejected DeviceNo/PipingNo merger and TubeSeq=device-sequence suggestion; accept only Photo1Ms/Photo2Ms→EnterMs/ExitMs.
3. Add200lines of synthetic irrelevant archived noise, so manual compaction can drain an actual old span.
4. Dispatch native Compact; require a committed `compacted` event, not only HTTP accepted.
5. Request final tables without restating the facts.
6. Stop daemon, restart with the same isolated storage, resume the same session ID and request final tables again.

The prompt budget40000 and configured context window128000 deliberately create drainable history. This is an artificial pressure case, not a measurement of ordinary user context size. Native provider caps are preserved: captured summary request has requested/effective max_tokens16000; normal replies32768. Relay fixes model/effort=max as before. Runtime token estimates are not billed provider tokens.

## Outcome

Initial attempt: native compaction committed and post-compaction answer completed, but test cleanup left the SSE connection open while waiting for graceful daemon shutdown. The restart step exceeded its cleanup deadline. This attempt remains recorded as execution_failure; it is not silently counted as full success. Processes were cleaned up. No conclusion about a production shutdown defect from that test-harness behavior.

After fixing only test SSE socket cleanup, the rerun completed all six user turns, one native compaction and restart/resume. Runtime reported summary of5messages, estimated ~81.1K→~9.0K tokens. Event log includes exactly one captured `compacted` fact. Both SSE snapshots carry the same session ID, with the second snapshot showing restored messages.

The native anchor retains:

- DeviceNo and PipingNo as separate columns, device and device-local pipe meanings.
- TubeSeq as pipe-local test-tube sequence, duplicate numbers permitted across pipes.
-7-column append-only event log, repeated/out-of-order reports retained.
-5-column detail inventory with accepted EnterMs/ExitMs names.
- Device milliseconds vs audit-only ReceivedAt; Duration calculated in queries, not stored.
- Explicit rejection of the identifier merger and device-sequence relabeling.

Targeted assistant review of both final replies (after compact and after restart): both retain these inventories, key meanings, accepted rename and query-only Duration. They do not reopen the design interview. This is not independent blind review or SQL execution.

## Actual-request verification

The two final native requests include the native anchor and do not include the original user message beginning `已确认完整方案：`. Thus the old confirmation did not survive merely because that original message remained verbatim. The protected initial user message contains no table inventory. On restart the previous post-compaction answer is also present, so the resumed output is supported by both anchor and retained recent answer; do not claim it tests the anchor alone.

Sanitized request captures, visible events, compact event, model/usage metadata and verification live under ignored `results/native-compact-live-restart-20261026`; failed initial attempt under `results/native-compact-live-20261026`. Credentials absent from saved JSON. Reasoning fields excluded. No full raw request byte-replay claim.

## Decision and limits

This supplies positive evidence that **this actual native compaction and event-log resume preserve these facts in one synthetic long-history scenario**. It does not prove all compaction, overflow, fallback, cancellation, provider errors, repeated anchored updates or session recovery paths correct, and does not reproduce the original missing user session. Initial setup asks the model to acknowledge rather than naturally debate the design, so it does not measure open-ended design convergence.

No production changes or new runtime hooks/state stores.18offline evaluation tests pass, script py_compile and diff checks pass. No Rust code changed; no new compile/test-suite claim for current unrelated workspace edits. No commit/push.

## Second-cycle anchored update

A follow-up native run (`native-compact-update-20261026`) adds one protocol revision after the first compaction: TubeSeq resets per boot and zero is explicitly a valid emitted identifier; every event carries a non-reused-per-device BootId; both tables gain BootId and the detail key becomes four columns. Another synthetic archive span makes a second real compaction drainable, followed by a final-table request and same-session daemon restart/resume.

Both native compactions committed; second notice estimates ~82.6K→~9.3K tokens. Two captured summarizer requests retain max_tokens16000. The second request contains `<previous-summary>` and the new zero-valid/BootId fact, confirming the actual anchored-update path rather than two unrelated initial summaries. The second anchor revises7/5columns to8/6, includes the four-column key and zero-valid rule, and retains independent IDs, rejected proposals, EnterMs/ExitMs and query-only Duration.

Both final replies (after second compact and after restart) retain8/6columns, four-column association and zero legality. Captured requests contain the updated anchor, while the original user message introducing BootId/zero is absent as a standalone message. On resume the recent final answer is also retained; not an anchor-only recall experiment. Same session ID is preserved.

This is a targeted preservation result, not an overall correctness pass. Both final replies introduce an unconfirmed assertion that EventId is generated by the reporting device; the input never establishes that source. Its combination with a primary-key recommendation and duplicate-retention requirement needs a separate event-identity contract; no claim is made that duplicate rows were actually rejected, because SQL was not executed and retransmission ID behavior is unspecified. The second summary also carries stale “reply已收录” next-step wording from the drained archive turn; final answers obey the later request rather than re-executing it.

Thus neither single compaction nor this successive anchored update reproduced the original field-loss complaint. Scope is one additional synthetic scenario on the same frozen binary, with acknowledgements rather than natural design debate. It does not validate unlimited repeated compactions, all model families, overflow/fallback/cancel or prove the original user's cause. No production patch justified.18offline tests and Python compilation pass; artifacts credential scan passed; no commit/push.

Reproduction script now accepts `--output <new-directory> --second-cycle`; output must not exist, so prior failed/successful evidence cannot be overwritten.
