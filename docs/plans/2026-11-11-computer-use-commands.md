> Historical: the built-in `/computer` integration described here has been removed. Current browser setup and automatic use are tracked in `docs/plans/2026-11-12-browser-default-and-computer-retirement.md`.

# Computer Use session commands and automatic skill context

Baseline: release/v5.2.7 at 1d946853286c98d2e7ec05b3b19b22ef18edb5bb; existing uncommitted image/backend work is preserved. Recent command, MCP pause/resume and skill selection history and their current callers were checked before changes.

The CLI host remains the owner of local setup. Move its existing computer service into the CLI library so terminal and HostControl callers use the same implementation, returning text rather than writing to TUI stdout. Add addressed HostCommand::Computer with a closed action enum and use DoneWithNote; unsupported hosts fail explicitly. No new kernel command, runtime owner, process shell or credentials in protocol data.

The TUI contributes /computer status|doctor|enable|disable|reload|help with bilingual help/completion. Configuration mutations do not pretend to change a running connection: /computer reload uses the existing Reload command and its busy/failure semantics. Backend path overrides remain terminal-only. Enable still needs preinstalled software and granted permissions, and disable cannot stop dispatched external operations.

Add explicit auto-load: computer-use metadata. Classify clear desktop操作 requests conservatively using existing quote/code exclusion, global user opt-out and static context expansion. Reject documentation/questions/code implementation and explicit refusal of desktop operations. Resolve at most one matching policy; ambiguous skills skip. Do not run shell expansion or activate allowed-tools. Only installed skill metadata opts in; disabled automatic loading and explicit skills retain priority. Guidance may explain missing/disabled MCP, but never enables it automatically.

Verify parser/selection positives and negatives, addressed host dispatch and TUI help/invalid args, automatic runtime context without tool grants, existing debugging selection, terminal startup invariants and relevant affected tests. Run compile/format/lock gates. Keep the two documented baseline CLI failures separate from feature results. Do not commit or push without a new request.

## Implemented checks

- Protocol tests cover all four ComputerAction values across serialization and addressed-session checks. TUI tests cover menu options, strict argument rejection (including executable overrides), host errors, no implicit reload, and busy refusal for explicit reload.
- The local service now returns text to its caller. Terminal calls print it; HostControl returns a display note. The host uses the runtime configuration's ProductDirs, refuses a mismatched session before launching anything, and never rebuilds the runtime for setup/status.
- Selector tests cover English/Chinese desktop actions, quoted/fenced material, questions, code implementation and automation scripts, negation, no-auto-skills, absent/disabled skill metadata, ambiguity and compatibility with existing debugging selection. A runtime test observes guidance before a tool action and proves that a risky tool listed in allowed-tools still asks for approval.
- `cargo nextest run -p atomcode-host-api -p atomcode-tui`: 1967 passed.
- CLI lib/bin, host and independent-process setup tests: 540 passed, with the previously reproduced baseline rewind failure excluded.
- Capabilities with `mcp,skills`: 1604 passed, 1 skipped. After the final selector compatibility refinement, all eight auto-load tests passed again.
- Coding automatic-skill runtime suite: 8 passed. Workspace/feature compilation and both negative controls passed. Format/diff/lock checks passed; no forbidden lock packages or index flags were introduced.
- Expanded coding/harness testing also exposed the old exact-three-second assertion in `coding_rate_limits_share_the_attempt_and_wait_allowance`; the production retry loop already applies jitter. Its clean-HEAD comparison and the remaining full-suite result are recorded below.

Actual desktop screenshot/input acceptance remains untested because this workstation has not granted the backend its OS permissions. No installation, permission grants, desktop actions, commits or pushes were performed by this task.

### Expanded-suite baseline comparison

The retry timing failure was reproduced independently on detached clean HEAD `1d946853286c98d2e7ec05b3b19b22ef18edb5bb`: elapsed 2.578s versus the test's exact 3s expectation (`host_rows.rs:4853`). The modified tree failed the same assertion with 2.581s. Neither this test nor the jitter implementation is modified here. The comparison used `/tmp/atomcode-computer-baseline-target`; its worktree was removed afterwards. Evidence: `/tmp/atomcode-computer-baseline-retry.log`. Do not characterize the unfiltered full suite as green.

### Final expanded result

Coding/harness remainder: **1634 passed, 5 failed, 12 skipped** (the skipped set includes the one confirmed exact-three-second baseline failure excluded from this run). The five additional failures all reproduced on clean HEAD in an independent target directory:

- `recovery::mixed_failures_share_six_attempts_and_keep_partial_progress`
- `recovery::mixed_failures_share_the_wait_allowance_without_shortening_retry_after`
- `recovery::ordinary_request_recovery_is_finite`
- `recovery::ordinary_requests_survive_a_short_gateway_outage`
- `loop_policy::the_guard_tells_a_repeating_todo_update_that_the_same_status_will_not_change_again`

The four recovery failures compare exact timings despite existing jitter; the todo test expects an update on nonexistent todo id 4 to report completion. These assertions and their implementation paths are unchanged. Thus six expanded-suite failures are confirmed baseline issues, not passing tests. Logs: `/tmp/atomcode-computer-slash-runtime-rest.log`, `/tmp/atomcode-computer-baseline-expanded.log`. The two restore/undo tests marked slow in the modified-tree full run both passed; an unnecessary extra baseline comparison for them was stopped and is not counted as verification. The comparison worktree is removed.

All new command dispatch, session addressing, classifier and static-injection/approval tests passed. No baseline tests were weakened or modified, and no golden files, gates or build profiles were changed.
