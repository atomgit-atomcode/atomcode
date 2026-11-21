> Historical: the built-in `/computer` integration described here has been removed. Current browser setup and automatic use are tracked in `docs/plans/2026-11-12-browser-default-and-computer-retirement.md`.

# macOS computer-use backend integration plan

**Goal:** Give the CLI an explicit, reversible way to connect a local desktop MCP executor and its usage skill, using the image pipeline from phase one.

**Architecture:** `atomcode computer` is a driver-local configuration and diagnostic command. It does not own an agent, call a second LLM, or reconfigure another process's live runtime. The normal MCP path remains owned by CodingRuntime. The backend is an independently installed `open-computer-use` executable; no runtime download or global installation runs on startup.

## Baseline and backend decision

- Existing phase-one edits are retained. Branch `release/v5.2.7`, HEAD `1d946853286c98d2e7ec05b3b19b22ef18edb5bb` at the start of phase two.
- Peekaboo currently requires macOS 15; this workstation is macOS 14.5. Open Computer Use's published 1.3.0 package supports macOS 14 and was inspected without installing a global MCP entry or granting OS permissions. npm integrity matches the published SHA-512; the bundled app signature verifies and both Mach-O slices declare macOS 14.0 as their minimum.
- Upstream: https://github.com/opensymph/open-computer-use (MIT). Recommend an explicit pinned install: `npm install -g @opensymph/open-computer-use@1.3.0`. Do not copy its private/native third-party app bindings or invoke its autonomous agent loop.

## Implementation

1. Add `crates/atomcode-cli/src/computer_cmd.rs`, wired into `main.rs` as `computer enable/status/doctor/disable`.
2. Resolve an already-installed executable (optional explicit path), canonicalize it, and run a bounded `doctor --no-ui` probe. Require schema version 1, `kind=doctor`, `ui=false`, and boolean Screen Recording/Accessibility observations. Missing permissions or a bad probe prevent enable before config writes.
3. Under a setup lock, merge a managed `computer-use` stdio entry into user MCP storage, preserving unrelated entries. Refuse unmanaged name collisions, comments on rewrite, customized skill collisions and non-regular files. Keep `trust=false`, no autoApprove, and explicitly disable known foreground/global input opt-ins inherited from the environment. AtomCode's regular approvals remain in force.
4. Install AtomCode-authored guidance only on explicit enable. Write it before the MCP config; an uncertain config commit reports failure and retains the identical skill for inspection/retry rather than guessing at rollback. Disable changes only the owned entry and removes only the exact bundled skill. Report configuration scope truthfully: external running sessions require reload; disabling is not an emergency stop for dispatched GUI operations.
5. Add tests for CLI dispatch, permission/schema failures, executable resolution, bounded subprocess diagnosis, ownership collisions, preservation, idempotence, no-write status/diagnosis, and disable behavior. Use a fake executable and a temporary product tree; no test drives the user's desktop.
6. Document installation, OS permissions and usage in both languages. Verify with real read-only permission diagnosis, focused/full affected tests, compile gate, format and lock checks.

## Scope boundary

This step connects an executor; it does not claim app-level authorization, cross-agent desktop exclusivity or remote cancellation guarantees. Those require a backend/host protocol decision and remain a separate lifecycle phase. No GUI input or capture of unrelated user content is performed during implementation.

## Verification and scope at delivery

- Added 12 helper/CLI parsing tests and two independent-process startup tests. The latter caught a real startup side effect: the old launch path created a log before local diagnosis. Valid `computer` commands now dispatch before file logging, installation bookkeeping and pending upgrades.
- `cargo nextest run -p atomcode --bin atomcode --test computer_setup`: 158 passed. Full CLI regression excluding two investigated navigation/rewind failures: 646 passed, 2 filtered out. See baseline comparison below; those paths are not changed by this integration.
- The compiled CLI was run against the inspected real backend with `doctor --no-ui`, in an isolated product directory. Both Accessibility and Screen Recording are absent on this workstation. `doctor` and `enable` exit with an explicit error and create no product directory. The real backend was not installed globally, granted permissions, or asked to capture/input anything. A fake backend exercises successful enable/status/disable with the compiled CLI in isolation.
- Final workspace/feature compile gate and its negative controls passed. Formatting, diff and forbidden lock-package checks passed. No dependencies were added for this phase.
- Real screenshot/action verification remains pending the user's OS permission grants. App-scope enforcement, multi-agent exclusivity and interruption of dispatched remote actions remain outside this phase; the CLI and skill explicitly describe that boundary.

### Clean-HEAD comparison

A detached worktree at `1d946853286c98d2e7ec05b3b19b22ef18edb5bb`, with an independent Cargo target directory, reproduced both excluded failures without any feature edits:

- `tui_bg::shared_navigation_to_a_missing_project_keeps_the_old_foreground`: the removed target project was accepted (`tui_bg.rs:1906`). The same assertion failed in the modified-tree full run.
- `host::a_turn_stopped_before_any_answer_can_be_undone_by_its_number`: `Err(RewindPointNotFound { turn: 2 })` instead of `Ok(Undone { ... })` (`host.rs:1424`), matching the previously observed baseline failure.

Neither path is changed here. Full CLI verification is therefore **646 passed with two known baseline failures excluded**, not an unconditional green full suite. Logs from this run: `/tmp/atomcode-computer-cli-rest.log`, `/tmp/atomcode-computer-baseline-test.log`, `/tmp/atomcode-computer-baseline-rewind.log`, `/tmp/atomcode-computer-startup-tests.log`, `/tmp/atomcode-computer-final-compile.log`. The comparison worktree is removed after verification.
