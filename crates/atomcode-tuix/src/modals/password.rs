// crates/atomcode-tuix/src/modals/password.rs
//
// A masked password prompt modal. The user sees bullets (•) instead of the
// characters they type; the actual password lives in a `Zeroizing<String>`
// and is sent over a `tokio::sync::oneshot` on Enter.  Esc sends `None`.
//
// The password NEVER touches `buf` (the shared input buffer / history).

use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers};
use tokio::sync::oneshot;
use zeroize::Zeroizing;

use super::{Modal, ModalAction};
use crate::event_loop::{build_status, Buffer, LoopCtx};
use crate::render::{Renderer, UiLine};
use crate::state::UiState;

// ── Pure logic outcome (no side-effects) ────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyOutcome {
    Continue,
    Submit,
    Cancel,
}

// ── Test-seam action (mirrors ModalAction but constructible without Renderer) ─

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalActionTest {
    Continue,
    Close,
}

// ── Struct ───────────────────────────────────────────────────────────────────

pub struct PasswordModal {
    pub(crate) prompt: String,
    pub(crate) pw: Zeroizing<String>,
    pub(crate) reply: Option<oneshot::Sender<Option<String>>>,
}

impl PasswordModal {
    pub fn new(prompt: String, reply: oneshot::Sender<Option<String>>) -> Self {
        Self {
            prompt,
            pw: Zeroizing::new(String::new()),
            reply: Some(reply),
        }
    }

    // ── Pure key logic ───────────────────────────────────────────────────────

    fn apply_key(&mut self, code: KeyCode, mods: KeyModifiers) -> KeyOutcome {
        match code {
            KeyCode::Char(c) if !mods.contains(KeyModifiers::CONTROL) => {
                self.pw.push(c);
                KeyOutcome::Continue
            }
            KeyCode::Backspace => {
                self.pw.pop();
                KeyOutcome::Continue
            }
            KeyCode::Enter => KeyOutcome::Submit,
            // Esc and Ctrl+C both dismiss the prompt (send None). Ctrl+C is the
            // universal escape hatch, so an orphaned modal can always be cleared.
            KeyCode::Esc => KeyOutcome::Cancel,
            KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => KeyOutcome::Cancel,
            _ => KeyOutcome::Continue,
        }
    }

    // ── Pure render ──────────────────────────────────────────────────────────

    pub(crate) fn masked_line(&self) -> String {
        format!("{} {}", self.prompt, "•".repeat(self.pw.chars().count()))
    }

    // ── Test seams (no LoopCtx / Renderer needed) ────────────────────────────

    #[cfg(test)]
    pub fn feed_for_test(&mut self, code: KeyCode, mods: KeyModifiers) -> ModalActionTest {
        match self.apply_key(code, mods) {
            KeyOutcome::Submit => {
                if let Some(tx) = self.reply.take() {
                    let _ = tx.send(Some(self.pw.to_string()));
                }
                ModalActionTest::Close
            }
            KeyOutcome::Cancel => {
                if let Some(tx) = self.reply.take() {
                    let _ = tx.send(None);
                }
                ModalActionTest::Close
            }
            KeyOutcome::Continue => ModalActionTest::Continue,
        }
    }

    #[cfg(test)]
    pub fn render_line_for_test(&self) -> String {
        self.masked_line()
    }
}

// ── Modal trait impl ─────────────────────────────────────────────────────────

impl Modal for PasswordModal {
    /// The password modal installs mid-turn (phase == Streaming), so it must
    /// capture every key/paste regardless of phase — otherwise typed chars leak
    /// into the type-ahead buffer and Esc/Ctrl+C cancel the turn.
    fn captures_all_keys(&self) -> bool {
        true
    }

