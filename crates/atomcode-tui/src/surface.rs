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
use crate::theme::{Palette, Rgb, Theme};

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

/// What the pointer did, in the only three shapes this UI has a use for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Click {
    /// The primary button went down at this cell.
    Press,
    /// The pointer moved with the button held. What a drag is made of.
    Drag,
    /// …and came up here. A release at the cell it was pressed on is a click;
    /// anywhere else it is the end of a selection.
    Release,
    WheelUp,
    WheelDown,
}

/// Everything that can arrive from the outside world.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Input {
    Key(KeyPress),
    /// A bracketed paste, delivered whole rather than as N keystrokes.
    Paste(String),
    /// A pointer event at a cell, in the frame's own coordinates.
    Mouse(Click, u16, u16),
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

    /// What the terminal on the other end can render.
    ///
    /// Detected once, here, because this is the only layer allowed to touch the
    /// environment or the tty — everything above is *given* the answer rather
    /// than asking for it (`caps.rs`, `docs/adr/0008`). A `render` that read
    /// `TERM` would be green on the developer's machine and wrong on the
    /// user's, with no test to say so.
    fn caps(&self) -> crate::caps::Caps {
        crate::caps::Caps::default()
    }

    /// Take the pointer, or hand it back to the terminal.
    ///
    /// Runtime rather than launch-time on purpose. Reporting the pointer buys a
    /// click that folds a tool call and costs the terminal's own click-drag
    /// selection, and which of those a person wants changes minute to minute —
    /// they are folding now and copying an error message next. A choice that
    /// can only be made by restarting is a choice made once, wrongly.
    fn set_mouse(&self, _on: bool) {}

    /// Whether the pointer is currently ours.
    fn mouse(&self) -> bool {
        false
    }

    /// Forget what is believed to be on screen, so the next frame is painted
    /// in full.
    ///
    /// The renderer only sends rows that changed, which is what stops an idle
    /// screen from churning — and which means anything *else* that writes to
    /// this terminal leaves marks that are never painted over, because from
    /// here nothing changed. Before the diff, a full erase every 110ms hid that
    /// class of damage by brute force. This is the way back from it.
    fn forget(&self) {}

    /// Put text on the system clipboard.
    ///
    /// The surface's job because it is the only layer that may talk to the
    /// terminal, and the terminal is what has a clipboard — see
    /// [`crate::ansi::set_clipboard`] for why not `pbcopy`.
    fn copy(&self, _text: &str) {}

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
    mouse: std::sync::atomic::AtomicBool,
    caps: crate::caps::Caps,
    painted: LastPainted,
    /// Where stderr was sent while we hold the screen, and the descriptor it
    /// came from. See [`Terminal::take_stderr`].
    stderr: Option<StderrHeld>,
}

/// The real stderr, set aside, and the file it was pointed at instead.
#[cfg(unix)]
struct StderrHeld {
    original: std::os::fd::RawFd,
    path: std::path::PathBuf,
}

#[cfg(not(unix))]
struct StderrHeld;

impl Terminal {
    /// The bytes this frame would send, given what is already on the screen —
    /// and the record of it, so the next frame can be a diff too.
    fn patch(&self, frame: &Frame) -> String {
        let next = ansi::encode_rows(frame, self.caps);
        let mut last = self.painted.0.lock().expect("last frame poisoned");
        let out = next.patch_from(last.as_ref());
        *last = Some(next);
        out
    }
}

impl Terminal {
    /// Take the screen. `theme` forces a palette; `None` means ask the terminal
    /// what colour it is and follow the answer.
    pub fn enter(theme: Option<Theme>, mouse: bool) -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let mut out = std::io::stdout();
        out.write_all(ansi::ENTER.as_bytes())?;
        if mouse {
            out.write_all(ansi::MOUSE_ON.as_bytes())?;
        }
        out.flush()?;
        let mut caps = crate::caps::Caps::detect();
        // Inside the alternate screen on purpose: a terminal that does not know
        // the queries may echo them, and here the first frame paints over it.
        //
        // A forced theme skips the exchange entirely. It is the escape hatch
        // for a terminal that will not answer (some tmux and ssh setups) — and
        // it costs contrast rather than correctness, because the resolver still
        // measures against whatever background the assumption implies.
        caps.palette = match theme {
            Some(theme) => Palette::assumed(theme),
            None => measure_palette(),
        };
        Ok(Self {
            raw: true,
            mouse: std::sync::atomic::AtomicBool::new(mouse),
            caps,
            painted: LastPainted::default(),
            stderr: take_stderr(),
        })
    }
}

