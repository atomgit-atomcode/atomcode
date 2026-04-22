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
    Whip,
    Insert(char),
    Complete,
    CursorLeft,
    CursorRight,
    LineStart,
    LineEnd,
    HistoryPrev,
    HistoryNext,
    Backspace,
    DeleteForward,
    NoOp,
}

pub fn classify(code: KeyCode, modifiers: KeyModifiers) -> Action {
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    let shift = modifiers.contains(KeyModifiers::SHIFT);
    let alt = modifiers.contains(KeyModifiers::ALT);

    match (code, ctrl) {
        (KeyCode::Enter, false) if shift || alt => Action::InsertNewline,
        (KeyCode::Enter, false) => Action::Submit,
        (KeyCode::Char('c'), true) => Action::Cancel,
        (KeyCode::Char('u'), true) => Action::ClearLine,
        (KeyCode::Char('w'), true) => Action::DeleteWordBackward,
        (KeyCode::Char('k'), true) => Action::DeleteToEnd,
        (KeyCode::Char('g'), true) => Action::Whip,
        (KeyCode::Char(c), false) => Action::Insert(c),
        (KeyCode::Tab, _) => Action::Complete,
        (KeyCode::Left, _) => Action::CursorLeft,
        (KeyCode::Right, _) => Action::CursorRight,
        (KeyCode::Home, _) => Action::LineStart,
        (KeyCode::End, _) => Action::LineEnd,
        (KeyCode::Up, _) => Action::HistoryPrev,
        (KeyCode::Down, _) => Action::HistoryNext,
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
    fn enter_submits() {
        assert_eq!(k(KeyCode::Enter, KeyModifiers::NONE), Action::Submit);
    }

    #[test]
    fn shift_enter_inserts_newline() {
        assert_eq!(k(KeyCode::Enter, KeyModifiers::SHIFT), Action::InsertNewline);
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
    fn ctrl_c_cancels() {
        assert_eq!(k(KeyCode::Char('c'), KeyModifiers::CONTROL), Action::Cancel);
    }

    #[test]
    fn ctrl_u_clears_line() {
        assert_eq!(k(KeyCode::Char('u'), KeyModifiers::CONTROL), Action::ClearLine);
    }

    #[test]
    fn ctrl_w_deletes_word() {
        assert_eq!(k(KeyCode::Char('w'), KeyModifiers::CONTROL), Action::DeleteWordBackward);
    }

    #[test]
    fn ctrl_k_deletes_to_end() {
        assert_eq!(k(KeyCode::Char('k'), KeyModifiers::CONTROL), Action::DeleteToEnd);
    }

    #[test]
    fn ctrl_g_whips() {
        assert_eq!(k(KeyCode::Char('g'), KeyModifiers::CONTROL), Action::Whip);
    }

    #[test]
    fn plain_letter_inserts() {
        assert_eq!(k(KeyCode::Char('a'), KeyModifiers::NONE), Action::Insert('a'));
    }

    #[test]
    fn shifted_letter_inserts() {
        assert_eq!(k(KeyCode::Char('A'), KeyModifiers::SHIFT), Action::Insert('A'));
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
        assert_eq!(k(KeyCode::Delete, KeyModifiers::NONE), Action::DeleteForward);
    }

    #[test]
    fn unknown_key_is_noop() {
        assert_eq!(k(KeyCode::F(5), KeyModifiers::NONE), Action::NoOp);
    }
}
