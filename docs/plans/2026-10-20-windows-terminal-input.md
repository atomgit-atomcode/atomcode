# Windows terminal input implementation plan

**Goal:** Keep Windows/IDE terminals on Win32 input records and SGR mouse encoding, restoring console state on failed setup and exit.

**Architecture:** The TUI terminal surface owns console state and protocol writes. A private module saves the original input mode, clears VT input while reading console records, and restores it on exit. One pointer setup function applies the native capture mode before writing/flushing the complete requested pointer state. No character filtering or IDE-name workaround.

**Tech Stack:** Rust, existing kernel32 FFI, crossterm EventStream, nextest.

Baseline: release/v5.2.4 at 3ccabdd436; tracked workspace clean, unrelated untracked files preserved. Implementation in an isolated worktree; tests run after applying the scoped patch to the main checkout to reuse its build cache.

1. Add private terminal-mode helpers and tests: preserve unrelated input flags, clear inherited/restored VT input, restore original mode, propagate failures; verify native capture precedes SGR writes and cleanup attempts both native and encoding resets even on failure.
2. Add an initialization guard around Terminal::enter, armed before screen writes; on error/panic restore screen/raw/mouse/input state. Route initial capture, pointer changes/healing and transcript restoration through the shared setup helper. Keep Unix pointer semantics.
3. Wrap the real input stream to enforce Win32 input mode before and after polling; terminate explicitly with a restored-terminal diagnostic on mode/read failure instead of discarding errors. Keep normal characters and paste semantics unchanged.
4. Run focused and full TUI tests, formatting and compile gates. Add Windows-only isolated console restoration test if the platform APIs can be exercised without changing dependency boundaries. Record unavailable Windows/IDEA manual verification.

Validation: 9 focused terminal-mode/input-failure tests and all 1,762 TUI tests passed. The exit guard now requires returning a precomputed success/error after restoration, preserving its no-output rule. `bash gates/compile.sh`, formatting and diff checks passed.

Windows validation: the new module and its Windows-only test type-check for x86_64-pc-windows-msvc in a small isolated Cargo fixture importing the real module and vendored crossterm (with a minimal ANSI type/constants stub). Full TUI Windows cross-check was blocked by the host lacking Windows C headers (`ring`: assert.h not found). The isolated Windows console test is included as source only; neither it nor IDEA was executed on this macOS host.

Manual verification after rebuilding the Windows binary: in IDEA Classic/Reworked with PowerShell/Git Bash, move/click/drag/scroll, toggle Ctrl+G twice, open/return from /raw, then exit and check shell input/selection. Compare the same binary in Windows Terminal. Do not claim the reported IDEA issue is confirmed fixed until that check succeeds.
