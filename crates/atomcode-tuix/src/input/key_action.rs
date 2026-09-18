// crates/atomcode-tuix/src/input/key_action.rs
use crossterm::event::{KeyCode, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Submit,
    InsertNewline,
    Cancel,
    ClearLine,
    DeleteWordBackward,
    DeleteToEnd,
    Insert(char),
    Complete,
    CursorLeft,
    CursorRight,
    LineStart,
    LineEnd,
    HistoryPrev,
    HistoryNext,
    /// Ctrl+R — enter reverse-i-search over the input history (readline /
    /// bash style). While searching, further Ctrl+R presses jump to the
    /// next older match.
    HistorySearch,
    Backspace,
    DeleteForward,
    ToggleToolOutput,
    NoOp,
}

/// Normalize the POSIX/readline backspace-delete aliases that some terminals emit
/// as Ctrl-chords instead of dedicated keys: `^H` (`Ctrl+Char('h')`) → Backspace,
/// `^?` (`Ctrl+Char('?')`) → Delete. These arrive over SSH and on Linux terminals
/// with `stty erase ^H`, where the physical Backspace key is reported as `Ctrl+h`
/// rather than `KeyCode::Backspace` (see the `Char('h')`/`Char('?')` arms in
/// [`classify`], which the main composer relies on).
///
/// The composer routes keys through `classify`, but MODALS handle raw `KeyCode`
/// directly, so without this their text fields become append-only on those
/// terminals (a `^H` even echoes a literal `h` in an unguarded insert arm). The
/// event loop applies this once at the modal-dispatch boundary so every modal —
/// current and future — gets consistent backspace/delete behavior. No modal binds
/// `^H`/`^?` for anything else, so the remap is unambiguous.
pub(crate) fn normalize_edit_key(
    code: KeyCode,
    modifiers: KeyModifiers,
) -> (KeyCode, KeyModifiers) {
    match (code, modifiers.contains(KeyModifiers::CONTROL)) {
        (KeyCode::Char('h'), true) => (KeyCode::Backspace, KeyModifiers::NONE),
        (KeyCode::Char('?'), true) => (KeyCode::Delete, KeyModifiers::NONE),
        _ => (code, modifiers),
    }
}

