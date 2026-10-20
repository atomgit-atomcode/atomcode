# Goal recovery status presentation

Baseline: release/v5.2.4 at e3186e06ebc8946f0f85f3b8d81c42a550febbb0. Existing pending-question and task-duration changes are retained.

The runtime owns goal state, recovery timers and pause reasons. CLI projects GoalProgress into host-api Running; TUI renders that sample. No new runtime lifecycle or retry policy is introduced.

Previously CLI populated Running.paused with recovery explanations, so the tip row labelled automatic recovery as paused. Reserve paused for inactive goals, retain recovering for active recovery/evaluation, and add a backwards-compatible optional retry_in_secs sample. TUI subtracts elapsed local sample time for display only; reaching zero displays recovering and never submits a retry itself. Actual GoalChanged events override the display. Legacy hosts with recovering plus paused remain displayed as recovering.

The composer tip shows active, retrying with remaining seconds, recovering, or paused. Pauses preserve their reason in the available tip space and link to /goal report. Existing notices take priority. Narrow terminals retain the short status badge where possible. Goal report remains the authoritative detailed explanation and recovery guidance.

Verification: projection tests distinguish active recovery and true pause; rendering tests cover countdown, recovery, pause reason and return to execution. Run affected crate tests, workspace compile gate, formatting and diff checks. Real model/network recovery and Windows terminal rendering are not exercised by these tests.
