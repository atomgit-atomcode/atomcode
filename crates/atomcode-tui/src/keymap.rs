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
    /// Stop the running turn. Cooperative.
    Cancel,
    Quit,
    Scroll(i32),
    ScrollToBottom,
    /// Fold or unfold every block of a kind.
    ToggleFold(&'static str),
    /// Show or hide a module.
    ToggleModule(&'static str),
    /// Paste arrived as one event rather than N keystrokes.
    Paste(String),
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
            (KeyPress::plain(Key::Esc), Action::Cancel),
            (KeyPress::ctrl('c'), Action::Cancel),
            (KeyPress::ctrl('d'), Action::Quit),
            (KeyPress::ctrl('u'), Action::Clear),
            (KeyPress::ctrl('w'), Action::DeleteWord),
            (KeyPress::plain(Key::Left), Action::CaretLeft),
            (KeyPress::plain(Key::Right), Action::CaretRight),
            (KeyPress::plain(Key::Home), Action::CaretHome),
            (KeyPress::plain(Key::End), Action::CaretEnd),
            (KeyPress::plain(Key::PageUp), Action::Scroll(-10)),
            (KeyPress::plain(Key::PageDown), Action::Scroll(10)),
            (KeyPress::plain(Key::Up), Action::Scroll(-1)),
            (KeyPress::plain(Key::Down), Action::Scroll(1)),
            (KeyPress::ctrl('r'), Action::ToggleFold("reasoning")),
            (KeyPress::ctrl('t'), Action::ToggleFold("tool_call")),
            (KeyPress::ctrl('n'), Action::ToggleModule("mascot")),
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