/// Point stderr at a file for as long as this UI owns the screen.
///
/// **The terminal is a physical singleton and this row holds it.** Anything
/// else that writes here — a library's `eprintln!`, a dependency's warning, a
/// panic message from a background task — lands as characters at whatever cell
/// the cursor happens to be on. That was survivable when every frame began with
/// a full-screen erase: the damage lasted 110ms. It is not survivable against a
/// renderer that only repaints rows it believes changed, because from here
/// nothing changed, and the marks stay until something else happens to touch
/// that row. A stray character in the middle of a session is exactly that.
///
/// Nothing is silenced: the output goes to a file, and [`Terminal::restore`]
/// says where when there is anything in it. Losing a diagnostic would be a
/// worse trade than the corruption it prevents.
#[cfg(unix)]
fn take_stderr() -> Option<StderrHeld> {
    use std::os::fd::IntoRawFd;

    let path = std::env::temp_dir().join(format!("atomcode-tui-{}.stderr.log", std::process::id()));
    let file = std::fs::File::create(&path).ok()?;
    // SAFETY: both are open descriptors for the length of these calls; `dup`
    // and `dup2` are the documented way to swap one, and failure is reported
    // rather than assumed away.
    let original = unsafe { libc::dup(libc::STDERR_FILENO) };
    if original < 0 {
        return None;
    }
    let fd = file.into_raw_fd();
    if unsafe { libc::dup2(fd, libc::STDERR_FILENO) } < 0 {
        unsafe { libc::close(fd) };
        unsafe { libc::close(original) };
        return None;
    }
    unsafe { libc::close(fd) };
    Some(StderrHeld { original, path })
}

#[cfg(not(unix))]
fn take_stderr() -> Option<StderrHeld> {
    None
}

/// Put stderr back, and say where anything that was written to it went.
#[cfg(unix)]
fn give_back_stderr(held: &StderrHeld) {
    use std::io::Write;
    // SAFETY: `original` is the descriptor `take_stderr` duplicated and has not
    // been closed; this is the matching half of that swap.
    unsafe {
        libc::dup2(held.original, libc::STDERR_FILENO);
        libc::close(held.original);
    }
    match std::fs::metadata(&held.path).map(|m| m.len()) {
        Ok(0) | Err(_) => {
            let _ = std::fs::remove_file(&held.path);
        }
        Ok(n) => {
            let mut err = std::io::stderr();
            let _ = writeln!(
                err,
                "{n} bytes went to stderr; kept at {}",
                held.path.display()
            );
        }
    }
}

#[cfg(not(unix))]
fn give_back_stderr(_held: &StderrHeld) {}

/// Hand the text to whatever this machine uses for a clipboard.
///
/// Skipped over ssh: the helper would put it on the *server's* clipboard, which
/// is not the one anybody is looking at. There OSC 52 is the only thing that
/// can work, and it is already on its way.
fn local_clipboard(text: &str) {
    use std::process::{Command, Stdio};

    if std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some() {
        return;
    }
    let helpers: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else if cfg!(windows) {
        &[("clip", &[])]
    } else {
        &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ]
    };
    for (bin, args) in helpers {
        let Ok(mut child) = Command::new(bin)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue; // not installed; try the next one
        };
        if let Some(mut stdin) = child.stdin.take() {
            // Best effort, and deliberately not `?`: a helper that exits before
            // reading gives EPIPE, and losing the copy — or hanging the UI —
            // over a broken pipe would be worse than a copy that half worked.
            let _ = stdin.write_all(text.as_bytes());
        }
        // `stdin` is dropped here, so the child sees EOF and can exit.
        let _ = child.wait();
        return;
    }
}

