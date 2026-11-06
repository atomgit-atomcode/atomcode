# Hooks Browser Implementation Plan

**Goal:** TUI `/hooks` and WebUI dialog for configuration inspection, paths, diagnostics and explicit script tests.

**Architecture:** Keep execution in the existing capabilities hook runner. Add a shared host-local inspection service, preserving configuration entry names and disabled/unknown-event diagnostics without changing live hook dispatch. CLI host and daemon call that service; no runtime command, new lifecycle hook or second state owner. The catalogue describes current disk configuration, not a claim about an already-mounted runtime snapshot. Test IDs bind the displayed definition and working directory; stale definitions, disabled/invalid entries and untrusted plugins cannot be tested. Plugin trust and execution validation remain unchanged.

**Tech Stack:** Rust capabilities/host-api/CLI/TUI/daemon; Preact WebUI.

Baseline: release/v5.2.6 @ bbf064a79; unrelated recovery/crypto changes are dirty and must be preserved. Inspected CLI `handle_hooks`, capabilities `cc_hooks`, plugin loader/trust and frontend command dispatch plus recent history. No runtime ownership changes.

1. Add shared inspection DTO/JSON catalogue and explicit diagnostic execution using existing parser, plugin trust and executor. Cover invalid/disabled configurations, exact selection, stale IDs, no automatic execution and test output.
2. Expose host command/reply and daemon GET catalogue/POST test. Reuse the synthetic test payload in CLI.
3. Add TUI selection/review/test flow and WebUI modal with matching Chinese/English copy. Opening or inspecting never executes a script; testing is labelled as real execution, capped to 30 seconds.
4. Run affected nextest tests, WebUI typecheck/build/tests and browser fixtures; serialize compile gate with Rust test discovery, then format/check. Check Cargo.lock before any commit, preserving unrelated overlay changes.
