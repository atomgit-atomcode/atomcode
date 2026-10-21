# Round budgets and text-only closing

Baseline: release/v5.2.4, 8143d3008ce22a8ae27660feff223a2629b3bdaf; clean worktree.

The harness agent loop owns execution; TurnStopping owns policy decisions. No new runtime owner or public event is needed. Disable the separate coding loop fuse (zero means unlimited), retain configured round-cap, child budgets, deadlines and repetition guards. A MaxRounds stop with outstanding tool results/truncation gets exactly one text-only closing request. Do not consume queued user input during closing. Record the injected instruction and response in the authoritative session log. Suppress tool schemas and request tool_choice none, and reject returned tool calls without execution. Cancellation, provider errors and timeouts retain existing terminal paths. Other stop reasons get no closing request. Successful closing retains MaxRounds and counts its extra model round. No automatic retry after closing.

Validate unlimited execution past 300 rounds, explicit budget closing, dishonest tool calls during closing, cancellation/provider failure during closing, child budgets and existing goal/loop tests. Run affected crate suites, format and compile gates.
