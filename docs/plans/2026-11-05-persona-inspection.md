# Persona inspection

Baseline release/v5.2.6 at 19a8d116a. Preserve unrelated staged model-card/i18n changes and proxy, file-index, Windows dependency edits in the shared checkout.

Add /context persona to the existing context command. CodingRuntime reads the mounted persona-atomcode row and PromptRegistry fragment, never reconstructing the displayed text from configuration. Return source (builtin/config/explicit/host), nullable append status and the exact selected persona fragment only on explicit context inspection. Full /context prompt and ordinary budget reads retain their existing meanings.

TUI carries the optional metadata through the existing host context reply, defaulting to absent for old hosts. WebUI uses an authenticated, session-scoped live read, checks binding/generation again after awaiting the runtime, and discards responses after view changes. Offline or mismatched viewed sessions fail visibly instead of showing an inferred persona or rebinding the runtime.

English and Chinese labels cover all sources and unavailable state. Tests cover effective source across model switches/rebuilds/resume, exact fragment vs full system prompt, no metadata on budget-only reads, and API session scoping/error propagation. No prompt behavior, persisted session format or runtime owner changes.
