# Browser terminal clipboard compatibility

Baseline: `release/v5.2.5`, `4d019e919`. Preserve unrelated worktree edits.
The earlier SSH/PuTTY startup policy (`f32fe488f`) only checks SSH environment
variables and PuTTY terminal identities. A NAS browser terminal can have neither.

## Decision and ownership

Keep terminal detection and mouse/clipboard policy in the TUI driver. On Linux,
read the process ancestry from `/proc/<pid>/status`, recognizing exact `gotty`
and `ttyd` process names through intermediate shells. Cache the result for the
process lifetime; bound traversal to 64 processes. Unreadable or malformed
ancestry is unknown, not a browser-terminal match. Do not infer browser access
from generic `TERM=xterm`, missing DISPLAY, or a headless Linux host.

A recognized browser terminal defaults to client-owned mouse input, using the
existing startup reminder below the working directory/model. `ATOMCODE_MOUSE`
still overrides the startup preference and Ctrl+G remains available. This does
not disable application mouse support. Clipboard access is independently remote
even when the user explicitly enables application mouse input: never read the
NAS clipboard as a substitute for the browser user's clipboard.

Reuse the existing remote-paste path: actual terminal paste payloads edit the
composer; a literal Ctrl+V or application paste-menu request gives a replaceable,
non-error hint without modifying the draft or appending transcript errors.
Update remote clipboard wording to cover both SSH and browser terminals.

Upstream evidence: goTTY starts its command with `exec.Command` and a PTY:
https://github.com/yudai/gotty/blob/master/backend/localcommand/local_command.go
No browser clipboard API is added to AtomCode's remote process.

## Rendering investigation

Replayed the actual current menu/frame ANSI encoder output in the hterm bundled
with upstream goTTY (libapps hterm 1.70), at 160x48, 100x30, and 60x18. With
matching terminal dimensions, every row matches the composed frame and the menu
border is aligned. An asynchronous font change in the replay harness initially
changed the browser's dimensions without updating the supplied frame; correcting
the harness removed those discrepancies. This demonstrates a size mismatch can
distort output, but does not establish the cause of the user's photograph.

No speculative menu rendering patch is included. The user's goTTY build,
emulator and resize behavior still need validation on the NAS. The default
native mouse path also avoids opening the application's right-click menu during
normal browser-terminal copy/paste.

## Verification and limits

- TUI and i18n nextest suites: 1908 tests passed, including ancestor recognition,
  unavailable/malformed/cyclic process trees, unchanged local defaults, explicit
  mouse overrides, and existing remote paste tests that panic on server clipboard
  reads and check actual pasted text still reaches the draft.
- Workspace and feature compilation gate passed; formatting checked.
- Browser replay uses upstream's emulator and actual AtomCode ANSI output, not
  the user's NAS or a complete authenticated AtomCode process under goTTY.
- Renamed servers, restricted `/proc`, and a pre-existing detached tmux server
  outside the goTTY ancestry cannot be identified this way. Existing
  `ATOMCODE_MOUSE=0` remains the explicit startup fallback. Browser clipboard
  shortcuts are still controlled by the terminal frontend.

## Pre-push audit

The final regression found an old copy-hint assertion requiring the literal
`SSH`. Update it to verify the shared hint is no longer SSH-specific while
retaining its unconfirmed-copy and native-selection fallback checks. Paste
guidance now uses a generic terminal/browser paste instruction rather than
promising a particular right-click or shortcut mapping. No production menu
renderer or local terminal mouse behavior is changed.
