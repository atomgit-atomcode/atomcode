# Explain approvals and save exact project rules

Baseline: release/v5.2.6 at 3441bc522; the first-stage loop changes are already
present in the worktree and are retained. Review current approval, JSON policy
loading, native mount, daemon chat/live and TUI/WebUI callers before modification.

Reuse existing approval reasons; the daemon currently replaces them with a generic
message. Keep rule provenance on parsed policies and expose matching/uncovered
command information. Persistent approval is an explicit choice with an exact
proposed rule and file shown before confirmation, available only for eligible Bash
requests from the native harness approval asker. A coding-owned injected service writes project
settings; capabilities owns no product paths or filesystem persistence.

The service binds the proposal to the requesting project's cwd and command. Save
before approving once; reject stale roots, denies, malformed/symlinked settings and
write failures. Preserve unrelated JSON and deduplicate grants. Saved settings take
effect on a normal rebuild/new session, avoiding any reload of an outstanding
request. This action approves only the pending call; it does not add a session
grant that could survive a project change.

All other approval gates keep their security floors and expose no save action.
No new runtime owner or sandbox. Verify policy explanation, file writes and failure
paths, TUI responses, daemon chat/live forwarding and WebUI rendering, then affected
tests, compile gate and formatting.

Validation: 5 affected Rust crates passed 5,986 tests (10 skipped); WebUI passed
379 tests, TypeScript checking, production build and browser checks at wide/narrow
viewports, including rejected delivery/retry and exact call correlation. Policy
tests cover config preservation, duplicate saves, deny changes, malformed files,
symlinks and stale project proposals. Saved rules are not a filesystem sandbox: a
test/build command can execute project code with its existing process permissions.

Final compile gate, formatting check and diff whitespace check passed. Cargo.lock
is unchanged; forbidden private-overlay package counts are all zero.