// ---- what colour is the terminal? ---------------------------------------

/// What the terminal answered, and what every role resolves to on it.
///
/// The half of the fix that is not code: a palette chosen by measurement is
/// only trustworthy if the measurement can be seen. When something still looks
/// wrong, this says whether the terminal answered at all, what it said, and
/// which roles are running below their contrast floor — instead of leaving
/// "still can't read it" as the only available bug report.
pub fn probe_report() -> String {
    use std::fmt::Write as _;

    let raw = crossterm::terminal::enable_raw_mode().is_ok();
    let palette = measure_palette();
    if raw {
        let _ = crossterm::terminal::disable_raw_mode();
    }
    let mut caps = crate::caps::Caps::detect();
    caps.palette = palette;

    let hex = |(r, g, b): Rgb| format!("#{r:02x}{g:02x}{b:02x}");
    let mut out = String::new();
    let env = |k: &str| std::env::var(k).unwrap_or_else(|_| "(unset)".into());
    let _ = writeln!(out, "terminal");
    let _ = writeln!(
        out,
        "  TERM={}  TERM_PROGRAM={}  COLORTERM={}  COLORFGBG={}",
        env("TERM"),
        env("TERM_PROGRAM"),
        env("COLORTERM"),
        env("COLORFGBG")
    );
    let _ = writeln!(
        out,
        "  colours: {:?}   unicode: {}",
        caps.colors, caps.unicode
    );
    let bg = palette.background();
    let _ = writeln!(
        out,
        "  background: {} ({})   leaning: {:?}",
        hex(bg),
        if palette.background_measured() {
            "answered by the terminal"
        } else {
            "assumed — the terminal did not answer"
        },
        palette.theme()
    );
    let _ = writeln!(
        out,
        "  slots answered: {}/16{}",
        palette.measured(),
        if palette.measured() == 0 {
            "  (falling back to xterm's values)"
        } else {
            ""
        }
    );
    for row in 0..2 {
        let _ = write!(out, "   ");
        for n in row * 8..row * 8 + 8 {
            let _ = write!(out, " {n:>2} {}", hex(palette.slot(n as u8)));
        }
        let _ = writeln!(out);
    }

    let _ = writeln!(out, "\nroles");
    // The palette explains itself: taking a resolved colour apart would mean
    // naming `Color::Ansi` here, which is the one thing the layering gate
    // forbids outside it — and a diagnostic is no reason to bypass it.
    for line in crate::theme::explain(caps) {
        let _ = writeln!(out, "{line}");
    }
    out
}

/// Ask the terminal what colours it actually renders.
///
/// Two questions in one exchange: OSC 11 for the background, OSC 4 for each of
/// the sixteen slots. What comes back is what the resolver measures against, so
/// a terminal that answers fully gets a palette chosen for *its* colours rather
/// than for a guess about which of two families it belongs to.
///
/// Anything unanswered falls back in order: `COLORFGBG` for the background —
/// a hint, not an answer, since it is set once and survives a theme change —
/// then a dark assumption. A missing slot falls back to xterm's value for it.
/// None of that is a failure mode: the resolver checks whatever it is given, so
/// a wrong assumption costs contrast, not correctness.
fn measure_palette() -> Palette {
    let (bg, slots) = query_terminal();
    let theme = bg
        .map(|rgb| {
            if crate::theme::luminance(rgb) > 0.18 {
                Theme::Light
            } else {
                Theme::Dark
            }
        })
        .or_else(colorfgbg_theme)
        .unwrap_or(Theme::Dark);
    let mut p = Palette::assumed(theme);
    if let Some(rgb) = bg {
        p = p.with_background(rgb);
    }
    for (n, rgb) in slots {
        p = p.with_slot(n, rgb);
    }
    p
}