    fn handle_key(
        &mut self,
        code: KeyCode,
        mods: KeyModifiers,
        buf: &mut Buffer,
        state: &mut UiState,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        match self.apply_key(code, mods) {
            KeyOutcome::Submit => {
                if let Some(tx) = self.reply.take() {
                    let _ = tx.send(Some(self.pw.to_string()));
                }
                Ok(ModalAction::Close)
            }
            KeyOutcome::Cancel => {
                if let Some(tx) = self.reply.take() {
                    let _ = tx.send(None);
                }
                Ok(ModalAction::Close)
            }
            KeyOutcome::Continue => {
                self.draw(buf, state, ctx, renderer);
                Ok(ModalAction::Continue)
            }
        }
    }

    fn draw(&self, _buf: &Buffer, state: &UiState, ctx: &LoopCtx, renderer: &mut dyn Renderer) {
        let line = self.masked_line();
        let cursor = line.len();
        renderer.render(UiLine::InputPrompt {
            buf: line,
            cursor_byte: cursor,
            menu: None,
            status: build_status(state, ctx),
            attachments: Vec::new(),
        });
        renderer.flush();
    }

    /// Override: append pasted text directly into `pw` (never into `buf`).
    fn handle_paste(
        &mut self,
        text: &str,
        buf: &mut Buffer,
        state: &mut UiState,
        ctx: &mut LoopCtx,
        renderer: &mut dyn Renderer,
    ) -> Result<ModalAction> {
        self.pw.push_str(text);
        self.draw(buf, state, ctx, renderer);
        Ok(ModalAction::Continue)
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    #[test]
    fn enter_sends_typed_password_and_closes() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut m = PasswordModal::new("[sudo] password:".into(), tx);
        for c in ['p', 'w', '1'] {
            assert_eq!(
                m.feed_for_test(KeyCode::Char(c), KeyModifiers::NONE),
                ModalActionTest::Continue
            );
        }
        assert_eq!(
            m.feed_for_test(KeyCode::Enter, KeyModifiers::NONE),
            ModalActionTest::Close
        );
        assert_eq!(rx.blocking_recv().unwrap().as_deref(), Some("pw1"));
    }

    #[test]
    fn esc_cancels_with_none() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut m = PasswordModal::new("p".into(), tx);
        m.feed_for_test(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(
            m.feed_for_test(KeyCode::Esc, KeyModifiers::NONE),
            ModalActionTest::Close
        );
        assert_eq!(rx.blocking_recv().unwrap(), None);
    }

    // Ctrl+C must be an escape hatch: dismiss the prompt (like Esc) rather than be
    // swallowed as a no-op — otherwise an orphaned password modal can't be cleared.
    #[test]
    fn ctrl_c_cancels_with_none() {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut m = PasswordModal::new("p".into(), tx);
        m.feed_for_test(KeyCode::Char('x'), KeyModifiers::NONE);
        assert_eq!(
            m.feed_for_test(KeyCode::Char('c'), KeyModifiers::CONTROL),
            ModalActionTest::Close
        );
        assert_eq!(rx.blocking_recv().unwrap(), None);
    }

    #[test]
    fn captures_all_keys_is_true() {
        // The password modal MUST capture keys regardless of UiPhase (it installs
        // mid-turn while a tool runs). The trait default is false; this override
        // is what makes the Streaming-phase routing in the event loop fire.
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let m = PasswordModal::new("p".into(), tx);
        assert!(m.captures_all_keys());
    }

    #[test]
    fn masked_render_shows_bullets_not_text() {
        let (tx, _rx) = tokio::sync::oneshot::channel();
        let mut m = PasswordModal::new("pp".into(), tx);
        m.feed_for_test(KeyCode::Char('s'), KeyModifiers::NONE);
        m.feed_for_test(KeyCode::Char('s'), KeyModifiers::NONE);
        let rendered = m.render_line_for_test();
        assert!(rendered.contains("••"), "masked: {rendered}");
        assert!(!rendered.contains("ss"), "must not leak chars: {rendered}");
    }
}
