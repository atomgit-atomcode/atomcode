//! Keys produce **actions**, never side effects.
//!
//! Bindings are *data*, not a `resolve()` function, and that is load-bearing:
//! two rows claiming one key can be found the moment they mount rather than the
//! moment someone presses it.

use std::collections::HashMap;
use std::sync::RwLock;

use crate::surface::{Key, KeyPress, Mods};

/// What a key or a command asks for. The one vocabulary a key and a slash
/// command share, so there is one implementation of each effect rather than
/// two.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Send what is typed.
    Submit,
    Insert(char),
    Backspace,
    /// Take out the character the caret is on.
    ///
    /// The composer had no forward delete at all until this: `Delete` was
    /// unbound, so the key did nothing and the only way to remove the character
    /// ahead of the caret was to step over it and press backspace.
    DeleteForward,
    DeleteWord,
    /// Take out everything from the caret to the end.
    ///
    /// To the end of the **buffer**, which is where [`Action::CaretEnd`] goes —
    /// the pair has to agree, or `ctrl-k` deletes to somewhere `ctrl-e` does not
    /// go and neither key can be reasoned about. (A multi-line draft is one
    /// buffer here; `Home`/`End` answer the same way, and changing that is a
    /// change to those keys, not something to smuggle in with this one.)
    DeleteToEnd,
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
    /// A double-click: select the word under this cell and copy it. The
    /// terminal's own word-select, which taking the mouse for drag disabled.
    SelectWord(u16, u16),
    /// A triple-click: select this whole row (its non-blank span) and copy it.
    SelectLine(u16, u16),
    /// Finish it: copy what it covers, and leave it up so it can be seen.
    CopySelection,
    /// Drop it.
    ClearSelection,
    /// Show more of every block of a kind, or put it away again: a one-line
    /// lid, then the whole thing, then — for a kind that may be hidden — off
    /// the screen.
    ToggleFold(&'static str),
    /// Set a tool call's output mode by name, rather than stepping to the next
    /// one. `/tools group` has to mean the same thing wherever it is typed;
    /// [`ToggleFold`](Self::ToggleFold) on `tool_call` means "the next one",
    /// which is not a thing a typed command can say.
    SetToolOutput(crate::host::ToolOutput),
    /// The same over several kinds at once, which is what `/showinject` with no
    /// argument is: one gesture over the environment's injections. The group is
    /// the unit a person has an opinion about — nobody wants to be told they may
    /// show the reminder but not the memory.
    ///
    /// A `Vec` rather than the `&'static [&'static str]` the group constant is,
    /// because `all` is built at dispatch time and leaking it to make the types
    /// match would be a leak per keystroke. The list is five strings long.
    ToggleFolds(Vec<&'static str>),
    /// Paste arrived as one event rather than N keystrokes.
    Paste(String),
    /// Take whatever image the clipboard holds and attach it to what is being
    /// typed. A no-op when there is none — the key is also how a person finds
    /// out that there is none.
    AttachImage,
    /// Pull the settings panel up over the composer, or put it away if it is
    /// already up.
    ///
    /// An action rather than something the command does itself, for the reason
    /// every other command that changes the screen is: `/config` and whatever
    /// key is bound to it later are one implementation, and the panel is
    /// screen state, which only the screen may write.
    ToggleSettings,
    /// Pull the providers panel up over the composer, or put it away.
    ///
    /// Its own action rather than a parameter on [`Action::ToggleSettings`]:
    /// they are two panels with two ports behind them, and a build may mount
    /// either without the other.
    ToggleProviders,
    /// Open the providers panel on its 模型 (models) list, opening the panel if
    /// it is not already up. What `/model` with no argument lands on: the model
    /// list is where a person switches models, and it is the same panel
    /// `/provider` opens (which starts on the 账号 tab) — one surface, not a
    /// second popup that only lists models.
    OpenModels,
    /// Step to the next configured model, or the previous one.
    ///
    /// The one-key form of `/model <id>`, and it runs exactly that — the panel
    /// switches the same way, so there is one road into a model change rather
    /// than a second that has to keep agreeing with it.
    ///
    /// It **says which model it landed on**, because a key that silently
    /// changes which model is answering is a key nobody can afford to press by
    /// accident. Reversible for the same reason: `Shift+F2` steps back.
    CycleModel {
        forward: bool,
    },
    /// Pull the plugins panel up over the composer, or put it away.
    ///
    /// Its own action for the reason [`Action::ToggleProviders`] is one: it has
    /// a port of its own, and a build may mount it without the others.
    TogglePlugins,
    /// Pull the tools panel up over the composer, or put it away.
    ///
    /// Its own action for the same reason: its port is its own, and a build may
    /// mount it without the others.
    ToggleTools,
    /// Pull the MCP panel up over the composer, or put it away.
    ///
    /// Its own action for the same reason: its port is its own, and a build may
    /// mount it without the others.
    ToggleMcp,
    /// Pull the rewind panel up, or put it away.
    ///
    /// Its own action for the same reason the other five are: its port is its
    /// own, and a build may mount it without the others. Two gestures ask for
    /// it — a double-tap on Esc and `/rewind` with nothing after it — and they
    /// are one implementation because they are one action.
    ToggleRewind,
    /// Bring the resume panel up on the sessions the host just answered.
    ///
    /// Carries its data, unlike the other panels, because that data is a disk
    /// read: `/resume` makes the trip itself (it is already an async command)
    /// and hands the answer in, rather than the screen holding a seam it would
    /// have to pump. One gesture asks for it — `/resume` with nothing after it.
    OpenResume(crate::resume::ResumeView),
    /// Put this session — the lead, or a member of its team — on screen.
    ///
    /// An action, and the only one carrying a value, for the reason
    /// [`Action::ToggleSettings`] is: which session a screen is drawn from is
    /// screen state, so a command may ask for it but never do it. The team
    /// strip asks for this on a click and `Enter`; `/agents` asks for it for a
    /// member the strip no longer has a row for (`docs/adr/0023` §5), and both
    /// land in the same place.
    LookAt(String),
    /// Step the execution mode on to the next one — the gesture behind
    /// Shift+Tab, and plain Tab where the configuration says so.
    ///
    /// An action rather than a binding because which key it is depends on a
    /// setting read live (`ui.mode_switch_key`), and a keymap is a fixed table:
    /// the predicate that asks the setting lives in the loop, and this is what
    /// it asks for. What it *does* is `/mode`'s business, so the key, the word
    /// and the command are one implementation.
    CycleMode,
}

/// A set of bindings contributed by one row.
pub trait Keymap: Send + Sync {
    fn id(&self) -> &'static str;
    fn bindings(&self) -> Vec<(KeyPress, Action)>;
    /// Presses this map takes over from whoever already has them.
    ///
    /// The one way a key may be bound twice, and it has to be said out loud —
    /// the same rule, for the same reason, as [`crate::command::CommandSet::overrides`].
    /// Without it a downstream build that wants ctrl-r for something else has
    /// to drop the whole shipped map and re-declare forty bindings to change
    /// one.
    fn overrides(&self) -> Vec<KeyPress> {
        Vec::new()
    }
}

/// Every binding mounted, with conflicts refused at mount time.
///
/// Behind a lock and reached through `KeysSvc`, because bindings arrive from
/// rows while the screen is already up: a capability that mounts brings its key
/// with it and takes it away again when it unloads.
#[derive(Default)]
pub struct Keys {
    map: RwLock<HashMap<KeyPress, (&'static str, Action)>>,
}

impl Keys {
    pub fn new() -> Self {
        Self::default()
    }

    /// Two rows claiming one key is an error, not last-write-wins: the user
    /// would get whichever row happened to mount second, and nothing would say
    /// so. Unless the second one says it is taking it ([`Keymap::overrides`]).
    pub fn add(&self, km: &dyn Keymap) -> Result<(), String> {
        let taken_over = km.overrides();
        let mut map = self.map.write().expect("keys poisoned");
        for (press, action) in km.bindings() {
            if let Some((owner, _)) = map.get(&press) {
                if !taken_over.contains(&press) {
                    return Err(format!(
                        "`{}` and `{}` both bind {press:?}; disable one",
                        owner,
                        km.id()
                    ));
                }
            }
            map.insert(press, (km.id(), action));
        }
        Ok(())
    }

    pub fn remove(&self, id: &str) {
        self.map
            .write()
            .expect("keys poisoned")
            .retain(|_, (owner, _)| *owner != id);
    }

    pub fn resolve(&self, press: KeyPress) -> Option<Action> {
        if let Some((_, a)) = self.map.read().expect("keys poisoned").get(&press) {
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
        self.map.read().expect("keys poisoned").len()
    }
    pub fn is_empty(&self) -> bool {
        self.map.read().expect("keys poisoned").is_empty()
    }
    /// For the help panel and for the model's view of what the keys do.
    pub fn describe(&self) -> Vec<(String, String)> {
        let mut out: Vec<_> = self
            .map
            .read()
            .expect("keys poisoned")
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
            // The rest of the line-editing reflexes every shell has. They were
            // missing, so the muscle memory landed on nothing: ctrl-a and
            // ctrl-e are documented in the keys help as "go to the start / the
            // end" everywhere else a person types, and ctrl-k is how a line is
            // cut short. Bound to the SAME actions `Home` and `End` already
            // use, rather than to line-relative ones — two answers to "where
            // does the caret go" is how they stop agreeing.
            (KeyPress::ctrl('a'), Action::CaretHome),
            (KeyPress::ctrl('e'), Action::CaretEnd),
            (KeyPress::ctrl('k'), Action::DeleteToEnd),
            (KeyPress::plain(Key::Delete), Action::DeleteForward),
            // What SSH, PuTTY and a few terminal emulators send instead of the
            // keys above: the physical Backspace arrives as `^H` and Delete as
            // `^?`. Unbound, backspace simply stops working over those links —
            // and it is the key a person presses most. `^H` IS backspace in
            // ASCII, so this is the meaning of the chord rather than a guess at
            // one.
            (KeyPress::ctrl('h'), Action::Backspace),
            (KeyPress::ctrl('?'), Action::DeleteForward),
            // Stepping between configured models without leaving the keyboard.
            // `F2` because it is what the other front end used and nothing else
            // here claims it; the list it steps through is the same one
            // `/model` and the providers panel show.
            (
                KeyPress::plain(Key::F(2)),
                Action::CycleModel { forward: true },
            ),
            (
                KeyPress::new(Key::F(2), Mods::SHIFT),
                Action::CycleModel { forward: false },
            ),
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
                        cmd: false,
                    },
                ),
                Action::AttachImage,
            ),
            // Hand the mouse back, and take it again. `o` for "off", and one of
            // the few control keys a terminal does not already claim.
            (KeyPress::ctrl('o'), Action::ToggleMouse),
            // ctrl-l is "redraw" in every terminal there has ever been, and
            // that is the reflex to serve: it is the key a person reaches for
            // when the screen is wrong.
            (KeyPress::ctrl('l'), Action::Redraw),
            // The copy reflex, bound to the OS chords rather than left to the
            // Insert fallthrough — so `Cmd+C`/`Ctrl+Shift+C` copy the selection
            // (as a mouse drag-release does) instead of typing a `c`. A no-op
            // when nothing is selected, but still consumed. Both the lowercase
            // and the shifted-uppercase char the enhanced protocol may report
            // for Ctrl+Shift+C are bound. Only fires on terminals that pass the
            // chord through; many handle their own copy and never send it.
            (
                KeyPress::new(
                    Key::Char('c'),
                    Mods {
                        cmd: true,
                        ..Mods::NONE
                    },
                ),
                Action::CopySelection,
            ),
            (
                KeyPress::new(
                    Key::Char('c'),
                    Mods {
                        ctrl: true,
                        shift: true,
                        ..Mods::NONE
                    },
                ),
                Action::CopySelection,
            ),
            (
                KeyPress::new(
                    Key::Char('C'),
                    Mods {
                        ctrl: true,
                        shift: true,
                        ..Mods::NONE
                    },
                ),
                Action::CopySelection,
            ),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_bindings_mount_without_conflicting_with_themselves() {
        let keys = Keys::new();
        keys.add(&Default_).unwrap();
        assert_eq!(keys.len(), Default_.bindings().len());
    }

    /// The line-editing reflexes every shell answers to.
    ///
    /// `ctrl-a` / `ctrl-e` are asserted to resolve to the **same** actions
    /// `Home` / `End` do, rather than to actions of their own: two ways to ask
    /// where the caret goes is how the two answers start to differ.
    #[test]
    fn the_shell_line_editing_chords_are_bound() {
        let keys = Keys::new();
        keys.add(&Default_).unwrap();

        let home = keys.resolve(KeyPress::plain(Key::Home));
        let end = keys.resolve(KeyPress::plain(Key::End));
        assert_eq!(keys.resolve(KeyPress::ctrl('a')), home, "ctrl-a is Home");
        assert_eq!(keys.resolve(KeyPress::ctrl('e')), end, "ctrl-e is End");
        assert_eq!(
            keys.resolve(KeyPress::ctrl('k')),
            Some(Action::DeleteToEnd),
            "ctrl-k cuts the rest of the line"
        );
        assert_eq!(
            keys.resolve(KeyPress::plain(Key::Delete)),
            Some(Action::DeleteForward),
            "the composer had no forward delete at all: Delete was unbound"
        );
    }

    /// One key steps to the next model, shifted steps back.
    ///
    /// Both directions asserted: a cycle you cannot walk back is one a person
    /// has to walk all the way round, and on a list of two that is the same
    /// key twice.
    #[test]
    fn f2_steps_between_models_and_shift_f2_steps_back() {
        let keys = Keys::new();
        keys.add(&Default_).unwrap();
        assert_eq!(
            keys.resolve(KeyPress::plain(Key::F(2))),
            Some(Action::CycleModel { forward: true })
        );
        assert_eq!(
            keys.resolve(KeyPress::new(Key::F(2), Mods::SHIFT)),
            Some(Action::CycleModel { forward: false })
        );
    }

    /// What SSH and PuTTY send instead. Unbound, backspace stops working over
    /// those links — and it is the key pressed most.
    #[test]
    fn the_ascii_control_forms_of_backspace_and_delete_still_work() {
        let keys = Keys::new();
        keys.add(&Default_).unwrap();
        assert_eq!(
            keys.resolve(KeyPress::ctrl('h')),
            Some(Action::Backspace),
            "^H is backspace in ASCII, and is what some terminals send for it"
        );
        assert_eq!(
            keys.resolve(KeyPress::ctrl('?')),
            Some(Action::DeleteForward),
            "^? is what they send for Delete"
        );
    }

    #[test]
    fn cmd_c_and_ctrl_shift_c_copy_rather_than_type_a_c() {
        // The copy reflex: a person who selects text and reaches for the OS copy
        // chord must not have a literal `c` land in the field. Both forms map to
        // the same copy the mouse drag already performs.
        let keys = Keys::new();
        keys.add(&Default_).unwrap();

        let cmd_c = KeyPress::new(
            Key::Char('c'),
            Mods {
                cmd: true,
                ..Mods::NONE
            },
        );
        assert_eq!(
            keys.resolve(cmd_c),
            Some(Action::CopySelection),
            "Cmd+C copies, it does not type a c"
        );

        let ctrl_shift_c = KeyPress::new(
            Key::Char('c'),
            Mods {
                ctrl: true,
                shift: true,
                ..Mods::NONE
            },
        );
        assert_eq!(
            keys.resolve(ctrl_shift_c),
            Some(Action::CopySelection),
            "Ctrl+Shift+C copies, it does not type a c"
        );

        // The shifted char some terminals report for the same chord.
        let ctrl_shift_upper = KeyPress::new(
            Key::Char('C'),
            Mods {
                ctrl: true,
                shift: true,
                ..Mods::NONE
            },
        );
        assert_eq!(
            keys.resolve(ctrl_shift_upper),
            Some(Action::CopySelection),
            "the uppercase form the enhanced protocol may send also copies"
        );
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
        let keys = Keys::new();
        keys.add(&Default_).unwrap();
        let err = keys.add(&Rival).unwrap_err();
        assert!(err.contains("both bind"), "{err}");
        assert!(err.contains("rival"), "it names the newcomer: {err}");
    }

    #[test]
    fn typing_falls_through_rather_than_being_bound_per_character() {
        let keys = Keys::new();
        keys.add(&Default_).unwrap();
        assert_eq!(keys.resolve(KeyPress::ch('x')), Some(Action::Insert('x')));
        assert_eq!(keys.resolve(KeyPress::ch('中')), Some(Action::Insert('中')));
        // A bound char still wins.
        assert_eq!(keys.resolve(KeyPress::ctrl('d')), Some(Action::Quit));
    }

    /// A downstream build can rebind one press without re-declaring the other
    /// forty. The clash test above is this one's negative control: the same
    /// collision without the declaration is still refused.
    #[test]
    fn a_row_takes_over_one_press_only_by_saying_so() {
        struct Mine;
        impl Keymap for Mine {
            fn id(&self) -> &'static str {
                "mine"
            }
            fn bindings(&self) -> Vec<(KeyPress, Action)> {
                vec![(KeyPress::ctrl('d'), Action::Clear)]
            }
            fn overrides(&self) -> Vec<KeyPress> {
                vec![KeyPress::ctrl('d')]
            }
        }
        let keys = Keys::new();
        keys.add(&Default_).unwrap();
        keys.add(&Mine).unwrap();
        assert_eq!(keys.resolve(KeyPress::ctrl('d')), Some(Action::Clear));
        // Everything else the shipped map bound is still bound.
        assert_eq!(keys.resolve(KeyPress::ctrl('w')), Some(Action::DeleteWord));
        // Unloading the row that took it leaves the press unbound rather than
        // restoring what it displaced: an override is a replacement, not a
        // stack, and a screen that quietly resurrected an old binding would be
        // a third answer to "what does ctrl-d do".

        keys.remove("mine");
        assert_eq!(keys.resolve(KeyPress::ctrl('d')), None);
    }

    #[test]
    fn unmounting_a_row_takes_its_bindings_with_it() {
        let keys = Keys::new();
        keys.add(&Default_).unwrap();
        keys.remove("keys-default");
        assert!(keys.is_empty());
        assert_eq!(keys.resolve(KeyPress::ctrl('d')), None);
    }
}
