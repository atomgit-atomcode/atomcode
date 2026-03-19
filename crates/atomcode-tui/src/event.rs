use std::time::Duration;

use crossterm::event::{Event, EventStream, KeyEvent, MouseEvent, MouseEventKind};
use futures::StreamExt;
use tokio::sync::mpsc;

use atomcode_core::stream::TokenUsage;
use atomcode_core::tool::{ToolCall, ToolResult};

#[derive(Debug)]
pub enum AppEvent {
    Key(KeyEvent),
    StreamDelta(String),
    StreamToolCallStart { id: String, name: String },
    StreamToolCallDelta(String),
    StreamToolCallDone(ToolCall),
    StreamUsage(TokenUsage),
    StreamDone,
    StreamError(String),
    ToolFinished(ToolResult),
    ScrollUp(u16),
    ScrollDown(u16),
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
        // Async crossterm event reader — no blocking of the tokio runtime
        tokio::spawn(async move {
            let mut reader = EventStream::new();
            loop {
                match reader.next().await {
                    Some(Ok(evt)) => {
                        let app_event = match evt {
                            Event::Key(key) => AppEvent::Key(key),
                            Event::Mouse(MouseEvent { kind: MouseEventKind::ScrollUp, .. }) => {
                                AppEvent::ScrollUp(3)
                            }
                            Event::Mouse(MouseEvent { kind: MouseEventKind::ScrollDown, .. }) => {
                                AppEvent::ScrollDown(3)
                            }
                            Event::Resize(w, h) => AppEvent::Resize(w, h),
                            _ => continue,
                        };
                        if tx.send(app_event).is_err() {
                            break;
                        }
                    }
                    Some(Err(_)) => break,
                    None => break,
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

    /// Receive the next event (blocking).
    pub async fn next(&mut self) -> Option<AppEvent> {
        self.rx.recv().await
    }

    /// Try to receive a pending event without blocking.
    /// Returns None if no events are queued.
    pub fn try_next(&mut self) -> Option<AppEvent> {
        self.rx.try_recv().ok()
    }
}
