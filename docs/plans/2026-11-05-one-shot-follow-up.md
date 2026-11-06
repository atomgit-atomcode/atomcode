# One-shot same-session follow-up

Baseline release/v5.2.6 at c4c97bc44. Reuse CodingRuntime's accepted-operation owner, existing kernel turn completion/snapshot boundary and normal input commands. No new lifecycle hook, scheduler, session writer or persistent workflow schema.

Runtime-owned registration binds to the current accepted turn, or the next accepted human submit. Consume before dispatch. Only normal Stopped with no pending request/background work/policy/persistence failure can dispatch. Treat the user-authored preset as a marked user input, not a synthetic autonomous reminder, so execution restrictions and history attribution remain correct. Keep the accepted operation open until the second kernel turn settles; aggregate statistics and duration. Clear on cancel, generation changes, errors, or superseding human input. Reject coexistence with goal/loop.

Expose user-only /followup set/show/clear in TUI/WebUI and --follow-up in the main CLI. Runtime controls validate session and generation; live management also rechecks binding after awaiting. No model-facing registration tool. Keep approval/mode unchanged and mark automatic input visibly. Registration is ephemeral and bounded to 16 KiB; no business-success inference, restart recovery or cross-session chaining.

Verify ordinary completion once, failure/cancel/budget exclusions, stale session/generation, explicit clear and new-input priority, goal/loop exclusion, actual same-session request history and per-turn permission restrictions; preserve existing driver and terminal tests.
