# Context-path probes — 2026-10-26

## What actually ran

- Native AtomCode: two isolated sessions, three user turns each, original brainstorming skill available, same-model loopback relay. Native binary frozen at `5.2.4 (6085861aa+dirty)`; exact hash/version saved in manifest. It is not a rebuild of current HEAD.
- Direct model calls: two independent three-turn conversations with the same frozen rendered persona and user turns. No tools, catalog, reminders or native runtime. Assistant replies differ between conversations, so this is a conversational workflow comparison, not a one-variable replay or causal attribution to a particular component.
- Summary controls: two synthetic fixed histories (explicit pipe-local identity; a rejected ID-merger proposal followed by accepted time-column rename). For each: create one model summary, generate one answer from original history and one answer from summarized history. This is one sampled pair per case, not a stability estimate.

All requested paths completed. Expected model `deepseek-v4.1-flash-1017` reported by direct/summary replies; native relay records upstream model identity. Effort=max, output cap32768. Native auxiliary/tool-driven calls are counted separately in relay metadata; six native turns do not mean six provider requests.

## Evidence retained

Ignored `results/context-paths-20261026` contains inputs, manifest, sanitized native request captures, visible native events, direct replies, summaries and both summary-control answers. The request observer removes provider reasoning fields and redacts the relay token; configured real credential scan passed. Captures are evidence of visible input, not byte-for-byte replay files. Real credential remains only in the parent relay/provider client.

Both native final requests contain the prior user confirmation that different pipes may share a sequence, and the latest reset/BootId evidence. `wire-verification.json` identifies the relevant captured files. In these runs, missing these client-side facts cannot explain the response errors. This does not prove all runtime history paths are correct.

## Targeted findings

1. **Neither path consistently avoids unsupported assumptions.** Direct sample1 invents protocol guidance including normalizing the opaque BootId by trimming/case folding, and discusses a reboot-reset counter despite the supplied non-reuse guarantee. Native replies also add protocol/timing assumptions. These are assistant-reviewed examples, not a scored full-SQL audit or a quality ranking.
2. **Native sample1 retains `CHECK (tube_seq > 0)` in final DDL after reset-to-zero evidence.** This rejects zero values; the fixture says reset-to-zero but does not explicitly specify whether zero is emitted as a valid tube identifier or merely the counter state before allocation. Record this as an unresolved boundary/constraint risk, not an already proven protocol violation. A future zero-valid fixture can distinguish omission from interpretation.
3. **Both sampled summaries preserve the important facts.** The explicit-scope summary retains the7/5-column inventories, independent device/pipe IDs, pipe-local sequence, query-only Duration and device-ms/audit-time distinction. The rejected-proposal summary explicitly says the ID merger and device-sequence relabeling were withdrawn; it preserves the accepted EnterMs/ExitMs rename.
4. **Both raw and summarized rename answers retain the separate IDs and accepted time-column rename.** New-evidence answers use the four-column key. The uncompressed new-evidence answer still asserts DeviceMs reset without input support; that particular failure is not evidence of summary loss. No original-user-session error was reproduced through summary omission here.

## Summary simulation limits

Read current `compaction.rs::summarize_span`, `render_transcript`, `build_summary_prompt` and framing. For these text-only histories with no previous anchor, the experiment renders role-tagged lines and uses the current summary-system/template/framing strings. It preserves the initial user message alongside the summary as an approximation of the protected prefix. It does not call native `/compact`, select a native drain boundary, run lifecycle hooks, update a native anchor, test overflow/fallback, restore sessions, or enforce native64KiB truncation. It uses32768 output tokens versus the production summary soft cap16000. No claim that native compaction is validated or failure-free follows from this experiment.

## Decision

Do not add a context store, second compaction state machine, or more generic persona text from these results. The samples do not establish AtomCode input loss or summary corruption as the root cause, and direct calls still make unsupported inferences. Keep tests for unsupported facts separate from final-key correctness, historical paraphrase, and zero-value boundary constraints.

No production changes, commit or push.17 offline tests passed, Python compilation and diff checks passed. No independent blind review, statistical ranking, live SQL execution, or original-session replay.
