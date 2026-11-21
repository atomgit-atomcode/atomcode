# Background wait guidance by host

Baseline release/v5.2.8 at 889aea9a2; two unrelated docs/plans edits remain in the shared worktree.

Evidence: session 810ff57c spent 92 of 204 rounds in `bash_poll` waits (41% of prompt tokens) while four external agents ran. Completions are delivered only between kernel turns (`runtime.rs`, `!kernel_turn_open`), so a model that keeps polling delays them; the first turn's end then delivered three stale notices one turn each. The host prompt said to end the response, but `bash_start`'s result showed a `wait_ms: 60000` poll and `bash_poll` said to repeat until exit.

Not every driver can take a turn a completion starts: `-p` breaks on its first `TurnFinished`, and ACP only forwards prompted turns. Telling the model to end its response there would lose the result.

Change, without touching turn lifecycle or delivery timing:

- `background_wakeups` (agent and runtime config, default false) reaches the `capability-commands` row through `HostState`. The interactive TUI sets it (startup and `ConfigFile` reloads) and so does daemon `/live`, whose hub shows turns it did not submit; `-p`, ACP, `/bg` and `/chat` keep false. With it the policy says to end the response and collect with `wait_ms: 0` after the notice; without it, to wait with `bash_poll` and collect jobs before answering. `describe_self` follows the same split and notes that replacement drops pending notices.
- `bash_start` and `bash_poll` text defer to that policy instead of contradicting it. The `Started background job <id>. Collect output with bash_poll ` prefix parsed by `discipline::started_job` is unchanged.
- Delivery takes the completions that have already happened (`BackgroundJobs::ready`, no timer) into one notice; the batch shares at most 8000 characters of output tail, with a cap of 2000 per job. A job stopped with `bash_kill` is left out, unless an active goal is parked on background work, which still needs the wakeup.
- Without `background_wakeups` the runtime opens no turn for a completion either: unless a goal is parked on it, the notice rides the next input, as under `/loop`.
- A full job store first reclaims jobs that ended after `bash_kill`. If output is already gone, a goal waiting on the job still receives its terminal status.

Not done here: mid-turn injection, `-p` waiting for observed jobs before exit, a longer `bash_poll` cap (needs steer-aware early return), re-registering jobs across runtime replacement, and a real WebUI run of a completion-started turn (only the hub code was read).

Verification: new `runtime::tests::completions_that_already_happened_arrive_together_without_killed_jobs`, `without_wakeups_a_completion_rides_the_next_input`, the killed-job case of `a_goal_waits_for_explicit_background_completion_instead_of_spinning`, `background_jobs::tests::ready_takes_only_completions_that_already_happened`, capabilities `a_full_store_makes_room_only_from_ended_killed_jobs`, criterion `the_model_is_told_how_this_host_waits_for_background_work`, and the daemon `/live` config assertion; atomcode-coding 1385 passed; capabilities bash tests 198 passed; `bash gates/compile.sh` passed; fmt check and diff check clean; prohibited lock packages absent.
