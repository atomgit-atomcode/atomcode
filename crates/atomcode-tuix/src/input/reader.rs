// crates/atomcode-tuix/src/input/reader.rs
use std::time::Duration;

use crossterm::event::{self, Event};
use tokio::sync::mpsc;

use super::InputEvent;

/// Spawn a blocking OS thread that reads crossterm events and forwards them
/// over `tx`. Returns a JoinHandle; the thread exits when `tx` is dropped
/// (send returns Err) or when a fatal read error occurs.
pub fn spawn(tx: mpsc::UnboundedSender<InputEvent>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        loop {
            match event::poll(Duration::from_millis(100)) {
                Ok(false) => {
                    if tx.is_closed() {
                        return;
                    }
                    continue;
                }
                Ok(true) => {}
                Err(_) => return,
            }
            let ev = match event::read() {
                Ok(e) => e,
                Err(_) => return,
            };
            let msg = match ev {
                Event::Key(k) => InputEvent::Key(k),
                Event::Paste(p) => InputEvent::Paste(p),
                Event::Resize(_, _) | Event::Mouse(_) | Event::FocusGained | Event::FocusLost => {
                    continue;
                }
            };
            if tx.send(msg).is_err() {
                return;
            }
        }
    })
}
