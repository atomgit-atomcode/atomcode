# Paste submit boundary

The current pump reproduced two leaks: delayed terminal records emitted pasted
newlines as Enter, and an 8192-character collection cap stranded a trailing Enter.
Use a 40ms continuation window only after a fast initial pair (single keys retain
4ms), and preserve paste classification across bounded chunks until silence or a
non-text event. Native bracketed paste remains unchanged. Keep regression coverage
for normal typed Enter, IME, escape/modifier boundaries, Unicode and Windows
release events. Timing-only fallback cannot perfectly distinguish all terminals'
unmarked pasted input from human keystrokes; native bracketed paste is preferred.

Preserve unrelated recovery-summary and changelog edits. Run TUI nextest, workspace
compile gate and fmt. No actual model calls or user sessions are needed.

Validation: both original diagnostic regressions failed before the change and
pass after it. Full atomcode-tui suite: 1910/1910 passed. Coverage includes capped
Unicode multiline preservation without a turn, a delayed manual Enter, native
paste, Windows release records, IME-related input, and ordinary human typing.
Workspace/feature compile gate, rustfmt check and diff check passed. No native
Windows/SSH terminal clipboard run has been performed; the fallback remains a
timing heuristic when the terminal supplies no paste boundary.
