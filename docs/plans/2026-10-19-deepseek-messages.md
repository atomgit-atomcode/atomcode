# DeepSeek Messages Implementation Plan

**Goal:** Optional native Messages protocol for DeepSeek without changing existing OpenAI routes.

**Architecture:** Coding provider factory owns protocol selection; the existing capabilities Messages adapter owns transport, retry and decoding. Reuse native tool execution and runtime approval. No kernel or session format changes. Reasoning provenance includes model identity for DeepSeek. Unsupported effort fails before network I/O; no protocol fallback.

**Baseline:** release/v5.2.4 at 45d23493a; preserve concurrent dirty runtime/UI changes.

1. Add explicit deepseek-messages preset/protocol and legacy config projection. Keep deepseek preset OpenAI.
2. Add isolated DeepSeek wire dialect: enabled/disabled thinking, low/high/max effort, no Claude cache markers; normalize optional trailing /v1. Bind thinking signatures to model.
3. Wire factory through existing timeouts/retry/headers; no second lifecycle owner.
4. Test wire requests, foreign signatures, stream provenance, configuration and unchanged Claude behavior. Run affected crate tests, fmt and gates/compile.sh.
5. Document opt-in configuration and unverified live API scope. No commit/push until requested.

Out of scope: in-history system updates, native deferred tools, Files API, claimed cache/performance gains.

## Verification

- Isolated checkout contains exactly the staged feature, based on 45d23493a, without concurrent goal, speed or relay edits.
- `cargo test -p atomcode-config --lib`: 315 passed in the isolated checkout.
- `cargo test -p atomcode-coding --lib`: 514 passed, 8 existing ignored in the isolated checkout.
- `cargo test -p atomcode --lib tui_providers`: 25 passed in the isolated checkout.
- `cargo test -p atomcode-capabilities --lib`: 1023 passed after the audit fixes; the isolated source was verified byte-for-byte against the staged source.
- Native HTTP fixture covers endpoint, x-api-key, input_schema and streamed tool JSON; regression tests cover model-bound signatures, initial thinking, empty/unsigned thinking and streamed-versus-initial input precedence.
- `cargo fmt --all -- --check`, `git diff --check` and the complete `bash gates/compile.sh` passed in the isolated checkout, including feature and negative-control checks.
- nextest is not installed, so use cargo test locally.
- No real API/account, Windows terminal or end-to-end approval UI test. Approval/lifecycle implementation is unchanged; all drivers share the same provider factory.

## Pre-commit audit

- Corrected empty DeepSeek thinking replay: optional signatures stay on thinking blocks; empty text no longer becomes Claude redacted_thinking.
- Filter signatures during serialization instead of cloning all history.
- Corrected streamed tool input precedence: JSON deltas replace the complete initial input rather than concatenating two JSON objects. Both initial-only and streamed cases have regression coverage.
- Validate only the staged feature in a detached checkout before committing; leave concurrent goal, speed and relay edits untouched.