/// `COLORFGBG` is `fg;bg` or `fg;<something>;bg`. The last field is the
/// background, as an ANSI slot.
fn colorfgbg_theme() -> Option<Theme> {
    theme_from_colorfgbg(&std::env::var("COLORFGBG").ok()?)
}

fn theme_from_colorfgbg(raw: &str) -> Option<Theme> {
    let bg: u8 = raw.rsplit(';').next()?.trim().parse().ok()?;
    // 7 (white) and 15 (bright white) are the light ones; 8 is bright black.
    Some(if bg == 7 || (9..=15).contains(&bg) {
        Theme::Light
    } else {
        Theme::Dark
    })
}

/// One exchange: the background and all sixteen slots, then a fence.
///
/// Unix only — it needs the tty as a file descriptor, with a timeout, which is
/// not something crossterm exposes. Elsewhere the palette is assumed and
/// `COLORFGBG` and config still apply.
#[cfg(unix)]
fn query_terminal() -> (Option<Rgb>, Vec<(u8, Rgb)>) {
    use std::os::fd::AsRawFd;

    let mut query: Vec<u8> = Vec::with_capacity(256);
    query.extend_from_slice(b"\x1b]11;?\x1b\\");
    for n in 0..16u8 {
        query.extend_from_slice(format!("\x1b]4;{n};?\x1b\\").as_bytes());
    }
    // DA1 last, as a fence. Every terminal answers it, and answers in order, so
    // a terminal that ignores the colour queries ends the wait immediately
    // instead of costing the whole timeout — and one that honours them has
    // already replied by the time this answer arrives.
    query.extend_from_slice(b"\x1b[c");

    let mut out = std::io::stdout();
    if out.write_all(&query).is_err() || out.flush().is_err() {
        return (None, Vec::new());
    }

    let stdin = std::io::stdin();
    let fd = stdin.as_raw_fd();
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(200);
    let mut seen: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 512];
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() || !readable(fd, left) {
            break;
        }
        // SAFETY: `fd` is stdin, borrowed for the length of this call, and the
        // buffer is a live local of exactly `chunk.len()` bytes.
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n <= 0 {
            break;
        }
        seen.extend_from_slice(&chunk[..n as usize]);
        if answered_da1(&seen) {
            break;
        }
    }
    (parse_osc11(&seen), parse_osc4(&seen))
}

#[cfg(not(unix))]
fn query_terminal() -> (Option<Rgb>, Vec<(u8, Rgb)>) {
    // No tty descriptor to read a reply from. `COLORFGBG` and config remain.
    (None, Vec::new())
}

/// Wait until `fd` has something to read, or the timeout passes.
#[cfg(unix)]
fn readable(fd: std::os::fd::RawFd, within: std::time::Duration) -> bool {
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = within.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: one initialised `pollfd`, and the count says so.
    unsafe { libc::poll(&mut poll, 1, ms) > 0 }
}

/// Has a Device Attributes reply (`ESC [ ... c`) arrived?
///
/// Checked structurally rather than by looking for a `c`, because `c` is also a
/// hex digit and the colour reply is full of them.
fn answered_da1(seen: &[u8]) -> bool {
    let mut from = 0;
    while let Some(at) = seen[from..].iter().position(|&b| b == 0x1b) {
        let esc = from + at;
        if seen.get(esc + 1) == Some(&b'[') {
            let mut i = esc + 2;
            while let Some(&b) = seen.get(i) {
                if b.is_ascii_digit() || b == b';' || b == b'?' {
                    i += 1;
                    continue;
                }
                return b == b'c';
            }
            return false;
        }
        from = esc + 1;
    }
    false
}

/// The payloads of every OSC reply in the stream — what sits between `ESC ]`
/// and its terminator (BEL, or `ESC \\`).
///
/// Scanned rather than pattern-matched on the whole buffer because seventeen
/// answers arrive interleaved with a device-attributes reply, in an order the
/// terminal chooses.
fn osc_payloads(seen: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 1 < seen.len() {
        if seen[i] != 0x1b || seen[i + 1] != b']' {
            i += 1;
            continue;
        }
        let body = i + 2;
        let mut j = body;
        while j < seen.len() {
            if seen[j] == 0x07 || (seen[j] == 0x1b && seen.get(j + 1) == Some(&b'\\')) {
                break;
            }
            j += 1;
        }
        if j < seen.len() {
            out.push(String::from_utf8_lossy(&seen[body..j]).into_owned());
        }
        i = j + 1;
    }
    out
}

