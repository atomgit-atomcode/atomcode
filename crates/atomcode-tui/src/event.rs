use std::time::Duration;

use crossterm::event::{self, Event, KeyEvent};
use tokio::sync::mpsc;

#[derive(Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    StreamDelta(String),
    StreamDone,
    StreamError(String),
    Resize(u16, u16),
    Tick,
}

pub struct EventLoop {
    rx: mpsc::UnboundedReceiver<AppEvent>,
    tx: mpsc::UnboundedSender<AppEvent>,
}

impl EventLoop {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self { rx, tx }
    }

    /// Returns a sender that LLM streaming tasks can use to push events.
    pub fn sender(&self) -> mpsc::UnboundedSender<AppEvent> {
        self.tx.clone()
    }

    /// Start polling crossterm events and tick timer in background tasks.
    pub fn start(&self) {
        let tx = self.tx.clone();
        // Crossterm event reader
        tokio::spawn(async move {
            loop {
                if event::poll(Duration::from_millis(50)).unwrap_or(false) {
                    if let Ok(evt) = event::read() {
                        let app_event = match evt {
                            Event::Key(key) => AppEvent::Key(key),
                            Event::Resize(w, h) => AppEvent::Resize(w, h),
                            _ => continue,
                        };
                        if tx.send(app_event).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let tx = self.tx.clone();
        // Tick timer (250ms)
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(250));
            loop {
                interval.tick().await;
                if tx.send(AppEvent::Tick).is_err() {
                    break;
                }
            }
        });
    }

    /// Receive the next event.
    pub async fn next(&mut self) -> Option<AppEvent> {
        self.rx.recv().await
    }
}
