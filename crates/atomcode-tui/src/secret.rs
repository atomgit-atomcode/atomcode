//! Asking the person for a secret — a password `sudo` or `ssh` wants.
//!
//! Its own module rather than a shape of [`crate::ask::Asks`], because the two
//! differ in the one way that matters: a question and its answer are **facts**
//! (`Asked` / `Answered`, `docs/adr/0024`), and a password must never become
//! one. Nothing here reaches the session log, the transcript, the composer's
//! buffer or its history; what is typed lives in a [`Zeroizing`] string, is
//! handed to the one waiter through a channel, and is wiped when this closes.
//!
//! **It is asked on the composer's line, not in a box of its own** — where
//! `atomcode-tuix` asks it, and not for consistency's sake: a password is
//! typed, and what a person types belongs where this screen puts everything
//! else it types. So the two halves are split the way the rest of the UI splits
//! them: this module owns the buffer and the keys, and the field
//! ([`crate::modules::input`]) owns the picture. The host mirrors [`Asking`] —
//! the asking program's words and a **count**, never the characters — into
//! [`crate::moment::Moment::secret`], and the composer draws that in place of
//! the draft for as long as it is there. The draft is untouched, and comes back.
//!
//! The wider mechanism (`docs/plans/2026-09-18-tui-panels-and-commands-inventory.md`
//! P0-1): `sudo` and `ssh` ask their `*_ASKPASS` helper instead of the tty when
//! one is set, the `bash` tool sets those for every child, and
//! `atomcode_capabilities::askpass` is the server they reach. Without a screen
//! answering it, a `sudo` inside a tool call waits for a tty that this UI owns
//! — which is the hang this closes.

use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;
use zeroize::{Zeroize, Zeroizing};

use crate::caps::{Caps, Glyph};
use crate::surface::{Key, KeyPress, Mods};

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

/// A password prompt as the **screen** sees it: the words the asking program
/// used, and how many characters have been typed.
///
/// The count and not the characters, and that is the whole point of the type: it
/// is what crosses into [`crate::moment::Moment`], which is cloned once a frame
/// and read by anything that draws. A copy of the password per frame is exactly
/// what this module exists to prevent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Asking {
    /// What to ask, as the program that wants it phrased it (`sudo`'s own
    /// prompt, say). Shown as given — it names the account and the host, which
    /// is how a person knows *which* password is being asked for.
    pub prompt: String,
    pub typed: usize,
}

impl Asking {
    /// The line the composer draws: `[sudo] password for lichao: ••••`.
    ///
    /// The program's own words, then one mask glyph per character — one space
    /// between them however the prompt ended, because `sudo`'s ends in a space
    /// and `ssh`'s does not, and a field with two spaces in it reads as a bug.
    pub fn line(&self, caps: &Caps) -> String {
        let words = self.prompt.trim_end();
        let masked = mask(caps).repeat(self.typed);
        if words.is_empty() {
            masked
        } else {
            format!("{words} {masked}")
        }
    }
}

/// The password being typed, and who is waiting for it.
struct Waiting {
    prompt: String,
    typed: Zeroizing<String>,
    reply: oneshot::Sender<Option<String>>,
}

impl Waiting {
    /// Hand over what was typed.
    fn answer(self) {
        let _ = self.reply.send(Some(self.typed.to_string()));
    }

    /// `None` downstream, and every reader must take it as a refusal — never as
    /// an empty password. The difference matters: `sudo` given an empty one
    /// *tries* it and burns an attempt.
    fn refuse(self) {
        let _ = self.reply.send(None);
    }
}

/// The mailbox between the program that wants a password and the screen.
///
/// At most one at a time, like [`crate::overlay::Overlays`] and for the same
/// reason: two passwords being typed into one line is two processes waiting on
/// the same keystrokes.
#[derive(Default)]
pub struct Secrets {
    waiting: Mutex<Option<Waiting>>,
}

impl Secrets {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Ask for one. Refuses whatever was already being asked.
    pub fn ask(&self, prompt: &str, reply: oneshot::Sender<Option<String>>) {
        let previous = self
            .waiting
            .lock()
            .expect("secret poisoned")
            .replace(Waiting {
                // One line: a prompt is a line, and a program that hands over a
                // paragraph must not turn the composer into a page.
                prompt: crate::ask::one_line(prompt),
                typed: Zeroizing::new(String::new()),
                reply,
            });
        if let Some(previous) = previous {
            previous.refuse();
        }
    }

    /// What the composer draws, or `None` when nothing is being asked.
    pub fn asking(&self) -> Option<Asking> {
        self.waiting
            .lock()
            .expect("secret poisoned")
            .as_ref()
            .map(|w| Asking {
                prompt: w.prompt.clone(),
                typed: w.typed.chars().count(),
            })
    }

