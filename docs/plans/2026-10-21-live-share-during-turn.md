# Live sharing during a running turn

Goal: `/webui`, `/sync`, and `/app` initialize without waiting for turn completion.

Runtime remains the sole lifecycle owner. Its sequenced output maintains a read-only view consisting of a complete baseline snapshot and subsequent events. This is presentation data, never a persistence or resume source. Session replacement reseeds from the native session projection; invalid/missing seeds fail closed. Existing snapshot/undo semantics remain unchanged.

1. Add a runtime presentation view, updated before each sequenced event is delivered. Bound retained events and refuse unavailable/stale generations.
2. Add an explicit hub binding entry point for a view captured during a turn. Seed replay under the hub lock, reject mismatched generation and replacement of an active binding. Preserve ordinary idle-only binding.
3. Serialize shared binding installation with host publication; replay through the captured sequence so delayed duplicate events are discarded, then deliver newer events normally.
4. Test running-turn reads, replay/sequence boundaries, requests, completion, and session invalidation. Run coding, daemon, CLI tests, compile gate, formatting, and lock checks.

Implementation notes:
- Native session projection supplies startup and session-replacement baselines. Missing data refuses live initialization; it never supplies an empty replacement conversation.
- The view retains unresolved request cards and Goal state across terminal snapshots. Adjacent text/reasoning chunks are merged without changing their content or final sequence. A 16,384-event replay bound refuses partial initialization; a subsequent complete snapshot recovers it.
- Same-conversation harness provider patches carry the projection and pending requests into the new generation. Rebuild/undo reseed privately, before replaying input. These private view updates do not consume or emit public event sequence numbers.
- Host publication and initial binding share a lock. The hub installs baseline plus replay atomically and records the captured sequence. Rejected delayed events must also skip host-side session-handover effects.
- Runtime handle Debug exposes status only, avoiding implicit printing of the retained conversation.

Validation includes a real runtime whose slow provider is still running: live view returns immediately while the strong snapshot waits, a model patch preserves the running turn, cancellation completes, and a fresh session exposes no old conversation. Hub coverage checks active binding, pending approval, duplicate sequence rejection, binding-id remapping, and stale-generation refusal. Real browser launch has not been exercised.

Per-crate verification: coding 1140 passed (9 pre-existing ignored scenarios), daemon 356 passed, CLI 589 passed. One intermediate CLI run hit the timing-sensitive `bg_is_refused_while_a_question_waits`; the subsequent standalone complete CLI run passed. A combined coding+daemon test invocation enabled AtomGit and conflicted with a product-identity test; the supported per-crate invocations above passed. No retry policy or unrelated test was changed.

Final compile gate, `cargo fmt --all -- --check`, and `git diff --check` passed. Cargo.lock is unchanged and contains none of the private overlay dependency names.

Audit follow-up: automatic compaction must preserve the active-turn start marker while replacing history. The dedicated regression verifies that old text is not replayed and the running turn remains visible.

Post-audit complete per-crate runs: coding 1141 passed (9 ignored), daemon 356 passed, CLI 589 passed.
