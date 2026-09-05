//! Where a frame is painted, and where input comes from.
//!
//! A seam with exactly one provider, bound at the root realm. The terminal is a
//! physical singleton: the region tree can divide it, realms cannot duplicate
//! it. Two implementations ship — a real terminal and a headless recorder —
//! and every test above this line runs against the second one, with no tty.

use std::io::Write;
use std::sync::{Arc, Mutex};

use crate::ansi;
use crate::frame::Frame;

/// A key the user pressed, in a form a test can construct.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    Char(char),
    Enter,
    Backspace,
    Delete,
    Tab,
    BackTab,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
}

/// Modifiers, as a set rather than a bitfield so an assertion reads plainly.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Mods {
    pub const NONE: Mods = Mods {
        ctrl: false,
        alt: false,
        shift: false,
    };
    pub const CTRL: Mods = Mods {
        ctrl: true,
        alt: false,
        shift: false,
    };
    pub const ALT: Mods = Mods {
        ctrl: false,
        alt: true,
        shift: false,
    };
    pub const SHIFT: Mods = Mods {
        ctrl: false,
        alt: false,
        shift: true,
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct KeyPress {
    pub key: Key,
    pub mods: Mods,
}

impl KeyPress {
    pub const fn new(key: Key, mods: Mods) -> Self {
        Self { key, mods }
    }
    pub const fn plain(key: Key) -> Self {
        Self::new(key, Mods::NONE)
    }
    pub const fn ch(c: char) -> Self {
        Self::new(Key::Char(c), Mods::NONE)
    }
    pub const fn ctrl(c: char) -> Self {
        Self::new(Key::Char(c), Mods::CTRL)
    }
}

/// Everything that can arrive from the outside world.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    Key(KeyPress),
    /// A bracketed paste, delivered whole rather than as N keystrokes.
    Paste(String),
    Resize(u16, u16),
}

/// The terminal, as a seam.
pub trait Surface: Send + Sync {
    fn describe(&self) -> String;
    fn size(&self) -> (u16, u16);
    /// Paint. Must be total: a frame larger than the surface is clipped, never
    /// an error and never a panic.
    fn present(&self, frame: &Frame);
    /// Called once on the way out. Restoring the terminal is not optional —
    /// leaving a shell in raw mode is worse than showing no UI at all.
    fn restore(&self) {}

    /// Take this surface's own input stream, once.
    ///
    /// `None` means "read the real terminal" — which is what the terminal
    /// surface says, because its input is the tty. A headless surface returns a
    /// channel it feeds itself, and that is the whole reason the UI can be
    /// driven end to end with no tty, no keyboard and no human.
    fn take_input(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<Input>> {
        None
    }

    /// The recorder behind this surface, when it is one. How a test reaches the
    /// frames without the tree having to know it is being tested.
    fn as_any_headless(&self) -> Option<Arc<Headless>> {
        None
    }
}

// ---- headless -----------------------------------------------------------

/// A surface that paints into memory and remembers everything.
///
/// The whole automated loop rests on this: no tty, no escape-sequence guessing,
/// and every frame kept so a test can assert on the *sequence* rather than only
/// on the end state.
#[derive(Debug)]
pub struct Headless {
    size: Mutex<(u16, u16)>,
    frames: Mutex<Vec<Frame>>,
    keys: tokio::sync::mpsc::UnboundedSender<Input>,
    incoming: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<Input>>>,
    /// A weak handle back to the `Arc` this lives in, so a consumer holding
    /// `Arc<dyn Surface>` can get the recorder back without downcasting.
    me: Mutex<Option<std::sync::Weak<Headless>>>,
}

impl Headless {
    pub fn new(w: u16, h: u16) -> Arc<Self> {
        let (keys, incoming) = tokio::sync::mpsc::unbounded_channel();
        let me = Arc::new(Self {
            size: Mutex::new((w, h)),
            frames: Mutex::new(Vec::new()),
            keys,
            incoming: Mutex::new(Some(incoming)),
            me: Mutex::new(None),
        });
        *me.me.lock().expect("headless poisoned") = Some(Arc::downgrade(&me));
        me
    }