/// `rgb:RRRR/GGGG/BBBB` — or `#RRGGBB`, the older form.
///
/// Components are one to four hex digits: xterm answers in sixteen bits per
/// channel, others in eight. Both scale to the top byte.
fn parse_colour(spec: &str) -> Option<Rgb> {
    let spec = spec.trim();
    let hex = match spec.strip_prefix("rgb:") {
        Some(rest) => rest,
        None => spec.strip_prefix('#').filter(|h| h.len() == 6)?,
    };
    let parts: Vec<&str> = if hex.contains('/') {
        hex.split('/').collect()
    } else {
        vec![hex.get(0..2)?, hex.get(2..4)?, hex.get(4..6)?]
    };
    if parts.len() < 3 {
        return None;
    }
    let scale = |p: &str| -> Option<u8> {
        let p = p.trim();
        if p.is_empty() || p.len() > 4 || !p.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let v = u32::from_str_radix(p, 16).ok()?;
        // One hex digit is four bits, four are sixteen; normalise to the top
        // eight so every width lands on the same scale.
        Some((v << (4 * (4 - p.len())) >> 8) as u8)
    };
    Some((scale(parts[0])?, scale(parts[1])?, scale(parts[2])?))
}

/// The background, from an OSC 11 reply.
fn parse_osc11(seen: &[u8]) -> Option<Rgb> {
    osc_payloads(seen)
        .iter()
        .find_map(|p| parse_colour(p.strip_prefix("11;")?))
}

/// The slots, from the OSC 4 replies. Slots the terminal did not answer for are
/// simply absent — the palette falls back to xterm's value for each.
fn parse_osc4(seen: &[u8]) -> Vec<(u8, Rgb)> {
    osc_payloads(seen)
        .iter()
        .filter_map(|p| {
            let rest = p.strip_prefix("4;")?;
            let (n, spec) = rest.split_once(';')?;
            Some((n.trim().parse::<u8>().ok()?, parse_colour(spec)?))
        })
        .filter(|(n, _)| *n < 16)
        .collect()
}

/// The last frame that reached the terminal, so an unchanged screen is not
/// repainted.
///
/// The animation rows ask to be woken about nine times a second so a spinner
/// has frames to show. Nothing else on the screen moves at that rate, and an
/// idle screen does not move at all — but every one of those wake-ups used to
/// arrive at the terminal as a full erase and a full redraw, whether or not a
/// single cell had changed. That is a screen that never settles.
#[derive(Debug, Default)]
pub struct LastPainted(Mutex<Option<ansi::Rows>>);

