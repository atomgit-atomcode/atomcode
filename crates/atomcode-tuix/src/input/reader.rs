// crates/atomcode-tuix/src/input/reader.rs
use std::sync::mpsc::{self as stdmpsc, TryRecvError};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::event::{DisableFocusChange, EnableFocusChange};
use crossterm::execute;
use tokio::sync::mpsc;

use super::InputEvent;

/// If a Key event could plausibly be part of a paste burst, return the
/// character it contributes. Enter maps to `\n`, Tab to `\t`, Char(c) to
/// itself. Modifier-carrying keys (Ctrl/Alt) and non-Press kinds are
/// excluded — those are commands, not pasted content.
fn paste_candidate_char(ev: &Event) -> Option<char> {
    let Event::Key(KeyEvent {
        kind,
        code,
        modifiers,
        ..
    }) = ev
    else {
        return None;
    };
    if *kind != KeyEventKind::Press {
        return None;
    }
    // Shift is fine (Shift+letter on paste of uppercase). Anything else
    // means the user is issuing a command.
    let allowed = KeyModifiers::SHIFT | KeyModifiers::NONE;
    if !(modifiers.difference(allowed).is_empty()) {
        return None;
    }
    match code {
        KeyCode::Char(c) => Some(*c),
        // Shift+Enter is "insert newline", a user command — never a
        // paste-burst char. Real pasted newlines arrive as Event::Paste
        // (bracketed paste) or as plain Enter with NO modifier (conhost
        // char-by-char). If we let Shift+Enter in here, the single-event
        // else-branch at the bottom reconstructs KeyEvent with NONE
        // modifiers and classify then collapses it to Submit.
        KeyCode::Enter if modifiers.contains(KeyModifiers::SHIFT) => None,
        KeyCode::Enter => Some('\n'),
        KeyCode::Tab => Some('\t'),
        _ => None,
    }
}

/// True when an aggregated `paste_candidate_char` burst should be treated
/// as a real `InputEvent::Paste` rather than emitted as individual key
/// events. Conjuncted conditions:
///
/// 1. **At least 2 chars** — singletons are normal typing.
/// 2. **Contains `\n`** — the unambiguous "this is multi-line content"
///    signal. Bursts of plain printable chars (someone typing fast) get
///    handled per-key just fine without aggregation.
/// 3. **At least one non-whitespace char** — distinguishes a real paste
///    from buffered Enter/Tab keystrokes left in the tty input queue at
///    startup. Without this guard, two Enters mashed by the user before
///    atomcode took over the terminal (e.g. while waiting for a slow
///    `cargo build` to finish) get aggregated into `Paste("\n\n")` and
///    inserted as text — the input box opens with two pre-typed blank
///    lines. Genuine pastes containing only whitespace + newlines are
///    vanishingly rare; falling back to per-key submission of those bursts
///    is the right trade-off.
/// 4. **Avg ≥ 2 non-newline chars per line** when the burst is 3+ lines.
///    Defends against the JediTerm IME commit storm reported on Windows:
///    every Pinyin candidate selection emitted `<char> + Enter` in rapid
///    succession (within the 2ms aggregation window), producing a burst
///    like `[首, \n, 页, \n, 中, \n, …]`. Old heuristic accepted that as
///    a paste, leaving the buffer with `\n` between every CJK char and
///    the input row showing `首↵页↵中↵…`. Genuine multi-line pastes
///    always have lines with text; IME bursts have exactly 1 text char
///    per line. Threshold scoped to 3+ lines so a legitimate 2-line
///    paste with two single-char lines (rare but possible) still flows
///    through the paste path.
fn is_paste_burst(chars: &[char]) -> bool {
    if chars.len() < 2 {
        return false;
    }
    let mut has_enter = false;
    let mut has_text_char = false;
    let mut newline_count = 0usize;
    for &c in chars {
        if c == '\n' {
            has_enter = true;
            newline_count += 1;
        }
        if !c.is_whitespace() {
            has_text_char = true;
        }
    }
    if !has_enter || !has_text_char {
        return false;
    }
    let line_count = newline_count + 1;
    let non_newline_count = chars.len() - newline_count;
    if line_count >= 3 && non_newline_count <= line_count {
        // Mean ≤ 1 char per line. JediTerm IME pattern, not a paste.
        return false;
    }
    true
}

