# Stream content deadline implementation plan

Baseline: 8e4a042c; pending TUI edits are preserved. CodingRuntime remains the sole lifecycle owner and supplies the first-content and idle budgets to its agent-loop row. No session format or retry state machine changes.

The existing harness used the idle budget before the first content, ignoring the host first-token budget. Provider byte watchdogs also switched phase on heartbeat bytes. Use one shared semantic deadline implementation in capabilities for both transport and harness: text, reasoning, opaque reasoning and tool data count as progress; empty chunks and metadata do not. First deadline is absolute from stream acquisition, idle deadline from last content. Keep the legacy row fallback when no first-content budget is supplied and preserve zero/disabled settings.

Verify late first content, first-budget exhaustion despite metadata, idle reset by reasoning and tools, cancellation, and existing bounded recovery tests using paused Tokio time. Run affected crate tests and compile/fmt gates. Record timeout stage/budget/silence and recovery terminal outcomes without logging content or credentials.

Validation: 275 provider tests, 248 harness tests and 1007 coding tests passed in an isolated worktree, including 102 runtime criteria and the row-wiring test. Paused-time tests cover late first content at 200 seconds, heartbeat/metadata budget exhaustion, idle preservation, unfinished tools, cancellation and ready events after expiry. Compile/fmt/diff gates pass. nextest is unavailable locally, so Cargo test was used. The affected Windows gateway has not been retested. Pending TUI edits are excluded.

Audit: provider and harness share the deadline-aware wait, including an explicit expiry check so ready metadata cannot bypass the deadline. Phased timeout errors remain classified as stream timeouts in telemetry. Validation is repeated in an isolated worktree outside the OS temp root, containing only this change.
