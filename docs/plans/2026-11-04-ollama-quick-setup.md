# Ollama Quick Setup Implementation Plan

**Goal:** Detect Ollama while configuring it and provide actionable startup guidance with retry in TUI and WebUI.

**Architecture:** Capabilities owns discovery and classifies failed Ollama connections. Drivers own draft forms, asynchronous request identity, localized guidance and retry. Configuration writes remain explicit; no runtime/session ownership changes and no process spawning.

**Tech Stack:** Rust/reqwest, existing TUI Providers port, Preact.

Baseline: branch `release/v5.2.6`, commit `0ac2d300bb5ebcac200164fb6e3bc4ce3a4d8466`. Existing unrelated VS Code edits are preserved. Recent provider history: `8795785bb`, `6239a2aed`, `d8fed29d0`.

1. Add shared Ollama connection-refused/timeout diagnostics in capabilities discovery. Preserve HTTP, TLS, proxy and response-read failures; distinguish loopback from remote endpoints. Test real refused connections and classification boundaries.
2. Add localized TUI guidance and draft discovery through the existing Providers port. Probe without writing config, retain the draft after failure, retry with Ctrl+F, invalidate obsolete responses and reuse successful inventory after explicit save. Wrap guidance within the panel.
3. Automatically discover in the WebUI Ollama draft, debounce endpoint edits, retain manual retry, and discard responses after endpoint/provider changes or unmount. Use the existing daemon discovery endpoint and shared diagnostics, without a new wire schema.
4. Run affected crate tests with nextest, WebUI tests/typecheck, `bash gates/compile.sh`, `cargo fmt --all` and its check. Review diff and Cargo.lock without touching unrelated edits.

Acceptance: running daemon lists models; stopped local daemon explains `ollama serve` on the atomcode host and port 11434; remote endpoints get remote guidance; HTTP failures retain their meaning; retry works without leaving the form; late results do not alter replacement drafts.

## Validation

- `cargo nextest run -p atomcode-capabilities -p atomcode-tui -p atomcode -p atomcode-daemon --lib`: 4,621 passed, 1 skipped (including the audit regressions).
- Discovery audit regression run: 7 passed (direct refusal, timeout, HTTP/HTTPS proxy refusal, bypass policy, and listing metadata).
- WebUI `npm test`: 385 passed; `npm run typecheck` and `npm run build` passed.
- `cd webui && node scripts/provider-ollama-e2e.mjs`: passed automatic discovery, startup guidance, in-place retry, stale address/provider response rejection, and no implicit writes. Screenshot inspected at `/tmp/atomcode-ollama-guidance.png`.
- `bash gates/compile.sh`: passed workspace/all-target and feature compilation, including negative controls.
- `cargo fmt --all -- --check` and `git diff --check`: passed. Cargo.lock unchanged, prohibited package names absent.

TUI deliberately retains the explicit account confirmation: its read-only probe displays availability/model count, and confirmation opens the existing model picker using that inventory. WebUI shows the picker directly. Tests use controlled HTTP endpoints; no installed Ollama daemon or model was required. Chat request error wording remains outside this configuration change.

## Pre-commit audit

Audited the capabilities/CLI/daemon discovery callers, TUI draft state and renderer, WebUI request identity and effects, localized messages, and browser fixture against `48c08d8f7` on `release/v5.2.6`.

Fixed four issues before delivery:

- Replace deferred WebUI connection cleanup with a layout effect, so an immediate manual discovery cannot be cleared by a delayed effect; cover the manual/automatic debounce overlap in the browser test.
- Count actual mutating requests in the browser fixture instead of the nonexistent `/accounts` path.
- Keep TUI retry visible after an address edit invalidates discovery state.
- Withhold Ollama startup guidance when a proxy may be the failed peer. Real HTTP/HTTPS proxy refusal tests confirm it is indistinguishable from endpoint refusal at the IO layer. The diagnostic uses the existing routing policy, preserving generic errors for uncertain proxy routes, including unhandled CIDR bypasses. Direct endpoints whose path contains `proxy` still get guidance.

No runtime/session lifecycle, persistence format, dependency, version or release configuration changed. No blocking audit findings remain after these fixes and validation.