pub fn classify(code: KeyCode, modifiers: KeyModifiers) -> Action {
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let shift = modifiers.contains(KeyModifiers::SHIFT);
    let alt = modifiers.contains(KeyModifiers::ALT);

    match (code, ctrl) {
        (KeyCode::Enter, ctrl) if ctrl || shift || alt => Action::InsertNewline,
        (KeyCode::Enter, _) => Action::Submit,
        // Ctrl+J = ASCII LF. On Kitty-aware terminals it arrives disambiguated
        // from Enter and gives users another newline chord when their primary
        // one is intercepted by the host terminal (e.g. Windows Terminal binds
        // Alt+Enter to toggleFullscreen by default).
        (KeyCode::Char('j'), true) => Action::InsertNewline,
        (KeyCode::Char('c'), true) => Action::Cancel,
        (KeyCode::Char('u'), true) => Action::ClearLine,
        (KeyCode::Char('w'), true) => Action::DeleteWordBackward,
        (KeyCode::Char('k'), true) => Action::DeleteToEnd,
        // Emacs-style line navigation — Home/End aliases. Docs already
        // promise these in site/docs/keybindings.html.
        (KeyCode::Char('a'), true) => Action::LineStart,
        (KeyCode::Char('e'), true) => Action::LineEnd,
        // Ctrl+O toggles real-time tool output visibility.
        (KeyCode::Char('o'), true) => Action::ToggleToolOutput,
        // Ctrl+R enters reverse-i-search over input history. Bash/zsh
        // users expect the chord; it was previously unmapped (NoOp), so
        // nothing depends on the old behaviour.
        (KeyCode::Char('r'), true) => Action::HistorySearch,
        // Ctrl+H is the POSIX / readline alias for Backspace. MobaXterm,
        // PuTTY and other Windows SSH clients often ship with "Backspace
        // sends ^H" turned on by default, so the physical Backspace key
        // arrives here as `Ctrl+Char('h')` rather than `KeyCode::Backspace`.
        // Without this arm the key is a no-op on those terminals — the
        // user sees their input line accumulate characters they can't
        // erase.
        (KeyCode::Char('h'), true) => Action::Backspace,
        // Ctrl+? (ASCII 0x7F with modifier coerced) — some xterm-family
        // terminals emit this for the literal Delete key. Keep the
        // behaviour symmetric with the bare `KeyCode::Delete` arm below.
        (KeyCode::Char('?'), true) => Action::DeleteForward,
        (KeyCode::Char(c), false) => Action::Insert(c),
        // A few terminals preserve Shift+Tab as `Tab + SHIFT` instead of
        // crossterm's usual `BackTab`. The main composer intercepts that chord
        // for mode cycling when no completion menu is open. If a menu is open,
        // keep it inert rather than accidentally accepting the highlighted
        // completion through the generic Tab fallback.
        (KeyCode::Tab, _) if shift => Action::NoOp,
        (KeyCode::Tab, _) => Action::Complete,
        (KeyCode::Left, _) => Action::CursorLeft,
        (KeyCode::Right, _) => Action::CursorRight,
        (KeyCode::Home, _) => Action::LineStart,
        (KeyCode::End, _) => Action::LineEnd,
        (KeyCode::Up, _) => Action::HistoryPrev,
        (KeyCode::Down, _) => Action::HistoryNext,
        (KeyCode::Esc, _) => Action::Cancel,
        (KeyCode::Backspace, _) => Action::Backspace,
        (KeyCode::Delete, _) => Action::DeleteForward,
        _ => Action::NoOp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn k(code: KeyCode, modifiers: KeyModifiers) -> Action {
        classify(code, modifiers)
    }

    #[test]
    fn normalize_edit_key_maps_ctrl_h_and_ctrl_question() {
        assert_eq!(
            normalize_edit_key(KeyCode::Char('h'), KeyModifiers::CONTROL),
            (KeyCode::Backspace, KeyModifiers::NONE),
            "^H is the readline Backspace alias"
        );
        assert_eq!(
            normalize_edit_key(KeyCode::Char('?'), KeyModifiers::CONTROL),
            (KeyCode::Delete, KeyModifiers::NONE),
            "^? is the Delete alias"
        );
    }

    #[test]
    fn normalize_edit_key_leaves_other_keys_untouched() {
        // Plain text, a real Backspace, and unrelated Ctrl-chords all pass through.
        assert_eq!(
            normalize_edit_key(KeyCode::Char('h'), KeyModifiers::NONE),
            (KeyCode::Char('h'), KeyModifiers::NONE)
        );
        assert_eq!(
            normalize_edit_key(KeyCode::Backspace, KeyModifiers::NONE),
            (KeyCode::Backspace, KeyModifiers::NONE)
        );
        assert_eq!(
            normalize_edit_key(KeyCode::Char('a'), KeyModifiers::CONTROL),
            (KeyCode::Char('a'), KeyModifiers::CONTROL)
        );
    }

    #[test]
    fn enter_submits() {
        assert_eq!(k(KeyCode::Enter, KeyModifiers::NONE), Action::Submit);
    }

    #[test]
    fn shift_tab_never_degrades_to_plain_completion() {
        assert_eq!(k(KeyCode::Tab, KeyModifiers::SHIFT), Action::NoOp);
        assert_eq!(k(KeyCode::Tab, KeyModifiers::NONE), Action::Complete);
        assert_eq!(k(KeyCode::BackTab, KeyModifiers::SHIFT), Action::NoOp);
    }

    #[test]
    fn shift_enter_inserts_newline() {
        assert_eq!(
            k(KeyCode::Enter, KeyModifiers::SHIFT),
            Action::InsertNewline
        );
    }

    #[test]
    fn alt_enter_inserts_newline() {
        assert_eq!(k(KeyCode::Enter, KeyModifiers::ALT), Action::InsertNewline);
    }

    #[test]
    fn alt_shift_enter_inserts_newline() {
        assert_eq!(
            k(KeyCode::Enter, KeyModifiers::ALT | KeyModifiers::SHIFT),
            Action::InsertNewline
        );
    }

    #[test]
    fn ctrl_j_inserts_newline() {
        // Ctrl+J = ASCII 0x0A (LF). On terminals that negotiate the Kitty
        // keyboard protocol, crossterm reports it as `Char('j'), CONTROL`
        // — give it the same role as Shift/Ctrl/Alt+Enter so users on
        // Kitty-aware terminals (kitty, wezterm, alacritty, WT ≥1.21) have
        // an extra fallback when their main chord is intercepted by the
        // host terminal (e.g. Windows Terminal eats Alt+Enter for full-
        // screen toggle by default).
        assert_eq!(
            k(KeyCode::Char('j'), KeyModifiers::CONTROL),
            Action::InsertNewline
        );
    }

    #[test]
    fn ctrl_c_cancels() {
        assert_eq!(k(KeyCode::Char('c'), KeyModifiers::CONTROL), Action::Cancel);
    }

    #[test]
    fn esc_cancels() {
        assert_eq!(k(KeyCode::Esc, KeyModifiers::NONE), Action::Cancel);
    }

    #[test]
    fn ctrl_u_clears_line() {
        assert_eq!(
            k(KeyCode::Char('u'), KeyModifiers::CONTROL),
            Action::ClearLine
        );
    }

    #[test]
    fn ctrl_w_deletes_word() {
        assert_eq!(
            k(KeyCode::Char('w'), KeyModifiers::CONTROL),
            Action::DeleteWordBackward
        );
    }

    #[test]
    fn ctrl_k_deletes_to_end() {
        assert_eq!(
            k(KeyCode::Char('k'), KeyModifiers::CONTROL),
            Action::DeleteToEnd
        );
    }

    #[test]
    fn ctrl_a_jumps_to_line_start() {
        assert_eq!(
            k(KeyCode::Char('a'), KeyModifiers::CONTROL),
            Action::LineStart
        );
    }

    #[test]
    fn ctrl_e_jumps_to_line_end() {
        assert_eq!(
            k(KeyCode::Char('e'), KeyModifiers::CONTROL),
            Action::LineEnd
        );
    }

    #[test]
    fn ctrl_o_toggles_tool_output() {
        assert_eq!(
            k(KeyCode::Char('o'), KeyModifiers::CONTROL),
            Action::ToggleToolOutput
        );
    }

    #[test]
    fn ctrl_r_starts_history_search() {
        assert_eq!(
            k(KeyCode::Char('r'), KeyModifiers::CONTROL),
            Action::HistorySearch
        );
        // Bare r still inserts a character.
        assert_eq!(
            k(KeyCode::Char('r'), KeyModifiers::NONE),
            Action::Insert('r')
        );
    }

    #[test]
    fn ctrl_h_acts_as_backspace() {
        // MobaXterm / PuTTY default: Backspace key sends ^H. Must delete
        // a char, not be a silent no-op.
        assert_eq!(
            k(KeyCode::Char('h'), KeyModifiers::CONTROL),
            Action::Backspace
        );
    }

    #[test]
    fn ctrl_questionmark_acts_as_delete_forward() {
        assert_eq!(
            k(KeyCode::Char('?'), KeyModifiers::CONTROL),
            Action::DeleteForward,
        );
    }

    #[test]
    fn plain_letter_inserts() {
        assert_eq!(
            k(KeyCode::Char('a'), KeyModifiers::NONE),
            Action::Insert('a')
        );
    }

    #[test]
    fn shifted_letter_inserts() {
        assert_eq!(
            k(KeyCode::Char('A'), KeyModifiers::SHIFT),
            Action::Insert('A')
        );
    }

    #[test]
    fn tab_completes() {
        assert_eq!(k(KeyCode::Tab, KeyModifiers::NONE), Action::Complete);
    }

    #[test]
    fn arrow_navigation() {
        assert_eq!(k(KeyCode::Left, KeyModifiers::NONE), Action::CursorLeft);
        assert_eq!(k(KeyCode::Right, KeyModifiers::NONE), Action::CursorRight);
        assert_eq!(k(KeyCode::Up, KeyModifiers::NONE), Action::HistoryPrev);
        assert_eq!(k(KeyCode::Down, KeyModifiers::NONE), Action::HistoryNext);
        assert_eq!(k(KeyCode::Home, KeyModifiers::NONE), Action::LineStart);
        assert_eq!(k(KeyCode::End, KeyModifiers::NONE), Action::LineEnd);
    }

    #[test]
    fn backspace_and_delete() {
        assert_eq!(k(KeyCode::Backspace, KeyModifiers::NONE), Action::Backspace);
        assert_eq!(
            k(KeyCode::Delete, KeyModifiers::NONE),
            Action::DeleteForward
        );
    }

    #[test]
    fn unknown_key_is_noop() {
        assert_eq!(k(KeyCode::F(5), KeyModifiers::NONE), Action::NoOp);
    }
}
