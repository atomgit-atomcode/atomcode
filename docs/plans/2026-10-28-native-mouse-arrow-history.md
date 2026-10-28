# Native mouse mode: preserve input arrow navigation

The input pump currently classifies even one bare Up/Down as ArrowBurst when the
pointer is handed back to the terminal (the default over SSH). The screen can
then scroll the conversation instead of recalling composer history.

Keep single arrows as ordinary key events. Only multiple same-direction arrows
within the existing 4ms gathering window retain the terminal-wheel burst path.
This is a TUI input decision; runtime, sessions and persistence do not change.

Single wheel-generated arrows and keyboard arrows are indistinguishable in this
terminal protocol. Prefer keyboard navigation for that ambiguous case. Explicit
mouse wheel events and rapid wheel bursts retain their existing routing; terminal
buffering can still make multiple keyboard presses look like a wheel burst.

Verify both directions with the pointer handed back, ordinary repeat intervals,
rapid bursts, direction changes and following text. Run the full TUI test suite,
compile gate and formatting check. A real Linux/SSH terminal remains a manual
validation step.