/// Lifecycle commands for the reader thread. Sent from the event loop
/// whenever an external process (OAuth browser flow, `/shell`, etc.)
/// needs stdin/stdout in cooked mode without our reader racing for bytes.
#[derive(Debug)]
pub enum ReaderCommand {
    /// Stop calling `event::poll` / `event::read`. The reader blocks on
    /// its command channel until Resume arrives. Sends a single `()` on
    /// `ack` once it's confirmed idle, so the caller can safely take
    /// over stdin without a race.
    Pause,
    /// Resume normal event dispatch. No ack — the next keystroke is
    /// the ack.
    Resume,
    /// Exit the thread. Idempotent; dropping the sender also triggers exit.
    Shutdown,
}

/// Control handle returned from `spawn`. Owns the join handle + the
/// command channel; dropping the handle shuts the reader down cleanly.
pub struct ReaderHandle {
    join: Option<std::thread::JoinHandle<()>>,
    cmd_tx: stdmpsc::Sender<(ReaderCommand, Option<stdmpsc::Sender<()>>)>,
    focus_tracking_enabled: bool,
}

impl ReaderHandle {
    /// Pause + wait for ack. After this returns, the reader is guaranteed
    /// to NOT be inside `event::poll` / `event::read`, so the caller can
    /// disable raw mode and hand stdin to a child process without the
    /// reader stealing bytes.
    ///
    /// Returns early (Ok) if the reader already exited — callers should
    /// treat that as "nothing to pause" rather than an error.
    pub fn pause_blocking(&self) -> std::io::Result<()> {
        let (ack_tx, ack_rx) = stdmpsc::channel();
        if self
            .cmd_tx
            .send((ReaderCommand::Pause, Some(ack_tx)))
            .is_err()
        {
            return Ok(()); // reader already gone
        }
        // Bounded wait — if the reader is stuck inside `event::poll` we
        // still ACK within the 100ms poll timeout.
        match ack_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(()) => Ok(()),
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "reader thread did not ack Pause within 2s",
            )),
        }
    }

    /// Resume from Pause. Fire-and-forget — the next keystroke the user
    /// presses becomes the implicit ack.
    pub fn resume(&self) {
        let _ = self.cmd_tx.send((ReaderCommand::Resume, None));
    }
}

impl Drop for ReaderHandle {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send((ReaderCommand::Shutdown, None));
        if self.focus_tracking_enabled {
            let _ = execute!(std::io::stdout(), DisableFocusChange);
            atomcode_core::notify::set_terminal_focus_state(None);
        }
        // Let the thread finish on its own — we don't join here because
        // the reader may be blocked inside `event::poll` for up to 100ms
        // and we'd rather not stall caller shutdown.
        if let Some(join) = self.join.take() {
            drop(join);
        }
    }
}

/// Spawn a blocking OS thread that reads crossterm events and forwards them
/// over `tx`. Returns a `ReaderHandle` for lifecycle control (Pause /
/// Resume / Shutdown). The thread exits when:
/// - the `ReaderHandle` is dropped (Shutdown sent),
/// - `tx` is closed (send returns Err),
/// - or a fatal crossterm read error fires.
pub fn spawn(tx: mpsc::UnboundedSender<InputEvent>) -> ReaderHandle {
    let focus_tracking_enabled = terminal_supports_focus_tracking();
    if focus_tracking_enabled {
        let _ = execute!(std::io::stdout(), EnableFocusChange);
        atomcode_core::notify::set_terminal_focus_state(Some(true));
    }
    let (cmd_tx, cmd_rx) = stdmpsc::channel::<(ReaderCommand, Option<stdmpsc::Sender<()>>)>();
    let join = std::thread::spawn(move || run(tx, cmd_rx));
    ReaderHandle {
        join: Some(join),
        cmd_tx,
        focus_tracking_enabled,
    }
}

fn terminal_supports_focus_tracking() -> bool {
    let term_program = std::env::var("TERM_PROGRAM").unwrap_or_default();
    let lc_terminal = std::env::var("LC_TERMINAL").unwrap_or_default();
    term_program == "iTerm.app"
        || term_program.eq_ignore_ascii_case("iTerm2")
        || lc_terminal.eq_ignore_ascii_case("iTerm2")
}

