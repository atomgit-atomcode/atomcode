# Question option editing implementation plan

**Goal:** Let users edit a suggested answer in the TUI and WebUI, including several independent replacements in a multiple-choice question.

**Architecture:** Editing belongs to the frontend's request-scoped drafts (`Sheet` / `UserInputCard`). Runtime ownership, request correlation, approvals and the wire schema stay unchanged. Original picks use `selected`; edited answers use `text`, together with any independently typed answer. Editing never mutates the original request.

**Tech stack:** Rust TUI and configuration catalog; Preact/TypeScript WebUI; existing native request transport.

## Baseline and boundaries

- Branch `release/v5.2.6`, commit `5813a48bbc88b938eca89cf4994671ecfcc6cccb`; clean worktree before implementation.
- Reviewed `ask.rs`, `modules/ask.rs`, host/plugin request routing, configuration/keymap, `UserInputCard.tsx`, daemon answer forwarding and capabilities answer formatting, plus their recent history.
- The runtime remains the request owner. Frontends hold only drafts tied to the displayed request. Withdrawal discards drafts; a rejected WebUI submission retains the card; an old request cannot answer its replacement.
- Scope: ordinary single/multiple `request_user_input` questions, including batch pages and requests forwarded from background sessions. Approval/checkpoint choices remain fixed. ACP and headless transports require no new behavior or schema.

## Interaction

- TUI: Ctrl+E on a suggested option opens an inline input prefilled with its label (not description). The key is configurable as `ui.keybindings.question_edit`; its binding appears in the legend. In text input Ctrl+E still moves the caret to the end.
- Enter commits a single answer or saves and checks a multiple-choice item. Esc cancels only the current edit; outside editing it declines as before. Ctrl+C always declines.
- Store replacements by option index, separately from free text. Re-edit starts with the saved replacement. Editing A to A′ removes A from the submitted picks. Empty edits cannot be committed.
- Batch drafts survive page navigation; unfinished edits cannot silently submit an older answer. Existing text editing, numeric and prefix selection continue to work outside editing.
- WebUI: accessible Edit / Save / Cancel controls per option; Enter saves, Esc cancels, IME composition does not submit. Keep full per-question state when moving between batch pages.

## Steps and verification

1. Extend TUI `ask.rs` draft/editor state and answer construction; test replacement, cancellation, Unicode caret movement, multiple edited picks, batch navigation and approval exclusion.
2. Render the inline editor and contextual legend in `modules/ask.rs`; extend screen i18n and rendering tests.
3. Add the setting in `atomcode-config`, validate it in the terminal driver and CLI settings port, and route it into each question sheet. Test configured bindings and reserved-key rejection.
4. Extract WebUI answer-state operations to `src/lib/userInput.ts`; update `UserInputCard.tsx`, styles and i18n. Test multiple edits, blank edits, cancellation, original-request immutability and batch state preservation.
5. Run focused tests while implementing, then affected crate tests with `cargo nextest run -p ...`; WebUI tests, typecheck and build; `bash gates/compile.sh`; `cargo fmt --all` and `cargo fmt --all -- --check`; review diff and Cargo.lock.

## Completed verification

- Final review: `cargo nextest run -p atomcode-tui -p atomcode-config -p atomcode-i18n -p atomcode`: 2,891 passed, none skipped.
- WebUI: 379 tests passed; TypeScript checking and production build passed. `npm run test:user-input` exercises the real component in Chrome: single/multiple/batch edits, IME Enter, blank edit, Esc cancellation, rejection/retry, request replacement and withdrawal. Desktop and 390px screenshots inspected.
- `bash gates/compile.sh`, `cargo fmt --all -- --check` and `git diff --check` passed. Cargo.lock unchanged; `hkdf`, `hmac`, `zeroize_derive` absent.
- Supplemental `gates/tui-i18n.sh` and `gates/tui-layers.sh` fail identically on the starting commit and this change: CJK literals 23, duplicate messages 0, literal glyphs 508, raw colours 1, OS probes 2, plus the existing `ansi.rs` environment read. Verified against a separate archive of `5813a48bbc`; no gate or baseline files changed.
- No live provider was used. Browser tests capture the existing answer payload locally; request transport code and runtime lifecycle were unchanged.

## Pre-commit review

- Reproduced and fixed three TUI cases: a saved multiple-choice replacement skipped through Tab could be declined at final submission; changes to a previously confirmed free-text draft could send the old words; prefix selection used the original label after it was replaced.
- Final submission now returns to nonempty unconfirmed drafts. Insertion/deletion invalidates their confirmation; caret movement and cancelling a provisional option edit preserve it. Prefix selection follows the displayed replacement. Regression tests cover these cases.
- WebUI normalizes control characters in prefilled labels to spaces, matching the TUI and preventing a single-line DOM input from silently dropping line breaks while its state retains them.
- Fetched `origin/release/v5.2.6` before review; local and remote both started at `5813a48bbc`. No protocol, runtime, approval, build-gate or Cargo.lock changes.
