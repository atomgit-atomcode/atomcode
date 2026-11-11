# Provider format diagnostics follow-up

**Goal:** Make the remaining native-tool-marker report attributable to a wire field without changing text into executable calls, and keep timeout attribution consistent with the earliest deadline.

**Architecture:** OpenAI-compatible SSE decoding owns wire-format observations. The decoder gets a bounded, per-stream matcher for the exact MiniMax chat-template token seen in the historical real response. It emits a constant metadata-only diagnostic once; it never buffers arbitrary output, rewrites text, retries, or synthesizes tool calls. The shared watchdog continues to own deadline selection.

**Baseline:** release/v5.2.7, 5760f5e25; clean worktree before this follow-up. Checked current decoder/latency/watchdog code and their recent Git history. No persistent format, driver command, hook, provider default or lifecycle owner changes.

## Evidence and design decision

- Historical real lumen-alpha responses contain `]<]minimax[>[<tool_call>` and broken invoke/argument fragments in the returned content text.
- Local oh-my-pi has both explicitly configured in-band dialects (`packages/ai/src/dialect/minimax.ts`) and provider-specific special-token cleanup (`packages/ai/src/providers/openai-completions.ts`). Those are declared compatibility modes; they do not establish that this gateway alias supports an executable text protocol.
- Keep literal examples and original content unchanged. Observe only the exact reserved chat-template prefix at `choices[0].delta.content`, rather than ordinary `<tool_call>` examples. A matching literal quotation may be observed too; this is diagnostic evidence, not a provider failure verdict.
- A delayed task can resume after both idle and reasoning deadlines. Classify the earliest deadline selected by `deadline()`; do not call a previously expired idle window a reasoning spending limit.

## Implementation and checks

1. Add the private metadata-only content marker matcher and wire it before inline-think routing in the existing SSE decoder.
2. Cover split points, overlapping prefixes, once-per-stream/reset behavior, quoted literals, structured calls alongside marker text, and no tool synthesis or payload logging.
3. Fix watchdog timeout classification and test delayed scheduling past both deadlines plus normal reasoning-first expiry.
4. Run capabilities tests, the affected harness deadline tests, compile gate, fmt and lock checks.
5. Build/test the signed release in an isolated private-overlay tree, preserving the public stub and lock. Record actual scope and remove temporary credential/source copies.


## Additional root cause found during live checks

The first real tool-read run succeeded, but its separate `purpose=session_title` request (tools=0, no native marker in the user prompt) also produced the observed reserved token. The original diagnostic expectation of one total observation was incorrect: there was one per response, two across the helper and the deliberate literal-echo response. Keep the observations separated by purpose/request id.

The CLI's `--ephemeral` flag is headless-only and explicitly opts out of session storage/resume. The shared `spawn_native_cli_runtime` nevertheless mounted the normal first-prompt title trigger, spending another model request for a session that is neither stored nor listed. Disable the existing `session-title-on-first-prompt` row through the CLI's existing host layer when and only when `ephemeral=true`. Do not change the person's naming setting or other drivers' naming policy. No replacement hook, titler, runtime owner, or persistence path is introduced.

Verify with complete CLI tests/compile gate and repeat the real ephemeral tool-read and literal-echo cases: expect only the two required main model requests for tool-read, one main request for literal echo, and zero session-title requests.


## Final results

- Capabilities: 1342 passed, 1 intentionally skipped. Harness reasoning-deadline tests: 6 passed. Complete CLI suite: 650 passed. Latest compile gate passed. Formatter/diff checks passed and root Cargo.lock remains unmodified with no hkdf/hmac/zeroize_derive packages.
- The initial live run observed one spontaneous native chat-template token in a **session-title helper**, upstream response id `07436cf0790967a0654b197c2377dc66`, and one deliberate literal-echo observation in the main reply. The observation is before inline-think routing at the original content field. It identifies the upstream side; it does not distinguish model generation from gateway serving/template processing. Main structured tool calls worked normally.
- After disabling ephemeral title generation, real lumen-alpha/high with a 1024 output cap: one structured read_file call read a temporary file and returned its nonce. Two main model requests, one tool execution, normal stop. No session-title request. The tool-read request produced no native marker observation.
- Real no-tools documentation echo returned exactly `]<]minimax[>[<tool_call>EXAMPLE</tool_call>`, with one main model request, zero tool calls and normal stop. Exactly one constant-metadata observation identifies `choices[0].delta.content` and `minimax_chat_template`. No body, argument, nonce or path appears in the diagnostic.
- The deliberate literal echo is a positive diagnostic/control test, not a spontaneous malformed response. The earlier spontaneous helper observation remains useful upstream evidence. The underlying sporadic model/gateway anomaly is not claimed fixed.
- Signed release built from 5760f5e25 plus this follow-up, including the preceding computer-use changes. All five changed production files exactly match the tested build tree. SHA-256: 232400e08f201383a03faa9016f13644fde416240a10631e9bc17615443d3f8c. This rebuild supersedes the older binary-snapshot caveat in the previous verification record.
- Evidence: `/var/folders/f_/yb9v7zss01sbpj531mbvd7q00000gn/T/atomcode-format-check-8oq2lfw8` (live-summary.json, verification-summary.json, provider-evidence.log, provider-evidence-initial.log, initial-live-summary.json, build.log and test logs).
- Temporary credential copies and owned isolated private build sources were removed after archiving sanitized evidence. The public crypto stub and root Cargo.lock were never overlaid.
