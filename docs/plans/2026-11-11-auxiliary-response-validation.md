# Auxiliary response validation

**Goal:** Prevent incomplete/malformed auxiliary model output from becoming a composer suggestion, recap, or positive goal verdict; bound retained text and preserve cancellation/deadlines.

**Baseline:** release/v5.2.7 at 99fd18f2e, initially clean. Reviewed current sampler callers (suggestion and recap), sanitizers, goal evaluator and its generation/controller/epoch envelope, existing recovery handling and recent history. Controller lifecycle, provider selection, session binding, retry/evidence-expansion owner and terminal result types remain unchanged.

## Confirmed defects and design

- The shared sampler accepts EOF without Done, ignores malformed/tool events, retains unbounded visible text, and can starve its timeout on an always-ready stream.
- Suggestion and recap consumers ignore a truncated sample when visible text exists. Preserve cutoff metadata for diagnostics, but discard incomplete display output.
- The goal evaluator already rejects truncation/missing terminal and bounds verdict text, but ignores malformed/tool events and can accept a following yes verdict. Return existing Inconclusive and keep the same generation/controller/epoch and usage envelope. Yield inside both readers so cancellation/deadline futures run.
- Reject the exact known native template token in auxiliary display sanitization and evaluator parsing using the existing provider predicate. Original conversation text is not changed. No textual tools are synthesized.
- Use a 32768-byte retained-text fuse for the shared sampler, comfortably beyond the 80/240-character accepted display outputs, without imposing a reasoning-starving output-token cap. Keep existing effort, timeouts and tool choice.
- Replace raw provider-error debug dumps in the sampler with metadata-only fields.

## Validation

Scripted and loopback-SSE tests for partial EOF, truncation, malformed/tool events, native markers, oversized text, valid responses, usage identity, no extra attempts, cancellation and continuously-ready streams. Then full coding tests, compile gate, format/lock checks and an isolated signed release build. No new hook, controller owner, public wire field, persistence format or dependency is introduced.


A further check found the separate explicit-unknown detector could request supplemental evidence for a native-token-contaminated unknown verdict. Reject that format there too: malformed output must not authorize the bounded evidence-expansion request. Keep genuine unknown verdict expansion and the runtime's existing evaluator recovery policy intact.

## Completed verification

- Final source includes parallel MCP commit `70d0db025` plus this change. Full `cargo nextest run -p atomcode-coding --no-fail-fast`: 1337 passed, 11 skipped. Eight new tests invoke the production auxiliary readers with scripted event streams, including deadline/cancel, retained-byte limits, identity/usage preservation and metadata-only error logging.
- `bash gates/compile.sh` passed, including feature-gated consumer compilation. Format and diff checks passed; root Cargo.lock remains unchanged and contains none of the three prohibited private-overlay packages.
- Isolated signed release build passed; `target/release/atomcode` was updated. Actual executable loopback-SSE main/title/session regressions: all 11 passed, including persisted Titled facts, resume, fallback titles, preserved body text and no execution of injected tool fragments. These are main/session nonregressions; auxiliary error coverage comes from the production-function tests above.
- An initial loopback fixture scanned integration installation directories during atomic replacement and disconnected its server. Restricted scanning to session facts and reran all 11 cases in fresh directories successfully.
- No external model requests or real API credentials were used in this round. Upstream model/gateway native-token emission remains an observed upstream issue; this change contains unsafe auxiliary output rather than claiming to fix its origin. Existing finite goal recovery and genuine-unknown evidence expansion policies remain unchanged.

Executable SHA-256: `5885dcc7be385728fda9ab8910509a4e239e1f902e0b89580f0451a1b24d7722`. Build logs, fault-injection fixtures and session evidence were archived under `/var/folders/f_/yb9v7zss01sbpj531mbvd7q00000gn/T/atomcode-aux-check-mxts77mj`.
