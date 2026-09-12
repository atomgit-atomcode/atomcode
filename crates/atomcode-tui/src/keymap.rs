//! Keys produce **actions**, never side effects.
//!
//! Bindings are *data*, not a `resolve()` function, and that is load-bearing:
//! two rows claiming one key can be found the moment they mount rather than the
//! moment someone presses it.

use std::collections::HashMap;

use crate::surface::{Key, KeyPress, Mods};

/// What a key or a command asks for. The one vocabulary three entry points
/// share — a key, a slash command, and the model's `adjust_layout` all end
/// here, so there is one implementation of each effect rather than three.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Send what is typed.
    Submit,
    Insert(char),
    Backspace,
    DeleteWord,
    Clear,
    CaretLeft,
    CaretRight,
    CaretHome,
    CaretEnd,
    /// Up a row inside what is being typed — or, at the top of it, back to the
    /// previous thing that was said.
    CaretUp,
    /// The mirror: down a row, or forward through the history and out the far
    /// side to the draft that was set aside.
    CaretDown,
    /// Stop the running turn. Cooperative.
    Cancel,
    Quit,
    Scroll(i32),
    ScrollToBottom,
    /// A click landed on this cell. What it means is decided by what is drawn
    /// there — a caret in the composer, a fold on a tool call, the way back
    /// down on the badge — because the alternative is three actions that all
    /// have to agree about the layout.
    ClickAt(u16, u16),
    /// Take the pointer, or hand it back to the terminal so click-drag selects
    /// text again.
    ToggleMouse,
    /// Forget what is believed to be on screen and paint all of it again.
    Redraw,
    /// A line break inside what is being typed, rather than sending it.
    Newline,
    /// Back out of the innermost thing: the selection, then what is typed, then
    /// the turn. Distinct from [`Action::Cancel`], which always stops the turn —
    /// pressing ctrl-c to stop a model that is running must not turn into
    /// "cleared your draft" just because there was one.
    Escape,
    /// Start a selection at this cell.
    SelectFrom(u16, u16),
    /// Drag it out to here.
    SelectTo(u16, u16),
    /// Finish it: copy what it covers, and leave it up so it can be seen.
    CopySelection,
    /// Drop it.
    ClearSelection,
    /// Fold or unfold every block of a kind.
    ToggleFold(&'static str),
    /// Show or hide a module.
    ToggleModule(&'static str),
    /// Change the screen's shape. The third entry point into `Layout::apply`,
    /// alongside a command and the model's tool.
    Layout(crate::layout::LayoutOp),
    /// Paste arrived as one event rather than N keystrokes.
    Paste(String),
    /// Take whatever image the clipboard holds and attach it to what is being
    /// typed. A no-op when there is none — the key is also how a person finds
    /// out that there is none.
    AttachImage,
}

/// A set of bindings contributed by one row.
pub trait Keymap: Send + Sync {
    fn id(&self) -> &'static str;
    fn bindings(&self) -> Vec<(KeyPress, Action)>;
}

/// Every binding mounted, with conflicts refused at mount time.
#[derive(Default)]
pub struct Keys {
    map: HashMap<KeyPress, (&'static str, Action)>,
}

impl Keys {
    pub fn new() -> Self {
        Self::default()
    }

    /// Two rows claiming one key is an error, not last-write-wins: the user
    /// would get whichever row happened to mount second, and nothing would say
    /// so.
    pub fn add(&mut self, km: &dyn Keymap) -> Result<(), String> {
        for (press, action) in km.bindings() {
            if let Some((owner, _)) = self.map.get(&press) {
                return Err(format!(
                    "`{}` and `{}` both bind {press:?}; disable one",
                    owner,
                    km.id()
                ));
            }
            self.map.insert(press, (km.id(), action));
        }
        Ok(())
    }

    pub fn remove(&mut self, id: &str) {
        self.map.retain(|_, (owner, _)| *owner != id);
    }

    pub fn resolve(&self, press: KeyPress) -> Option<Action> {
        if let Some((_, a)) = self.map.get(&press) {
            return Some(a.clone());
        }
        // Typing is the fallthrough, not a binding: binding every printable
        // character would make every conflict check meaningless.
        match (press.key, press.mods) {
            (Key::Char(c), Mods::NONE) | (Key::Char(c), Mods::SHIFT) => Some(Action::Insert(c)),
            _ => None,
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
    /// For the help panel and for the model's view of what the keys do.
    pub fn describe(&self) -> Vec<(String, String)> {
        let mut out: Vec<_> = self
            .map
            .iter()
            .map(|(k, (_, a))| (format!("{k:?}"), format!("{a:?}")))
            .collect();
        out.sort();
        out
    }
}

/// The bindings the shipped TUI comes with.
pub struct Default_;

impl Keymap for Default_ {
    fn id(&self) -> &'static str {
        "keys-default"
    }
    fn bindings(&self) -> Vec<(KeyPress, Action)> {
        vec![
            (KeyPress::plain(Key::Enter), Action::Submit),
            (KeyPress::plain(Key::Backspace), Action::Backspace),
            (KeyPress::plain(Key::Esc), Action::Escape),
            (KeyPress::ctrl('c'), Action::Cancel),
            // Three keys for one action, because only one of them can be
            // relied on. Shift-enter is what people reach for and needs the
            // keyboard protocol to arrive at all; alt-enter is what several
            // terminals send instead; ctrl-j is a literal line feed and works
            // everywhere, including through the terminals that pass none of
            // the modifiers on.
            (KeyPress::new(Key::Enter, Mods::SHIFT), Action::Newline),
            (KeyPress::new(Key::Enter, Mods::ALT), Action::Newline),
            (KeyPress::ctrl('j'), Action::Newline),
            (KeyPress::ctrl('d'), Action::Quit),
            (KeyPress::ctrl('u'), Action::Clear),
            (KeyPress::ctrl('w'), Action::DeleteWord),
            (KeyPress::plain(Key::Left), Action::CaretLeft),
            (KeyPress::plain(Key::Right), Action::CaretRight),
            (KeyPress::plain(Key::Home), Action::CaretHome),
            (KeyPress::plain(Key::End), Action::CaretEnd),
            (KeyPress::plain(Key::PageUp), Action::Scroll(-10)),
            (KeyPress::plain(Key::PageDown), Action::Scroll(10)),
            // The arrows belong to the field, the way they do in every other
            // text input. Scrolling the conversation is pgup/pgdn and the
            // wheel — which now moves a line a notch, so nothing was lost.
            (KeyPress::plain(Key::Up), Action::CaretUp),
            (KeyPress::plain(Key::Down), Action::CaretDown),
            (KeyPress::ctrl('r'), Action::ToggleFold("reasoning")),
            (KeyPress::ctrl('t'), Action::ToggleFold("tool_call")),
            // Ctrl+V, and the alternate Windows Terminal sends instead. A
            // terminal with no bracketed-paste support delivers a screenshot
            // paste as a literal `\x16` — which is exactly why the chord is
            // bound rather than left to `Input::Paste`: the byte that arrives
            // is a key, and a key that nothing binds is a keystroke that
            // vanishes. Ctrl+Shift+V stays unbound, so a terminal's own
            // "paste as plain text" still gets through.
            (KeyPress::ctrl('v'), Action::AttachImage),
            (
                KeyPress::new(
                    Key::Char('v'),
                    Mods {
                        ctrl: true,
                        alt: true,
                        shift: false,
                    },
                ),
                Action::AttachImage,
            ),
            (KeyPress::ctrl('n'), Action::ToggleModule("mascot")),
            // Hand the mouse back, and take it again. `o` for "off", and one of
            // the few control keys a terminal does not already claim.
            (KeyPress::ctrl('o'), Action::ToggleMouse),
            // ctrl-l is "redraw" in every terminal there has ever been, and
            // that is the reflex to serve: it is the key a person reaches for
            // when the screen is wrong.
            (KeyPress::ctrl('l'), Action::Redraw),
            // The `focus` preset moved off ctrl-l rather than being dropped —
            // it is one of the three routes to a layout op, and the layout
            // tests exist to check that all three still agree.
            (
                KeyPress::ctrl('f'),
                Action::Layout(crate::layout::LayoutOp::Preset {
                    name: "focus".into(),
                }),
            ),
            (
                KeyPress::ctrl('z'),
                Action::Layout(crate::layout::LayoutOp::Undo),
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_bindings_mount_without_conflicting_with_themselves() {
        let mut keys = Keys::new();
        keys.add(&Default_).unwrap();
        assert_eq!(keys.len(), Default_.bindings().len());
    }

    #[test]
    fn two_rows_claiming_one_key_is_caught_at_mount_not_at_press() {
        struct Rival;
        impl Keymap for Rival {
            fn id(&self) -> &'static str {
                "rival"
            }
            fn bindings(&self) -> Vec<(KeyPress, Action)> {
                vec![(KeyPress::ctrl('d'), Action::Clear)]
            }
        }
        let mut keys = Keys::new();
        keys.add(&Default_).unwrap();
        let err = keys.add(&Rival).unwrap_err();
        assert!(err.contains("both bind"), "{err}");
        assert!(err.contains("rival"), "it names the newcomer: {err}");
    }

    #[test]
    fn typing_falls_through_rather_than_being_bound_per_character() {
        let mut keys = Keys::new();
        keys.add(&Default_).unwrap();
        assert_eq!(keys.resolve(KeyPress::ch('x')), Some(Action::Insert('x')));
        assert_eq!(keys.resolve(KeyPress::ch('中')), Some(Action::Insert('中')));
        // A bound char still wins.
        assert_eq!(keys.resolve(KeyPress::ctrl('d')), Some(Action::Quit));
    }

    #[test]
    fn unmounting_a_row_takes_its_bindings_with_it() {
        let mut keys = Keys::new();
        keys.add(&Default_).unwrap();
        keys.remove("keys-default");
        assert!(keys.is_empty());
        assert_eq!(keys.resolve(KeyPress::ctrl('d')), None);
    }
}
