# TUI newline shortcut configuration

Approved scope: users configure the newline action through a settings editor, with chord capture and manual names. The persisted form is `[ui.keybindings]` with `newline = ["alt+j", "ctrl+j"]`.

## Ownership and behavior

The product config owns the optional string list and atomic document patch. The CLI settings port validates names against the shipped map. The TUI owns parsing, effective bindings and editor state; the runtime and session lifecycle do not participate. The current branch has one TUI, so no retired classic driver is restored.

An omitted list follows shipped defaults; an explicit list replaces newline chords; an empty list disables them. Backslash followed by Enter remains a fallback. The settings editor offers add/capture/confirm, manual comma-separated names, remove and reset. Escape cancels without a write. Successful writes apply immediately; `/keys` reads the effective map.

## Conflicts and failure

Plain text keys, duplicate names, reserved navigation/cancel gestures and bindings owned by another action are refused before writing. Mounted keymaps are checked as well as the shipped defaults. Persistence and activation are serialized against registration, so a failed file write keeps the previous bindings, and a later row cannot silently claim a custom chord. Invalid strings loaded from a hand-edited file produce a diagnostic and retain the usable map. Host/IDE interception cannot be detected reliably; the capture prompt offers manual entry and docs require a terminal check.

## Verification

Check parser round trips and invalid names, conflict refusal and failed writes preserving the effective map, later registration refusing conflicts, settings capture/manual/cancel, array persistence with comments retained, reset/empty lists, effective `/keys`, and an end-to-end multiline submission. Run affected crate tests, formatter and `gates/compile.sh`. Real terminal delivery remains a manual check.
