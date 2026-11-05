# Context summary count

Baseline: release/v5.2.6 at ecbf58c8d, clean worktree. Approved UI: append the
successful summary count to existing token usage; never put in-progress status
in the footer. Keep existing runtime progress and transcript summary notices.

Authority: persisted SessionEvent::Compacted summary boundaries, not transient
AgentEvent::Compacted (which also reports tool rewrites/no-ops). Derive count from
sequenced facts; replay must not increment it independently. Count recorded summary
checkpoints, including inherited/imported checkpoints actually present in the log;
unknown pre-import history cannot be reconstructed. Historical summaries remain
historical after undo. No metadata counter or lifecycle hook is introduced.

1. Add a shared event-log summary count projection and tests (dedup by sequence,
   no count for tool rewrites or empty/no-op boundaries).
2. TUI: derive footer count and numbered transcript notices from the same facts.
   Existing start/finish live status stays in its current position.
3. Daemon: piggyback absolute count on session detail/live snapshot and a summary
   SSE update after committed compaction. Read the native log, never guess from
   the number of transient events. Failure to read means unknown, not a fabricated 0.
4. WebUI: session-scoped absolute count, reset on switch, restore on load/reconnect,
   show beside existing tokens; no in-progress footer and no extra HTTP requests.
5. Verify projections, rendering, transport, replay/reset, relevant nextest suites,
   WebUI tests/typecheck/build, compile gate, fmt and lock hygiene.

Implementation notes: the runtime normalizes raw AgentEvent compaction events into
CodingRuntimeEvent::CompactionStarted/Finished; daemon presentation consumes that
existing seam, with no new runtime owner/event or compaction hook. A changed cwd
resolves the original native storage bucket rather than inventing a fresh count.

Validation: 2,649 kernel/TUI/daemon tests passed; the final five focused projection
and rendering regressions also passed after terminal/deduplication review. WebUI
380 tests, TypeScript check, production build and real-app browser checks passed
at 1200px and 390px (count beside tokens, progress outside footer, duplicate
completion, latest-record navigation, reconnect and fresh-session reset).
Browser captures: /tmp/atomcode-context-summary-1200.png and -390.png.
No Cargo.lock/dependency change. Existing compaction lifecycle and durable event
format are unchanged; additive browser DTO fields/events carry presentation only.
Final compile gate, cargo fmt check, diff whitespace check and lock hygiene passed.

Pre-commit review: count lookup checks catalog ambiguity even if a different
session with the same ID exists in the current cwd. Added duplicate-bucket
coverage. Unknown/late history readings preserve the latest known browser count;
only an explicit session reset clears it. Browser and unit regressions cover this.
Review validation: 2,649 Rust tests, 380 WebUI tests, typecheck, build, browser
checks, compile gate and formatting passed. Cargo.lock remains unchanged.