    pub fn is_waiting(&self) -> bool {
        self.waiting.lock().expect("secret poisoned").is_some()
    }

    /// Take one key. `true` when the prompt closed — answered or refused.
    ///
    /// Everything it has no use for is swallowed rather than passed on: while a
    /// password is being asked this has the keyboard, and a key that fell
    /// through to the screen behind it would act on a screen whose field is
    /// showing something else.
    pub fn key(&self, press: KeyPress) -> bool {
        let mut slot = self.waiting.lock().expect("secret poisoned");
        if slot.is_none() {
            return false;
        }
        match (press.key, press.mods) {
            // A refusal, and the only way out that is not an answer.
            (Key::Esc, _) | (Key::Char('c'), Mods::CTRL) => {
                if let Some(waiting) = slot.take() {
                    waiting.refuse();
                }
                true
            }
            (Key::Enter, _) => {
                if let Some(waiting) = slot.take() {
                    waiting.answer();
                }
                true
            }
            (Key::Backspace, _) => {
                if let Some(waiting) = slot.as_mut() {
                    waiting.typed.pop();
                }
                false
            }
            // Start over, the shell's own key for it. Zeroed rather than
            // cleared: `String::clear` leaves the characters in the allocation,
            // and only the `Zeroizing` wrapper's own drop would have wiped them.
            (Key::Char('u'), Mods::CTRL) => {
                if let Some(waiting) = slot.as_mut() {
                    waiting.typed.zeroize();
                }
                false
            }
            (Key::Char(c), Mods::NONE) | (Key::Char(c), Mods::SHIFT) => {
                if let Some(waiting) = slot.as_mut() {
                    waiting.typed.push(c);
                }
                false
            }
            _ => false,
        }
    }

    /// Paste into the password rather than into the draft.
    ///
    /// Its own route because a paste is not a key: without this it would reach
    /// [`crate::moment::Moment::input`] — the composer's buffer, which is
    /// history, and which becomes a `UserMessage` fact the moment it is sent.
    /// A password manager's paste is how most people answer a prompt like this,
    /// so the leak is the likely path, not the exotic one.
    pub fn paste(&self, text: &str) -> bool {
        let mut slot = self.waiting.lock().expect("secret poisoned");
        if slot.is_none() {
            return false;
        }
        // A pasted newline is the enter the person did not press: everything up
        // to it is the password, and the rest is not typed at all.
        let (text, sends) = match text.split_once('\n') {
            Some((first, _)) => (first, true),
            None => (text, false),
        };
        if let Some(waiting) = slot.as_mut() {
            waiting.typed.push_str(text);
        }
        if sends {
            if let Some(waiting) = slot.take() {
                waiting.answer();
            }
        }
        sends
    }