/// Decide what the reader loop should do next, given the `event::poll`
/// result and whether the input channel is still alive. Extracted from
/// `run` so the four-way classification can be unit-tested without
/// spinning up a real TTY.
#[derive(Debug, PartialEq, Eq)]
enum PollAction {
    /// `poll` said "event available" — proceed to `event::read`.
    Read,
    /// No event in this tick and channel still open — loop again.
    Continue,
    /// No event and the input channel was dropped — exit the thread.
    Exit,
    /// `poll` returned `Err` — treat as a transient glitch (Windows
    /// crossterm has been seen to fail `poll`/`read` during terminal
    /// resize). Sleep briefly and loop. Critically, this is NOT
    /// `Exit` — returning here would kill the reader thread and
    /// collapse the event loop (`input_rx` closes → `maybe = None`
    /// → break), which is the "atomcode exits when I resize on
    /// Windows" bug.
    Sleep,
}

fn classify_poll(res: std::io::Result<bool>, tx_closed: bool) -> PollAction {
    match res {
        Ok(true) => PollAction::Read,
        Ok(false) if tx_closed => PollAction::Exit,
        Ok(false) => PollAction::Continue,
        Err(_) => PollAction::Sleep,
    }
}

/// Minimum gap between two modifier+Enter Press events to count them as
/// distinct user actions. Anything closer is treated as OS key autorepeat
/// leaking through as Press events (happens on terminals that advertise
/// CSI u support but don't implement `REPORT_EVENT_TYPES`, so crossterm
/// can't tag autorepeat as `KeyEventKind::Repeat`).
///
/// 40 ms sits between OS autorepeat cadence (~30 ms on macOS / Linux) and
/// the fastest humans can actually chord Shift+Enter twice (~100+ ms).
/// Scoped to Enter-with-modifiers only — plain-key autorepeat (Backspace,
/// arrows) remains useful and is left untouched.
const MODIFIER_ENTER_DEDUP: Duration = Duration::from_millis(40);

