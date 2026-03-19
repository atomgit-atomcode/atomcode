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

    pub fn sender(&self) -> mpsc::UnboundedSender<AppEvent> {
        self.tx.clone()
    }

    pub fn start(&self) {
        let tx = self.tx.clone();
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
                            _ => continue, // Other mouse events (click/drag) ignored — terminal handles selection
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

    pub async fn next(&mut self) -> Option<AppEvent> {
        self.rx.recv().await
    }

    pub fn try_next(&mut self) -> Option<AppEvent> {
        self.rx.try_recv().ok()
    }
}
