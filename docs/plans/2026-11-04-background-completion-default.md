# Default background completion delivery

Baseline release/v5.2.6 at 03ec69917; first-stage polling changes and unrelated proxy/TUI changes remain in the shared worktree.

Reuse BackgroundJobs owned by CodingRuntime and the capabilities process store. Completion observation defaults on for bash_start/pwsh_start; explicit false preserves manual polling and services/watchers. Generation replacement and cancellation retain existing retirement behavior. No second process or runtime owner is introduced.

Deliver only between kernel turns and without pending requests, so completion cannot interrupt streaming or approval. Before delivery, check that the process store still holds collectable output; terminal polling removes the entry and suppresses redundant wakeup. Include a sanitized, bounded output tail without consuming output. Existing goal/loop cadence and policy failure handling remain authoritative. Non-supporting tool hosts continue to collect via polling.

Verify default registration, explicit opt-out, generation retirement, cancellation, completion collection suppression, existing goal/loop tests and workspace compilation.
