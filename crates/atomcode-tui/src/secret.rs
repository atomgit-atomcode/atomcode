//! Asking the person for a secret — a password `sudo` or `ssh` wants.
//!
//! Its own module rather than a shape of [`crate::ask::Asks`], because the two
//! differ in the one way that matters: a question and its answer are **facts**
//! (`Asked` / `Answered`, `docs/adr/0024`), and a password must never become
//! one. Nothing here reaches the session log, the transcript, the composer or
//! its history; what is typed lives in a [`Zeroizing`] string, is handed to the
//! one waiter through a channel, and is wiped when this closes.
//!
//! The wider mechanism (`docs/plans/2026-09-18-tui-panels-and-commands-inventory.md`
//! P0-1): `sudo` and `ssh` ask their `*_ASKPASS` helper instead of the tty when
//! one is set, the `bash` tool sets those for every child, and
//! `atomcode_capabilities::askpass` is the server they reach. Without a screen
//! answering it, a `sudo` inside a tool call waits for a tty that this UI owns
//! — which is the hang this closes.

use std::sync::Mutex;

use zeroize::Zeroizing;

use crate::caps::{Caps, Glyph};
use crate::frame::{Color, Line, Span, Style};
use crate::moment::Viewport;
use crate::overlay::{Overlay, Step};
use crate::surface::{Key, KeyPress, Mods};
use crate::theme::Role;
use crate::width;

/// What is drawn for one typed character.
///
/// One glyph per character, not a fixed-width blob: a person needs to see that
/// a keystroke arrived — the usual reason a password prompt feels broken is
/// that it looks identical before and after typing. Width, not the characters:
/// a mask that varied with what was typed would leak it to anyone watching.
///
/// Through the shield rather than as a literal, like every other decoration
/// here: an ASCII terminal gets `*` instead of a dot it would draw as tofu, and
/// a mask nobody can see is a prompt that looks broken.
fn mask(caps: &Caps) -> &'static str {
    caps.g(Glyph::Bullet)
}

/// The secret being typed, and who is waiting for it.
pub struct SecretPrompt {
    /// What to ask, as the program that wants it phrased it (`sudo`'s own
    /// prompt, say). Shown as given — it names the account and the host, which
    /// is how a person knows *which* password is being asked for.
    prompt: String,
    typed: Mutex<Zeroizing<String>>,
}

impl SecretPrompt {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            typed: Mutex::new(Zeroizing::new(String::new())),
        }
    }

    fn with_typed<T>(&self, f: impl FnOnce(&mut Zeroizing<String>) -> T) -> T {
        let mut typed = self.typed.lock().unwrap_or_else(|e| e.into_inner());
        f(&mut typed)
    }

    /// How many characters have been typed — for the mask, and for nothing else.
    fn len(&self) -> usize {
        self.with_typed(|typed| typed.chars().count())
    }

    /// Take what was typed, leaving nothing behind.
    fn take(&self) -> String {
        self.with_typed(|typed| std::mem::take(&mut **typed))
    }
}

