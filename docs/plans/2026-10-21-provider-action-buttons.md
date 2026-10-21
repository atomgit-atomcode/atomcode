# Provider action buttons

Keep the provider panel's keyboard shortcuts and add clickable footer buttons for add, edit and delete. Both account and model tabs act on the highlighted row. Model-row clicks retain their existing switch behavior.

The renderer and hit test share button text and terminal cell widths. The host translates a button hit to the existing key handler, so forms, secret handling and two-step deletion remain owned by the existing provider state machine. Managed items and the add row have disabled edit/delete actions; clipped buttons cannot activate.

Verify rendered hit positions, keyboard/click equivalence on both tabs, add and two-step delete, disabled rows and clipping. Run the TUI suite, formatting and compile gate. No runtime, persistence or permission changes.
