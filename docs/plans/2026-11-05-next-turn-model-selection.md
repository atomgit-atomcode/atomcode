# Next-turn model selection implementation plan

Goal: permit selecting a model during an active turn in TUI and WebUI, without replacing the provider used by the current turn; show the current-turn identity separately from the next selection.

Baseline: 63e9fcab3 on release/v5.2.6. Implement in an isolated checkout to preserve concurrent endpoint-allowance changes in runtime.rs.

Architecture: reuse CodingRuntime ReassembleProvider and the existing transactional harness provider patch. The active agent-loop owns its captured provider for the full turn. Add presentation-only current-turn identity when a selection changes mid-turn; clear it at the next idle boundary. Preserve current-turn snapshot attribution until completion. No new runtime owner or pending execution queue. Provider/config preparation failures keep the previous selection. The explicit WebUI selection route permits safe patches; implicit provider reload during message/steer submission keeps existing idle guards.

1. Runtime: add sanitized selection-pending event, retain original current-turn identity across repeated choices, defer snapshot attribution until idle, and cover failure/cancel/next-turn behavior.
2. Drivers: map the pending identity into TUI guidance, retained live projection and additive SSE state; allow explicit WebUI provider selection to use the existing safe patch.
3. WebUI: show selected model and a current/next status; retain active identity across reconnect and prevent late callbacks from leaking across sessions. Unsynced chat preserves its explicit next-request selection.
4. Validate affected crate tests, frontend unit tests/typecheck/build, sync browser regression, formatting, diff and full compile gate. Integrate by three-way merge, preserving unrelated work.

Implementation refinement: every provider patch retains the existing ordered generation receipt. Safe active-turn patches carry the same agent's approval and steer correlations to that generation; unrelated runtime replacements keep their clearing behavior. Image admission queries current-turn identity through the runtime owner rather than a potentially delayed driver projection. TUI receives a dedicated neutral host event; SSE adds a presentation event that is restored on reconnect.

Verification: isolated affected-crate library suite passed 3167 tests (8 skipped); integrated host/model/protocol regressions passed 223 tests, with the final turn-ID-scoping refinement passing the three focused model/approval/cancel tests. WebUI typecheck, 402 unit tests, production build, and 1200px/390px browser tests passed. Browser coverage includes repeated selection, preparation failure rollback, late metadata from the original request, refresh recovery, background streaming, and the next submit carrying the chosen model. Existing mode-binding and session-browsing browser regressions passed. Final integrated full compile gate and formatting checks passed; Cargo.lock unchanged.

The current-turn metadata is scoped by generation and turn ID so autonomous controller transitions cannot carry a previous turn's label into a new turn. Integration used a three-way merge against the baseline and preserved the concurrent quota endpoint fix. The subsequent user-requested audit authorizes committing and pushing the reviewed feature to release/v5.2.6.

Release audit fix: retaining the same generation for active patches would allow older acknowledgements to overwrite a newer concurrent selection. Restored monotonic generations and preserved active requests in both event-first and acknowledgement-first hub ordering. Regression also rejects an old selection acknowledgement and confirms approval response and web-steer correlation remain usable. The pending UI projection is updated only by ordered runtime events.

Final release audit verification: 3168 affected-crate library tests passed (8 skipped), with the monotonic-generation/approval/steer fix included. The full compile gate passed on this final source. Frontend remains at the verified 402 passing unit tests, TypeScript/build and desktop/mobile browser regressions. Cargo.lock is unchanged and contains none of hkdf, hmac or zeroize_derive.