    /// Refuse what is waiting. For shutdown: a `sudo` blocked on an answer that
    /// is never coming would hold the turn open forever, so this fails closed.
    pub fn refuse(&self) {
        let waiting = self.waiting.lock().expect("secret poisoned").take();
        if let Some(waiting) = waiting {
            waiting.refuse();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn typed(secrets: &Secrets, text: &str) {
        for c in text.chars() {
            assert!(
                !secrets.key(press(Key::Char(c))),
                "typing does not close the prompt"
            );
        }
    }

    fn asking(secrets: &Secrets) -> Asking {
        secrets.asking().expect("a password is being asked for")
    }

    /// What the line says is the program's own words and a count — the
    /// characters are nowhere in what crosses to the screen.
    ///
    /// The count is drawn on purpose: a prompt that looks identical before and
    /// after a keystroke is the one people report as broken.
    #[test]
    fn a_secret_is_drawn_as_its_length_and_never_as_itself() {
        let secrets = Secrets::default();
        let (reply, _answered) = oneshot::channel();
        secrets.ask("[sudo] password for lichao:", reply);
        let caps = Moment::default().caps;

        assert_eq!(
            asking(&secrets).line(&caps).matches(mask_here()).count(),
            0,
            "nothing typed, nothing masked"
        );

        typed(&secrets, "hunter2");
        let line = asking(&secrets).line(&caps);
        // The prompt itself is the asking program's, shown as given: it names
        // which password is wanted. Everything after it is mask and nothing
        // else — asserted over the tail rather than over the whole line,
        // because the words themselves contain most letters a password could.
        let masked = line
            .strip_prefix("[sudo] password for lichao: ")
            .unwrap_or_else(|| panic!("asked in the words the program used: {line:?}"));
        assert!(
            masked.chars().all(|c| c.to_string() == mask_here()),
            "only the mask follows the words: {line:?}"
        );
        assert_eq!(
            masked.chars().count(),
            7,
            "one glyph per character, so a keystroke is visible: {line:?}"
        );
        assert!(!line.contains("hunter2"), "{line:?}");
    }

    /// Enter hands the secret over exactly as typed, and leaves nothing behind.
    #[tokio::test]
    async fn enter_hands_over_what_was_typed_and_keeps_none_of_it() {
        let secrets = Secrets::default();
        let (reply, answered) = oneshot::channel();
        secrets.ask("password:", reply);
        typed(&secrets, "correct horse");
        assert!(secrets.key(press(Key::Enter)), "enter closes the prompt");
        assert_eq!(answered.await.unwrap().as_deref(), Some("correct horse"));
        assert_eq!(secrets.asking(), None, "and nothing is left on screen");
        assert!(!secrets.is_waiting());
    }

    /// Esc is a refusal — `None` downstream, never an empty password. The
    /// difference matters: `sudo` given an empty password *tries* it and burns
    /// an attempt.
    #[tokio::test]
    async fn esc_refuses_rather_than_answering_with_nothing() {
        let secrets = Secrets::default();
        let (reply, answered) = oneshot::channel();
        secrets.ask("password:", reply);
        typed(&secrets, "secret");
        assert!(secrets.key(press(Key::Esc)), "esc closes the prompt");
        assert_eq!(answered.await.unwrap(), None, "a refusal, not a blank");
        assert_eq!(secrets.asking(), None, "and what was typed is gone");
    }

    /// Shutdown refuses what is waiting rather than dropping it: a `sudo`
    /// holding the turn open for an answer that is never coming is the hang
    /// this whole module exists to close.
    #[tokio::test]
    async fn shutdown_refuses_what_is_still_waiting() {
        let secrets = Secrets::default();
        let (reply, answered) = oneshot::channel();
        secrets.ask("password:", reply);
        typed(&secrets, "half a password");
        secrets.refuse();
        assert_eq!(answered.await.unwrap(), None);
        assert!(!secrets.is_waiting());
    }

    /// A second prompt refuses the first rather than stacking behind it.
    #[tokio::test]
    async fn a_second_prompt_refuses_the_one_it_replaces() {
        let secrets = Secrets::default();
        let (first, refused) = oneshot::channel();
        secrets.ask("first:", first);
        let (second, answered) = oneshot::channel();
        secrets.ask("second:", second);
        assert_eq!(refused.await.unwrap(), None, "the one it replaced");
        assert_eq!(asking(&secrets).prompt, "second:");
        typed(&secrets, "pw");
        assert!(secrets.key(press(Key::Enter)));
        assert_eq!(answered.await.unwrap().as_deref(), Some("pw"));
    }

    /// Editing: backspace takes one character, ctrl-u starts over.
    #[test]
    fn a_typo_can_be_taken_back_one_character_or_all_of_them() {
        let secrets = Secrets::default();
        let (reply, _answered) = oneshot::channel();
        secrets.ask("password:", reply);
        typed(&secrets, "abcd");
        assert!(!secrets.key(press(Key::Backspace)));
        assert_eq!(asking(&secrets).typed, 3);
        assert!(!secrets.key(KeyPress::ctrl('u')));
        assert_eq!(asking(&secrets).typed, 0);
    }

    /// A pasted password goes into the password, never into the composer's
    /// buffer — which is history, and which becomes a fact when it is sent.
    /// A newline in what was pasted is the enter the person did not press.
    #[tokio::test]
    async fn a_pasted_password_is_the_password_and_a_newline_sends_it() {
        let secrets = Secrets::default();
        let (reply, _answered) = oneshot::channel();
        secrets.ask("password:", reply);
        assert!(!secrets.paste("from-the-"), "still open");
        assert_eq!(asking(&secrets).typed, 9);

        let (reply, answered) = oneshot::channel();
        secrets.ask("password:", reply);
        assert!(secrets.paste("pw\n"), "the newline is the enter");
        assert_eq!(answered.await.unwrap().as_deref(), Some("pw"));
    }

    /// Every other key is swallowed and none of them types anything.
    #[test]
    fn keys_it_has_no_use_for_type_nothing() {
        let secrets = Secrets::default();
        let (reply, _answered) = oneshot::channel();
        secrets.ask("password:", reply);
        for key in [Key::Up, Key::Down, Key::Tab, Key::PageUp] {
            assert!(!secrets.key(press(key)), "{key:?} closed the prompt");
        }
        assert_eq!(asking(&secrets).typed, 0);
    }

    /// With nothing being asked, a key is not this module's: it says so, so the
    /// screen behind it keeps its keyboard.
    #[test]
    fn a_key_with_nothing_being_asked_belongs_to_the_screen() {
        let secrets = Secrets::default();
        assert!(!secrets.key(press(Key::Enter)));
        assert!(!secrets.paste("text"));
        assert_eq!(secrets.asking(), None);
    }
}
