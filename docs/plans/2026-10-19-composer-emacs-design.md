# Composer Emacs editing and EOF safety

The TUI owns keyboard interpretation and exit confirmation. Runtime cancellation and shutdown remain unchanged.

- Ctrl-D follows forward deletion for a nonempty draft or pending attachments, including existing selection, Unicode and image-marker handling. At the draft end it does nothing.
- An idle empty composer requires two Ctrl-D presses within two seconds to quit. Editing, other actions and an active turn reset confirmation. Ctrl-C and Ctrl-D confirmations are independent.
- Questions and the team panel consume Ctrl-D without quitting. Other focused panels retain their own bindings.
- Ctrl-B/F move by character. Home/End and Ctrl-A/E move within the current logical line. Ctrl-U deletes to that line’s beginning; Ctrl-K deletes to its end, or removes the following newline at the end.
- Help and confirmation messages support Chinese and English. Ctrl-P/N retain history navigation.

Verification covers Unicode, multiline deletion, draft-end no-op, expiration, attachments, active turns and focused question/team paths.
