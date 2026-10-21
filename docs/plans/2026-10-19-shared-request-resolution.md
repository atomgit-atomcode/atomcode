# Shared request resolution implementation plan

**Goal:** A question answered in the terminal disappears from WebUI immediately and stays gone after reconnect.

**Architecture:** CodingRuntime remains the single owner of pending requests. After accepting a response and delivering it to the kernel, it publishes a typed RequestResolved event. LiveViewHub consumes that fact, removes only the correlated pending request from replay, and reuses the existing user_input_resolved wire event. The ordinary chat projector carries the same terminal. No second runtime or request state machine is introduced.

**Baseline:** release/v5.2.4 at 579b63aa6207a51387b7ddc4b054d1109e3d75a8. Existing cancellation diagnostics and TUI lifecycle changes are unrelated and must be retained.

**Failure semantics:** A stale request, stale generation or failed kernel delivery must not publish successful resolution. Hub terminals are idempotent when the WebUI response route already retired the request. Generation/sequence fences apply before resolution; old terminals cannot remove a replacement request. Cancellation, shutdown and session replacement retain their existing authoritative terminal cleanup.

## Steps

1. Add runtime regression tests for accepted responses, duplicate/stale responses and failed delivery; publish the typed terminal only on success.
2. Add Hub regressions for terminal-driven cleanup, reconnect replay, duplicate notifications and stale generations. Consume runtime terminals through the existing resolution helper.
3. Project the terminal on ordinary chat as well as live sync. Correlate runtime terminals to request ids and late HTTP completion to its submitted card instance; remount each new question with fresh editing state.
4. Run targeted regressions, affected coding/daemon tests, WebUI tests/typecheck/build, formatting and gates/compile.sh. Check the final diff and preserve unrelated work.

**Manual validation:** Open a shared terminal/WebUI session, ask a structured question, answer in either UI while the turn continues, reconnect the browser, and verify that an older answer cannot dismiss a newer question.

## Verification

- Four targeted regressions passed: successful resolution, failed delivery, Hub replay/generation handling, and ordinary chat projection.
- Coding and daemon: 1,384 tests passed, 10 skipped after excluding one unrelated existing failure. The first full run exposed `product_identity::the_agent_says_it_is_the_product_that_started_it`: it rejects `AtomGit` anywhere in the prompt, including the existing AtomGit tool instructions. That failure is retained rather than changing product identity in this fix.
- WebUI tests, TypeScript typecheck and production build passed.
- `gates/compile.sh` passed on retry; the initial run's final negative-control stage saw a transient unrelated workspace compilation failure during concurrent TUI edits. A direct workspace check passed immediately afterward.
- Real terminal/browser interaction remains a manual verification item.
