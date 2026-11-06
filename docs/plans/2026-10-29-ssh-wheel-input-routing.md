# SSH mouse wheel and input history

Baseline: release/v5.2.6, a796adf14; clean before this change.

Report: Windows 11 terminal, either cmd.exe or PowerShell launching SSH to Ubuntu
22.04. Keyboard Up/Down recalls history correctly, but the wheel moves both
history and the transcript.

Cause: all SSH connections default to terminal-owned mouse. Alternate-screen
wheel translation can produce the exact bytes of keyboard arrows. The 4ms burst
heuristic separates multi-arrow runs, but SSH may split one swipe into bursts and
single arrows. Increasing the threshold cannot reliably distinguish a keyboard.

Use the existing real mouse reporting path by default for ordinary SSH, subject
to ui.mouse, --no-mouse, the existing reporting capability and ATOMCODE_MOUSE
explicit override. A confirmed tmux mouse=off disables application capture,
including later settings/toggle requests, for this launch. Explicit PuTTY and
detected browser terminals retain their
native-selection preference. No client-shell inference (cmd vs PowerShell), new
configuration, input state machine, runtime or persistence change is needed.

With app mouse reporting, wheel events scroll the transcript (or a pointed-at
scrollable panel); they never recall composer history. Up/Down keep their normal
caret/history routing. Shift+drag selects using the client terminal where
supported; Ctrl+G switches ownership. Native mode remains protocol-ambiguous:
if a terminal translates its wheel into arrows, use Ctrl+G to capture the mouse,
or start on Ubuntu with ATOMCODE_MOUSE=1 atomcode (with ui.mouse enabled).

Tests: fail the ordinary-SSH startup policy against the prior default; retain
explicit/browser/PuTTY overrides. Drive real wheel reports over the composer,
assert transcript scroll plus unchanged draft, then keyboard Up/Down history and
draft restoration. Run TUI nextest, workspace compile gate and formatting check.
Windows Terminal -> SSH -> Ubuntu physical acceptance remains outstanding.

Verification: the new SSH startup criterion failed against the prior policy;
all 1860 TUI tests pass with the protocol/tmux changes, including the real mouse-report versus
keyboard-history regression. Workspace/feature compile gate, rustfmt and lock
checks pass. No Windows/SSH terminal was available for physical acceptance.

Codex comparison (local 806d9732c9): its owned transcript captures real mouse
reports, disables alternate-scroll 1007 while captured, requests SGR 1006 and
respects a current-pane tmux mouse=off. Match those protocol boundaries here.
Query tmux only when TMUX/TMUX_PANE identifies it; use a 150ms process deadline,
null stdin/stderr and bounded file output, never another terminal-input reader.
Unknown/failed tmux queries leave the existing capture capability unchanged.
The confirmed policy is cached; after changing tmux mouse, restart AtomCode.

Compatibility limit: a PuTTY client reporting only xterm is indistinguishable
from an anonymous Windows Terminal SSH client. Its original native selection
cannot be guaranteed by a generic SSH default. Use --no-mouse or
ATOMCODE_MOUSE=0 (or ui.mouse=false) for native PuTTY behavior; identified PuTTY
and detected browser terminals keep their startup exception. No server-side
clipboard reads are enabled by this change. Actual device acceptance is pending.

Protocol handoff preserves HarmonyOS's existing native alternate-scroll policy:
a capture disables 1007, and handing back restores it only where that platform
already enables it. A simulated-platform test covers this with the real encoder.

Pre-commit audit caught the settings gate being applied only to Headless. The
real Terminal setter now uses the same effective reporting/tmux gate. The
1860-case TUI suite passed after that fix; an additional isolated-process test
constructs the real Terminal without a tty, refreshes its settings under a
disabled launch policy and verifies capture stays off. That test also passed.