impl Overlay for SecretPrompt {
    fn id(&self) -> &'static str {
        "secret"
    }

    fn title(&self) -> String {
        // The asking program's own words, capped: a prompt is a line, and a
        // program that hands over a paragraph must not push the box off screen.
        width::take_width(&crate::ask::one_line(&self.prompt), 60)
    }

    fn render(&self, vp: &Viewport<'_>) -> Vec<Line> {
        let w = vp.rect.w as usize;
        if w == 0 || vp.rect.h == 0 {
            return Vec::new();
        }
        let masked = mask(&vp.moment.caps).repeat(self.len().min(w));
        vec![
            Line::from_spans(vec![
                Span::styled("  ", Style::new()),
                Span::styled(masked, Style::new().fg(Color::role(Role::Accent))),
            ])
            .truncate(w),
            Line::styled(
                width::take_width("  enter 送出 · esc 不给", w),
                Style::new().fg(Color::role(Role::Muted)),
            ),
        ]
    }

    fn key(&self, press: KeyPress) -> Step {
        match (press.key, press.mods) {
            // A refusal, and the only way out that is not an answer. What it
            // means downstream is "no password", never "empty password".
            (Key::Esc, _) | (Key::Char('c'), Mods::CTRL) => {
                let _ = self.take();
                Step::Cancelled
            }
            (Key::Enter, _) => Step::Chose(self.take()),
            (Key::Backspace, _) => {
                self.with_typed(|typed| {
                    typed.pop();
                });
                Step::Stay
            }
            // Start over, the shell's own key for it.
            (Key::Char('u'), Mods::CTRL) => {
                let _ = self.take();
                Step::Stay
            }
            (Key::Char(c), Mods::NONE) | (Key::Char(c), Mods::SHIFT) => {
                self.with_typed(|typed| typed.push(c));
                Step::Stay
            }
            // Everything else is swallowed rather than passed on: while this is
            // open it has the keyboard, and a key that fell through to the
            // screen behind it would act on a screen the person cannot see.
            _ => Step::Stay,
        }
    }

    fn size(&self) -> (u8, u8) {
        (60, 20)
    }

    fn rows(&self) -> Option<u16> {
        Some(2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Rect;
    use crate::moment::Moment;

    /// The mask as the terminal these judgements draw against renders it —
    /// asked of the shield rather than spelled out, so a capability that
    /// downgrades it does not quietly make them judge nothing.
    fn mask_here() -> &'static str {
        mask(&Moment::default().caps)
    }

    fn press(key: Key) -> KeyPress {
        KeyPress {
            key,
            mods: Mods::NONE,
        }
    }

    fn typed(prompt: &SecretPrompt, text: &str) {
        for c in text.chars() {
            assert_eq!(prompt.key(press(Key::Char(c))), Step::Stay);
        }
    }

    fn drawn(prompt: &SecretPrompt) -> String {
        let moment = Moment::default();
        let vp = Viewport::new(Rect::sized(40, 4), &moment);
        prompt
            .render(&vp)
            .iter()
            .map(|l| l.plain())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// What is typed is never drawn — one mask glyph per character, and the
    /// characters themselves appear nowhere on screen.
    ///
    /// The count is drawn on purpose: a prompt that looks identical before and
    /// after a keystroke is the one people report as broken.
    #[test]
    fn a_secret_is_drawn_as_its_length_and_never_as_itself() {
        let prompt = SecretPrompt::new("[sudo] password for lichao:");
        assert_eq!(
            drawn(&prompt).matches(mask_here()).count(),
            0,
            "nothing typed, nothing masked"
        );

        typed(&prompt, "hunter2");
        let screen = drawn(&prompt);
        assert!(
            !screen.contains("hunter2") && !screen.contains('h'),
            "the characters are not on screen: {screen:?}"
        );
        assert_eq!(
            screen.matches(mask_here()).count(),
            7,
            "one glyph per character, so a keystroke is visible: {screen:?}"
        );
        // The prompt itself is the asking program's, shown as given: it names
        // which password is wanted.
        assert!(prompt.title().contains("password for lichao"));
    }

    /// Enter hands the secret over exactly as typed, and leaves nothing behind.
    #[test]
    fn enter_hands_over_what_was_typed_and_keeps_none_of_it() {
        let prompt = SecretPrompt::new("password:");
        typed(&prompt, "correct horse");
        assert_eq!(
            prompt.key(press(Key::Enter)),
            Step::Chose("correct horse".into())
        );
        assert_eq!(prompt.len(), 0, "the buffer is empty afterwards");
        assert_eq!(
            drawn(&prompt).matches(mask_here()).count(),
            0,
            "and so is the screen"
        );
    }

    /// Esc is a refusal — `None` downstream, never an empty password. The
    /// difference matters: `sudo` given an empty password *tries* it and burns
    /// an attempt.
    #[test]
    fn esc_refuses_rather_than_answering_with_nothing() {
        let prompt = SecretPrompt::new("password:");
        typed(&prompt, "secret");
        assert_eq!(prompt.key(press(Key::Esc)), Step::Cancelled);
        assert_eq!(prompt.len(), 0, "and what was typed is gone");
    }

    /// Editing: backspace takes one character, ctrl-u starts over.
    #[test]
    fn a_typo_can_be_taken_back_one_character_or_all_of_them() {
        let prompt = SecretPrompt::new("password:");
        typed(&prompt, "abcd");
        assert_eq!(prompt.key(press(Key::Backspace)), Step::Stay);
        assert_eq!(prompt.len(), 3);
        assert_eq!(
            prompt.key(KeyPress {
                key: Key::Char('u'),
                mods: Mods::CTRL,
            }),
            Step::Stay
        );
        assert_eq!(prompt.len(), 0);
        typed(&prompt, "efg");
        assert_eq!(prompt.key(press(Key::Enter)), Step::Chose("efg".into()));
    }

    /// Every other key is swallowed. While this is open it has the keyboard,
    /// and a key that fell through would act on a screen nobody can see.
    #[test]
    fn keys_it_has_no_use_for_do_not_reach_the_screen_behind_it() {
        let prompt = SecretPrompt::new("password:");
        for key in [Key::Up, Key::Down, Key::Tab, Key::PageUp] {
            assert_eq!(prompt.key(press(key)), Step::Stay, "{key:?} was passed on");
        }
        assert_eq!(prompt.len(), 0, "and none of them typed anything");
    }
}