fn run(
    tx: mpsc::UnboundedSender<InputEvent>,
    cmd_rx: stdmpsc::Receiver<(ReaderCommand, Option<stdmpsc::Sender<()>>)>,
) {
    let mut paused = false;
    // Last accepted (modifiers, timestamp) for a modifier+Enter Press.
    // Used to drop autorepeat duplicates that slip past the terminal
    // protocol's Repeat filtering.
    let mut last_mod_enter: Option<(KeyModifiers, std::time::Instant)> = None;
    loop {
        // If paused, block on the command channel — no poll, no read, so
        // the child process owns stdin cleanly. Only Resume / Shutdown
        // exit the paused state.
        if paused {
            match cmd_rx.recv() {
                Ok((ReaderCommand::Resume, _)) => {
                    paused = false;
                }
                Ok((ReaderCommand::Shutdown, _)) | Err(_) => return,
                Ok((ReaderCommand::Pause, ack)) => {
                    // Already paused — just re-ack so the caller unblocks.
                    if let Some(ack) = ack {
                        let _ = ack.send(());
                    }
                }
            }
            continue;
        }

        // Non-blocking drain of any pending command before each poll.
        // Multiple Pause requests can coalesce here.
        match cmd_rx.try_recv() {
            Ok((ReaderCommand::Pause, ack)) => {
                paused = true;
                if let Some(ack) = ack {
                    let _ = ack.send(());
                }
                continue;
            }
            Ok((ReaderCommand::Resume, _)) => {
                // Already running — ignore.
            }
            Ok((ReaderCommand::Shutdown, _)) => return,
            Err(TryRecvError::Disconnected) => return,
            Err(TryRecvError::Empty) => {}
        }

        match classify_poll(event::poll(Duration::from_millis(100)), tx.is_closed()) {
            PollAction::Read => {}
            PollAction::Continue => continue,
            PollAction::Exit => return,
            PollAction::Sleep => {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        }
        let ev = match event::read() {
            Ok(e) => e,
            Err(_) => {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        };

        // Autorepeat dedup for modifier+Enter. iTerm2's current CSI u
        // implementation (3.5+/3.6) disambiguates Shift+Enter modifiers
        // correctly but doesn't honour `REPORT_EVENT_TYPES`, so a held
        // Shift+Enter emits N Press events at OS autorepeat cadence and
        // the input box inserts N newlines for one physical keystroke.
        // Drop same-modifier repeats that arrive within the dedup window.
        if let Event::Key(k) = &ev {
            if k.kind == KeyEventKind::Press && k.code == KeyCode::Enter && !k.modifiers.is_empty()
            {
                let now = std::time::Instant::now();
                if let Some((last_mods, last_at)) = last_mod_enter {
                    if last_mods == k.modifiers
                        && now.duration_since(last_at) < MODIFIER_ENTER_DEDUP
                    {
                        crate::tuix_trace!("RD", "dedup mod+Enter {:?}", k.modifiers);
                        last_mod_enter = Some((k.modifiers, now));
                        continue;
                    }
                }
                last_mod_enter = Some((k.modifiers, now));
            }
        }

        // Paste-burst detection for terminals without bracketed paste
        // (Windows conhost, some PowerShell setups). When a user pastes
        // multi-line text there, crossterm emits each character as an
        // individual `Event::Key` — including embedded Enters, which
        // individually trigger submit and produced "many queued
        // submits". Real bracketed paste lands here as `Event::Paste`
        // and this block is a no-op.
        //
        // Heuristic: if this event is a printable char / Enter / Tab
        // AND more events are ALREADY queued (peek with 0-timeout
        // poll), we're almost certainly inside a paste burst — real
        // typing has human-scale gaps so the queue is empty on peek.
        // Aggregate consecutive paste-candidate events and emit one
        // synthetic `InputEvent::Paste`. Only triggers when the burst
        // contains an Enter (the unambiguous "this is multi-line
        // pasted text, not typing" signal); burst of chars without
        // Enter falls through to the normal per-key path — it looks
        // the same to the user either way and keeps the heuristic
        // conservative.
        if let Some(c0) = paste_candidate_char(&ev) {
            let mut chars = vec![c0];
            let mut trailing: Option<Event> = None;
            const BATCH_CAP: usize = 8192;
            while chars.len() < BATCH_CAP {
                // 2ms timeout is way under any human typing cadence
                // (fastest typists are ~60ms/char) but bridges the
                // transient gap Windows crossterm takes to translate
                // each console record into an Event — without it, a
                // paste arriving as 8 records in the console buffer
                // can emit events with 100µs-1ms inter-event gaps that
                // a strict `poll(0)` misses, and every line gets
                // treated as an independent keystroke sequence.
                match event::poll(Duration::from_millis(2)) {
                    Ok(true) => {}
                    _ => break,
                }
                let nxt = match event::read() {
                    Ok(e) => e,
                    Err(_) => break,
                };
                // Windows crossterm in raw mode emits Press + Release
                // (and Repeat on autorepeat). Release/Repeat interleaved
                // with the paste burst used to kill aggregation — the
                // very next event after 'A' Press is 'A' Release, which
                // `paste_candidate_char` rejects, so we'd break out with
                // chars=[A] and never see the rest of the burst. Skip
                // non-Press Key events silently so the burst detector
                // walks through to the next printable-char Press.
                if let Event::Key(k) = &nxt {
                    if k.kind != KeyEventKind::Press {
                        continue;
                    }
                }
                match paste_candidate_char(&nxt) {
                    Some(c) => {
                        chars.push(c);
                    }
                    None => {
                        trailing = Some(nxt);
                        break;
                    }
                }
            }
            if is_paste_burst(&chars) {
                let text: String = chars.into_iter().collect();
                crate::tuix_trace!("RD", "paste-burst synth len={}", text.len());
                if tx.send(InputEvent::Paste(text)).is_err() {
                    return;
                }
            } else {
                // Not a clear paste signature — emit originals per-key.
                // We only kept chars, so reconstruct KeyEvents. The
                // first event we read is `ev`; subsequent ones we
                // discarded in favour of `chars`. Rebuild from chars
                // using a minimal KeyEvent (no modifiers) — this path
                // fires in the rare case where events piled up but
                // there was no Enter, i.e. fast typing or single-line
                // paste. Both look the same on screen, so a synthetic
                // reconstruction is faithful to user intent.
                for c in chars {
                    let code = match c {
                        '\n' => KeyCode::Enter,
                        '\t' => KeyCode::Tab,
                        other => KeyCode::Char(other),
                    };
                    let k = KeyEvent::new(code, KeyModifiers::NONE);
                    if tx.send(InputEvent::Key(k)).is_err() {
                        return;
                    }
                }
            }
            // Dispatch whatever non-paste event broke the burst.
            if let Some(ev) = trailing {
                let msg = match ev {
                    Event::Key(k) => {
                        crate::tuix_trace!("RD", "key {:?} {:?}", k.kind, k.code);
                        InputEvent::Key(k)
                    }
                    Event::Paste(p) => InputEvent::Paste(p),
                    Event::Resize(w, h) => InputEvent::Resize(w, h),
                    Event::Mouse(m) => match mouse_input_event(m) {
                        Some(ev) => ev,
                        None => continue,
                    },
                    Event::FocusGained => {
                        atomcode_core::notify::set_terminal_focus_state(Some(true));
                        continue;
                    }
                    Event::FocusLost => {
                        atomcode_core::notify::set_terminal_focus_state(Some(false));
                        continue;
                    }
                };
                if tx.send(msg).is_err() {
                    return;
                }
            }
            continue;
        }

        let msg = match ev {
            Event::Key(k) => {
                crate::tuix_trace!("RD", "key {:?} {:?}", k.kind, k.code);
                InputEvent::Key(k)
            }
            Event::Paste(p) => {
                crate::tuix_trace!("RD", "paste len={}", p.len());
                InputEvent::Paste(p)
            }
            Event::Resize(w, h) => {
                crate::tuix_trace!("RD", "resize {}x{}", w, h);
                InputEvent::Resize(w, h)
            }
            Event::Mouse(m) => match mouse_input_event(m) {
                Some(ev) => ev,
                None => continue,
            },
            Event::FocusGained => {
                atomcode_core::notify::set_terminal_focus_state(Some(true));
                continue;
            }
            Event::FocusLost => {
                atomcode_core::notify::set_terminal_focus_state(Some(false));
                continue;
            }
        };
        if tx.send(msg).is_err() {
            return;
        }
    }
}

fn mouse_input_event(m: crossterm::event::MouseEvent) -> Option<InputEvent> {
    // Trace EVERY arrival, regardless of kind. The kind-specific arms
    // below only log scroll/down/drag/up; on Windows conhost a wheel
    // tick can arrive as `Moved` or another variant we silently drop,
    // and without this top-of-function trace there's no way to tell
    // "no mouse events arriving" from "events arriving but ignored".
    crate::tuix_trace!("RD", "mouse kind={:?} col={} row={}", m.kind, m.column, m.row);
    match m.kind {
        crossterm::event::MouseEventKind::ScrollUp => {
            crate::tuix_trace!("RD", "mouse scroll up");
            Some(InputEvent::MouseScroll(-3))
        }
        crossterm::event::MouseEventKind::ScrollDown => {
            crate::tuix_trace!("RD", "mouse scroll down");
            Some(InputEvent::MouseScroll(3))
        }
        crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
            Some(InputEvent::MouseDown {
                col: m.column,
                row: m.row,
            })
        }
        crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left) => {
            Some(InputEvent::MouseDrag {
                col: m.column,
                row: m.row,
            })
        }
        crossterm::event::MouseEventKind::Up(crossterm::event::MouseButton::Left) => {
            Some(InputEvent::MouseUp)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pause/Resume round trip without touching crossterm — feeds commands
    /// directly into the `run` worker via an in-memory channel pair. This
    /// exercises the paused-state ACK path that the OAuth flow depends on
    /// without needing a real TTY.
    #[test]
    fn pause_acks_then_resume_wakes() {
        let (tx, _rx) = mpsc::unbounded_channel::<InputEvent>();
        let (cmd_tx, cmd_rx) = stdmpsc::channel();
        let worker = std::thread::spawn(move || run(tx, cmd_rx));

        // Send Pause and wait for ack.
        let (ack_tx, ack_rx) = stdmpsc::channel();
        cmd_tx
            .send((ReaderCommand::Pause, Some(ack_tx)))
            .expect("send pause");
        ack_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("pause ACK arrives within 2s");

        // Resend Pause — already paused, the worker must still ACK so
        // callers don't deadlock on a re-entrant pause.
        let (ack_tx2, ack_rx2) = stdmpsc::channel();
        cmd_tx
            .send((ReaderCommand::Pause, Some(ack_tx2)))
            .expect("send second pause");
        ack_rx2
            .recv_timeout(Duration::from_secs(2))
            .expect("re-entrant pause also ACKs");

        // Resume — should unblock the worker's recv loop.
        cmd_tx
            .send((ReaderCommand::Resume, None))
            .expect("send resume");

        // Shutdown so the thread exits and the test doesn't leak.
        cmd_tx
            .send((ReaderCommand::Shutdown, None))
            .expect("send shutdown");
        worker.join().expect("worker thread joins cleanly");
    }

    /// `MODIFIER_ENTER_DEDUP` must sit above OS autorepeat cadence but
    /// well below any realistic human chord rate. macOS / Linux autorepeat
    /// ticks every ~30 ms; the next intentional Shift+Enter can't physically
    /// happen faster than ~100 ms. 40 ms lands cleanly between the two.
    #[test]
    fn modifier_enter_dedup_window_brackets_autorepeat_but_not_humans() {
        let win = MODIFIER_ENTER_DEDUP.as_millis() as u64;
        assert!(
            win > 30,
            "dedup window {}ms must exceed typical OS autorepeat (30ms) \
             so autorepeat duplicates are caught",
            win
        );
        assert!(
            win < 80,
            "dedup window {}ms must stay below fastest realistic human \
             chord repeat (~100ms) so intentional Shift+Enter×2 still works",
            win
        );
    }

    /// Shift+Enter must NOT qualify as a paste-burst char. If it did,
    /// the single-event else-branch of the burst path reconstructs the
    /// KeyEvent with `KeyModifiers::NONE`, stripping SHIFT, and
    /// `key_action::classify` collapses the result to `Submit` instead
    /// of `InsertNewline` — i.e. Shift+Enter silently sends the message.
    #[test]
    fn paste_candidate_rejects_shift_enter() {
        let ev = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
        assert_eq!(
            paste_candidate_char(&ev),
            None,
            "Shift+Enter is a command (InsertNewline), not paste content"
        );
    }

    /// Plain Enter must still flow through the paste-burst path so
    /// multi-line pastes on terminals without bracketed paste (Windows
    /// conhost) still aggregate into a single Paste event.
    #[test]
    fn paste_candidate_accepts_plain_enter() {
        let ev = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(paste_candidate_char(&ev), Some('\n'));
    }

    /// Regression: two Enters left in the tty input queue at startup
    /// (e.g. user mashed Enter while waiting for `cargo build` to
    /// finish before atomcode took over) used to aggregate into a
    /// synthetic `Paste("\n\n")` and insert two blank lines into the
    /// input box on launch. Pure-newline bursts must NOT count as paste.
    #[test]
    fn pure_newline_burst_is_not_paste() {
        assert!(!is_paste_burst(&['\n', '\n']));
        assert!(!is_paste_burst(&['\n', '\n', '\n']));
    }

    /// Whitespace-only bursts (newline + space, newline + tab) likewise
    /// fail the "real content" test — same root cause as the buffered-
    /// Enter case, just with adjacent whitespace instead.
    #[test]
    fn whitespace_only_burst_is_not_paste() {
        assert!(!is_paste_burst(&[' ', '\n']));
        assert!(!is_paste_burst(&['\t', '\n']));
        assert!(!is_paste_burst(&['\n', ' ', '\t', '\n']));
    }

    /// Real multi-line paste (text + embedded newline) must still be
    /// recognised — that's the entire reason the burst path exists for
    /// terminals without bracketed paste.
    #[test]
    fn text_with_newline_burst_is_paste() {
        assert!(is_paste_burst(&['h', 'i', '\n']));
        assert!(is_paste_burst(&['\n', 'h', 'i']));
        assert!(is_paste_burst(&['l', 'i', 'n', 'e', '1', '\n', 'l', 'i', 'n', 'e', '2']));
    }

    /// Bursts without any newline fall through to per-key handling
    /// regardless of length — just fast typing, not a paste signal.
    #[test]
    fn no_newline_burst_is_not_paste() {
        assert!(!is_paste_burst(&['a', 'b', 'c', 'd']));
    }

    /// Regression: JediTerm IME on Windows commits each Pinyin candidate
    /// as `<char> + Enter`, producing bursts of single-char-per-line.
    /// Old heuristic accepted these as pastes; the buffer ended up with
    /// `\n` between every CJK char and the input row showed `首↵页↵中↵…`.
    /// New rule: 3+ lines averaging ≤1 non-newline char per line is the
    /// IME pattern, not a paste.
    #[test]
    fn ime_commit_storm_is_not_paste() {
        // Real-world reproduction from the user screenshot: typing
        // `首页中的` via IME emits `首 \n 页 \n 中 \n 的 \n`.
        assert!(!is_paste_burst(&['首', '\n', '页', '\n', '中', '\n', '的', '\n']));
        // Bare CJK without trailing newline — same shape, also rejected.
        assert!(!is_paste_burst(&['首', '\n', '页', '\n', '中']));
        // ASCII char-per-line bursts also caught (rare keyboard
        // remapping but same root cause — phantom Enter between chars).
        assert!(!is_paste_burst(&['a', '\n', 'b', '\n', 'c', '\n']));
    }

    /// 2-line pastes with two short lines must still flow through the
    /// paste path — the IME-rejection threshold is gated on 3+ lines so
    /// legitimate short pastes aren't caught as collateral.
    #[test]
    fn two_line_short_paste_still_recognised() {
        assert!(is_paste_burst(&['a', '\n', 'b']));
    }

    /// Multi-line paste with substantial text per line stays a paste
    /// even when CJK is involved — char-per-line check counts NON-newline
    /// chars, so `你好世界 \n 再见` (7 non-newline + 1 newline = 2 lines,
    /// avg 3.5/line) sails through.
    #[test]
    fn cjk_multi_line_paste_still_recognised() {
        assert!(is_paste_burst(&['你', '好', '世', '界', '\n', '再', '见']));
    }

    /// Singleton "bursts" are never pastes; aggregation requires ≥ 2.
    #[test]
    fn singleton_burst_is_not_paste() {
        assert!(!is_paste_burst(&['\n']));
        assert!(!is_paste_burst(&['x']));
        assert!(!is_paste_burst(&[]));
    }

    /// Regression for the Windows-resize crash. `crossterm::event::poll`
    /// has been observed to return `Err` during terminal resize on
    /// Windows; the original loop `return`'d on Err, which killed the
    /// reader thread and collapsed the event loop ("atomcode exits
    /// when I resize on Windows"). `classify_poll` must classify
    /// `Err` as `Sleep` (loop again after a short delay), never `Exit`.
    #[test]
    fn classify_poll_err_is_sleep_not_exit() {
        // Real error construction — ErrorKind doesn't matter, the
        // classifier treats all Err the same.
        let boom = std::io::Error::new(std::io::ErrorKind::Other, "resize glitch");
        assert_eq!(classify_poll(Err(boom), false), PollAction::Sleep);
        let boom = std::io::Error::new(std::io::ErrorKind::Other, "another glitch");
        assert_eq!(
            classify_poll(Err(boom), true),
            PollAction::Sleep,
            "Err must NOT be Exit even when tx is closed — exit path \
             is only for clean shutdown via Ok(false) + closed tx"
        );
    }

    /// The three `Ok` branches must classify exactly one action each,
    /// and `Ok(false)` splits on `tx_closed` (the only place the
    /// reader self-terminates in the happy path).
    #[test]
    fn classify_poll_ok_branches() {
        assert_eq!(classify_poll(Ok(true), false), PollAction::Read);
        assert_eq!(
            classify_poll(Ok(true), true),
            PollAction::Read,
            "Ok(true) always reads — caller will notice tx closed on send"
        );
        assert_eq!(classify_poll(Ok(false), false), PollAction::Continue);
        assert_eq!(classify_poll(Ok(false), true), PollAction::Exit);
    }

    /// Dropping the sender side must terminate the worker even while paused.
    /// Without this the event-loop shutdown path would leak the thread on
    /// any session that ever called Pause.
    #[test]
    fn paused_worker_exits_on_sender_drop() {
        let (tx, _rx) = mpsc::unbounded_channel::<InputEvent>();
        let (cmd_tx, cmd_rx) = stdmpsc::channel();
        let worker = std::thread::spawn(move || run(tx, cmd_rx));

        let (ack_tx, ack_rx) = stdmpsc::channel();
        cmd_tx
            .send((ReaderCommand::Pause, Some(ack_tx)))
            .expect("send pause");
        ack_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("pause ACK");

        drop(cmd_tx); // Err on next recv → exit
        worker
            .join()
            .expect("paused worker joins after sender drop");
    }
}