    /// Press a key.
    pub fn press(&self, press: KeyPress) {
        let _ = self.keys.send(Input::Key(press));
    }

    /// Type a line and submit it — the single most common scripted gesture.
    pub fn type_line(&self, text: &str) {
        for c in text.chars() {
            self.press(KeyPress::ch(c));
        }
        self.press(KeyPress::plain(Key::Enter));
    }

    /// Type without submitting, for asserting on a half-finished line.
    pub fn type_text(&self, text: &str) {
        for c in text.chars() {
            self.press(KeyPress::ch(c));
        }
    }

    pub fn paste(&self, text: &str) {
        let _ = self.keys.send(Input::Paste(text.to_string()));
    }

    /// Wait until the screen stops changing.
    ///
    /// A quiescence predicate, not a sleep: `settle` that timed out would be a
    /// test that passes while nothing happened, so the caller gets `false` and
    /// is expected to fail on it.
    pub async fn settle(&self, quiet_for: std::time::Duration, limit: std::time::Duration) -> bool {
        let start = std::time::Instant::now();
        let mut last = self.frame_count();
        let mut still = std::time::Instant::now();
        while start.elapsed() < limit {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            let now = self.frame_count();
            if now != last {
                last = now;
                still = std::time::Instant::now();
            } else if still.elapsed() >= quiet_for {
                return true;
            }
        }
        false
    }

    pub fn resize(&self, w: u16, h: u16) {
        *self.size.lock().expect("headless poisoned") = (w, h);
    }

    /// Every frame painted, in order.
    pub fn frames(&self) -> Vec<Frame> {
        self.frames.lock().expect("headless poisoned").clone()
    }

    pub fn frame_count(&self) -> usize {
        self.frames.lock().expect("headless poisoned").len()
    }

    pub fn last(&self) -> Option<Frame> {
        self.frames
            .lock()
            .expect("headless poisoned")
            .last()
            .cloned()
    }

    /// The last frame as plain rows — what a person would see.
    pub fn screen(&self) -> Vec<String> {
        self.last().map(|f| f.rows()).unwrap_or_default()
    }

    /// The last frame as one string, for `contains` assertions.
    pub fn text(&self) -> String {
        self.screen().join("\n")
    }

    /// The bytes the real terminal would have received for the last frame.
    /// The input to the external oracle.
    pub fn bytes(&self) -> String {
        self.last().map(|f| ansi::encode(&f)).unwrap_or_default()
    }