impl LastPainted {
    /// Forget what was painted, so the next frame is drawn in full. For after
    /// anything that may have written over the screen behind our back.
    pub fn forget(&self) {
        *self.0.lock().expect("last frame poisoned") = None;
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
        // Only the rows that moved, and silence for a screen that did not.
        let patch = self.patch(frame);
        if patch.is_empty() {
            return;
        }
        let mut out = std::io::stdout();
        let _ = out.write_all(patch.as_bytes());
        let _ = out.flush();
    }
    fn caps(&self) -> crate::caps::Caps {
        self.caps
    }
    fn set_mouse(&self, on: bool) {
        use std::sync::atomic::Ordering;
        if self.mouse.swap(on, Ordering::SeqCst) == on {
            return;
        }
        let mut out = std::io::stdout();
        let _ = out.write_all(if on { ansi::MOUSE_ON } else { ansi::MOUSE_OFF }.as_bytes());
        let _ = out.flush();
    }
    fn mouse(&self) -> bool {
        self.mouse.load(std::sync::atomic::Ordering::SeqCst)
    }
    fn forget(&self) {
        self.painted.forget();
    }
    fn copy(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        // Both paths, because either can be unavailable and they fail
        // differently. OSC 52 crosses ssh and tmux, but the terminal may refuse
        // it — iTerm2 ships with clipboard access *off* and the refusal is
        // silent, which is a copy that looks like it worked. A local helper
        // always works locally and is the wrong machine over ssh. Doing both
        // means the copy lands whichever of those is true; writing the same
        // text to the same clipboard twice costs nothing.
        let mut out = std::io::stdout();
        let _ = out.write_all(ansi::set_clipboard(text).as_bytes());
        let _ = out.flush();
        local_clipboard(text);
    }
    fn restore(&self) {
        self.painted.forget();
        let mut out = std::io::stdout();
        if self.mouse() {
            let _ = out.write_all(ansi::MOUSE_OFF.as_bytes());
        }
        let _ = out.write_all(ansi::LEAVE.as_bytes());
        let _ = out.flush();
        let _ = crossterm::terminal::disable_raw_mode();
        // Last, so anything it has to report is printed to a terminal that is
        // back in its normal mode.
        if let Some(held) = &self.stderr {
            give_back_stderr(held);
        }
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
    use crossterm::event::{MouseButton, MouseEventKind};
    match event {
        Event::Resize(w, h) => Some(Input::Resize(w, h)),
        Event::Paste(text) => Some(Input::Paste(text)),
        // Press, not release: a fold should happen under the finger. Drags and
        // moves are dropped — this UI has nothing that follows a pointer, and
        // reporting them would be a stream of events nothing reads.
        Event::Mouse(m) => {
            let click = match m.kind {
                MouseEventKind::Down(MouseButton::Left) => Click::Press,
                MouseEventKind::Drag(MouseButton::Left) => Click::Drag,
                MouseEventKind::Up(MouseButton::Left) => Click::Release,
                MouseEventKind::ScrollUp => Click::WheelUp,
                MouseEventKind::ScrollDown => Click::WheelDown,
                _ => return None,
            };
            Some(Input::Mouse(click, m.column, m.row))
        }
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

    /// A screen of `n` rows where only the last one animates.
    fn screen(rows: u16, last: &str) -> Frame {
        let mut f = Frame::new(20, rows);
        for y in 0..rows - 1 {
            f.place(
                format!("m{y}"),
                Rect::new(0, y, 20, 1),
                vec![Line::raw(format!("row {y}"))],
            );
        }
        f.place(
            "status",
            Rect::new(0, rows - 1, 20, 1),
            vec![Line::raw(last)],
        );
        f
    }

    #[test]
    fn an_unchanged_screen_is_not_repainted_at_all() {
        // The defect this closes: the spinner asks to be woken nine times a
        // second, and every wake-up used to reach the terminal as a full erase
        // and a full redraw — on an idle screen, with nothing to redraw.
        let a = ansi::encode_rows(&screen(6, "idle"), crate::caps::Caps::default());
        let b = ansi::encode_rows(&screen(6, "idle"), crate::caps::Caps::default());
        assert!(
            b.patch_from(Some(&a)).is_empty(),
            "an identical frame has nothing to say"
        );
        assert!(
            !b.patch_from(None).is_empty(),
            "but a first paint still draws"
        );
    }

    #[test]
    fn only_the_row_that_moved_is_repainted() {
        // A spinner is one row. Repainting six because one of them ticked is
        // what a terminal sees as the screen never settling.
        let caps = crate::caps::Caps::default();
        let a = ansi::encode_rows(&screen(6, "· thinking"), caps);
        let b = ansi::encode_rows(&screen(6, "⋯ thinking"), caps);
        let patch = b.patch_from(Some(&a));
        assert!(patch.contains("\x1b[6;1H"), "the status row: {patch:?}");
        for row in 1..=5 {
            assert!(
                !patch.contains(&format!("\x1b[{row};1H")),
                "row {row} did not change: {patch:?}"
            );
        }
        assert!(patch.len() < b.full().len() / 2, "a patch, not a repaint");
    }

    #[test]
    fn a_resize_repaints_everything_rather_than_diffing_a_reflow() {
        let caps = crate::caps::Caps::default();
        let small = ansi::encode_rows(&screen(4, "idle"), caps);
        let big = ansi::encode_rows(&screen(8, "idle"), caps);
        assert_eq!(
            big.patch_from(Some(&small)),
            big.full(),
            "a different size is not a diff"
        );
    }

    #[test]
    fn the_terminals_own_background_decides_the_palette() {
        // The exchange this parses is the whole of theme detection: get it
        // wrong and a light terminal is painted in a dark palette, which is
        // unreadable rather than merely ugly.
        let dark = b"\x1b]11;rgb:1c1c/1c1c/1c1c\x1b\\";
        assert_eq!(parse_osc11(dark), Some((0x1c, 0x1c, 0x1c)));

        // Eight bits per channel, BEL-terminated — the other common shape.
        let light = b"\x1b]11;rgb:ff/ff/f8\x07";
        assert_eq!(parse_osc11(light), Some((0xff, 0xff, 0xf8)));

        // And the older `#RRGGBB`.
        assert_eq!(parse_osc11(b"\x1b]11;#ffffff\x07"), Some((255, 255, 255)));

        // A terminal that answered only the fence has said nothing about colour.
        assert_eq!(parse_osc11(b"\x1b[?62;1;2;6;9;15;22c"), None);
        assert_eq!(parse_osc11(b""), None);
        assert_eq!(parse_osc11(b"\x1b]11;not-a-colour\x07"), None);
    }

    #[test]
    fn the_fence_is_recognised_by_shape_not_by_the_letter_c() {
        // `c` is also a hex digit, and the colour reply is full of them. A
        // substring check would end the wait on `rgb:1c1c/...` and throw the
        // answer away — the bug this shape check exists to prevent.
        assert!(answered_da1(b"\x1b[?62;1;2c"));
        assert!(answered_da1(b"\x1b]11;rgb:1c1c/1c1c/1c1c\x07\x1b[?6c"));
        assert!(
            !answered_da1(b"\x1b]11;rgb:1c1c/1c1c/1c1c\x07"),
            "the colour reply alone is not the fence"
        );
        assert!(!answered_da1(b"\x1b[?62;1"), "still arriving");
        assert!(!answered_da1(b""));
    }

    #[test]
    fn colorfgbg_is_read_as_a_hint_when_the_terminal_will_not_answer() {
        assert_eq!(theme_from_colorfgbg("0;15"), Some(Theme::Light));
        assert_eq!(theme_from_colorfgbg("15;0"), Some(Theme::Dark));
        assert_eq!(theme_from_colorfgbg("0;default;15"), Some(Theme::Light));
        assert_eq!(theme_from_colorfgbg("7"), Some(Theme::Light));
        assert_eq!(theme_from_colorfgbg("8"), Some(Theme::Dark), "bright black");
        assert_eq!(theme_from_colorfgbg(""), None);
        assert_eq!(theme_from_colorfgbg("default;default"), None);
    }

    #[test]
    fn a_white_background_reads_as_light_and_a_dark_one_as_dark() {
        let theme = |(r, g, b): (u8, u8, u8)| {
            let l = 0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32;
            if l > 127.5 {
                Theme::Light
            } else {
                Theme::Dark
            }
        };
        assert_eq!(theme((255, 255, 255)), Theme::Light);
        assert_eq!(theme((250, 250, 245)), Theme::Light, "off-white");
        assert_eq!(theme((0, 0, 0)), Theme::Dark);
        assert_eq!(theme((0x1c, 0x1c, 0x1c)), Theme::Dark, "a dark grey");
        // Solarized light and dark, the two that a naive average gets wrong.
        assert_eq!(theme((0xfd, 0xf6, 0xe3)), Theme::Light);
        assert_eq!(theme((0x00, 0x2b, 0x36)), Theme::Dark);
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
