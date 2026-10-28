# Shared background session navigation

Baseline: release/v5.2.5 `88e947b3d`; tracked worktree clean. Preserve unrelated
untracked work. Background owns the slot table; each CodingRuntime continues to
own its agent, session lease and pending requests. The daemon live hub owns only
the shared presentation and remote-control binding.

Unopened automatic reviews are hidden from ordinary pickers while the background
owner is alive. Keep its existing Manual disk origin until its result is
committed to the parent, so process loss exposes undelivered work for recovery.
Opening it promotes it through the existing take_up path. No session format or
new persistence source is introduced.

Route embedded WebUI session navigation to the same Background host used by
TUI. A host-owned background runtime is foregrounded rather than resumed/forked.
Replace the shared view with an expected-binding check before committing the
slot transition. Waiting approvals and reconfiguration still reject switching;
failure leaves the old foreground and binding intact. Tag event publication by
runtime identity under the sharing lock so late old-runtime events cannot enter
the new binding. In-flight incoming view events use the existing sequence
watermark. Notify the old stream of the session change before it reconnects.

Validation must cover pending and delivered visibility, explicit access,
shutdown recovery, shared running review navigation from both frontends,
approval/reconfiguration rejection, late events, and failed handover. WebUI
history loading should wait for a successful synchronized switch and discard
superseded navigation results. The reported OOM remains unconfirmed.

## Verified behavior

The existing Background is registered weakly with the embedded daemon for
navigation and live review visibility; no second agent or session owner is
created. Independent processes retain the existing busy-session fork behavior.
Manual background tasks keep their previous history behavior. The visibility
filter applies to the owning TUI and its embedded/shared WebUI, not a separate
daemon that has no connection to that background owner.

The driver commit executes synchronously under the hub's replacement lock;
remote commands cannot see a replacement binding before its foreground slot
is committed. Publication checks runtime identity under the sharing lock, and
the staged view's sequence watermark deduplicates captured events. Starting a
new share captures the current runtime under that same lock. Pending requests
also block a handover before the runtime status watch has caught up; continued
goal questions remain nonblocking.

Validation: 960 CLI/daemon tests, 56 final targeted sharing/background tests,
327 frontend tests, workspace compilation gate, formatting, frontend typecheck
and production build passed. Tests exercise active navigation through both
entry points, no fork for an existing background runtime, unopened-review
visibility and recovery, explicit promotion, approval rejection, late events,
failed driver commit, and superseded history fetches. Actual user OOM diagnosis
still needs its original error and memory context.

## Pre-push audit

Navigation checks the runtime's bounded pending-request control projection,
without cloning transcript data. This avoids treating the TUI's cached request
as still pending after WebUI has answered it, including when transcript replay
is too large to serve. Switching already committed by the driver is reported
from its binding/identity, not a second full-replay read that could fail after
the commit.

Embedded browser navigation runs to completion independently of an HTTP client
disconnect. Idle parents with undelivered background results are retained in
slots until their result can be committed; ordinary idle history runtimes are
retired rather than exhausting the background-slot limit. Regression tests
cover a browser disconnect during preparation, a WebUI answer followed by a
switch before turn completion, and the result fact committed to the original
parent after navigating to another history session.

Final audit validation: 2,150 CLI, daemon and coding tests passed (9 skipped),
and the recovery scenario passed 10 consecutive runs. All 327 frontend tests,
the workspace compilation gate, formatting, frontend typecheck and production
build passed.
