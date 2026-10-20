# Goal stage feedback

Baseline: release/v5.2.4 at 4649aafa7ec401825323deb6110f1f6bbbe53fa2, tracked worktree clean; local test artifacts retained.

A completed model reply can be followed by goal evaluation, an unmet verdict and an automatic continuation. A later provider failure is a real failure of that subsequent request, not retroactive failure of the earlier reply. The screen previously showed normal turn completion then a generic execution-stopped error without explaining these stage boundaries.

Reuse existing runtime GoalProgress and host Autonomy events. Add optional host presentation fields for working/evaluating/continuing/recovering/paused and a continuation explanation. The runtime remains the only owner; no new goal lifecycle, retry policy, hooks or session facts. TUI projects stage changes into neutral local notices, scoped to the visible session and deduplicated across clock/retry updates. The continuation explanation is labelled as an explanation, not an independently verified finding. Evaluating and continuing get distinct tip badges. Provider errors describe this model request rather than claiming the whole execution stopped. Existing goal completion reports remain the only successful goal-ending notice. Terminal statistics summaries omit the active stage and pause reason; a satisfied or ended goal is not projected as paused.

Verify host projection, normal reply/evaluation/continuation/failure-recovery/pause presentation, duplicate suppression, background isolation, old-wire defaults, related crate tests, compile gate and formatting. No real Lumen or Windows terminal test is implied.
