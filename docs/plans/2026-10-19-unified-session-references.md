# Unified session references implementation plan

**Goal:** Preserve file mentions and add historical session references to the TUI and WebUI @ picker on all platforms.

**Architecture:** SessionManager remains the authority for historical reads; CodingRuntime owns submit and resolves references before changing live state. References are immutable, bounded excerpts passed through the existing SendMessageWithContext boundary, never peer messages or user policy. No persistence schema or kernel command changes.

**Tech Stack:** Rust capabilities/coding/TUI; Preact WebUI; native session catalogs.

Baseline: release/v5.2.4 at 32562a4f7. Main checkout has unrelated edits in CLI webui_relay.rs and daemon live_api.rs plus untracked screenshots. Work in feat/unified-session-references isolated checkout; preserve those changes.

1. Add shared reference spelling/parser and bounded native session resolver. Explicit errors for malformed/missing/newer/delegated/oversized sources. Deduplicate stable bucket/id identities. Read only user/assistant visible text, newest content first within budget, then present chronologically with source/truncation metadata. Never recursively expand references.
2. Resolve on real-user submit, before goal/policy/turn mutation. Use a cancellable bounded blocking read; errors reject submission. Combine context with goal recovery without overwriting either. Preserve existing context-bearing queued-turn semantics.
3. Extend WebUI @ picker with All/Files/Sessions filters, metadata search, project ranking and stable reference selection; retain directory browsing. Selected references can be removed before sending.
4. Extend TUI @ discovery with session candidates and explicit file/session filtering while retaining existing completion keys.
5. Test parser, source bounds, trust provenance and UI replacement. Run affected crate checks/tests, WebUI tests/typecheck/build, fmt, compile gate and Cargo.lock private-marker check. Do not commit unrelated changes.

Failure semantics: missing or unreadable source rejects the submit before a turn starts; never silently fall back or wake a source session. A running source contributes its last committed disk projection only. References inherit existing queued next-turn behavior while another turn is active.

User steering: remove `/peers` as the final interaction. Delete the CLI peer row, tool registration, socket/inbox implementation and their dedicated tests, retaining kernel PeerNote and runtime expected-session delivery because background reports still consume them. Verify no production peer transport references remain.

Runtime seam audit found SendMessageWithContext was not atomic in the harness consumer: a free injection could be claimed by another turn or survive withdrawal of its message. Extend the existing inbox Message envelope with owned context (no new command, hook or persisted fact). The ordinary claim path commits the context with its message; late-injection reads leave it untouched, cancellation removes both. Cover these properties before relying on the seam for references or goal recovery.

Validation (isolated checkout, dedicated build directory): capabilities session-enabled lib 1300 passed/1 ignored; coding lib 548 passed/8 ignored; TUI lib 1526 passed; CLI background lib 8 passed; WebUI 289 passed, typecheck and production build passed. Harness tests: 248 passed with one pre-existing source guard excluded after confirming the exact same failure against the original main checkout (`row_contributions::every_tool_mount_goes_through_the_door`, unchanged `mcp_tool_search.rs:532` inline test registration). The guard was not weakened. Both compile gate branches and their negative controls passed. Final reference-specific tests: 6 passed. fmt/check and diff whitespace checks passed; Cargo.lock unchanged and private markers absent. Windows terminal/browser manual validation remains outstanding.

Pre-commit audit fixes:
- Enforce the reference read ceiling through SessionManager's existing bounded readers for event logs, snapshots and presentation files, rather than a separate metadata peek. Keep default native read limits unchanged. Reject newer format_version metadata before projecting content, including readable older headers.
- Bound source title/path fields in model-visible context so metadata cannot bypass excerpt limits.
- Reserve an internal NUL-bearing menu identity for TUI category actions, which no real filename can match; retain the original FileIndex result set instead of imposing a new 16-file cutoff.
- Track the WebUI session request identity and wait for both categories in All mode, so Enter cannot submit while session discovery is pending or select stale results.

Audit verification: 9 reference tests passed (including both native storage formats, presentation limit, future metadata and output bounds); TUI 1527 passed; coding 548 passed/8 ignored on the isolated baseline; WebUI 289 passed plus typecheck/build. A real headless Chrome smoke test with deferred API responses passed all six checks: pending Enter, reference insertion, chip removal, file completion, session filtering, Escape, with no page errors. No Windows real-terminal run was available. A concurrent goal diagnostics commit 42a09b78e was preserved when integrating into the release working tree.

Release integration validation: on release/v5.2.4 with parent 42a09b78e, `bash gates/compile.sh` passed both compilation branches and both negative controls; coding lib regression passed 552 tests/8 ignored. Final fmt and diff checks passed. Only the 31 audited feature paths are included; unrelated WebUI relay/live API work is excluded.
