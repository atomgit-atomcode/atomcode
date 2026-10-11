# Conversation scrollbar and copy responsiveness

The Windows 5.2.2 recording shows a terminal scrollbar which does not position
AtomCode's conversation. Add a scrollbar inside the conversation, driven by the
existing `Moment::scroll`. Keep wheel steps and PageUp/PageDown bindings unchanged.

Reserve one column in the stream region consistently for both rendering and row
measurement. Record the painted scrollbar geometry in `Hits`; clicking the track
positions history, grabbing the thumb preserves its pointer offset, and releases
never become selection-copy gestures. Hide it when there is nothing to scroll.

Use a bounded, serial clipboard worker so a native clipboard retry cannot block
input or repaint. Report completion through the UI wake channel, only for the
session that requested it. Keep all clipboard backends in `Surface`. Monochrome
selection uses reverse video; colour selection retains the existing band.

Verification: endpoint and thumb mapping, click/drag without selection, blocked
clipboard with responsive input and ordered completion, monochrome highlight,
existing TUI suite, Windows cross-compilation and the compile gate. Windows
Terminal visual acceptance (track clicks, dragging, long transcript copying,
resize during drag) still requires a Windows machine.