    pub fn clear(&self) {
        self.frames.lock().expect("headless poisoned").clear();
    }
}

impl Surface for Headless {
    fn describe(&self) -> String {
        "headless (frames kept in memory)".into()
    }
    fn size(&self) -> (u16, u16) {
        *self.size.lock().expect("headless poisoned")
    }
    fn present(&self, frame: &Frame) {
        self.frames
            .lock()
            .expect("headless poisoned")
            .push(frame.clone());
    }
    fn take_input(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<Input>> {
        self.incoming.lock().expect("headless poisoned").take()
    }
    fn as_any_headless(&self) -> Option<Arc<Headless>> {
        self.me
            .lock()
            .expect("headless poisoned")
            .clone()?
            .upgrade()
    }
}

// ---- a real terminal ----------------------------------------------------

/// The alternate screen, restored on drop.
///
/// Full-screen rather than inline because folding a settled block, and putting
/// a panel beside the transcript, both require the whole stream to stay
/// addressable — native scrollback is not. See `docs/adr/0006`.
pub struct Terminal {
    raw: bool,
}

impl Terminal {
    pub fn enter() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let mut out = std::io::stdout();
        out.write_all(ansi::ENTER.as_bytes())?;
        out.flush()?;
        Ok(Self { raw: true })
    }
}

impl Surface for Terminal {
    fn describe(&self) -> String {
        "the terminal, full screen".into()
    }
    fn size(&self) -> (u16, u16) {
        crossterm::terminal::size().unwrap_or((80, 24))
    }
    fn present(&self, frame: &Frame) {
        let mut out = std::io::stdout();
        let _ = out.write_all(ansi::encode(frame).as_bytes());
        let _ = out.flush();
    }
    fn restore(&self) {
        let mut out = std::io::stdout();
        let _ = out.write_all(ansi::LEAVE.as_bytes());
        let _ = out.flush();
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

impl Drop for Terminal {
    /// Restores on every path out, including a panic. Without this a crash
    /// leaves the user in a raw-mode alternate screen with no echo.
    fn drop(&mut self) {
        if self.raw {
            self.restore();
        }
    }
}

/// Translate a crossterm event. `None` for events this UI has no use for.
pub fn from_crossterm(event: crossterm::event::Event) -> Option<Input> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
    match event {
        Event::Resize(w, h) => Some(Input::Resize(w, h)),
        Event::Paste(text) => Some(Input::Paste(text)),
        Event::Key(k) if k.kind == KeyEventKind::Press => {
            let key = match k.code {
                KeyCode::Char(c) => Key::Char(c),
                KeyCode::Enter => Key::Enter,
                KeyCode::Backspace => Key::Backspace,
                KeyCode::Delete => Key::Delete,
                KeyCode::Tab => Key::Tab,
                KeyCode::BackTab => Key::BackTab,
                KeyCode::Esc => Key::Esc,
                KeyCode::Up => Key::Up,
                KeyCode::Down => Key::Down,
                KeyCode::Left => Key::Left,
                KeyCode::Right => Key::Right,
                KeyCode::Home => Key::Home,
                KeyCode::End => Key::End,
                KeyCode::PageUp => Key::PageUp,
                KeyCode::PageDown => Key::PageDown,
                _ => return None,
            };
            Some(Input::Key(KeyPress::new(
                key,
                Mods {
                    ctrl: k.modifiers.contains(KeyModifiers::CONTROL),
                    alt: k.modifiers.contains(KeyModifiers::ALT),
                    shift: k.modifiers.contains(KeyModifiers::SHIFT),
                },
            )))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{Line, Rect};

    #[test]
    fn a_headless_surface_keeps_every_frame_not_just_the_last() {
        let s = Headless::new(10, 2);
        for n in 0..3 {
            let mut f = Frame::new(10, 2);
            f.place(
                "m",
                Rect::new(0, 0, 10, 1),
                vec![Line::raw(format!("f{n}"))],
            );
            s.present(&f);
        }
        assert_eq!(s.frame_count(), 3, "the sequence is what a test asserts on");
        assert_eq!(s.frames()[0].rows()[0].trim(), "f0");
        assert_eq!(s.text().lines().next().unwrap().trim(), "f2");
    }

    #[test]
    fn the_bytes_are_available_for_an_external_oracle() {
        let s = Headless::new(6, 1);
        let mut f = Frame::new(6, 1);
        f.place("m", Rect::new(0, 0, 6, 1), vec![Line::raw("hi")]);
        s.present(&f);
        assert!(s.bytes().contains("\x1b[1;1Hhi"));
    }

    #[test]
    fn a_key_press_is_constructible_without_a_keyboard() {
        assert_eq!(KeyPress::ctrl('c').mods, Mods::CTRL);
        assert_eq!(KeyPress::ch('a').key, Key::Char('a'));
    }

    #[test]
    fn crossterm_key_releases_are_dropped_not_doubled() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
        let press = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        assert!(from_crossterm(Event::Key(press)).is_some());
        let mut release = press;
        release.kind = KeyEventKind::Release;
        assert!(
            from_crossterm(Event::Key(release)).is_none(),
            "a release must not read as a second press"
        );
    }
}
