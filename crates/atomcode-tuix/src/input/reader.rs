// crates/atomcode-tuix/src/input/reader.rs
use std::sync::mpsc::{self as stdmpsc, TryRecvError};
use std::time::Duration;

use crossterm::event::{self, Event};
use tokio::sync::mpsc;

use super::InputEvent;

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
    let (cmd_tx, cmd_rx) =
        stdmpsc::channel::<(ReaderCommand, Option<stdmpsc::Sender<()>>)>();
    let join = std::thread::spawn(move || run(tx, cmd_rx));
    ReaderHandle {
        join: Some(join),
        cmd_tx,
    }
}

fn run(
    tx: mpsc::UnboundedSender<InputEvent>,
    cmd_rx: stdmpsc::Receiver<(ReaderCommand, Option<stdmpsc::Sender<()>>)>,
) {
    let mut paused = false;
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

        match event::poll(Duration::from_millis(100)) {
            Ok(false) => {
                if tx.is_closed() {
                    return;
                }
                continue;
            }
            Ok(true) => {}
            Err(_) => {
                // Transient poll error. Windows crossterm has been
                // observed to fail `event::poll` / `event::read`
                // during terminal resize; returning here would kill
                // the reader thread and collapse the event loop
                // (input_rx channel closes, `maybe = None` → break).
                // Users report "atomcode exits when I resize on
                // Windows". Small sleep + continue absorbs the
                // glitch while staying responsive.
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
            Event::Mouse(_) | Event::FocusGained | Event::FocusLost => {
                continue;
            }
        };
        if tx.send(msg).is_err() {
            return;
        }
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
        worker.join().expect("paused worker joins after sender drop");
    }
}

