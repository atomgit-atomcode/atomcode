# TUI input caret audit

Baseline: `release/v5.2.6` at `ebd6c0e608396f71b49cb315410440f2222cf206`, clean worktree.

## Findings and boundary

- Provider account/model key fields accept input but their renderer marks them as non-text fields, so no caret is drawn.
- Shared `chrome::caret_spans` ignores its width budget. Long text loses its caret when the caller truncates the row. Fixed label columns also consume all the space on narrow terminals.
- Background replies budget for the border but not the recipient title. Plugin market URLs duplicate the fixed label layout.
- Wizard text steps draw only text/placeholder. Manual shortcut entry uses a literal pipe that can be clipped.
- ANSI painting drops reverse-video styling when colours are disabled, so a software caret needs a visible plain-text fallback.
- Audited the composer/password prompt, questions, settings, providers, plugins, MCP/tools/resume filters, generic list sheets, background input and wizard inputs. The composer/password prompt uses the terminal cursor; panels use drawn carets.

State stays in the existing frontend drafts. Provider secrets stay in the host's secret buffer: the renderer continues to receive only a character count. No credential, runtime, protocol or persistence ownership changes.

## Work and verification

1. Reproduce missing/overflowing carets in renderer tests (empty, long, narrow and Unicode input).
2. Keep the caret inside the shared width budget; reserve input room before labels/prefixes; use a visible fallback with styling disabled.
3. Use the shared renderer for focused provider masks, wizard text, manual shortcuts and existing panel fields. Keep unfocused fields and non-text choices without carets.
4. Run focused tests, then the TUI crate's tests, formatting and the compile gate. Inspect representative rendered output without real credentials or network requests.

## Completed verification

- The first five focused regression tests failed on the starting implementation, reproducing absent provider/wizard carets and clipping in shared/background fields.
- After the initial fixes, 67 focused tests and all 1,881 TUI tests passed. After the pre-commit height-clipping fix, `cargo nextest run -p atomcode-tui --no-fail-fast` passed all 1,882 tests, with none skipped.
- `bash gates/compile.sh`, `cargo fmt --all -- --check` and `git diff --check` passed. Cargo.lock unchanged; `hkdf`, `hmac` and `zeroize_derive` absent.
- Rendered the actual Rust provider/wizard/shared field output through xterm in Chrome at 80 and 30 columns, including plain output with colours disabled. Inspected empty and masked credentials, an overflowing address and an empty wizard field. Only synthetic account data and mask counts were used; no provider requests or real credentials.
- Existing composer/password input, question editing and panel tests passed in the full TUI suite. Changes are confined to frontend rendering and the manual-shortcut view's text assembly.

## Pre-commit review

- Fetched `origin/release/v5.2.6`; it still matches the starting commit. Reviewed every changed renderer and shared helper caller, focused/unfocused masks, key handling and settings hit testing.
- Reproduced one remaining height-clipping issue: short settings panels could drop the active field, including manual shortcut entry. The layout now keeps the row receiving text visible even with one row available, and hit testing consumes that same layout. Editing rows remain non-clickable as before.
- Added regression coverage for 1/3/6-row panels, including regular settings edits, manual shortcuts and the search field. The two focused checks failed before this review fix.
- One full-suite attempt failed in the existing `a_turn_the_model_never_answers_says_so_and_stops_spinning` test before its error message appeared. That test, its frame-quiescence helper and the message-handling path are unchanged. The isolated rerun passed, followed by the complete 1,882-test run above. This suggests an existing scheduling-sensitive check; no runtime or test timing changes were included in this rendering fix.
