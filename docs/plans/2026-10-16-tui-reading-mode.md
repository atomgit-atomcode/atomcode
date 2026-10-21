# TUI reading mode implementation plan

Goal: read long conversations over SSH without a GUI or clipboard.

Ownership: TUI owns a read-only snapshot of its existing blocks and the reader position. Runtime/session ownership and persistence are unchanged.

1. Add viewport-relative page actions to the main conversation, retaining one overlap row and existing scroll bounds.
2. Add `/read` through ScreenCommands. Reuse the document sheet with plain text that rewraps on resize; snapshot visible conversation blocks with tool calls expanded, omitting rewound turns and hidden non-tool kinds. Do not change the main presentation or composer.
3. Add whole/half-page control-key alternatives to the document reader, localized hints and command help. Keep search out of scope.
4. Test page geometry, navigation bounds, resize, expanded output and draft preservation. Run TUI tests, formatting and compile gate. If the reading view cannot mount, show an explicit refusal.
